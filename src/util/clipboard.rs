//! Clipboard helpers — OSC 52 for terminal copy, file fallback.
//!
//! The fallback file can contain arbitrary player/DM text, so it is written
//! owner-only, atomically, and never through a symlink.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use base64::Engine;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    ensure_private_parent(path)?;
    reject_symlink_target(path)?;

    let tmp = temp_path(path);
    let result = write_private_temp(&tmp, text.as_bytes()).and_then(|()| {
        fs::rename(&tmp, path)
            .with_context(|| format!("replacing clipboard fallback {}", path.display()))
    });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Create the parent directory when needed, owner-only on Unix. Refuse to
/// follow a symlinked directory.
fn ensure_private_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }

    match fs::symlink_metadata(parent) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!(
                "refusing to write clipboard fallback through symlinked directory {}",
                parent.display()
            );
        }
        Ok(meta) if !meta.is_dir() => {
            bail!(
                "clipboard fallback parent {} is not a directory",
                parent.display()
            );
        }
        Ok(_) => return Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("checking clipboard directory {}", parent.display()));
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting owner-only permissions on {}", parent.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    Ok(())
}

/// Explicitly refuse symlink targets even though the temp+rename below would
/// replace the link rather than write through it.
fn reject_symlink_target(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!(
                "refusing to write clipboard fallback through symlink {}",
                path.display()
            );
        }
        Ok(meta) if meta.is_dir() => {
            bail!("clipboard fallback path {} is a directory", path.display());
        }
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("checking clipboard fallback {}", path.display()))
        }
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| OsString::from("last-copy.txt"));
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    path.with_file_name(name)
}

fn write_private_temp(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating temporary clipboard fallback {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing {}", path.display()))?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting owner-only permissions on {}", path.display()))?;
    }
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

    #[cfg(unix)]
    #[test]
    fn write_fallback_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("last-copy.txt");
        write_fallback_file(&path, "secret").unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "fallback file mode was {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn write_fallback_refuses_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "keep me").unwrap();
        let link = dir.path().join("last-copy.txt");
        symlink(&outside, &link).unwrap();

        let err = write_fallback_file(&link, "attacker text").unwrap_err();
        assert!(
            format!("{err:#}").contains("symlink"),
            "unexpected error: {err:#}"
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep me");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_fallback_creates_private_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("cache").join("last-copy.txt");
        write_fallback_file(&path, "note").unwrap();

        let mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "cache dir mode was {mode:o}");
    }
}
