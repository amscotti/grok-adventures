//! Scrollback transcript for fullscreen TUI.

use crate::ui::blocks::Block;

/// One entry in the fullscreen scrollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub block: Block,
}

/// Ordered list of transcript entries with a scroll offset (lines from bottom).
#[derive(Debug, Clone, Default)]
pub struct Scrollback {
    entries: Vec<TranscriptEntry>,
    /// How many lines to scroll up from the bottom (0 = pinned to latest).
    scroll: u16,
}

impl Scrollback {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a block and pin view to the bottom.
    pub fn push(&mut self, block: Block) {
        self.entries.push(TranscriptEntry { block });
        self.scroll = 0;
    }

    /// Append many blocks.
    pub fn extend(&mut self, blocks: impl IntoIterator<Item = Block>) {
        for b in blocks {
            self.entries.push(TranscriptEntry { block: b });
        }
        self.scroll = 0;
    }

    pub fn entries(&self) -> &[TranscriptEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn scroll(&self) -> u16 {
        self.scroll
    }

    pub fn set_scroll(&mut self, scroll: u16) {
        self.scroll = scroll;
    }

    /// Scroll toward older content (up).
    pub fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_add(n);
    }

    /// Scroll toward newer content (down).
    pub fn scroll_down(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    /// Pop the last entry if any (test helper / undo).
    pub fn pop(&mut self) -> Option<TranscriptEntry> {
        self.entries.pop()
    }

    /// Clear all entries and reset scroll.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.scroll = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::blocks::{Block, SystemKind};

    #[test]
    fn push_and_pop() {
        let mut sb = Scrollback::new();
        assert!(sb.is_empty());
        sb.push(Block::Dm {
            text: "hello".into(),
        });
        sb.push(Block::System {
            text: "ok".into(),
            kind: SystemKind::Info,
        });
        assert_eq!(sb.len(), 2);
        assert_eq!(sb.scroll(), 0);
        let last = sb.pop().unwrap();
        assert!(matches!(last.block, Block::System { .. }));
        assert_eq!(sb.len(), 1);
    }

    #[test]
    fn push_resets_scroll() {
        let mut sb = Scrollback::new();
        sb.push(Block::Dm { text: "a".into() });
        sb.scroll_up(5);
        assert_eq!(sb.scroll(), 5);
        sb.push(Block::Dm { text: "b".into() });
        assert_eq!(sb.scroll(), 0);
    }

    #[test]
    fn scroll_up_down() {
        let mut sb = Scrollback::new();
        sb.scroll_up(3);
        assert_eq!(sb.scroll(), 3);
        sb.scroll_down(1);
        assert_eq!(sb.scroll(), 2);
        sb.scroll_down(10);
        assert_eq!(sb.scroll(), 0);
    }
}
