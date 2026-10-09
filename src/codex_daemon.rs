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
use crate::herdr::{AgentSnapshot, HerdrApi};

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
}

/// The agent in `pane_id`, with its session bound as described above.
pub fn resolve_agent(
    herdr: &dyn HerdrApi,
    daemon: Option<&dyn CodexDaemon>,
    pane_id: &str,
) -> Result<AgentSnapshot, HandoffError> {
    let mut info = herdr.agent_info(pane_id)?;
    let server_key = herdr.server_key();
    let reported = AgentSnapshot::from_info(&info, &server_key);
    let Some(daemon) = daemon.filter(|_| info["agent"] == "codex") else {
        return reported;
    };
    // Herdr's report is trusted only where Herdr's hook ran in the pane
    // itself: Codex started with --no-daemon, or no daemon at all.
    if started_without_daemon(herdr, pane_id) {
        return reported;
    }
    let threads = match daemon.loaded_threads() {
        Ok(threads) => threads,
        Err(DaemonError::NotRunning) => return reported,
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
    let thread = match_thread(&threads, &title, &cwd)?;
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
    let unknown = || {
        HandoffError::SessionUnavailable(
            "cannot tell which Codex thread this pane shows; send it one prompt, or start Codex with `codex --no-daemon`".into(),
        )
    };
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
    let found: Vec<&DaemonThread> = threads
        .iter()
        .filter(|t| t.name.as_deref().map(str::trim) == Some(name))
        .filter(|t| t.cwd.as_deref().is_some_and(|c| same_dir(c, cwd)))
        .collect();
    match found.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(unknown()),
        many => Err(HandoffError::SessionAmbiguous(format!(
            "{} Codex threads in this directory are named {name:?}; rename one",
            many.len()
        ))),
    }
}

/// Whether the pane's Codex was started with `--no-daemon`.
fn started_without_daemon(herdr: &dyn HerdrApi, pane_id: &str) -> bool {
    herdr.foreground_argv(pane_id).is_ok_and(|procs| {
        procs.iter().any(|argv| {
            let is_codex = argv
                .first()
                .and_then(|a| Path::new(a).file_name())
                .is_some_and(|n| n.to_string_lossy().starts_with("codex"));
            is_codex && argv.iter().skip(1).any(|a| a == "--no-daemon")
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
    fn no_daemon_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            DaemonClient::at(dir.path().join("none.sock")).loaded_threads(),
            Err(DaemonError::NotRunning)
        );
    }
}
