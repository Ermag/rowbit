//! Rowbit core.
//!
//! Every surface (MCP, CLI, web, Slack) is a thin client over the change
//! state machine in [`change`]. Engines plug in through [`adapter`].

pub mod adapter;
pub mod change;
