//! Search roots and limits. Defaults cover the standard install; a
//! `config.json` in the plugin's config directory can override them:
//!
//! ```json
//! {"claude_roots": ["/home/me/.claude-work/projects"],
//!  "codex_roots": ["/home/me/.codex/sessions"],
//!  "max_payload_bytes": 262144}
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Bounds on what an adapter reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadLimits {
    pub max_file_bytes: u64,
    pub max_line_bytes: usize,
    /// Files looked at while searching for a session's transcript.
    pub max_candidates: usize,
    /// Waits before re-reading a transcript whose end is still being
    /// written. Their sum is the longest wait.
    pub retry_delays: Vec<Duration>,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 256 * 1024 * 1024,
            max_line_bytes: 8 * 1024 * 1024,
            max_candidates: 10_000,
            retry_delays: [100, 200, 400, 800, 1000, 500]
                .into_iter()
                .map(Duration::from_millis)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Directories holding `<project>/<session>.jsonl`.
    pub claude_roots: Vec<PathBuf>,
    /// Directories holding `YYYY/MM/DD/rollout-*.jsonl`.
    pub codex_roots: Vec<PathBuf>,
    pub limits: ReadLimits,
    /// The largest prompt (instruction plus answer) to send, in bytes.
    pub max_payload_bytes: usize,
}

impl Config {
    pub fn defaults(home: &Path) -> Self {
        Self {
            claude_roots: vec![home.join(".claude/projects")],
            codex_roots: vec![home.join(".codex/sessions")],
            limits: ReadLimits::default(),
            max_payload_bytes: 256 * 1024,
        }
    }

    /// The defaults with `config.json` from `dir` applied, if it exists.
    pub fn load(home: &Path, dir: Option<&Path>) -> Result<Self, String> {
        let mut config = Self::defaults(home);
        let Some(path) = dir.map(|d| d.join("config.json")) else {
            return Ok(config);
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(config),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let file: ConfigFile =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some(roots) = file.claude_roots {
            config.claude_roots = roots.iter().map(|r| expand_home(r, home)).collect();
        }
        if let Some(roots) = file.codex_roots {
            config.codex_roots = roots.iter().map(|r| expand_home(r, home)).collect();
        }
        if let Some(n) = file.max_payload_bytes {
            config.max_payload_bytes = n;
        }
        if let Some(n) = file.max_file_bytes {
            config.limits.max_file_bytes = n;
        }
        if let Some(n) = file.max_line_bytes {
            config.limits.max_line_bytes = n;
        }
        if let Some(n) = file.max_candidates {
            config.limits.max_candidates = n;
        }
        Ok(config)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    claude_roots: Option<Vec<String>>,
    codex_roots: Option<Vec<String>>,
    max_payload_bytes: Option<usize>,
    max_file_bytes: Option<u64>,
    max_line_bytes: Option<usize>,
    max_candidates: Option<usize>,
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_waits_stay_within_three_seconds() {
        let total: Duration = ReadLimits::default().retry_delays.iter().sum();
        assert!(total <= Duration::from_secs(3), "{total:?}");
    }

    #[test]
    fn config_file_overrides_roots() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"claude_roots": ["~/alt/projects"], "max_payload_bytes": 1000}"#,
        )
        .unwrap();
        let home = Path::new("/home/me");
        let config = Config::load(home, Some(dir.path())).unwrap();
        assert_eq!(
            config.claude_roots,
            [PathBuf::from("/home/me/alt/projects")]
        );
        assert_eq!(
            config.codex_roots,
            [PathBuf::from("/home/me/.codex/sessions")]
        );
        assert_eq!(config.max_payload_bytes, 1000);
    }

    #[test]
    fn unknown_config_keys_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), r#"{"claude_root": []}"#).unwrap();
        assert!(Config::load(Path::new("/h"), Some(dir.path())).is_err());
    }
}
