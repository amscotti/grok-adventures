# Grok Adventures — Design Plan (archived)

> **Historical document — not a live roadmap or setup guide.**
> This file started as the MVP implementation plan and is kept for design
> context. Current behavior is defined by [README](../README.md) and the source.
> It was last reconciled on 2026-11-08 against the working tree.

## Premise

Grok is the dungeon master; Rust is the rules engine. The story can invent
freely, but HP, inventory, dice, companions, journal, rooms, and initiative live
in authoritative state. Grok changes them only by calling tools. Narration
without a tool does not quietly change the world; a marker fallback can apply a
change once, and only when no mutating tool already ran that turn.

The original plan borrowed mechanics patterns from Crystal Quest and terminal
UX ideas from Grok Build. Neither project is vendored here.

### Split of responsibilities

| Layer | Owns | Does not own |
|-------|------|--------------|
| Grok | Narrative, NPCs, atmosphere, when to call tools | True HP, inventory, dice outcomes, room state |
| Rust engine | State, dice RNG, tool handlers, markers, saves, invariants | Plot twists |
| UI | How turns are shown (blocks, chrome, themes, input) | Rules or API policy |
| Audio I/O | Mic capture, STT, TTS playback | Game rules |

## Architecture (current)

```text
Terminal
  minimal: linear blocks on stdout (default)
  fullscreen: ratatui alt-screen with status, scrollback, prompt
        |
    GameLoop (src/game/loop_.rs)
        |-- DmClient -> POST https://api.x.ai/v1/chat/completions (tool loop)
        |-- GameState + tools + markers + JSON saves
        |-- STT: mic -> wss://api.x.ai/v1/stt, fallback POST /v1/stt
        `-- TTS: DM text -> POST /v1/tts per sentence -> local player
```

There is no image generation or terminal image protocol in the current tree.
Superseded details from the original plan: fullscreen is not the default,
push-to-talk is Ctrl+Space or F2 (not Space), and the Kitty/scene-image pipeline
was removed.

## Current defaults and controls

- UI mode: `minimal`; fullscreen is opt-in via `--fullscreen` or
  `[ui].mode = "fullscreen"`.
- Models: `grok-4.7` for the DM and autopilot player; reasoning effort `low`.
- Rules: **dnd5e-lite** — d20 + ability modifier, standard dice, HP/death at
  0, inventory, initiative, and homebrew rests. No AC, proficiency, levels,
  spell slots, advantage state, death saves, or enemy stat blocks.
- TTS and STT are enabled by default; typing never requires either.
- Fullscreen themes cycle `groknight` -> `grokday` -> `mono`; `auto` is not a
  theme. Minimal mode uses the fixed GrokNight palette.
- Fullscreen keys: Ctrl+Space or F2 hold-to-talk; Enter sends; Esc cancels an
  in-flight turn (and restores pre-turn state), otherwise clears a non-empty
  prompt, otherwise quits; Tab completes or toggles focus; Ctrl+L clears
  scrollback; Ctrl+C quits.
- Minimal voice input: `talk` or `/talk [seconds]`, Enter to stop.
- Fullscreen voice input: hold Ctrl+Space/F2, release to stop, or `/talk`.
- `record_seconds` is the fullscreen `/talk` default; `max_record_seconds` is
  the hard cap for minimal `/talk` and fullscreen hold-to-talk.
- STT streams over a WebSocket when enabled and falls back to batch upload.
- TTS is one `/v1/tts` request per sentence, not a chunked server stream.

## Project structure (current)

```text
src/
  main.rs                 clap entry, config load, character creation
  lib.rs
  config.rs               config.toml + environment overrides
  dm/
    mod.rs
    client.rs             chat completions + tool loop
  game/
    mod.rs
    state.rs              GameState, items, companions, combat, invariants
    tools.rs              tool definitions and handlers
    markers.rs            marker safety net
    delta.rs              HP/room/inventory state deltas
    save.rs               JSON save/load and slot resolution
    loop_.rs              interactive loop, turn pipeline, autopilot
    commands.rs           slash commands, local state queries, help
    player_agent.rs       autopilot player model and scripted actions
  io/
    mod.rs
    audio.rs              mic capture, WAV helpers, local playback
    listen.rs             live capture + STT session
    stt.rs                batch STT client
    stt_stream.rs         streaming STT WebSocket
    tts.rs                TTS client and sentence pipeline
  ui/
    mod.rs                UiMode
    app.rs                fullscreen app state, theme palettes
    blocks.rs             Player/Tools/DM/State/System blocks
    completion.rs         slash-command autocomplete
    creation.rs           name and class picker
    fullscreen.rs         alt-screen loop, keys, PTT, cancel
    line_editor.rs        minimal-mode prompt editor
    markdown.rs           light inline markdown
    prompt.rs             prompt buffer and history
    scrollback.rs         fullscreen scrollback
    shortcuts.rs          shortcuts bar
    status.rs             status line from GameState
    theme.rs              minimal-mode colors and NO_COLOR
  util/
    clipboard.rs          OSC 52 + cache fallback
    dice.rs
    http.rs
    message_content.rs
tests/
  integration_mvp.rs
  live_api.rs             ignored by default
  live_tts.rs             ignored by default
docs/
  plan.md                 this historical file
assets/
  banner.jpg
  sounds/                 optional SFX; .gitkeep
```

## Tech stack (current)

| Component | Choice |
|-----------|--------|
| Language | Rust 2024; `rust-version = "1.98"`; toolchain pinned to 1.98.0 |
| CLI | `clap` |
| Fullscreen UI | `ratatui` + `crossterm` |
| Minimal UI | direct stdout + `owo-colors` |
| HTTP / async | `reqwest` (rustls) + `tokio` |
| Streaming STT | `tokio-tungstenite` |
| Serialization | `serde` + `serde_json` + `toml` |
| Audio capture | `cpal` + `hound` |
| Audio playback | `afplay` on macOS; `ffplay` / `mpg123` elsewhere |
| Errors | `anyhow` + `thiserror` |
| Tests | `wiremock`, `tokio-test`, `tempfile`, `pretty_assertions` |

No image crate, Kitty protocol, or image-generation client is present.

## Phase history

- **Phase 0 — rules + minimal UI:** complete.
- **Phase 1 — markers, saves, companions, journal, deltas:** complete.
- **Phase 2 — fullscreen TUI, themes, cancel:** complete.
- **Phase 3 — STT input:** complete; later upgraded to streaming STT.
- **Phase 4 — TTS output:** complete; later upgraded to sentence pipelining.
- **Phase 5 — vision:** implemented once, then removed. The current tree has no
  scene images, `/scene`, Kitty escapes, or Imagine client, and none should be
  documented as available.
- **Phase 6 — autopilot and polish:** complete, plus later work on multi-turn
  history, class kits, rest/resurrect, and initiative combat.

## Design decisions

### Tools first, markers second
Tool calls are the primary path. Marker parsing is a safety net that only runs
when no mutating tool ran, so an effect is never applied twice.

### Minimal first, fullscreen opt-in
The linear block transcript is the default and the mode used by autopilot. The
fullscreen TUI renders the same block stream in an alt-screen.

### Type and/or talk
The prompt is always a text field. Voice fills the same action path as typing;
it never bypasses tools or the rules engine.

### State stays authoritative
Status lines and state deltas come from `GameState`, not from prose. Saves are
versioned JSON (`schema_version: 1` plus `state`) under `saves/` unless the
player gives an explicit path; legacy raw-state saves still load.
