# kcl Code Review

Comprehensive audit of the kcl Rust CLI project. Findings organized by severity.

---

## Critical

### C1: UTF-8 panic in history question truncation
**File:** `src/commands/history.rs:77-78`

```rust
let question_display = if conv.question.len() > 80 {
    format!("{}...", &conv.question[..77])
} else {
```

`&conv.question[..77]` performs **byte-slice** indexing. If the 77th byte falls inside a multi-byte UTF-8 character (emoji, CJK, accented Latin), this panics at runtime. No test exercises non-ASCII question text.

**Fix:** Use `conv.question.chars().take(77).collect::<String>()` or the `.truncate()` method.

---

### C2: `std::process::exit()` bypasses Drop — can corrupt SQLite WAL
**File:** `src/commands/ask.rs:187-188` and `242-244`

```rust
std::process::exit(2);
```

Two `process::exit(2)` calls skip Rust destructors. The SQLite `conn` (opened line 60) is never explicitly dropped, so WAL pages may not be checkpointed. The Tokio runtime on line 166 is also leaked.

**Fix:** Either call `drop(conn); drop(rt);` before each `process::exit(2)`, or refactor `run()` to return an exit code and let `main()` handle termination after cleanup.

---

### C3: No SQLite busy timeout — concurrent writes fail immediately
**File:** `src/db.rs:7-24`

`open()` sets `journal_mode=WAL` and `foreign_keys=ON` but omits `busy_timeout`. SQLite's default is 0ms, so two concurrent `kcl` processes (e.g., `kcl ask` in two terminals) cause the second to immediately receive `SQLITE_BUSY`.

**Fix:** Add `conn.pragma_update(None, "busy_timeout", 5000)?;` after the existing pragmas.

---

### C4: Tilde (`~`) in `--path` not expanded before resolution
**File:** `src/commands/packages.rs:202-218`

`resolve_and_validate_path` joins `~/code/myproj` to `current_dir()` because `Path::new("~/…")` is not absolute. The `expand_tilde()` function exists (line 188) but is only used for `clone_dir`, not `--path`. Result: nonsensical paths like `/cwd/~/code/myproj`.

**Fix:** Call `expand_tilde(p)` before path resolution logic.

---

### C5: Path traversal via unsanitized `identifier`
**Files:** `src/commands/packages.rs:136`, `src/commands/ask.rs:196-199`

The `identifier` CLI argument is used directly in `clone_dir.join(identifier)` and `log_dir.join(&pkg.identifier)` without validation. An identifier like `../../etc/cron.d` would cause clones to arbitrary directories and log writes outside the log directory.

**Fix:** Validate identifiers to match `^[a-zA-Z0-9_-]+$` at the `cmd_add` entry point.

---

### C6: Harness subprocess orphan processes on timeout
**File:** `src/harness.rs:206-213`

`child.kill()` sends SIGKILL to the direct child but not its process group. Harnesses like Claude Code spawn their own subprocesses, which become orphans. After killing, `child.wait()` is not called, risking zombie processes.

**Fix:** Use `Command::new(...).process_group(0)` when spawning, and call `child.wait().await` after `kill()`.

---

## High

### H1: Failed harness runs not logged
**File:** `src/commands/ask.rs:182-189`

When the harness returns `Err`, the code prints the error and calls `process::exit(2)` without writing a conversation log or inserting a DB row. Failed invocations leave no trace in `kcl history`.

**Fix:** Save the conversation log (including the error in the response field) before exiting.

---

### H2: No SIGINT/SIGTERM handling — subprocess survives Ctrl+C
**File:** `src/harness.rs` (entire `run_harness` function), `src/main.rs`

No signal handler is installed. If a user presses Ctrl+C while a harness subprocess runs, kcl terminates immediately but the subprocess continues running.

**Fix:** Install a SIGINT handler with `tokio::signal` or `ctrlc` crate that kills the child process group.

---

### H3: Zero timeout accepted — kills harness immediately
**Files:** `src/config.rs:17-18`, `src/commands/ask.rs:90`

`default_timeout` is `u64` with no minimum validation. A value of `0` creates `Duration::from_secs(0)`, which fires immediately in `tokio::select!`, killing the harness before it produces any output.

**Fix:** Reject `0` (and possibly very low values) in config set and CLI arg parsing.

---

### H4: Non-atomic config file writes risk corruption
**File:** `src/config.rs:109-117`

`std::fs::write` is not atomic. If kcl is killed during `kcl config set`, the config file can be left empty or partially written, making it unparseable.

**Fix:** Write to a temp file in the same directory, then `fs::rename` (atomic on POSIX) to the target.

---

### H5: `--path --git <invalid-url>` registers package without URL validation
**File:** `src/commands/packages.rs:95-109`

When `--path` and `--git` are both provided, the git URL is stored without any validity check. Without `--path`, `git clone` would fail fast. This inconsistency means one code path silently accepts invalid URLs.

**Fix:** Optionally validate the URL format (or attempt a `git ls-remote`) before DB insertion.

---

## Medium

### M1: New tokio runtime per `ask` invocation
**File:** `src/commands/ask.rs:166`

Each `ask` creates and destroys a full multi-threaded `tokio::Runtime`. Expensive antipattern.

**Fix:** Create runtime once in `main()` or make `main()` async with `#[tokio::main]`.

---

### M2: `git fetch` failure silently swallowed
**File:** `src/git.rs:111-117`

`let _ = Command::new("git")...fetch...output();` discards all errors. If offline or remote is down, the subsequent `git checkout` fails with a confusing message rather than explaining the fetch failed.

**Fix:** Log fetch failure to stderr; surface it if checkout also fails.

---

### M3: `packages remove` doesn't delete on-disk clone or log files
**File:** `src/commands/packages.rs:220-234`

Only removes the DB row (with CASCADE to conversations). Leaves orphan git clone directory and log files on disk. No `gc` command exists.

**Fix:** After DB deletion, also remove `clone_dir.join(&pkg.identifier)` and `log_dir.join(&pkg.identifier)`.

---

### M4: Missing compound index for primary query pattern
**File:** `src/db.rs:66-69`

Two separate indexes on `(package_id)` and `(created_at)`. The common query `WHERE package_id = ?1 ORDER BY created_at DESC` would benefit from a compound index on `(package_id, created_at DESC)`.

**Fix:** Add `CREATE INDEX IF NOT EXISTS idx_conversations_pkg_created ON conversations(package_id, created_at DESC);`

---

### M5: Config `prompt_mode` parsing roundabout
**File:** `src/commands/config_cmd.rs:106-113`

```rust
serde_json::from_value(serde_json::Value::String(value.to_string()))
```

Wraps a string in a JSON value only to deserialize it. A `match` would be clearer, faster, and give better error messages.

---

### M6: `SourceType::from_str` shadows `std::str::FromStr`
**File:** `src/models/package.rs:23`

Defines `pub fn from_str(s: &str)` as an inherent method, making `"local".parse::<SourceType>()` fail with a confusing error. Should implement the `FromStr` trait instead.

---

### M7: `Conversation::new()` is dead code
**File:** `src/models/conversation.rs:23`

Marked `#[allow(dead_code)]` but never used in production code. `ask.rs` constructs `Conversation` with struct literals.

---

### M8: `expand_tilde` silently returns unexpanded path when home_dir unavailable
**File:** `src/commands/packages.rs:188-198`

If `home_dir()` returns `None` (container environments), `expand_tilde("~/src/…")` returns `~/src/…` as a `PathBuf`, which becomes a nonsensical relative path.

**Fix:** Return `Result<PathBuf>` and propagate the error.

---

### M9: Drain loop after stdout/stderr close has no timeout
**File:** `src/harness.rs:167-173` and `189-199`

After one pipe closes, the drain loop for the other runs indefinitely. If the subprocess dies abnormally with a broken pipe on the closed side and an open pipe on the other, the drain could hang forever.

**Fix:** Wrap drain loops in `tokio::time::timeout`.

---

### M10: `--branch` forces pull regardless of `--no-pull`
**File:** `src/commands/ask.rs:135-139`

When `--branch` is specified, pulling is forced even if `--no-pull` is set. Documented intent, but could surprise users who set `--no-pull --branch X` expecting no network access.

---

### M11: Empty config file produces cryptic parse error
**File:** `src/config.rs:99-106`

If `config.json` exists but is empty (zero bytes), `load()` returns a JSON parse error instead of falling back to defaults.

**Fix:** Check for empty file and treat as missing, or catch parse errors for empty files.

---

### M12: Release workflow missing `CC_aarch64_unknown_linux_gnu` for cross-compilation
**File:** `.github/workflows/release.yml:74-90`

The `rusqlite` bundled feature compiles SQLite C code via the `cc` crate. Without setting `CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc`, the bundled SQLite may compile for the host architecture instead of the target.

**Fix:** Add `CC_aarch64_unknown_linux_gnu` and `CXX_aarch64_unknown_linux_gnu` env vars to the cross-compilation step.

---

### M13: Release workflow version regex matches `rust-version`
**File:** `.github/workflows/release.yml:31`

```bash
current="$(grep '^version' Cargo.toml | head -1 | sed ...)"
```

Matches any line starting with `version`, which could match `rust-version = "1.85"`. The `head -1` saves it because `version = "0.1.7"` comes first, but this is fragile.

**Fix:** Use `grep '^version = '` instead of `grep '^version'`.

---

### M14: Install script skips checksum verification when no sha256sum tool available
**File:** `scripts/install.sh:73-81`

If neither `sha256sum` nor `shasum` is found, `verify_checksum` prints a warning but does **not** fail. The binary is used without verification.

---

### M15: Local module `dirs` shadows external `dirs` crate
**File:** `src/dirs.rs`

Creates a naming confusion where `dirs::home_dir()` inside the module works differently than expected. The code mixes `dirs::` and `::dirs::` calls, reducing clarity.

**Fix:** Rename local module to `paths` or `directories`.

---

## Low

### L1: Unused `thiserror` dependency
**File:** `Cargo.toml:18`

`thiserror = "2"` is declared but never used. The project uses `anyhow` exclusively. Removing it reduces compile time.

### L2: `tokio` with `features = ["full"]` is overbroad
**File:** `Cargo.toml:19`

Only `rt`, `macros`, `process`, `time`, and `io-util` are used. `full` pulls in unnecessary features, increasing compile time and binary size.

### L3: `chrono` pulls WASM dependencies unnecessarily
**File:** `Cargo.toml:11`

Use `default-features = false, features = ["serde", "clock", "std"]` to avoid `js-sys` and `wasm-bindgen`.

### L4: `clap` missing `wrap_help` feature
**File:** `Cargo.toml:12`

Adding `wrap_help` improves help text formatting to terminal width.

### L5: `generate_completions` takes `&PathBuf` instead of `&Path`
**File:** `src/commands/init.rs:261`

Clippy `ptr_arg` lint. Functions that only read a path should take `&Path`.

### L6: `KNOWN_HARNESS_NAMES` must be manually kept in sync with `known_harnesses()`
**File:** `src/commands/init.rs:125`

No compile-time enforcement. Adding a harness to one and not the other causes subtle detection failures.

### L7: `cmd_set` doesn't validate harness name existence
**File:** `src/commands/packages.rs:321-361`

Setting `harness = "nonexistent"` stores it silently. User only discovers the error when running `kcl ask`.

### L8: Missing `.with_context()` on `create_dir_all` and `fs::write` in ask.rs
**File:** `src/commands/ask.rs:200, 222-223`

All other file I/O uses `.with_context()` for descriptive errors. These two bare `?` produce unhelpful `std::io::Error` messages.

### L9: No cleanup of partial clone on failure
**File:** `src/commands/packages.rs:136-148`

If `git::clone()` fails, the (possibly partially created) directory remains. Next attempt hits "clone target already exists."

### L10: Network-dependent tests without `#[ignore]`
**File:** `src/git.rs:147-189`

Tests `clone_creates_directory_and_repo` and `pull_on_cloned_repo` clone from GitHub. They fail in offline environments and are flaky in CI.

### L11: `Package::update()` doesn't verify rows affected
**File:** `src/models/package.rs:162-179`

Returns `Ok(())` even if 0 rows were updated (e.g., row was deleted between read and write).

### L12: `let _ =` for process kill on timeout is appropriate but lacks `wait()`
**File:** `src/harness.rs:208`

After `child.kill().await`, should call `child.wait().await` to reap the zombie before returning.

### L13: `stdin` drop in harness is implicit
**File:** `src/harness.rs:129-137`

The stdin drop that signals EOF relies on implicit drop at end of the `if let` block. An explicit `drop(stdin)` would make intent clearer.

### L14: Inconsistent error message formatting
Throughout the codebase, some errors use single quotes (`'identifier'`), some use backticks, and some use no delimiters.

### L15: `Codex` args use `approval_policy=never` which may not be the correct config key
**File:** `src/commands/init.rs:88-91`

The `-c approval_policy=never` flag needs verification against actual Codex CLI API. The test only checks for the string, not whether Codex accepts it.

### L16: `db::open_memory()` doesn't enable WAL
**File:** `src/db.rs:29`

In-memory test databases don't use WAL, meaning they behave slightly differently from production databases. Low risk but worth noting.

### L17: `tokio` dependency duplicates `getrandom` versions
**File:** `Cargo.lock`

Both `getrandom 0.2.17` (via `dirs-sys`) and `getrandom 0.4.2` (via `uuid`) are compiled. Minor duplication.

### L18: Config `Show` subcommand ignores `--json` flag
**File:** `src/commands/config_cmd.rs:9`

`json: _` is parsed but `cmd_show()` always outputs JSON. The flag exists only for consistency.

### L19: Redundant `from_row` wrapper delegates to `from_row_inner`
**File:** `src/models/package.rs:189-191`

No added logic, just indirection.

### L20: `list_filtered` uses dynamic SQL with boxed trait objects unnecessarily
**File:** `src/models/conversation.rs:105-133`

Two SQL variants could use `params!` macro and separate branches, avoiding allocation overhead.

### L21: `rd` toolchain in release workflow doesn't pin minimum version
**File:** `.github/workflows/release.yml:69`

`dtolnay/rust-toolchain@stable` doesn't enforce the MSRV (1.85) specified in `Cargo.toml`. Could fail if an older stable is cached.

---

## Summary

| Severity | Count | Top Issues |
|----------|-------|-----------|
| **Critical** | 6 | UTF-8 panic, process::exit bypassing Drop, no busy timeout, tilde path bug, path traversal, orphan processes |
| **High** | 5 | Unlogged harness failures, no SIGINT handling, zero timeout accepted, non-atomic config writes, unchecked git URLs |
| **Medium** | 15 | Runtime per invocation, swallowed git errors, orphan files on remove, missing index, dead code, drain hangs, cross-compilation |
| **Low** | 21 | Unused deps, overbroad features, inconsistent errors, network tests, minor code smells |

**Recommended priority order:**
1. C1 (UTF-8 panic) — one-line fix, high user impact
2. C5 (path traversal) — validate identifiers at entry point
3. C2 (process::exit) — refactor to return exit codes from main
4. C3 (busy timeout) — one-line fix
5. C4 (tilde expansion) — use existing `expand_tilde`
6. C6 (orphan processes) — add process group + wait
7. H2 (SIGINT handling) — add signal handler
8. H4 (atomic config writes) — write-then-rename pattern