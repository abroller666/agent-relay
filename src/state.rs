//! What the plugin action hands the popup, and the popup's progress, kept
//! in one file per launch. Each launch gets its own key (server, source
//! terminal, random id), so popups of other Herdr servers or other launches
//! never read each other's file. The file holds the answer and the
//! instruction, so it is private (0600 in a 0700 directory), replaced
//! atomically, and removed when the popup closes; leftovers of crashed
//! popups are removed after a day.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::model::{AnswerSnapshot, PaneBinding};
use crate::ui::Screen;

const PREFIX: &str = "pane-relay-";
const TMP_PREFIX: &str = ".tmp-pane-relay-";

/// How long a state file may outlive its popup.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PopupState {
    /// This launch's key (see `op_key`).
    pub op: String,
    /// Pane A, fixed when the shortcut was pressed.
    pub source: Option<PaneBinding>,
    /// Why A could not be fixed, if it could not.
    pub source_error: Option<String>,
    pub answer: Option<AnswerSnapshot>,
    pub target: Option<PaneBinding>,
    pub instruction: String,
    /// Char index of the cursor in `instruction`.
    pub cursor: usize,
    pub screen: Screen,
}

impl PopupState {
    pub fn new(op: &str, source: Option<PaneBinding>) -> Self {
        Self {
            op: op.to_string(),
            source,
            source_error: None,
            answer: None,
            target: None,
            instruction: String::new(),
            cursor: 0,
            screen: Screen::Loading,
        }
    }
}

/// A new key for one launch from `terminal_id` on `server_key`.
pub fn op_key(server_key: &str, terminal_id: &str) -> String {
    let server = &crate::adapters::fingerprint(&[server_key])[..12];
    let terminal: String = terminal_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(40)
        .collect();
    format!("{server}-{terminal}-{}", random_hex(16))
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let filled = fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !filled {
        // Not secret, only distinct: fall back to time and process id.
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let seed = format!("{}-{}", now.as_nanos(), std::process::id());
        return crate::adapters::fingerprint(&[&seed])[..bytes * 2].to_string();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// The file of launch `op` in `dir`.
pub fn path(dir: &Path, op: &str) -> Result<PathBuf, String> {
    let ok = !op.is_empty()
        && op.len() <= 128
        && op
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        return Err(format!("不正な起動ID: {op:?}"));
    }
    Ok(dir.join(format!("{PREFIX}{op}.json")))
}

pub fn save(dir: &Path, state: &PopupState) -> Result<(), String> {
    let target = path(dir, &state.op)?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!("{TMP_PREFIX}{}-{}", state.op, random_hex(4)));
    let text = serde_json::to_vec(state).map_err(|e| e.to_string())?;
    let write = || -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&text)?;
        f.sync_all()?;
        fs::rename(&tmp, &target)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", target.display())
    })
}

pub fn load(dir: &Path, op: &str) -> Result<Option<PopupState>, String> {
    let path = path(dir, op)?;
    let text = match fs::read(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let state: PopupState =
        serde_json::from_slice(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if state.op != op {
        return Err(format!("{}: 起動IDが一致しません", path.display()));
    }
    Ok(Some(state))
}

/// Removes the file of launch `op`.
pub fn clear(dir: &Path, op: &str) {
    if let Ok(path) = path(dir, op) {
        let _ = fs::remove_file(path);
    }
}

/// Removes this plugin's state files older than `max_age`; other files in
/// `dir` are left alone.
pub fn sweep(dir: &Path, max_age: Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ours =
            (name.starts_with(PREFIX) && name.ends_with(".json")) || name.starts_with(TMP_PREFIX);
        if !ours {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > max_age));
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}
