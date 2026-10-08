//! Hands the last finished answer of the AI agent in one Herdr pane,
//! together with an instruction, to the agent in another pane.
//!
//! Answers come from the agents' own transcripts, read by one adapter per
//! agent (`adapters`). Herdr tells which session a pane runs (`herdr`,
//! `session`); `handoff` checks both panes again and sends the prompt once.

pub mod config;
pub mod error;
pub mod herdr;
pub mod model;
pub mod session;
