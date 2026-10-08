//! Herdr plugin entry points.
//!
//! - `agent-relay open` (the plugin action) fixes the focused pane as the
//!   source, saves that for this launch and opens the popup.
//! - `agent-relay` is the popup: it reads the source's last answer, lets the
//!   user pick the target and type an instruction, and sends it once.

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use nix::sys::termios::{self, FlushArg, SetArg, Termios};
use serde_json::json;

use agent_relay::adapters::DefaultAdapters;
use agent_relay::codex_daemon::{DaemonClient, resolve_agent};
use agent_relay::config::Config;
use agent_relay::handoff::HandoffService;
use agent_relay::herdr::{HerdrApi, HerdrClient};
use agent_relay::state::{self, PopupState};
use agent_relay::ui::{Flow, LiveService, Popup, Screen};

/// Popup width, as in herdr-plugin.toml.
const POPUP_WIDTH: &str = "60%";
const OP_ENV: &str = "AGENT_RELAY_OP";
/// How long to wait for the rest of a split escape sequence before taking
/// it as a lone Esc.
const ESCAPE_TIMEOUT_MS: i32 = 50;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("open") {
        if let Err(e) = open() {
            eprintln!("agent-relay: {e}");
            std::process::exit(1);
        }
        return;
    }
    let raw = RawMode::enable().unwrap_or_else(|e| fail(&e));
    let result = run_popup();
    drop(raw);
    if let Err(e) = result {
        let _raw = RawMode::enable();
        fail(&e);
    }
}

fn state_dir() -> PathBuf {
    let base = std::env::var_os("HERDR_PLUGIN_STATE_DIR").map_or_else(
        || std::env::temp_dir().join(format!("agent-relay-{}", unsafe { nix::libc::getuid() })),
        PathBuf::from,
    );
    base.join("popups")
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn config() -> Result<Config, String> {
    let dir = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR").map(PathBuf::from);
    Config::load(&home(), dir.as_deref())
}

/// The plugin action: fixes the focused pane as the source and opens the
/// popup for it.
fn open() -> Result<(), String> {
    let herdr = HerdrClient::from_env().map_err(|e| e.to_string())?;
    let plugin_id = std::env::var("HERDR_PLUGIN_ID").map_err(|_| "HERDR_PLUGIN_ID is not set")?;
    let pane_id = std::env::var("HERDR_PANE_ID")
        .map_err(|_| "HERDR_PANE_ID is not set: run this from a focused pane")?;
    let dir = state_dir();
    state::sweep(&dir, state::MAX_AGE);

    let daemon = DaemonClient::from_env(&home());
    let (source, error, terminal) = match resolve_agent(&herdr, Some(&daemon), &pane_id) {
        Ok(agent) => {
            let terminal = agent.binding.terminal_id.clone();
            (Some(agent.binding), None, terminal)
        }
        Err(e) => (None, Some(e.to_string()), pane_id.clone()),
    };
    let op = state::op_key(&herdr.server_key(), &terminal);
    let mut st = PopupState::new(&op, source);
    st.source_error = error;
    state::save(&dir, &st)?;

    let height = herdr
        .layout(&pane_id)
        .map(|l| popup_height(l.area.height as usize))
        .unwrap_or(16);
    let opened = herdr.open_popup(
        &plugin_id,
        "console",
        json!(POPUP_WIDTH),
        json!(height),
        &[(OP_ENV, &op)],
    );
    if let Err(e) = opened {
        state::clear(&dir, &op);
        return Err(e.to_string());
    }
    Ok(())
}

/// Popup height: 60% of the screen, at least 12 rows when they fit.
fn popup_height(screen_rows: usize) -> usize {
    (screen_rows * 3 / 5)
        .max(12)
        .min(screen_rows.saturating_sub(2))
        .max(6)
}

fn run_popup() -> Result<(), String> {
    let op = std::env::var(OP_ENV).map_err(|_| "no launch id; open this from the plugin action")?;
    let dir = state_dir();
    let state = state::load(&dir, &op)?.ok_or("the state of this launch is missing")?;
    let result = popup_loop(&dir, state);
    state::clear(&dir, &op);
    result
}

fn popup_loop(dir: &Path, state: PopupState) -> Result<(), String> {
    let config = config()?;
    let herdr = HerdrClient::from_env().map_err(|e| e.to_string())?;
    let adapters = DefaultAdapters;
    let daemon = DaemonClient::from_env(&home());
    let svc = LiveService {
        herdr: &herdr,
        adapters: &adapters,
        config: &config,
        daemon: Some(&daemon),
    };
    let mut popup = Popup::new(state, &svc);
    let mut screen = Screen::Loading;
    print(&popup.render(terminal_size().0, terminal_size().1));

    if popup.screen() == Screen::Loading {
        // Read the answer on another thread so Ctrl+G still works.
        let source = popup.source().cloned().ok_or("no source pane")?;
        let (tx, rx) = mpsc::channel();
        let thread_config = config.clone();
        std::thread::spawn(move || {
            let result = HerdrClient::from_env().and_then(|herdr| {
                let daemon = DaemonClient::from_env(&home());
                HandoffService::new(&herdr, &DefaultAdapters, &thread_config)
                    .with_codex_daemon(&daemon)
                    .prepare(source)
            });
            let _ = tx.send(result);
        });
        loop {
            match rx.recv_timeout(Duration::from_millis(30)) {
                Ok(result) => {
                    popup.loaded(result);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("reading the answer failed unexpectedly".into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if readable_within(0) {
                let mut buf = [0u8; 256];
                let n = std::io::stdin().read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 || popup.feed(&buf[..n]) == Flow::Quit {
                    return Ok(());
                }
            }
        }
    }

    let mut buf = [0u8; 8192];
    let mut stdin = std::io::stdin().lock();
    loop {
        if popup.screen() != screen {
            screen = popup.screen();
            // Keep the launch's progress (answer, target, instruction) on disk.
            let _ = state::save(dir, popup.state());
        }
        let (cols, rows) = terminal_size();
        print(&popup.render(cols, rows));
        let flow = if popup.has_pending_input() && !readable_within(ESCAPE_TIMEOUT_MS) {
            popup.expire()
        } else {
            let n = stdin.read(&mut buf).map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                return Ok(());
            }
            popup.feed(&buf[..n])
        };
        if popup.take_discard_input() {
            // Keys pressed while sending (a repeated Enter) are dropped.
            let _ = termios::tcflush(std::io::stdin().as_fd(), FlushArg::TCIFLUSH);
        }
        if flow == Flow::Quit {
            return Ok(());
        }
    }
}

fn print(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// Raw mode with bracketed paste on; restored when dropped, including on
/// panic.
struct RawMode(Termios);

impl RawMode {
    fn enable() -> Result<Self, String> {
        let stdin = std::io::stdin();
        let saved = termios::tcgetattr(stdin.as_fd()).map_err(|e| format!("tcgetattr: {e}"))?;
        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &raw)
            .map_err(|e| format!("tcsetattr: {e}"))?;
        print("\x1b[?2004h");
        Ok(Self(saved))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        print("\x1b[?2004l\x1b[?7h\x1b[?25h");
        let _ = termios::tcsetattr(std::io::stdin().as_fd(), SetArg::TCSADRAIN, &self.0);
    }
}

/// Terminal size as (columns, rows), or 80x16 if it cannot be read.
fn terminal_size() -> (usize, usize) {
    let mut ws: nix::libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ only writes into the winsize struct we pass.
    let ok =
        unsafe { nix::libc::ioctl(nix::libc::STDOUT_FILENO, nix::libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        (usize::from(ws.ws_col), usize::from(ws.ws_row))
    } else {
        (80, 16)
    }
}

/// Whether stdin has input within `ms` milliseconds.
fn readable_within(ms: i32) -> bool {
    let mut fds = nix::libc::pollfd {
        fd: nix::libc::STDIN_FILENO,
        events: nix::libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll only reads and writes the one pollfd we pass.
    unsafe { nix::libc::poll(&mut fds, 1, ms) > 0 }
}

/// Shows an error until a key is pressed; otherwise the popup would close
/// before it can be read.
fn fail(msg: &str) -> ! {
    print(&format!(
        "\x1b[2J\x1b[H\x1b[31magent-relay: {msg}\x1b[0m\r\n\x1b[2mpress any key to close\x1b[0m"
    ));
    let _ = std::io::stdin().read(&mut [0u8; 1]);
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_fits_the_screen() {
        assert_eq!(popup_height(60), 36);
        assert_eq!(popup_height(16), 12);
        assert_eq!(popup_height(10), 8);
        assert_eq!(popup_height(5), 6);
    }
}
