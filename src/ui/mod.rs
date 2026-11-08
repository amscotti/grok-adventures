//! Presentation layer: transcript blocks, status, and prompts.

pub mod app;
pub mod blocks;
pub mod completion;
pub mod creation;
pub mod fullscreen;
pub mod line_editor;
pub mod markdown;
pub mod prompt;
pub mod scrollback;
pub mod shortcuts;
pub mod status;
pub mod theme;

/// Terminal UI render mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiMode {
    #[default]
    Minimal,
    Fullscreen,
}
