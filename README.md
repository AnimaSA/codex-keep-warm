# Codex Keep Warm

A local Dioxus desktop app for viewing Codex subscription limits and warming Codex and Claude accounts.

## What it does

- Keeps each Codex login in its own permanent `CODEX_HOME` and Codex keyring entry.
- Keeps each Claude login in its own permanent `CLAUDE_CONFIG_DIR`.
- Shows remaining 5-hour burst and weekly capacity, reset countdowns, and available banked resets for Codex accounts.
- Shows Claude's OAuth-backed 5-hour and weekly quota windows and reset countdowns when a supported Claude OAuth credential is available.
- Shows daily warmup times per account in the computer's local timezone.
- Shows average and trailing-hour burndown projections with optional zero usage outside Monday-Friday, 09:00-17:00, plus hoverable early-runout dates.
- Starts newly reset weekly windows first, then scheduled warmups, then opportunistic 5-hour windows when quota data is available.
- Runs manual warmups for either provider.
- Runs in the system tray; closing the window keeps the scheduler running.
- Can start with Windows and launch hidden in the tray.

The app invokes the official [Codex CLI](https://developers.openai.com/codex/cli/) app-server protocol and the official [Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code/overview) for sign-in, identity, and warmups. Claude quota refresh reads only each account's isolated `<CLAUDE_CONFIG_DIR>/.credentials.json` and calls `GET https://api.anthropic.com/api/oauth/usage` with its OAuth token. When that access token is within five minutes of expiry, the app refreshes it the way Claude Code does (`POST https://platform.claude.com/v1/oauth/token` with the stored refresh token) and atomically writes the rotated tokens back into the same file, preserving its other fields, so the Claude CLI and later refreshes keep the current single-use refresh token. Refresh runs in the same queue as the app's Claude CLI processes, so they never rotate the token concurrently. It never reads the default Claude profile, browser cookies or web pages, statusline output, transcripts, or CLI `/usage`, and never logs tokens. It does not directly read or write the active `~/.codex/auth.json`. These OAuth endpoints are undocumented, subject to change, and subject to rate limits. Click the tray icon to reopen the window, or use its Exit item to stop the scheduler.

Claude account status still requires first-party Claude OAuth. Claude quota refresh requires an OAuth credential with `user:profile`; if scopes are present and omit that scope, refresh fails with a clear error. `setup-token` and inference-only tokens may not work. Missing, invalid, or unsupported credentials, and expired credentials whose refresh token is missing or rejected, produce no quota windows instead of fabricated data; a rejected refresh token asks you to sign in again. When the usage endpoint answers HTTP 429, the app stops calling it for that account until the `Retry-After` delay passes, or, without that header, for 5 minutes, doubling per consecutive 429 up to an hour; a successful call resets the backoff. The app displays only core `five_hour` and `seven_day` windows; model-specific and extra-usage windows are not displayed.

Each account gets separate provider storage:

- Codex: `accounts/<id>/codex-home`, passed as `CODEX_HOME`.
- Claude: `accounts/<id>/claude-home`, passed as `CLAUDE_CONFIG_DIR`.
- Warmups: shared `accounts/<id>/warmup` workspace.

- Claude subprocesses set each account's `CLAUDE_CONFIG_DIR` and remove these ambient auth/provider variables before launch:
  `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_AWS_API_KEY`, `ANTHROPIC_AWS_BASE_URL`, `ANTHROPIC_AWS_WORKSPACE_ID`, `ANTHROPIC_BASE_URL`,
  `ANTHROPIC_BEDROCK_BASE_URL`, `ANTHROPIC_BEDROCK_MANTLE_BASE_URL`, `ANTHROPIC_CUSTOM_HEADERS`, `ANTHROPIC_FEDERATION_RULE_ID`, `ANTHROPIC_FOUNDRY_API_KEY`, `ANTHROPIC_FOUNDRY_AUTH_TOKEN`,
  `ANTHROPIC_FOUNDRY_BASE_URL`, `ANTHROPIC_FOUNDRY_RESOURCE`, `ANTHROPIC_IDENTITY_TOKEN`, `ANTHROPIC_IDENTITY_TOKEN_FILE`, `ANTHROPIC_ORGANIZATION_ID`, `ANTHROPIC_PROFILE`,
  `ANTHROPIC_SERVICE_ACCOUNT_ID`, `ANTHROPIC_VERTEX_BASE_URL`, `ANTHROPIC_VERTEX_PROJECT_ID`, `ANTHROPIC_WORKSPACE_ID`, `AWS_BEARER_TOKEN_BEDROCK`, `CLAUDE_CODE_OAUTH_REFRESH_TOKEN`,
  `CLAUDE_CODE_OAUTH_SCOPES`, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_SKIP_ANTHROPIC_AWS_AUTH`, `CLAUDE_CODE_SKIP_BEDROCK_AUTH`, `CLAUDE_CODE_SKIP_FOUNDRY_AUTH`, `CLAUDE_CODE_SKIP_MANTLE_AUTH`,
  `CLAUDE_CODE_SKIP_VERTEX_AUTH`, `CLAUDE_CODE_USE_ANTHROPIC_AWS`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_FOUNDRY`, `CLAUDE_CODE_USE_MANTLE`, and `CLAUDE_CODE_USE_VERTEX`.
  Unattended Claude status, warmup, and logout commands receive no stdin; interactive browser sign-in remains interactive.

## Run

Install recent [Codex CLI](https://developers.openai.com/codex/cli/), [Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code/overview), and Rust toolchain, then:

```sh
cargo run --release
```

The current Codex protocol integration is tested with `codex-cli 0.151.0`. Codex app-server schemas are versioned, so update the app if a future CLI reports an unsupported request. Claude sign-in, identity, and warmups use documented CLI commands; Claude quota refresh uses the undocumented OAuth endpoint described above.

Add an account, choose Codex or Claude, complete browser sign-in, and edit its warmup times. The app refreshes account status and quota windows automatically: Codex every 60 seconds (range 5–60) and Claude every 5 minutes (range 60–3600 seconds) by default, since Claude's usage endpoint rate-limits aggressively. Change both intervals, and Windows startup, under Settings. Existing settings keep their old shared interval as the Codex interval. Claude quota windows require a supported first-party OAuth credential with `user:profile`.

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

Also verify both account paths manually:

1. Add Codex account. Complete sign-in, refresh limits, and confirm account data uses its `codex-home`.
2. Add Claude account. Complete first-party browser OAuth sign-in, refresh identity, and confirm account data uses its isolated `claude-home`.
3. Refresh Claude quota with an OAuth credential that has `user:profile`; confirm 5-hour and weekly windows and reset countdowns appear. Confirm model-specific and extra-usage windows do not appear.
4. Leave a Claude account running past the `expiresAt` in its `.credentials.json`. Confirm quota keeps refreshing, `expiresAt` and `refreshToken` in that file change, and a warmup still succeeds afterwards.
5. Test missing, expired-without-refresh-token, or insufficient-scope Claude credentials. Confirm refresh reports an actionable error and shows no fabricated quota windows. Do not use `setup-token` or inference-only tokens for this check.
6. Restart with existing settings and confirm saved accounts still use their configured providers.
