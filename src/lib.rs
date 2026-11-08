//! Grok Adventures — Grok is the DM, Rust is the rules engine.

pub mod config;
pub mod demo;
pub mod dm;
pub mod game;
pub mod io;
pub mod ui;
pub mod util;

pub use config::Config;
pub use game::state::GameState;
