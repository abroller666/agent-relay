//! Requests to the Herdr socket API.
//!
//! Herdr answers one request per connection, so every call opens a fresh
//! Unix socket connection. Text goes as a JSON string inside the request;
//! nothing is ever passed through a shell.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::HandoffError;
use crate::model::PaneBinding;
use crate::session::binding_from_agent;

/// Herdr reads at most this many bytes of a request line
/// (`MAX_INITIAL_REQUEST_BYTES` in Herdr 0.9.3).
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

const TIMEOUT: Duration = Duration::from_secs(5);
/// `agent.prompt` answers after the text and Enter were written, which for
/// a long prompt takes a while; Herdr gives up on its side after 5 s.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(20);

/// An agent pane as Herdr reports it right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSnapshot {
    pub binding: PaneBinding,
    /// idle, working, blocked, done or unknown.
    pub agent_status: String,
    /// True only for agents started with `herdr agent start` once they are
    /// ready; Herdr omits it (false) for agents started by hand.
    pub interactive_ready: bool,
    /// A managed agent still starting up.
    pub launch_pending: bool,
    /// Changes whenever Herdr sees the agent's state change.
    pub state_change_seq: u64,
}

impl AgentSnapshot {
    /// The snapshot for an `AgentInfo` object.
    pub fn from_info(info: &Value, server_key: &str) -> Result<Self, HandoffError> {
        Ok(Self {
            binding: binding_from_agent(info, server_key)?,
            agent_status: info["agent_status"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            interactive_ready: info["interactive_ready"].as_bool().unwrap_or(false),
            launch_pending: info["launch_pending"].as_bool().unwrap_or(false),
            state_change_seq: info["state_change_seq"].as_u64().unwrap_or(0),
        })
    }

    /// Idle or done, and not a managed agent still starting.
    pub fn is_ready(&self) -> bool {
        matches!(self.agent_status.as_str(), "idle" | "done") && !self.launch_pending
    }
}

/// A pane of the tab, for the target list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PaneSummary {
    pub pane_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
}

/// Where a pane sits on screen, from `pane.layout`.
#[derive(Debug, Clone, Deserialize)]
pub struct Layout {
    pub area: Area,
    pub panes: Vec<PanePlace>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Area {
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PanePlace {
    pub pane_id: String,
    pub rect: Rect,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
}

/// What the handoff needs from Herdr. Tests use fakes.
pub trait HerdrApi {
    /// The normalized address of the Herdr server.
    fn server_key(&self) -> String;
    /// The raw `AgentInfo` of the agent in `pane_id`.
    fn agent_info(&self, pane_id: &str) -> Result<Value, HandoffError>;
    /// Submits `text` to the agent in `pane_id`, then Enter.
    fn prompt(&self, pane_id: &str, text: &str) -> Result<(), HandoffError>;
    fn list_panes(&self) -> Result<Vec<PaneSummary>, HandoffError>;
    fn layout(&self, pane_id: &str) -> Result<Layout, HandoffError>;
    /// (workspace id, label) of every workspace, in Herdr's order.
    fn workspace_labels(&self) -> Result<Vec<(String, String)>, HandoffError> {
        Ok(Vec::new())
    }

    /// The agent in `pane_id` with its session binding.
    fn agent(&self, pane_id: &str) -> Result<AgentSnapshot, HandoffError> {
        AgentSnapshot::from_info(&self.agent_info(pane_id)?, &self.server_key())
    }
}

pub struct HerdrClient {
    socket_path: String,
    server_key: String,
}

impl HerdrClient {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            server_key: server_key(Path::new(socket_path)),
        }
    }

    /// Plugin panes and actions always receive `HERDR_SOCKET_PATH`.
    pub fn from_env() -> Result<Self, HandoffError> {
        let path = std::env::var("HERDR_SOCKET_PATH").map_err(|_| {
            HandoffError::Herdr("HERDR_SOCKET_PATH is not set; run this as a Herdr plugin".into())
        })?;
        Ok(Self::new(&path))
    }

    /// Opens a pane entrypoint of `plugin_id` as a popup.
    pub fn open_popup(
        &self,
        plugin_id: &str,
        entrypoint: &str,
        width: Value,
        height: Value,
        env: &[(&str, &str)],
    ) -> Result<(), HandoffError> {
        let env: serde_json::Map<String, Value> = env
            .iter()
            .map(|(k, v)| ((*k).to_string(), Value::String((*v).to_string())))
            .collect();
        self.call(
            "plugin.pane.open",
            json!({
                "plugin_id": plugin_id,
                "entrypoint": entrypoint,
                "placement": "popup",
                "width": width,
                "height": height,
                "env": env,
            }),
        )
        .map(drop)
        .map_err(|e| e.into_herdr())
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        let mut req = json!({"id": "pane-relay", "method": method, "params": params}).to_string();
        req.push('\n');
        if req.len() > MAX_REQUEST_BYTES {
            return Err(CallError::TooLarge(req.len()));
        }
        let stream = UnixStream::connect(&self.socket_path)
            .map_err(|e| CallError::BeforeWrite(format!("cannot reach Herdr: {e}")))?;
        let timeout = if method == "agent.prompt" {
            PROMPT_TIMEOUT
        } else {
            TIMEOUT
        };
        stream.set_read_timeout(Some(timeout)).ok();
        stream.set_write_timeout(Some(timeout)).ok();
        (&stream)
            .write_all(req.as_bytes())
            .map_err(|e| CallError::AfterWrite(format!("{method}: {e}")))?;
        let mut line = String::new();
        BufReader::new(&stream)
            .read_line(&mut line)
            .map_err(|e| CallError::AfterWrite(format!("{method}: {e}")))?;
        let mut resp: Value = serde_json::from_str(&line)
            .map_err(|_| CallError::AfterWrite(format!("{method}: unexpected reply from Herdr")))?;
        if let Some(err) = resp.get("error") {
            return Err(CallError::Rejected {
                code: err["code"].as_str().unwrap_or_default().to_string(),
                message: err["message"].as_str().unwrap_or_default().to_string(),
            });
        }
        Ok(resp["result"].take())
    }
}

/// How a request failed, which decides whether input may have been written.
#[derive(Debug)]
enum CallError {
    TooLarge(usize),
    /// Nothing reached Herdr.
    BeforeWrite(String),
    /// The request was sent but no answer came back.
    AfterWrite(String),
    /// Herdr answered with an error.
    Rejected {
        code: String,
        message: String,
    },
}

impl CallError {
    /// For read-only requests nothing can have been written to a pane.
    fn into_herdr(self) -> HandoffError {
        match self {
            CallError::TooLarge(n) => HandoffError::PayloadTooLarge(format!("{n} bytes")),
            CallError::BeforeWrite(m) | CallError::AfterWrite(m) => HandoffError::Herdr(m),
            CallError::Rejected { code, message } => rejected(&code, &message),
        }
    }
}

fn rejected(code: &str, message: &str) -> HandoffError {
    match code {
        "agent_blocked" | "agent_not_ready" => HandoffError::AgentNotReady(message.to_string()),
        "agent_not_found" | "pane_not_found" => HandoffError::TargetChanged(message.to_string()),
        _ => HandoffError::Herdr(format!("{code}: {message}")),
    }
}

/// How a failed `agent.prompt` maps to an error. A timeout or a failure
/// reported after Herdr queued the input may have left text in the pane.
fn prompt_error(e: CallError) -> HandoffError {
    match e {
        CallError::AfterWrite(m) => HandoffError::DeliveryUnknown(m),
        CallError::Rejected { code, message }
            if code == "timeout" || code == "agent_prompt_failed" =>
        {
            HandoffError::DeliveryUnknown(format!("{code}: {message}"))
        }
        other => other.into_herdr(),
    }
}

impl HerdrApi for HerdrClient {
    fn server_key(&self) -> String {
        self.server_key.clone()
    }

    fn agent_info(&self, pane_id: &str) -> Result<Value, HandoffError> {
        let mut result =
            self.call("agent.get", json!({"target": pane_id}))
                .map_err(|e| match e {
                    CallError::Rejected { code, .. } if code == "agent_not_found" => {
                        HandoffError::UnsupportedAgent("no agent running".into())
                    }
                    other => other.into_herdr(),
                })?;
        Ok(result["agent"].take())
    }

    fn prompt(&self, pane_id: &str, text: &str) -> Result<(), HandoffError> {
        self.call("agent.prompt", json!({"target": pane_id, "text": text}))
            .map(drop)
            .map_err(prompt_error)
    }

    fn list_panes(&self) -> Result<Vec<PaneSummary>, HandoffError> {
        let mut result = self
            .call("pane.list", json!({}))
            .map_err(CallError::into_herdr)?;
        serde_json::from_value(result["panes"].take())
            .map_err(|e| HandoffError::Herdr(format!("pane.list: {e}")))
    }

    fn workspace_labels(&self) -> Result<Vec<(String, String)>, HandoffError> {
        let result = self
            .call("workspace.list", json!({}))
            .map_err(CallError::into_herdr)?;
        Ok(result["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|w| {
                let id = w["workspace_id"].as_str()?;
                let label = w["label"].as_str().filter(|l| !l.trim().is_empty());
                Some((id.to_string(), label.unwrap_or(id).to_string()))
            })
            .collect())
    }

    fn layout(&self, pane_id: &str) -> Result<Layout, HandoffError> {
        let mut result = self
            .call("pane.layout", json!({"pane_id": pane_id}))
            .map_err(CallError::into_herdr)?;
        serde_json::from_value(result["layout"].take())
            .map_err(|e| HandoffError::Herdr(format!("pane.layout: {e}")))
    }
}

/// The socket path with symbolic links resolved, so two spellings of one
/// server compare equal.
pub fn server_key(socket_path: &Path) -> String {
    std::fs::canonicalize(socket_path)
        .unwrap_or_else(|_| socket_path.to_path_buf())
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

    /// A one-shot fake Herdr: answers one connection with `reply` (or
    /// closes it without answering when `reply` is None).
    fn serve_once(reply: Option<&'static str>) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            if let Some(reply) = reply {
                (&stream).write_all(reply.as_bytes()).unwrap();
            }
        });
        let path = path.display().to_string();
        (dir, path)
    }

    #[test]
    fn prompt_accepted() {
        let (_dir, path) = serve_once(Some(
            "{\"id\":\"pane-relay\",\"result\":{\"type\":\"agent_prompted\"}}\n",
        ));
        HerdrClient::new(&path).prompt("w1:p1", "hi").unwrap();
    }

    #[test]
    fn no_answer_after_sending_is_delivery_unknown() {
        let (_dir, path) = serve_once(None);
        let err = HerdrClient::new(&path).prompt("w1:p1", "hi").unwrap_err();
        assert!(matches!(err, HandoffError::DeliveryUnknown(_)), "{err:?}");
    }

    #[test]
    fn herdr_timeout_is_delivery_unknown() {
        let (_dir, path) = serve_once(Some(
            "{\"id\":\"pane-relay\",\"error\":{\"code\":\"timeout\",\"message\":\"t\"}}\n",
        ));
        let err = HerdrClient::new(&path).prompt("w1:p1", "hi").unwrap_err();
        assert!(matches!(err, HandoffError::DeliveryUnknown(_)), "{err:?}");
    }

    #[test]
    fn blocked_agent_is_not_ready() {
        let (_dir, path) = serve_once(Some(
            "{\"id\":\"pane-relay\",\"error\":{\"code\":\"agent_blocked\",\"message\":\"b\"}}\n",
        ));
        let err = HerdrClient::new(&path).prompt("w1:p1", "hi").unwrap_err();
        assert!(matches!(err, HandoffError::AgentNotReady(_)), "{err:?}");
    }

    #[test]
    fn unreachable_herdr_is_not_a_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sock").display().to_string();
        let err = HerdrClient::new(&path).prompt("w1:p1", "hi").unwrap_err();
        assert!(matches!(err, HandoffError::Herdr(_)), "{err:?}");
    }

    #[test]
    fn request_over_herdr_limit_is_refused_before_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sock").display().to_string();
        let text = "x".repeat(MAX_REQUEST_BYTES);
        let err = HerdrClient::new(&path).prompt("w1:p1", &text).unwrap_err();
        assert!(matches!(err, HandoffError::PayloadTooLarge(_)), "{err:?}");
    }

    #[test]
    fn manually_started_agent_is_ready_without_interactive_ready() {
        let info = json!({
            "agent": "claude", "agent_status": "idle", "pane_id": "w1:p1",
            "tab_id": "w1:t1", "terminal_id": "t", "state_change_seq": 3,
            "agent_session": {"source": "herdr:claude", "agent": "claude",
                              "kind": "id", "value": "c07c63dd-285d-4412-bb32-5654dd417828"},
        });
        let snap = AgentSnapshot::from_info(&info, "srv").unwrap();
        assert!(snap.is_ready());
        assert!(!snap.interactive_ready);
        let mut pending = info.clone();
        pending["launch_pending"] = json!(true);
        assert!(
            !AgentSnapshot::from_info(&pending, "srv")
                .unwrap()
                .is_ready()
        );
        let mut working = info;
        working["agent_status"] = json!("working");
        assert!(
            !AgentSnapshot::from_info(&working, "srv")
                .unwrap()
                .is_ready()
        );
    }
}
