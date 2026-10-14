//! Why a file is not being synced right now.
//!
//! On Linux this scans `/proc/*/fd` and `/proc/*/maps` for the path (filtering
//! out our own pid) and matches `/proc/<pid>/comm` and executable names against
//! the folder's hold globs; other Unix platforms fall back to `lsof`/`ps` when
//! available, and always to `quiet_timeout_secs`.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::policy::HoldRule;

/// Above this many dirty files in one folder, per-file fd scanning yields to
/// the quiet timer so a churn storm does not walk `/proc` per file.
pub const MAX_FD_SCAN_FILES: usize = 50;

/// The reason a path is held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeldReason {
    /// A process has the file open or mapped (by pid).
    FileOpen { pid: u32 },
    /// A process matching a hold glob is running.
    ProcessRunning { name: String },
    /// Holder detection was unavailable or skipped; waiting out the quiet timeout.
    QuietPeriod,
}

impl HeldReason {
    /// Machine-readable code stored in `agent.db` (`held_by_pid:1234`,
    /// `process:retroarch`, `quiet_until`).
    pub fn code(&self) -> String {
        match self {
            Self::FileOpen { pid } => format!("held_by_pid:{pid}"),
            Self::ProcessRunning { name } => format!("process:{name}"),
            Self::QuietPeriod => "quiet_until".to_string(),
        }
    }
}

impl std::fmt::Display for HeldReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.code())
    }
}

/// Inspect the system for an open-file hold on `path`.
pub fn held(path: &Path) -> Option<HeldReason> {
    check_file(path, &HoldRule::default(), false)
}

/// Check whether any running process matches `rule.process_globs`.
pub fn check_processes(rule: &HoldRule) -> Option<HeldReason> {
    if rule.process_globs.is_empty() {
        return None;
    }
    matching_process(rule).map(|name| HeldReason::ProcessRunning { name })
}

/// Check whether `path` should be held under an `on_close` policy.
///
/// - When `rule.quiet_timeout_secs > 0`, a file whose mtime is at least that old
///   is released even if a holder is still present (`quiet_timeout_secs` bounds
///   every hold).
/// - Matching `rule.process_globs` holds the file with [`HeldReason::ProcessRunning`].
/// - When holder detection is supported and `!skip_fd_scan`, an open descriptor
///   or memory map holds the file with [`HeldReason::FileOpen`]; if no process
///   holds it, the file is released immediately without waiting out the quiet
///   timeout.
/// - When holder detection is unavailable or skipped (`skip_fd_scan`), a
///   configured `quiet_timeout_secs` holds the file with
///   [`HeldReason::QuietPeriod`] until its mtime ages past the timeout.
pub fn check_file(path: &Path, rule: &HoldRule, skip_fd_scan: bool) -> Option<HeldReason> {
    let quiet_elapsed = if rule.quiet_timeout_secs > 0 {
        file_age_secs(path).is_some_and(|age| age >= rule.quiet_timeout_secs)
    } else {
        false
    };
    if quiet_elapsed {
        return None;
    }

    if let Some(reason) = check_processes(rule) {
        return Some(reason);
    }

    if !skip_fd_scan {
        match open_holder(path) {
            Detection::Supported(Some(pid)) => return Some(HeldReason::FileOpen { pid }),
            Detection::Supported(None) => return None,
            Detection::Unavailable => {}
        }
    }

    if rule.quiet_timeout_secs > 0 {
        return Some(HeldReason::QuietPeriod);
    }
    None
}

fn file_age_secs(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|d| d.as_secs())
}

enum Detection {
    Supported(Option<u32>),
    Unavailable,
}

#[cfg(target_os = "linux")]
fn open_holder(path: &Path) -> Detection {
    use std::os::unix::fs::MetadataExt as _;

    let proc_dir = Path::new("/proc");
    if !proc_dir.is_dir() {
        return Detection::Unavailable;
    }
    let Ok(target_meta) = fs::metadata(path) else {
        return Detection::Supported(None);
    };
    let target_key = (target_meta.dev(), target_meta.ino());
    let canonical = fs::canonicalize(path).ok();
    let canonical_str = canonical
        .as_ref()
        .and_then(|p| p.to_str())
        .map(str::to_owned);
    let ino_str = target_meta.ino().to_string();
    let self_pid = std::process::id();

    let Ok(entries) = fs::read_dir(proc_dir) else {
        return Detection::Unavailable;
    };

    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let pid_path = entry.path();

        // 1. Open file descriptors (`/proc/<pid>/fd/*`).
        if let Ok(fds) = fs::read_dir(pid_path.join("fd")) {
            for fd in fds.flatten() {
                let fd_path = fd.path();
                if let Ok(meta) = fs::metadata(&fd_path)
                    && (meta.dev(), meta.ino()) == target_key
                {
                    return Detection::Supported(Some(pid));
                }
                if let (Some(expected), Ok(link)) = (&canonical, fs::read_link(&fd_path))
                    && &link == expected
                {
                    return Detection::Supported(Some(pid));
                }
            }
        }

        // 2. Memory-mapped files (`/proc/<pid>/maps`).
        if let Ok(maps) = fs::read_to_string(pid_path.join("maps")) {
            for line in maps.lines() {
                let mut fields = line.split_whitespace();
                // address perms offset dev inode pathname
                let _ = (fields.next(), fields.next(), fields.next(), fields.next());
                let Some(inode) = fields.next() else {
                    continue;
                };
                if inode == ino_str {
                    return Detection::Supported(Some(pid));
                }
                if let Some(expected) = &canonical_str
                    && line.ends_with(expected.as_str())
                {
                    return Detection::Supported(Some(pid));
                }
            }
        }
    }

    Detection::Supported(None)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_holder(path: &Path) -> Detection {
    use std::process::Command;

    let output = match Command::new("lsof").arg("-t").arg("--").arg(path).output() {
        Ok(output) => output,
        Err(_) => return Detection::Unavailable,
    };
    let self_pid = std::process::id();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Ok(pid) = line.trim().parse::<u32>()
            && pid != self_pid
        {
            return Detection::Supported(Some(pid));
        }
    }
    Detection::Supported(None)
}

#[cfg(not(unix))]
fn open_holder(_path: &Path) -> Detection {
    Detection::Unavailable
}

#[cfg(target_os = "linux")]
fn matching_process(rule: &HoldRule) -> Option<String> {
    let entries = fs::read_dir("/proc").ok()?;
    let self_pid = std::process::id();

    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let pid_path = entry.path();

        if let Ok(comm) = fs::read_to_string(pid_path.join("comm")) {
            let name = comm.trim();
            if !name.is_empty() && rule.holds_process(name) {
                return Some(name.to_string());
            }
        }
        if let Ok(exe) = fs::read_link(pid_path.join("exe"))
            && let Some(name) = exe.file_name().and_then(|n| n.to_str())
            && rule.holds_process(name)
        {
            return Some(name.to_string());
        }
        if let Ok(cmdline) = fs::read(pid_path.join("cmdline")) {
            for arg in cmdline.split(|b| *b == 0) {
                if arg.is_empty() {
                    continue;
                }
                let text = String::from_utf8_lossy(arg);
                let base = Path::new(text.as_ref())
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(text.as_ref());
                if !base.is_empty() && rule.holds_process(base) {
                    return Some(base.to_string());
                }
            }
        }
    }
    None
}

#[cfg(all(unix, not(target_os = "linux")))]
fn matching_process(rule: &HoldRule) -> Option<String> {
    use std::process::Command;

    let output = Command::new("ps")
        .args(["-Ao", "pid=,comm="])
        .output()
        .ok()?;
    let self_pid = std::process::id();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        let Some((pid_str, comm)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        if pid_str.trim().parse::<u32>().ok() == Some(self_pid) {
            continue;
        }
        let comm = comm.trim();
        let base = Path::new(comm)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(comm);
        if !base.is_empty() && rule.holds_process(base) {
            return Some(base.to_string());
        }
    }
    None
}

#[cfg(not(unix))]
fn matching_process(_rule: &HoldRule) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_reason_codes_match_the_schema_format() {
        assert_eq!(
            HeldReason::FileOpen { pid: 4123 }.code(),
            "held_by_pid:4123"
        );
        assert_eq!(
            HeldReason::ProcessRunning {
                name: "retroarch".into()
            }
            .code(),
            "process:retroarch"
        );
        assert_eq!(HeldReason::QuietPeriod.code(), "quiet_until");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_process_holding_an_fd_is_detected_until_it_closes() {
        use std::process::{Command, Stdio};

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("save.dat");
        std::fs::write(&file, b"initial").unwrap();

        // Unheld when only our own process touches it.
        assert_eq!(held(&file), None);

        // Spawn a child `sleep` with stdin redirected from `save.dat`, so the
        // child holds an open fd on the file.
        let opened = std::fs::File::open(&file).unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::from(opened))
            .spawn()
            .unwrap();
        let child_pid = child.id();

        let reason = held(&file);
        assert_eq!(reason, Some(HeldReason::FileOpen { pid: child_pid }));

        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(held(&file), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_running_process_matching_a_hold_glob_is_detected() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("save.dat");
        std::fs::write(&file, b"state").unwrap();

        let mut child = Command::new("sleep").arg("30").spawn().unwrap();

        let rule = HoldRule::new(vec!["sleep*".into()], 0);
        let reason = check_file(&file, &rule, false);
        assert_eq!(
            reason,
            Some(HeldReason::ProcessRunning {
                name: "sleep".into()
            })
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn quiet_timeout_bounds_a_hold_and_covers_skipped_fd_scans() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("save.dat");
        std::fs::write(&file, b"state").unwrap();

        // When fd scanning is skipped (e.g. >50 dirty files), the quiet timeout
        // holds a freshly written file.
        let rule = HoldRule::new(Vec::new(), 60);
        assert_eq!(
            check_file(&file, &rule, true),
            Some(HeldReason::QuietPeriod)
        );

        // Once the file's age exceeds quiet_timeout_secs, the hold expires.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let short_rule = HoldRule::new(Vec::new(), 1);
        assert_eq!(check_file(&file, &short_rule, true), None);
    }
}
