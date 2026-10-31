//! Data both the agent and the UI read, below `maki-agent` and
//! `maki-providers`, so a client can draw what an agent sends without
//! linking the agent.

pub mod btw;
pub mod cancel;
pub mod command;
pub mod diff;
pub mod display_order;
pub mod event;
pub mod hook;
pub mod inbox;
pub mod input;
pub mod mcp;
pub mod message;
pub mod model;
pub mod paths;
pub mod snapshot;
pub mod stop_reason;
pub mod text;
pub mod thinking;
pub mod tokens;
pub mod tool;
pub mod usage;

pub use tool::MoveOutcome;
