# kcl — Memory, Prepare, Shallow, Embeddings Config (2026 Implementation)

This document records the architectural decisions and scope for the "make kcl better" round that adds:

- Optional shallow clones (`--depth 1 --no-single-branch`)
- `kcl prepare <pkg>` — high-density, low-token orientation map produced by the harness itself
- Semantic question memory via embeddings (OpenAI + OpenRouter) + `kcl remember <conv-id>`
- Full provenance (git commit + branch) recorded on every conversation
- Configurable prepare scope (global vs per-branch) per package
- `kcl init` support (interactive + `--non-interactive` flags) for embeddings
- Everything implemented fully, with tests, docs, style compliance, graceful degradation, no tech debt.

**Date of plan**: 2026-05 (post user decisions)

## High-Level Scope (user-confirmed)

1. **Shallow clones** (opt-in, per-package)
2. **Prepare orientation maps** (user can set `prepare_scope = 'global' | 'branch'` on a package; default global)
3. **Question embeddings + semantic "remember"** (the only embedding we do; *no* codebase/RAG embeddings)
4. **Provenance** on conversations (commit sha + branch at ask time) surfaced by `remember` and `history show`
5. Supporting changes: new top-level commands, DB migrations, embedding client, init flow, etc.

**Explicitly out of scope for this round**: embeddings over the actual source files (codebase RAG).

## Key Architectural Decisions (locked)

### 1. Embeddings (question memory only)
- **Providers**: `openai` and `openrouter` (OpenAI-compatible `/v1/embeddings` endpoint).
- **Auth**: Environment variables only — `OPENAI_API_KEY` or `OPENROUTER_API_KEY`. Never stored in `config.json`.
  - Config stores only `provider` + `model` (the exact string sent to the API).
  - Graceful degradation: if key missing at runtime, `ask` still works (just skips embedding + similarity for that run) with a single warning.
- **Storage & search**:
  - `sqlite-vec` crate (v0.1.9+) used for **static auto-registration** via `register_auto_extension` + `sqlite3_vec_init`.
    - Single binary, no separate loadable extensions shipped.
    - Gives us `vec_distance_cosine(blob, blob)` SQL function (and friends) + optional `vec0` virtual tables.
  - Embeddings stored in a normal table `conversation_embeddings` (conversation_id, model, dim, embedding BLOB, created_at).
  - Similarity uses the extension's distance function when available; pure-Rust fallback otherwise (for robustness).
  - Brute-force over the (small) set of past questions for a package is acceptable and intentional for v1.
- **When we embed**: On a successful `ask` (after the harness answers), if embeddings are configured. We embed the *question text*.
- **Similarity injection**: If a sufficiently similar past question is found, the prompt to the harness contains a hint:
  ```
  Similar past question (0.91): "How does routing work?"
  You can run `kcl remember abc12345` to retrieve the prior full answer + file references.
  ```
- **Model changes**: If the user later changes the embedding model (different dim), old rows are simply ignored for similarity (different dim). Re-asking will embed under the new model. No automatic backfill (user can delete/ignore old convos).

### 2. Shallow clones
- New field on `Package`: `shallow: bool` (default false).
- `kcl packages add <id> --git <url> [--shallow]`
- `git::clone` gains `shallow: bool` param.
- When shallow: `git clone --depth 1 --no-single-branch ...`
  - The `--no-single-branch` is **mandatory** so that `kcl ask --branch other` and `git::checkout` can still fetch other branches (shallowly).
- `pull` and `checkout` paths continue to work; shallow clones can deepen naturally on fetch of new branches.
- `packages show`, export/import, and `set` surface / preserve the flag.
- Documentation clearly states the limitation: history before the depth-1 tip is unavailable; `git log`, blame on old commits, etc. will be incomplete. This is the accepted tradeoff.

### 3. `kcl prepare <identifier>`
- New top-level command (symmetric to `ask`).
- Runs the harness (respecting package harness override, branch, etc.) with a **carefully engineered, high-density prompt** that asks the agent to produce a compact orientation map.
- The prepare prompt (hard-coded constant in `commands/prepare.rs`) instructs the harness to output in a structured, token-efficient format (short keys, lists, "when you need X start in Y", build commands, etc.).
- Result is stored in a new table `package_prepared_contexts`:
  - `package_id`, `content` (the map text), `created_at`, `harness`, `model`, `git_commit_sha`, `git_branch`, `prepare_scope_at_time`.
- On `ask`, the latest applicable prepared context (global or matching current branch) is injected early in the prompt, clearly delimited, with staleness note if the recorded commit != current HEAD.
- Re-running `prepare` replaces the previous map for that (package, scope).
- `kcl packages set <id> prepare-scope global|branch` controls future lookups (existing maps are not auto-migrated).

### 4. `kcl remember <conversation-id>`
- New top-level command.
- Looks up the conversation (by short prefix or full id), reads the on-disk JSON log, prints:
  - Original question
  - Full response
  - Harness + model used
  - Provenance: "Answered at commit `abc123` on branch `main` (prepared map scope: global)"
  - Timestamp, exit code, any pull error.
- `--json` for agent consumption (returns the full log object).
- The command is safe to run from inside a harness working directory (it only needs the kcl binary on PATH and the DB).

### 5. Provenance on conversations (required for "remember" and auditability)
- `Conversation` struct + `conversations` table + `ConversationLog` JSON gain two new optional fields:
  - `git_commit_sha: Option<String>`
  - `git_branch: Option<String>`
- Captured in `run_after_checkout` right before/after the harness runs (using the already-checked-out state).
- Displayed by `history show`, `remember`, and in the prompt context where relevant.
- Old rows have NULL (backward compatible).

### 6. Config & Init
- New struct in `config.rs`:
  ```rust
  pub embeddings: Option<EmbeddingConfig>,   // None = disabled
  ```
  ```rust
  pub struct EmbeddingConfig {
      pub provider: EmbeddingProvider, // Openai | Openrouter
      pub model: String,               // e.g. "text-embedding-3-small" or "openai/text-embedding-3-small"
  }
  #[derive(..., Serialize, Deserialize)]
  pub enum EmbeddingProvider { Openai, Openrouter }
  ```
- `kcl init` (interactive):
  - After harness/clone_dir prompts, asks: "Enable semantic question memory with embeddings? (y/N)"
  - If yes: choose provider (menu or typed), choose model (with good defaults per provider), print the exact `export XXX_API_KEY=...` instruction.
  - Never prompts for or stores the key itself.
- `kcl init --non-interactive`:
  - Supports `--enable-embeddings --embedding-provider openrouter --embedding-model text-embedding-3-small`
  - During init we check that the corresponding env var is present (warning only; user may set it later).
- `kcl config` can still `set embeddings.provider ...` etc. for power users (we add a small validator).
- At runtime `EmbeddingConfig::load()` (or similar) reads from the main `Config`; the actual key is fetched from env inside the embeddings client.

### 7. DB Schema (migrations in db.rs)
- `user_version` bumps to 3, 4, ...
- New columns on `packages`: `shallow INTEGER NOT NULL DEFAULT 0`, `prepare_scope TEXT NOT NULL DEFAULT 'global'`
- New columns on `conversations`: `git_commit_sha TEXT`, `git_branch TEXT`
- New table `conversation_embeddings` (for the vectors)
- New table `package_prepared_contexts` (the maps)
- All foreign keys, indexes, and `ON DELETE CASCADE` where appropriate.
- `db::open` and `open_memory` register the sqlite-vec auto-extension exactly once.

### 8. Prompt injection (build_prompt + ask)
- `build_prompt` signature extended (or a new `build_ask_prompt` that takes more context):
  - Optional `prepared_map: Option<PreparedContext>`
  - Optional `similar_memories: Vec<SimilarMemory>` (id, question, similarity)
- The injected blocks are clearly labeled so the agent can ignore or act on them (`kcl remember ...`).
- Recent-questions context remains (now even more useful because the agent has the map + memories).

### 9. Style, Quality, No Tech Debt
- Every identifier and literal in user-facing messages uses backticks (per AGENTS.md).
- All new commands have `--help`, examples in README + `agent-usage.md`.
- Full test coverage (unit + integration-style with real git repos + mock harnesses that return deterministic maps).
- Graceful degradation everywhere (missing key, extension registration failure, model dim mismatch, stale map, etc.).
- No dead code, no duplicated logic between prepare/ask, no "TODO" left in the shipped changes.
- Release process unchanged (sqlite-vec is a normal crate; binary is still self-contained).

### 10. File Ownership (for subagent work)

- `src/cli.rs`, `src/main.rs` — CLI surface + dispatch
- `src/config.rs` — EmbeddingConfig + Provider enum + validation
- `src/db.rs` — migrations + sqlite-vec registration
- `src/models/package.rs` + `src/models/conversation.rs` + new `models/prepared_context.rs`
- `src/git.rs` — shallow clone/checkout/pull adjustments + tests
- `src/commands/packages.rs` — `--shallow` flag, `prepare-scope` setter, export/import
- `src/commands/prepare.rs` (new) — full command
- `src/commands/remember.rs` (new) — full command
- `src/commands/ask.rs` + `src/harness.rs` — wiring + prompt building
- `src/commands/init.rs` — interactive + flag parsing for embeddings
- `src/embeddings.rs` (new) — client, similarity, env-key loading, errors
- `README.md`, `skills/agent-usage.md`, `AGENTS.md`, `CLAUDE.md`, inline help — docs
- All tests updated / added in the relevant modules

### 11. Subagent Execution Strategy
Subagents will be given narrow, complete ownership of vertical slices with the explicit mandate "deliver a fully working, tested, documented piece that compiles cleanly and passes `cargo test` when integrated". They read this plan + the existing AGENTS.md style guide.

The main agent (codex) will:
- Lay the skeleton (deps, CLI enums, model fields, migration scaffolding, registration call)
- Launch 5–6 parallel subagents for the slices
- Perform final integration, `cargo test`, doc review, and produce the final diff for human + claude/codex audit.

No commits until the human says so.

---

This plan exists so that future reviewers (and the subagents themselves) have a single source of truth for the "why" behind every cross-cutting decision. Any deviation during implementation must be justified and recorded here.

End of plan.