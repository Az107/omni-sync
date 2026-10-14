//! Per-folder sync policy: `live` or `on_close`, plus the process holds that
//! defer a folder while a game (or anything else) is running.

/// How a folder reacts to a change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncMode {
    /// Hash and push on any change, debounced. Obsidian vaults.
    #[default]
    Live,
    /// Hold a changed file until no process has it open. Game saves.
    OnClose,
}

impl SyncMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "live" => Some(Self::Live),
            "on_close" | "on-close" => Some(Self::OnClose),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::OnClose => "on_close",
        }
    }
}

impl std::fmt::Display for SyncMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SyncMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
            .ok_or_else(|| format!("unknown sync mode '{s}' (expected 'live' or 'on_close')"))
    }
}

/// Folder-level holds that defer both uploads and applies.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HoldRule {
    /// Process-name globs (`steam*`, `retroarch`) matched against `comm`/exe.
    pub process_globs: Vec<String>,
    /// Safety net when holder detection is unavailable on this platform.
    pub quiet_timeout_secs: u64,
}

impl HoldRule {
    pub fn new(process_globs: Vec<String>, quiet_timeout_secs: u64) -> Self {
        Self {
            process_globs,
            quiet_timeout_secs,
        }
    }

    /// Does `name` match any configured process glob?
    pub fn holds_process(&self, name: &str) -> bool {
        self.process_globs.iter().any(|glob| glob_match(glob, name))
    }

    /// Is any configured hold present at all?
    pub fn is_active(&self) -> bool {
        !self.process_globs.is_empty() || self.quiet_timeout_secs > 0
    }
}

/// A minimal glob: `*` matches any run of characters, everything else literal.
/// Deliberately not a full glob engine — process names do not need one, and a
/// small matcher is easier to reason about than `globset` pulled in for `steam*`.
fn glob_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_globs_match_prefixes_and_exact_names() {
        let rule = HoldRule::new(vec!["steam*".into(), "retroarch".into()], 30);
        assert!(rule.holds_process("steam"));
        assert!(rule.holds_process("steamwebhelper"));
        assert!(
            rule.holds_process("steamcmd-other"),
            "a prefix glob matches any suffix"
        );
        assert!(rule.holds_process("retroarch"));
        assert!(!rule.holds_process("obsidian"));
        assert!(!rule.holds_process("discord"));
    }

    #[test]
    fn a_prefix_glob_does_not_match_a_shorter_name() {
        let rule = HoldRule::new(vec!["retroarch*".into()], 0);
        assert!(!rule.holds_process("retro"));
        assert!(rule.holds_process("retroarch"));
    }
}
