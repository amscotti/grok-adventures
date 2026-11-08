//! `--demo` must work with no key, no config, and no `.env`.

use std::process::Command;
use tempfile::TempDir;

#[test]
fn demo_runs_without_api_key_or_env_file() {
    let dir = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_grok-adventures"))
        .arg("--demo")
        .current_dir(dir.path())
        .env_remove("XAI_API_KEY")
        .env_remove("XAI_MODEL")
        .env("NO_COLOR", "1")
        .output()
        .expect("run --demo");

    assert!(
        output.status.success(),
        "--demo exited with {}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Demo (no API calls)"), "{stdout}");
    assert!(stdout.contains("You"), "{stdout}");
    assert!(stdout.contains("DM"), "{stdout}");
    assert!(stdout.contains("State"), "{stdout}");
    assert!(stdout.contains("Tools (2)"), "{stdout}");
    assert!(stdout.contains("HP 18/20"), "{stdout}");
    assert!(
        stdout.contains("Run `cargo run` with XAI_API_KEY for a live game."),
        "{stdout}"
    );

    // A temp cwd has no .env and no config.toml; the demo must not need either.
    assert!(!dir.path().join(".env").exists());
    assert!(!dir.path().join("config.toml").exists());
}
