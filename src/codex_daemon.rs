//! Codex panes on the shared app-server daemon.
//!
//! Since Codex 0.157 every Codex TUI attaches by default to one machine-wide
//! app-server daemon, which runs lifecycle hooks with the environment of
//! whichever terminal started it. Herdr's Codex hook then reports sessions
//! for the wrong pane or not at all (herdrdev/herdr#4649, openai/codex#48500).
//!
//! What a pane does show reliably is its own terminal title, which the Codex
//! TUI sets to "<thread name> | <project>". A pane is bound to a daemon
//! thread only when exactly one loaded thread has that name and the pane's
//! working directory; otherwise nothing is guessed. A Herdr session report
//! naming a daemon thread is only trusted when the title agrees; one naming
//! a thread the daemon does not run (`codex --no-daemon`) came from the pane
//! itself and is trusted as before.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::HandoffError;
use crate::herdr::{AgentSnapshot, HerdrApi, PaneProcess};
use crate::model::{AgentKind, SessionRef};
use crate::session::{occupant_binding, rollout_segment};

/// A thread loaded in the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonThread {
    pub id: String,
    pub name: Option<String>,
    pub cwd: Option<String>,
}

/// Why the daemon's threads are not known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    /// Nothing listens on its socket: every Codex runs in its own process.
    NotRunning,
    /// It runs but could not be asked (timeout, protocol change).
    Failed(String),
}

pub trait CodexDaemon {
    /// The threads the daemon has loaded.
    fn loaded_threads(&self) -> Result<Vec<DaemonThread>, DaemonError>;
    /// The files process `pid` has open; None when that cannot be told.
    fn files_open_by(&self, _pid: u32) -> Option<Vec<PathBuf>> {
        None
    }
}

/// The agent in `pane_id` as the target of a handoff. Nothing is read from
/// a target, and the prompt goes to the pane, so a Codex target needs no
/// thread: it is bound to the Codex process running in the pane, and a
/// restarted Codex is another target. (Switching threads inside one Codex
/// with `/new` or `/resume` is not seen.) Other agents are bound as by
/// `resolve_agent`.
pub fn resolve_target(
    herdr: &dyn HerdrApi,
    daemon: Option<&dyn CodexDaemon>,
    pane_id: &str,
) -> Result<AgentSnapshot, HandoffError> {
    let info = herdr.agent_info(pane_id)?;
    if info["agent"] != "codex" {
        return resolve_agent(herdr, daemon, pane_id);
    }
    let unseen =
        || HandoffError::SessionUnavailable("cannot see the Codex process in this pane".into());
    let processes = herdr.foreground_processes(pane_id).map_err(|_| unseen())?;
    let pids: Vec<String> = processes
        .iter()
        .filter(|p| is_codex(p))
        .map(|p| p.pid.to_string())
        .collect();
    if pids.is_empty() {
        return Err(unseen());
    }
    let session = SessionRef {
        kind: "process".into(),
        value: pids.join(","),
    };
    let binding = occupant_binding(&info, &herdr.server_key(), AgentKind::Codex, session)?;
    Ok(AgentSnapshot::with_binding(&info, binding))
}

/// The agent in `pane_id`, with its session bound as described above.
pub fn resolve_agent(
    herdr: &dyn HerdrApi,
    daemon: Option<&dyn CodexDaemon>,
    pane_id: &str,
) -> Result<AgentSnapshot, HandoffError> {
    resolve_agent_with(herdr, daemon, pane_id, &|_| None)
}

/// `resolve_agent`, where `tiebreak` may name which of the threads sharing
/// the pane's title it shows (see `shown_on_screen`); None leaves it
/// undecided.
pub fn resolve_agent_with(
    herdr: &dyn HerdrApi,
    daemon: Option<&dyn CodexDaemon>,
    pane_id: &str,
    tiebreak: &dyn Fn(&[String]) -> Option<String>,
) -> Result<AgentSnapshot, HandoffError> {
    let mut info = herdr.agent_info(pane_id)?;
    let server_key = herdr.server_key();
    let reported = AgentSnapshot::from_info(&info, &server_key);
    let Some(daemon) = daemon.filter(|_| info["agent"] == "codex") else {
        return reported;
    };
    // How this pane's Codex was started decides what can be trusted; when
    // that cannot be seen, nothing is (it may run --no-daemon beside a
    // daemon thread of the same name).
    let launch_unknown = || {
        HandoffError::SessionUnavailable(
            "cannot tell how Codex was started in this pane (no process information)".into(),
        )
    };
    let processes = herdr
        .foreground_processes(pane_id)
        .map_err(|_| launch_unknown())?;
    let codex: Vec<&PaneProcess> = processes.iter().filter(|p| is_codex(p)).collect();
    if codex.is_empty() {
        return Err(launch_unknown());
    }
    // Herdr's report stands only when the pane's own Codex process writes
    // the thread it names. `codex --no-daemon` writes its rollout itself;
    // on the daemon, the daemon does. Where it came from cannot be told
    // otherwise: the daemon may have reported another pane's thread here.
    if let Ok(agent) = &reported {
        if holds_rollout(daemon, &codex, &agent.binding.session.value) {
            return reported;
        }
    }
    if codex
        .iter()
        .any(|p| p.argv.iter().skip(1).any(|a| a == "--no-daemon"))
    {
        return Err(HandoffError::SessionUnavailable(
            "Herdr's session for this Codex pane is not the one it runs; restart Codex in this pane".into(),
        ));
    }
    let threads = match daemon.loaded_threads() {
        Ok(threads) => threads,
        Err(DaemonError::NotRunning) => {
            return Err(HandoffError::SessionUnavailable(
                "cannot confirm which Codex session this pane runs; send it one prompt, or restart Codex in this pane".into(),
            ));
        }
        Err(DaemonError::Failed(e)) => {
            return Err(HandoffError::SessionUnavailable(format!(
                "cannot ask the Codex daemon which thread this pane shows ({e})"
            )));
        }
    };
    let str_field = |key: &str| info[key].as_str().filter(|s| !s.trim().is_empty());
    let title = str_field("terminal_title_stripped")
        .or(str_field("terminal_title"))
        .unwrap_or_default()
        .to_string();
    let cwd = str_field("foreground_cwd")
        .or(str_field("cwd"))
        .unwrap_or_default()
        .to_string();
    let thread = match match_thread(&threads, &title, &cwd) {
        Err(HandoffError::ThreadNameShared(name)) => {
            let ids = matching_threads(&threads, &title, &cwd)?;
            tiebreak(&ids)
                .filter(|id| ids.contains(id))
                .ok_or(HandoffError::ThreadNameShared(name))?
        }
        found => found?,
    };
    info["agent_session"] =
        json!({"source": "herdr:codex", "agent": "codex", "kind": "id", "value": thread});
    AgentSnapshot::from_info(&info, &server_key)
}

/// The one thread named by a Codex terminal title "<name> | <project>" whose
/// working directory is `cwd`.
pub fn match_thread(
    threads: &[DaemonThread],
    title: &str,
    cwd: &str,
) -> Result<String, HandoffError> {
    match matching_threads(threads, title, cwd)?.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(HandoffError::ThreadNameShared(title_name(title, cwd)?)),
    }
}

/// The name in a Codex terminal title "<name> | <project>", where
/// <project> is the last component of `cwd`.
fn title_name(title: &str, cwd: &str) -> Result<String, HandoffError> {
    let unknown = unknown_thread;
    let project = Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(unknown)?;
    let name = title
        .trim()
        .strip_suffix(&format!(" | {project}"))
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(unknown)?;
    Ok(name.to_string())
}

/// The threads named by a Codex terminal title whose working directory is
/// `cwd`: at least one.
fn matching_threads(
    threads: &[DaemonThread],
    title: &str,
    cwd: &str,
) -> Result<Vec<String>, HandoffError> {
    let name = title_name(title, cwd)?;
    let found: Vec<String> = threads
        .iter()
        .filter(|t| t.name.as_deref().map(str::trim) == Some(name.as_str()))
        .filter(|t| t.cwd.as_deref().is_some_and(|c| same_dir(c, cwd)))
        .map(|t| t.id.clone())
        .collect();
    if found.is_empty() {
        return Err(unknown_thread());
    }
    Ok(found)
}

fn unknown_thread() -> HandoffError {
    HandoffError::SessionUnavailable(
        "cannot tell which Codex thread this pane shows; send it one prompt, or start Codex with `codex --no-daemon`".into(),
    )
}

/// Lines shorter than this, in letters and digits, are too common to tell
/// two answers apart ("Done.", "Hello! How can I help?").
const DISTINCT_LETTERS: usize = 16;

/// Which of `answers` (thread id, latest answer) the pane whose screen text
/// is `screen` shows, if exactly one. An answer counts as shown when its
/// last line of some length is on screen and occurs in no other answer:
/// the latest answer is the last thing Codex prints, so its end stays in
/// view (unless the pane was scrolled back, and then nothing matches). Text is compared by its letters and digits only, which survive
/// the TUI's wrapping, indentation and Markdown rendering. A quote of
/// another answer shown together with the pane's own makes two, and
/// nothing is decided.
pub fn shown_on_screen(screen: &str, answers: &[(String, String)]) -> Option<String> {
    let screen = letters(screen);
    let all: Vec<String> = answers.iter().map(|(_, text)| letters(text)).collect();
    let mut shown = answers.iter().enumerate().filter_map(|(i, (id, text))| {
        let last = text
            .lines()
            .rev()
            .map(letters)
            .find(|l| l.chars().count() >= DISTINCT_LETTERS)?;
        let distinct = all
            .iter()
            .enumerate()
            .all(|(j, other)| j == i || !other.contains(&last));
        (distinct && screen.contains(&last)).then(|| id.clone())
    });
    let first = shown.next()?;
    shown.next().is_none().then_some(first)
}

/// `s` reduced to its letters and digits, lowercased.
fn letters(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `p` is Codex: its program, or the script its interpreter runs
/// (`node …/codex.js` when installed through npm), is named codex.
fn is_codex(p: &PaneProcess) -> bool {
    p.argv.iter().take(2).any(|a| {
        Path::new(a)
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("codex"))
    })
}

/// Whether one of `processes` has a rollout file of thread `id` open.
fn holds_rollout(daemon: &dyn CodexDaemon, processes: &[&PaneProcess], id: &str) -> bool {
    processes.iter().any(|p| {
        daemon.files_open_by(p.pid).is_some_and(|files| {
            files.iter().any(|f| {
                f.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| rollout_segment(n, id).is_some())
            })
        })
    })
}

fn same_dir(a: &str, b: &str) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

/// The local daemon, reached through its control socket.
pub struct DaemonClient {
    socket: PathBuf,
}

impl DaemonClient {
    /// The daemon of `$CODEX_HOME` (default `~/.codex`).
    pub fn from_env(home: &Path) -> Self {
        let codex_home =
            std::env::var_os("CODEX_HOME").map_or_else(|| home.join(".codex"), PathBuf::from);
        Self::at(codex_home.join("app-server-control/app-server-control.sock"))
    }

    pub fn at(socket: PathBuf) -> Self {
        Self { socket }
    }
}

impl CodexDaemon for DaemonClient {
    fn loaded_threads(&self) -> Result<Vec<DaemonThread>, DaemonError> {
        let stream = match UnixStream::connect(&self.socket) {
            Ok(stream) => stream,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Err(DaemonError::NotRunning);
            }
            Err(e) => return Err(daemon_error(e)),
        };
        let mut ws = WsJsonRpc::handshake(stream)?;
        ws.call(
            "initialize",
            json!({"clientInfo": {"name": "agent-relay", "version": env!("CARGO_PKG_VERSION")}}),
        )?;
        ws.notify("initialized")?;
        let mut ids = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = ws.call("thread/loaded/list", json!({"cursor": cursor}))?;
            for id in page["data"].as_array().into_iter().flatten() {
                if let Some(id) = id.as_str() {
                    ids.push(id.to_string());
                }
            }
            match page["nextCursor"].as_str() {
                Some(next) if ids.len() < 1000 => cursor = Some(next.to_string()),
                _ => break,
            }
        }
        let mut threads = Vec::new();
        for id in ids {
            let read = ws.call(
                "thread/read",
                json!({"threadId": id, "includeTurns": false}),
            )?;
            let t = &read["thread"];
            threads.push(DaemonThread {
                id,
                name: t["name"].as_str().map(str::to_string),
                cwd: t["cwd"].as_str().map(str::to_string),
            });
        }
        Ok(threads)
    }

    fn files_open_by(&self, pid: u32) -> Option<Vec<PathBuf>> {
        open_files(pid)
    }
}

/// The files process `pid` has open: `/proc` on Linux, `lsof` elsewhere.
fn open_files(pid: u32) -> Option<Vec<PathBuf>> {
    let proc_fd = PathBuf::from(format!("/proc/{pid}/fd"));
    if let Ok(entries) = std::fs::read_dir(&proc_fd) {
        return Some(
            entries
                .filter_map(Result::ok)
                .filter_map(|e| std::fs::read_link(e.path()).ok())
                .collect(),
        );
    }
    let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap_or("lsof");
    let out = std::process::Command::new(lsof)
        .args(["-a", "-w", "-Fn", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix('n'))
            .map(PathBuf::from)
            .collect(),
    )
}

const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MESSAGE: usize = 16 * 1024 * 1024;

fn daemon_error(what: impl std::fmt::Display) -> DaemonError {
    DaemonError::Failed(what.to_string())
}

/// JSON-RPC over a WebSocket on a Unix socket, as the Codex app-server
/// speaks it. Just enough of RFC 6455 for one client.
struct WsJsonRpc {
    stream: UnixStream,
    buf: Vec<u8>,
    next_id: u64,
}

impl WsJsonRpc {
    fn handshake(stream: UnixStream) -> Result<Self, DaemonError> {
        stream.set_read_timeout(Some(TIMEOUT)).ok();
        stream.set_write_timeout(Some(TIMEOUT)).ok();
        let mut ws = Self {
            stream,
            buf: Vec::new(),
            next_id: 0,
        };
        let key = base64(&random_bytes::<16>());
        let request = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        ws.stream
            .write_all(request.as_bytes())
            .map_err(daemon_error)?;
        let end = loop {
            if let Some(i) = ws.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            if ws.buf.len() > 64 * 1024 {
                return Err(daemon_error("oversized handshake"));
            }
            ws.fill()?;
        };
        let head = String::from_utf8_lossy(&ws.buf[..end]).to_string();
        ws.buf.drain(..end + 4);
        if !head.starts_with("HTTP/1.1 101") {
            return Err(daemon_error(format!(
                "handshake refused: {}",
                head.lines().next().unwrap_or_default()
            )));
        }
        Ok(ws)
    }

    fn fill(&mut self) -> Result<(), DaemonError> {
        let mut chunk = [0u8; 64 * 1024];
        let n = self.stream.read(&mut chunk).map_err(daemon_error)?;
        if n == 0 {
            return Err(daemon_error("connection closed"));
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<Vec<u8>, DaemonError> {
        while self.buf.len() < n {
            self.fill()?;
        }
        Ok(self.buf.drain(..n).collect())
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), DaemonError> {
        let mut frame = vec![0x80 | opcode];
        match payload.len() {
            n if n < 126 => frame.push(0x80 | n as u8),
            n if n < 65536 => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let mask = random_bytes::<4>();
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame).map_err(daemon_error)
    }

    /// The next text message.
    fn recv(&mut self) -> Result<Value, DaemonError> {
        let mut message = Vec::new();
        loop {
            let head = self.take(2)?;
            let (fin, opcode) = (head[0] & 0x80 != 0, head[0] & 0x0f);
            let masked = head[1] & 0x80 != 0;
            let len = match head[1] & 0x7f {
                126 => u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as usize,
                127 => u64::from_be_bytes(self.take(8)?.try_into().unwrap()) as usize,
                n => n as usize,
            };
            if message.len() + len > MAX_MESSAGE {
                return Err(daemon_error("oversized message"));
            }
            let mask = if masked { self.take(4)? } else { Vec::new() };
            let mut payload = self.take(len)?;
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            match opcode {
                0x9 => self.send_frame(0xA, &payload)?, // ping
                0xA => {}                               // pong
                0x8 => return Err(daemon_error("closed by the daemon")),
                0x0..=0x2 => {
                    message.extend_from_slice(&payload);
                    if fin {
                        return serde_json::from_slice(&message).map_err(daemon_error);
                    }
                }
                other => return Err(daemon_error(format!("unexpected frame {other}"))),
            }
        }
    }

    fn notify(&mut self, method: &str) -> Result<(), DaemonError> {
        let msg = json!({"jsonrpc": "2.0", "method": method}).to_string();
        self.send_frame(0x1, msg.as_bytes())
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, DaemonError> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_frame(0x1, msg.to_string().as_bytes())?;
        // Notifications may arrive before the answer.
        for _ in 0..10_000 {
            let mut m = self.recv()?;
            if m["id"] != id {
                continue;
            }
            if let Some(e) = m.get("error") {
                return Err(daemon_error(format!("{method}: {e}")));
            }
            return Ok(m["result"].take());
        }
        Err(daemon_error(format!("{method}: no answer")))
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut out))
        .is_err()
    {
        // Masking keys need not be secret; only distinct.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for (i, b) in out.iter_mut().enumerate() {
            *b = (seed >> ((i % 16) * 8)) as u8 ^ i as u8;
        }
    }
    out
}

fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, b| n << 8 | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn base64_encodes() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"hi"), "aGk=");
        assert_eq!(base64(b"abc"), "YWJj");
    }

    /// A one-connection fake daemon: answers the handshake, then each
    /// request with `reply(method, params)`, sending a notification first.
    fn fake_daemon(reply: fn(&str, &Value) -> Value) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                s.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n")
                .unwrap();
            let send = |s: &mut UnixStream, v: Value| {
                let p = v.to_string().into_bytes();
                let mut f = vec![0x81];
                if p.len() < 126 {
                    f.push(p.len() as u8);
                } else {
                    f.push(126);
                    f.extend_from_slice(&(p.len() as u16).to_be_bytes());
                }
                f.extend_from_slice(&p);
                s.write_all(&f).unwrap();
            };
            loop {
                let mut h = [0u8; 2];
                if s.read_exact(&mut h).is_err() {
                    return;
                }
                let mut len = (h[1] & 0x7f) as usize;
                if len == 126 {
                    let mut l = [0u8; 2];
                    s.read_exact(&mut l).unwrap();
                    len = u16::from_be_bytes(l) as usize;
                }
                let mut mask = [0u8; 4];
                s.read_exact(&mut mask).unwrap();
                let mut p = vec![0u8; len];
                s.read_exact(&mut p).unwrap();
                for (i, b) in p.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
                let m: Value = serde_json::from_slice(&p).unwrap();
                if m.get("id").is_none() {
                    continue;
                }
                send(&mut s, json!({"method": "noise", "params": {}}));
                let result = reply(m["method"].as_str().unwrap(), &m["params"]);
                send(&mut s, json!({"id": m["id"], "result": result}));
            }
        });
        (dir, path)
    }

    #[test]
    fn lists_loaded_threads_over_websocket() {
        let (_dir, path) = fake_daemon(|method, params| match method {
            "initialize" => json!({"userAgent": "fake"}),
            "thread/loaded/list" => json!({"data": ["t1", "t2"], "nextCursor": null}),
            "thread/read" => {
                let id = params["threadId"].as_str().unwrap();
                json!({"thread": {"id": id, "name": format!("name {id}"), "cwd": "/w"}})
            }
            _ => json!(null),
        });
        let threads = DaemonClient::at(path).loaded_threads().unwrap();
        assert_eq!(
            threads,
            [
                DaemonThread {
                    id: "t1".into(),
                    name: Some("name t1".into()),
                    cwd: Some("/w".into())
                },
                DaemonThread {
                    id: "t2".into(),
                    name: Some("name t2".into()),
                    cwd: Some("/w".into())
                },
            ]
        );
    }

    #[test]
    fn open_files_lists_a_file_this_process_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held.jsonl");
        let _held = std::fs::File::create(&path).unwrap();
        let files = open_files(std::process::id()).expect("open files of this process");
        let want = std::fs::canonicalize(&path).unwrap();
        assert!(
            files
                .iter()
                .any(|f| std::fs::canonicalize(f).ok().as_ref() == Some(&want)),
            "{files:?}"
        );
    }

    #[test]
    fn no_daemon_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            DaemonClient::at(dir.path().join("none.sock")).loaded_threads(),
            Err(DaemonError::NotRunning)
        );
    }
}
