# kcl — Local Code Knowledge CLI

A Rust CLI that gives agents and humans instant Q&A access to any codebase on the local machine. Clones repos to disk and invokes coding harnesses (Claude Code, opencode, copilot, etc.) in non-interactive mode to answer queries. No server, no API keys for kcl itself.

## Design Spec

The full design spec is in `kcl-plan.md` at the project root. Read it before making changes.

## Tech Stack

- **Language:** Rust
- **CLI:** clap v4 with derive macros
- **Database:** rusqlite with bundled feature (zero system deps)
- **Async:** tokio (for subprocess management and streaming)
- **Serialization:** serde + serde_json
- **Platform dirs:** dirs crate
- **Error handling:** anyhow + thiserror

## Project Structure

```
src/
  main.rs              — Entry point, clap CLI dispatch
  cli.rs               — Clap command/arg definitions
  config.rs            — Config loading (~/.config/kcl/config.json)
  db.rs                — SQLite connection, migrations
  paths.rs             — Platform directory resolution
  models/
    package.rs         — Package struct + CRUD
    conversation.rs    — Conversation index + CRUD
  harness.rs           — Subprocess spawning, prompt building, streaming + capture
  git.rs               — Clone, pull operations (shells out to system git)
  commands/
    ask.rs             — kcl ask handler
    packages.rs        — kcl packages handler
    history.rs         — kcl history handler
    config_cmd.rs      — kcl config handler
    init.rs            — kcl init handler
```

## Key Design Decisions

- **XDG paths on every platform** (including macOS — we deliberately do *not* use `~/Library/Application Support`). See `src/dirs.rs` for the resolution logic. `$XDG_CONFIG_HOME` / `$XDG_DATA_HOME` / `$XDG_STATE_HOME` override the defaults below.
- **JSON config** at `~/.config/kcl/config.json`
- **SQLite database** at `~/.local/share/kcl/kcl.db`
- **Conversation logs** as JSON files at `~/.local/state/kcl/logs/<package>/<timestamp>-<id>.json`
- **Conversation index** in SQLite for fast listing/filtering; full logs on disk
- **Shell out to `git`** rather than libgit2 — simpler, respects user's git config/SSH
- **Exit codes:** 0 = success, 1 = kcl error, 2 = harness terminated without a normal exit status (signal-killed, spawn failure, timeout), 3 = harness ran to completion but exited non-zero
- **Agent-friendly output:** `--json` flag on read commands, terse defaults, no color in non-TTY
- **User-facing message delimiters:** wrap identifiers and literal values in backticks (e.g. ``Package `axum` not found``, ``expected `true` or `false` ``) in error, warning, and status messages. Do not use single quotes for this purpose. Single quotes are reserved for Rust char literals and SQL string literals.

## Querying Dependencies with kctx

This project's coding agents have access to **kctx** — a dependency knowledge service. Use the `mcp__kctx__query_dependency` and `mcp__kctx__list_dependencies` MCP tools to ask usage questions about external libraries.

Relevant dependencies available via kctx:
- `claude-code` — Claude Code CLI (harness reference)
- `opencode` — OpenCode CLI (harness reference)
- `copilot-cli` — GitHub Copilot CLI (harness reference)
- `pi` — Pi CLI (harness reference)
- `codex` — Codex CLI (harness reference)

## Solved Problems

The following solved problems (via `mcp__sp__get_solved_problems`) are relevant:
- `rust-cli-github-releases-install-script-distribution` — GitHub Releases + install script for Rust CLI distribution. Use this when setting up CI/CD and distribution.

## Build & Test

```bash
cargo build
cargo test
```

## Releasing

Releases are fully automated via `.github/workflows/release.yml`:

1. Bump `version` in `Cargo.toml`
2. Merge to `master`
3. The workflow detects the version change, creates a GitHub Release, and uploads cross-compiled binaries for `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, and `aarch64-unknown-linux-gnu` (each with a `.sha256` checksum).

`scripts/install.sh` is the user-facing `curl | bash` installer. If you change binary name, target list, or release asset naming, update both the workflow and the script together.

## Known Dependency Duplications

- **`getrandom` 0.2 and 0.4** both appear in `Cargo.lock`. `getrandom 0.2` is pulled in transitively by `dirs` → `dirs-sys` → `redox_users`; `getrandom 0.4` is pulled in by `uuid`. All intermediate crates are at their latest stable releases, so this cannot be resolved without forking `dirs` or replacing it with another platform-dir crate. Accepted as-is.

## Related Projects

- **kctx** (sibling at `../kctx/`) — Server-based MCP dependency knowledge service. kcl is the local-first complement.
- **ralph2** (sibling at `../ralph2/`) — Agentic planning orchestrator. Uses similar Rust patterns (clap, serde, tokio, dirs).
