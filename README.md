# Codex Keep Warm

A local Dioxus desktop app for viewing Codex subscription limits and aligning warmup windows across multiple ChatGPT accounts.

## What it does

- Keeps each login in its own permanent `CODEX_HOME` and Codex keyring entry.
- Shows remaining 5-hour burst and weekly capacity with reset countdowns.
- Stores daily warmup times per account in the computer's local timezone.
- Starts newly reset weekly windows first, then scheduled warmups, then opportunistic 5-hour windows.
- Starts an opportunistic 5-hour window only when its observed duration plus a two-minute guard fits before the next scheduled time.

The app invokes the official Codex app-server protocol for login, limits, token refresh, and a minimal ephemeral turn. It does not read or write the active `~/.codex/auth.json`. The scheduler runs while the app is open.

## Run

Install a recent [Codex CLI](https://developers.openai.com/codex/cli/) and Rust toolchain, then:

```sh
cargo run --release
```

The current protocol integration is tested with `codex-cli 0.151.0`. Codex app-server schemas are versioned, so update the app if a future CLI reports an unsupported request.

Add an account, complete the browser sign-in, and edit its warmup times. Limits refresh every 30 seconds.

App metadata is stored under:

- Windows: `%LOCALAPPDATA%\CodexKeepWarm`
- macOS: `~/Library/Application Support/CodexKeepWarm`
- Linux: `$XDG_CONFIG_HOME/codex-keep-warm` or `~/.config/codex-keep-warm`

For an isolated development profile, set `CODEX_KEEP_WARM_HOME` to another directory before launching.

## Verify

```sh
cargo test
cargo build --release
```

The scheduling checks cover per-account times, reversed limit windows, reset priority, safe-gap protection, and expired weekly windows.
