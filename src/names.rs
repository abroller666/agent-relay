//! How panes are named on screen and in the prompt: the name given in
//! Herdr, else the agent and the working directory. Never the pane id,
//! which Herdr does not show.

use crate::herdr::{HerdrApi, PaneSummary};
use crate::model::{AgentKind, PaneBinding};

/// The name of `pane`, looked up in Herdr; just the agent if Herdr does
/// not list it.
pub fn pane_display(herdr: &dyn HerdrApi, pane: &PaneBinding) -> String {
    herdr
        .list_panes()
        .ok()
        .and_then(|panes| panes.into_iter().find(|p| p.pane_id == pane.pane_id))
        .map_or_else(
            || pane.agent.display_name().to_string(),
            |p| summary_name(&p),
        )
}

/// `display_name` for a pane as `pane.list` reports it.
pub fn summary_name(p: &PaneSummary) -> String {
    let agent = p.agent.as_deref().unwrap_or_default();
    let agent = match AgentKind::from_herdr(agent) {
        Some(kind) => kind.display_name(),
        None => agent,
    };
    let cwd = p.foreground_cwd.as_deref().or(p.cwd.as_deref());
    let home = std::env::var("HOME").unwrap_or_default();
    display_name(p.label.as_deref(), agent, cwd, &home)
}

/// The pane's Herdr name if it has one, else the agent and the working
/// directory (home shown as `~`).
pub fn display_name(label: Option<&str>, agent: &str, cwd: Option<&str>, home: &str) -> String {
    if let Some(label) = label.map(str::trim).filter(|l| !l.is_empty()) {
        return label.to_string();
    }
    let cwd = cwd.map(|c| tilde(c, home)).unwrap_or_default();
    [agent, cwd.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `path` with a leading `home` replaced by `~`.
fn tilde(path: &str, home: &str) -> String {
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_prefers_the_pane_name() {
        let home = "/Users/me";
        assert_eq!(display_name(Some("api"), "Codex", Some("/x"), home), "api");
        assert_eq!(
            display_name(Some("  "), "Claude Code", Some("/Users/me/dev/app"), home),
            "Claude Code ~/dev/app"
        );
        assert_eq!(
            display_name(None, "Codex", Some("/Users/meg"), home),
            "Codex /Users/meg"
        );
        assert_eq!(display_name(None, "Codex", None, home), "Codex");
    }
}
