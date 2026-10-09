//! Codex panes on the shared app-server daemon: Herdr's session report is
//! unreliable there (the daemon runs Herdr's hook with another pane's
//! environment), so the pane's own terminal title, "<thread name> |
//! <project>", is matched against the daemon's loaded threads.

use std::cell::RefCell;
use std::path::PathBuf;

use agent_relay::codex_daemon::{
    CodexDaemon, DaemonError, DaemonThread, match_thread, resolve_agent,
};
use agent_relay::error::HandoffError;
use agent_relay::herdr::{HerdrApi, Layout, PaneProcess, PaneSummary};
use serde_json::{Value, json};

const CWD: &str = "/home/user/dev/app";
const T1: &str = "01a11c89-abb9-72f0-9466-e21f979ca373";
const T2: &str = "01a11c89-ca9f-7e43-9f03-71148f4500a8";
const T3: &str = "01a11c8a-0000-7000-8000-000000000003";

fn thread(id: &str, name: Option<&str>, cwd: &str) -> DaemonThread {
    DaemonThread {
        id: id.into(),
        name: name.map(str::to_string),
        cwd: Some(cwd.into()),
    }
}

#[test]
fn title_matches_the_one_thread_with_that_name_and_directory() {
    let threads = [
        thread(T1, Some("Fix the parser"), CWD),
        thread(T2, None, CWD),                              // not named yet
        thread(T3, Some("Fix the parser"), "/home/user/x"), // elsewhere
    ];
    assert_eq!(
        match_thread(&threads, "Fix the parser | app", CWD).unwrap(),
        T1
    );
}

#[test]
fn a_thread_name_may_contain_the_separator() {
    let threads = [thread(T1, Some("a | b"), CWD)];
    assert_eq!(match_thread(&threads, "a | b | app", CWD).unwrap(), T1);
}

#[test]
fn two_threads_with_the_same_name_are_ambiguous() {
    let threads = [
        thread(T1, Some("Fix the parser"), CWD),
        thread(T2, Some("Fix the parser"), CWD),
    ];
    let err = match_thread(&threads, "Fix the parser | app", CWD).unwrap_err();
    assert!(matches!(err, HandoffError::SessionAmbiguous(_)), "{err:?}");
}

#[test]
fn titles_that_do_not_name_a_thread_are_refused() {
    let threads = [thread(T1, Some("Fix the parser"), CWD)];
    // Before the first prompt the title is the project only; another
    // project; a custom title format; no matching thread.
    for title in [
        "app",
        "Fix the parser | other",
        "Fix the parser",
        "Other | app",
        "",
    ] {
        let err = match_thread(&threads, title, CWD).unwrap_err();
        assert!(
            matches!(err, HandoffError::SessionUnavailable(_)),
            "{title:?}: {err:?}"
        );
    }
}

struct Daemon {
    threads: RefCell<Result<Vec<DaemonThread>, DaemonError>>,
    /// The files the pane's Codex process (pid 42) has open.
    open: RefCell<Vec<PathBuf>>,
}

impl CodexDaemon for Daemon {
    fn loaded_threads(&self) -> Result<Vec<DaemonThread>, DaemonError> {
        self.threads.borrow().clone()
    }
    fn files_open_by(&self, pid: u32) -> Option<Vec<PathBuf>> {
        (pid == 42).then(|| self.open.borrow().clone())
    }
}

/// The rollout file of thread `id`, as Codex names it.
fn rollout(id: &str) -> PathBuf {
    PathBuf::from(format!(
        "/home/user/.codex/sessions/2026/10/09/rollout-2026-10-09T10-43-57-{id}.jsonl"
    ))
}

struct Herdr {
    info: RefCell<Value>,
    argv: RefCell<Vec<String>>,
    /// `pane.process_info` fails.
    no_process_info: RefCell<bool>,
}

impl HerdrApi for Herdr {
    fn server_key(&self) -> String {
        "/tmp/herdr.sock".into()
    }
    fn agent_info(&self, _: &str) -> Result<Value, HandoffError> {
        Ok(self.info.borrow().clone())
    }
    fn prompt(&self, _: &str, _: &str) -> Result<(), HandoffError> {
        unreachable!()
    }
    fn list_panes(&self) -> Result<Vec<PaneSummary>, HandoffError> {
        Ok(Vec::new())
    }
    fn layout(&self, _: &str) -> Result<Layout, HandoffError> {
        Err(HandoffError::Herdr("no layout".into()))
    }
    fn foreground_processes(&self, _: &str) -> Result<Vec<PaneProcess>, HandoffError> {
        if *self.no_process_info.borrow() {
            return Err(HandoffError::Herdr("process info unavailable".into()));
        }
        Ok(vec![PaneProcess {
            pid: 42,
            argv: self.argv.borrow().clone(),
        }])
    }
}

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

fn codex_pane(session: Option<&str>) -> Herdr {
    let mut info = json!({
        "agent": "codex", "agent_status": "idle", "pane_id": "w1:p5", "tab_id": "w1:t1",
        "terminal_id": "term_5", "state_change_seq": 4, "foreground_cwd": CWD, "cwd": CWD,
        "terminal_title": "Fix the parser | app",
        "terminal_title_stripped": "Fix the parser | app",
    });
    if let Some(s) = session {
        info["agent_session"] =
            json!({"source": "herdr:codex", "agent": "codex", "kind": "id", "value": s});
    }
    Herdr {
        info: RefCell::new(info),
        argv: RefCell::new(argv(&["codex"])),
        no_process_info: RefCell::new(false),
    }
}

fn daemon(threads: Vec<DaemonThread>) -> Daemon {
    Daemon {
        threads: RefCell::new(Ok(threads)),
        open: RefCell::new(Vec::new()),
    }
}

fn daemon_down(error: DaemonError) -> Daemon {
    Daemon {
        threads: RefCell::new(Err(error)),
        open: RefCell::new(Vec::new()),
    }
}

#[test]
fn unregistered_codex_pane_is_bound_through_its_title() {
    let herdr = codex_pane(None);
    let d = daemon(vec![
        thread(T1, Some("Fix the parser"), CWD),
        thread(T2, None, CWD),
    ]);
    let agent = resolve_agent(&herdr, Some(&d), "w1:p5").unwrap();
    assert_eq!(agent.binding.session.value, T1);
    assert_eq!(agent.binding.terminal_id, "term_5");
    assert!(agent.is_ready());
}

#[test]
fn a_herdr_session_on_the_daemon_must_agree_with_the_title() {
    // Herdr bound T2 to this pane, but the pane shows T1: the report came
    // from the daemon's borrowed environment. The pane's own title wins.
    let herdr = codex_pane(Some(T2));
    let d = daemon(vec![
        thread(T1, Some("Fix the parser"), CWD),
        thread(T2, Some("Other"), CWD),
    ]);
    let agent = resolve_agent(&herdr, Some(&d), "w1:p5").unwrap();
    assert_eq!(agent.binding.session.value, T1);

    // And when the title proves nothing, that report is not trusted.
    herdr.info.borrow_mut()["terminal_title_stripped"] = json!("app");
    let err = resolve_agent(&herdr, Some(&d), "w1:p5").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
}

#[test]
fn a_herdr_session_absent_from_the_daemon_is_not_proof_of_no_daemon() {
    // Herdr's report names a thread the daemon has since unloaded: it may
    // still be a wrong report. The pane's title decides.
    let herdr = codex_pane(Some(T3));
    let d = daemon(vec![thread(T1, Some("Fix the parser"), CWD)]);
    let agent = resolve_agent(&herdr, Some(&d), "w1:p5").unwrap();
    assert_eq!(agent.binding.session.value, T1);
    herdr.info.borrow_mut()["terminal_title_stripped"] = json!("app");
    let err = resolve_agent(&herdr, Some(&d), "w1:p5").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
}

#[test]
fn a_herdr_session_held_open_by_the_pane_is_trusted() {
    // `codex --no-daemon` writes its own rollout: the pane's process holds
    // the file of the thread Herdr names. A daemon thread of the same name
    // and directory is someone else's.
    let herdr = codex_pane(Some(T3));
    *herdr.argv.borrow_mut() = argv(&["codex", "--no-daemon"]);
    let d = daemon(vec![thread(T1, Some("Fix the parser"), CWD)]);
    *d.open.borrow_mut() = vec![PathBuf::from("/dev/null"), rollout(T3)];
    let agent = resolve_agent(&herdr, Some(&d), "w1:p5").unwrap();
    assert_eq!(agent.binding.session.value, T3);

    // After a rewind it writes a segment of the same thread.
    *d.open.borrow_mut() = vec![PathBuf::from(format!(
        "/home/user/.codex/sessions/2026/10/09/rollout-2026-10-09T10-50-00-{T3}_{T2}.jsonl"
    ))];
    assert_eq!(
        resolve_agent(&herdr, Some(&d), "w1:p5")
            .unwrap()
            .binding
            .session
            .value,
        T3
    );
}

#[test]
fn a_no_daemon_pane_overwritten_from_the_daemon_is_refused() {
    // The pane that once started the daemon now runs `codex --no-daemon`
    // (its own thread T3); the daemon, still holding that pane's
    // environment, reported another pane's thread T2 for it.
    let herdr = codex_pane(Some(T2));
    *herdr.argv.borrow_mut() = argv(&["codex", "--no-daemon"]);
    let d = daemon(vec![
        thread(T2, Some("Other"), CWD),
        thread(T1, Some("Fix the parser"), CWD),
    ]);
    *d.open.borrow_mut() = vec![rollout(T3)];
    let err = resolve_agent(&herdr, Some(&d), "w1:p5").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
    // Its title must not bind it to a daemon thread either.
    let unregistered = codex_pane(None);
    *unregistered.argv.borrow_mut() = argv(&["codex", "--no-daemon"]);
    let err = resolve_agent(&unregistered, Some(&d), "w1:p5").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
}

#[test]
fn without_a_daemon_herdr_still_needs_proof() {
    // A report made while a daemon ran may outlive it.
    let d = daemon_down(DaemonError::NotRunning);
    let herdr = codex_pane(Some(T3));
    let err = resolve_agent(&herdr, Some(&d), "w1:p5").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
    *d.open.borrow_mut() = vec![rollout(T3)];
    assert_eq!(
        resolve_agent(&herdr, Some(&d), "w1:p5")
            .unwrap()
            .binding
            .session
            .value,
        T3
    );
    let unregistered = codex_pane(None);
    let err = resolve_agent(&unregistered, Some(&d), "w1:p5").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
}

#[test]
fn a_daemon_that_cannot_be_asked_stops_the_handoff() {
    // A timeout or protocol change is not "no daemon": Herdr's report may
    // be the wrong one, so nothing is trusted.
    let failing = daemon_down(DaemonError::Failed("timed out".into()));
    let herdr = codex_pane(Some(T3));
    let err = resolve_agent(&herdr, Some(&failing), "w1:p5").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
}

#[test]
fn claude_panes_never_ask_the_daemon() {
    let herdr = Herdr {
        info: RefCell::new(json!({
            "agent": "claude", "agent_status": "idle", "pane_id": "w1:p1", "tab_id": "w1:t1",
            "terminal_id": "term_1",
            "agent_session": {"source": "herdr:claude", "agent": "claude", "kind": "id",
                              "value": "c07c63dd-285d-4412-bb32-5654dd417828"},
        })),
        argv: RefCell::new(argv(&["claude"])),
        no_process_info: RefCell::new(false),
    };
    let d = daemon_down(DaemonError::Failed("must not be called".into()));
    let agent = resolve_agent(&herdr, Some(&d), "w1:p1").unwrap();
    assert_eq!(
        agent.binding.session.value,
        "c07c63dd-285d-4412-bb32-5654dd417828"
    );
}

#[test]
fn an_unknown_launch_mode_never_reaches_the_title_match() {
    // The pane may run `codex --no-daemon` (thread T3) while the daemon
    // has a thread of the same name and directory (T1): without process
    // information that cannot be ruled out.
    let d = daemon(vec![thread(T1, Some("Fix the parser"), CWD)]);
    let herdr = codex_pane(None);
    *herdr.no_process_info.borrow_mut() = true;
    let err = resolve_agent(&herdr, Some(&d), "w1:p5").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
    // Herdr may leave argv out.
    let herdr = codex_pane(None);
    *herdr.argv.borrow_mut() = Vec::new();
    let err = resolve_agent(&herdr, Some(&d), "w1:p5").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
    // A foreground process that is not Codex at all.
    let herdr = codex_pane(None);
    *herdr.argv.borrow_mut() = argv(&["zsh"]);
    let err = resolve_agent(&herdr, Some(&d), "w1:p5").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
}

#[test]
fn codex_installed_through_npm_is_recognized() {
    let d = daemon(vec![thread(T1, Some("Fix the parser"), CWD)]);
    let herdr = codex_pane(None);
    *herdr.argv.borrow_mut() = argv(&[
        "node",
        "/usr/local/lib/node_modules/@openai/codex/bin/codex.js",
    ]);
    assert_eq!(
        resolve_agent(&herdr, Some(&d), "w1:p5")
            .unwrap()
            .binding
            .session
            .value,
        T1
    );
    *herdr.argv.borrow_mut() = argv(&[
        "node",
        "/usr/local/lib/node_modules/@openai/codex/bin/codex.js",
        "--no-daemon",
    ]);
    assert!(resolve_agent(&herdr, Some(&d), "w1:p5").is_err());
}
