//! Clipboard helpers — OSC 52 for terminal copy, file fallback.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;

/// OSC 52 clipboard set: `\x1b]52;c;{base64}\x07`
pub fn osc52_copy_sequence(text: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{b64}\x07")
}

/// Write text via OSC 52 to stdout and also to a fallback file under `cache/`.
///
/// Returns a short success message for the UI.
pub fn copy_text(text: &str) -> Result<String> {
    if text.is_empty() {
        anyhow::bail!("nothing to copy");
    }

    // OSC 52 — works in many modern terminals (Ghostty, Kitty, iTerm2, …).
    let seq = osc52_copy_sequence(text);
    {
        let mut out = io::stdout().lock();
        out.write_all(seq.as_bytes())
            .context("writing OSC 52 clipboard sequence")?;
        out.flush().ok();
    }

    let path = fallback_path();
    write_fallback_file(&path, text)?;

    Ok(format!(
        "Copied {} chars to clipboard (OSC 52). Fallback: {}",
        text.chars().count(),
        path.display()
    ))
}

/// Default fallback path: `cache/last-copy.txt` under the current working directory
/// (project-local). Falls back to a temp path only if creating `cache/` fails
/// and home is unavailable — prefer project cache.
pub fn fallback_path() -> PathBuf {
    PathBuf::from("cache/last-copy.txt")
}

fn write_fallback_file(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Home-dir fallback used only when documenting optional path.
#[allow(dead_code)]
pub fn home_fallback_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".grok-adventures-last-copy.txt"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn osc52_is_well_formed() {
        let seq = osc52_copy_sequence("hello");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\u{07}') || seq.ends_with('\x07'));
        let b64 = base64::engine::general_purpose::STANDARD.encode("hello");
        assert!(seq.contains(&b64));
    }

    #[test]
    fn write_fallback_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("last-copy.txt");
        write_fallback_file(&path, "dm narration").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "dm narration");
    }
}
