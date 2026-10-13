//! Magic-link enrollment.
//!
//! A link from the dashboard looks like `<server>/enroll?code=ABCD-EFGHJ`.
//! With a browser we start a single-shot loopback listener, hand it to the
//! server page as `cb`, and let the page bounce the one-time code straight
//! back to us. Without a browser (`--no-browser`) we just read the code out of
//! the link and enroll directly.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use rand::Rng as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// The two pieces a magic link carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MagicLink {
    pub server: String,
    pub code: String,
}

/// Parse `<server>/enroll?code=...`, tolerating surrounding quotes and a
/// fragment. Returns the server base (deployment prefix included) and the code.
pub fn parse(link: &str) -> Result<MagicLink> {
    let link = link.trim().trim_matches(|c| c == '\'' || c == '"');
    let without_fragment = link.split('#').next().unwrap_or(link);
    let (base, query) = match without_fragment.split_once('?') {
        Some((base, query)) => (base, query),
        None => (without_fragment, ""),
    };

    let server = base
        .strip_suffix("/enroll/")
        .or_else(|| base.strip_suffix("/enroll"))
        .ok_or_else(|| {
            anyhow!("that is not an enroll link (expected .../enroll?code=...): {link}")
        })?;
    if !(server.starts_with("http://") || server.starts_with("https://")) {
        return Err(anyhow!("the enroll link must be an http(s) URL"));
    }

    let code = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "code")
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("the enroll link has no ?code= pairing code"))?;

    Ok(MagicLink {
        server: server.trim_end_matches('/').to_string(),
        code,
    })
}

/// Resolve the code for a magic link. Opens a browser and waits for the
/// loopback handoff, unless `no_browser`, in which case the link's own code is
/// used directly.
pub async fn resolve(link: &str, no_browser: bool, timeout: Duration) -> Result<MagicLink> {
    let parsed = parse(link)?;
    if no_browser {
        return Ok(parsed);
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("binding a loopback listener")?;
    let port = listener
        .local_addr()
        .context("reading the loopback port")?
        .port();

    let mut nonce_bytes = [0u8; 16];
    rand::rng().fill(&mut nonce_bytes);
    let nonce = hex::encode(nonce_bytes);

    let callback = format!("http://127.0.0.1:{port}");
    let url = format!(
        "{}/enroll?code={}&cb={}&state={}",
        parsed.server, parsed.code, callback, nonce
    );

    match opener::open(&url) {
        Ok(()) => println!("opened the enroll page in your browser"),
        Err(error) => println!(
            "could not open a browser ({error}); open this URL to finish enrolling:\n  {url}"
        ),
    }

    let code = wait_for_callback(listener, &nonce, timeout).await?;
    Ok(MagicLink {
        server: parsed.server,
        code,
    })
}

/// Accept connections until one hands back the code with our nonce. Anything
/// else (a favicon request, a stray page) is politely refused and ignored.
async fn wait_for_callback(
    listener: TcpListener,
    nonce: &str,
    timeout: Duration,
) -> Result<String> {
    let work = async {
        loop {
            let (mut socket, _) = listener.accept().await?;
            let mut buffer = [0u8; 4096];
            let read = socket.read(&mut buffer).await?;
            let request = String::from_utf8_lossy(&buffer[..read]);
            let target = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");

            let mut code = None;
            let mut state = None;
            if let Some((_, query)) = target.split_once('?') {
                for pair in query.split('&') {
                    if let Some((key, value)) = pair.split_once('=') {
                        match key {
                            "code" => code = Some(value.to_string()),
                            "state" => state = Some(value.to_string()),
                            _ => {}
                        }
                    }
                }
            }

            let accepted = state.as_deref() == Some(nonce)
                && code.as_deref().is_some_and(|code| !code.is_empty());
            let (status, body) = if accepted {
                ("200 OK", DONE_PAGE)
            } else {
                ("400 Bad Request", FAIL_PAGE)
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;

            if accepted {
                return Ok::<String, std::io::Error>(code.unwrap_or_default());
            }
        }
    };

    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| anyhow!("timed out waiting for the browser to finish enrolling"))?
        .context("the loopback listener failed")
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>omni-sync</title>\
<body style=\"font-family:system-ui;max-width:32rem;margin:4rem auto;padding:0 1rem\">\
<h1>Code received</h1><p>Your terminal is finishing the enrollment. You can close this tab \
once it prints that this device is enrolled; then run <code>omni-sync add</code> to pick a \
folder.</p></body>";

const FAIL_PAGE: &str = "<!doctype html><meta charset=utf-8><title>omni-sync</title>\
<body style=\"font-family:system-ui;max-width:32rem;margin:4rem auto;padding:0 1rem\">\
<h1>Not this request</h1><p>This endpoint is waiting for the enrollment page from your \
omni-sync server. Open the magic link there.</p></body>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_link_parses() {
        let link = parse("https://sync.example.com/enroll?code=ABCD-EFGHJ").unwrap();
        assert_eq!(link.server, "https://sync.example.com");
        assert_eq!(link.code, "ABCD-EFGHJ");
    }

    #[test]
    fn a_subpath_deployment_and_trailing_slash_parse() {
        let link = parse("https://host/app/omni-sync/enroll/?code=ABCD-EFGHJ").unwrap();
        assert_eq!(link.server, "https://host/app/omni-sync");
        assert_eq!(link.code, "ABCD-EFGHJ");
    }

    #[test]
    fn quotes_fragments_and_extra_params_are_tolerated() {
        let link = parse("'https://host/enroll?code=XY#anchor'").unwrap();
        assert_eq!(link.code, "XY");
        let link = parse("\"http://127.0.0.1:8787/enroll?cb=x&code=CODE\"").unwrap();
        assert_eq!(link.server, "http://127.0.0.1:8787");
        assert_eq!(link.code, "CODE");
    }

    #[test]
    fn malformed_links_are_rejected() {
        assert!(parse("https://host/devices?code=X").is_err());
        assert!(parse("not a url").is_err());
        assert!(parse("https://host/enroll").is_err());
        assert!(parse("ftp://host/enroll?code=X").is_err());
    }
}
