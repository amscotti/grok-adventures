# Grok Adventures

![A 16-bit party at the mouth of a torchlit dungeon](assets/banner.jpg)

A terminal dungeon crawl. You type or talk, Grok is the dungeon master, and Rust keeps the score.

Hit points, inventory, dice, companions, and the journal live in game state. The model can change them only by calling tools. If it narrates a change without a tool, a marker fallback applies it once, and only when no mutating tool already ran that turn.

## Play

You need a recent Rust toolchain and an xAI API key. The microphone and speech playback are built for macOS. Typing works without either.

```bash
cp .env.example .env   # set XAI_API_KEY
cargo run
```

A new game on a real terminal asks for a name and a class: fighter, rogue, wizard, cleric, or none. `--name` and `--class` skip that prompt. Saves land in `saves/` and stay out of git. Tone, models, and voice live in `config.toml`.

| Flag | Effect |
|------|--------|
| `--name`, `-n` | Character name |
| `--class` | `fighter`, `rogue`, `wizard`, `cleric`, or `none` |
| `--load` | Save slot or path |
| `--minimal` | Linear transcript |
| `--fullscreen` | Status, scrollback, and prompt |
| `--autopilot` | A second model plays for `--turns` (default 20) |
| `--scripted-autopilot` | Fixed actions instead of the player model |
| `--player-style` | `cautious`, `balanced`, or `reckless` |
| `--no-tts`, `--no-stt` | Silence speech or the mic for this session |

Talk with `talk` or `/talk`. In fullscreen, hold Ctrl+Space (F2 works too). Esc cancels an in-flight turn there and restores the state from before that turn. Slash commands, and questions like “what’s in my inventory?”, stay on the machine and do not call the model.

`/help` lists the local commands: stats, inventory, companions, journal, rest, save, load, and quit.

## Develop

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

`cargo test` does not call the network. The live API smokes are ignored:

```bash
cargo test --test live_api -- --ignored --nocapture
cargo test --test live_tts -- --ignored --nocapture
```

GitHub Actions checks formatting, clippy, and tests on every push and pull request. Pushing a `v*` tag builds release binaries and attaches them to a GitHub release.

## License

MIT
