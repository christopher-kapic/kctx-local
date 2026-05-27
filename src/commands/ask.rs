use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::db;
use crate::embeddings::{EmbeddingConfig, SimilarMemory, embed_question, find_similar_questions};
use crate::git;
use crate::git::HeadState;
use crate::harness;
use crate::models::conversation::Conversation;
use crate::models::package::{Package, SourceType};
use crate::models::prepared_context::PreparedContext;
use crate::paths;

/// The JSON log file written to disk for each conversation.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ConversationLog {
    pub(crate) id: String,
    pub(crate) package_id: String,
    pub(crate) package_identifier: String,
    pub(crate) question: String,
    pub(crate) harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    pub(crate) started_at: String,
    pub(crate) finished_at: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) response: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pull_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) git_commit_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) git_branch: Option<String>,
    /// The value of the package's `prepare_scope` ("global" or "branch") at
    /// the moment this `ask` was performed. Used by `kcl remember` for
    /// provenance reporting.
    #[serde(default = "default_prepare_scope")]
    pub(crate) prepare_scope: String,
}

fn default_prepare_scope() -> String {
    "global".to_string()
}

/// Arguments for a single `kcl ask` invocation.
///
/// Grouped into a struct so the call site (main.rs) and `run` itself stay
/// readable as the set of knobs grows.
pub struct AskArgs<'a> {
    pub identifier: &'a str,
    pub question: &'a str,
    pub harness_override: Option<&'a str>,
    pub model: Option<&'a str>,
    pub timeout_override: Option<u64>,
    pub no_pull: bool,
    pub branch_override: Option<&'a str>,
    pub context: u32,
}

pub async fn run(args: AskArgs<'_>) -> Result<i32> {
    let AskArgs {
        identifier,
        question,
        harness_override,
        model,
        timeout_override,
        no_pull,
        branch_override,
        context,
    } = args;

    // 1. Open DB and look up package.
    let db_path = paths::db_file()?;
    let conn = db::open(&db_path)?;

    let pkg = Package::get_by_identifier(&conn, identifier)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Package `{}` not found. Run `kcl list` to see available packages.",
            identifier
        )
    })?;
    // `pkg.shallow` is recorded at registration time. It currently only affects
    // the initial `git clone` (see git.rs). It is not yet used to change
    // harness invocation or prompt construction; the known history-truncation
    // limitation of shallow clones applies to all operations on the package.

    // Drop the connection below once we've finished all DB reads (step 4) so
    // it isn't held open for the duration of the harness run, which could
    // otherwise block concurrent writers.

    // 2. Load config to resolve harness.
    let config = Config::load_or_default()?;

    // Resolve harness name: CLI flag > package override > config default.
    let harness_name = harness_override
        .map(|s| s.to_string())
        .or_else(|| pkg.harness.clone())
        .unwrap_or_else(|| config.default_harness.clone());

    let harness_config = config
        .harnesses
        .get(&harness_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Harness `{}` not found in config. Run `kcl init` to detect harnesses or `kcl config set harnesses.{}.command <path>` to add it manually.",
                harness_name,
                harness_name
            )
        })?
        .clone();

    let timeout = timeout_override.unwrap_or(config.default_timeout);
    crate::config::validate_timeout(timeout)?;

    // Resolve the effective model: CLI flag wins, otherwise fall back to the
    // harness's configured `default_model` (if any). The resolved value is
    // forwarded to the harness and recorded in the conversation log so users
    // can tell which model actually answered.
    let effective_model: Option<String> = model
        .map(|s| s.to_string())
        .or_else(|| harness_config.default_model.clone());

    // 3a. Resolve the effective branch to check out. CLI `--branch` wins;
    //     otherwise fall back to the package's pinned `source_branch` (set
    //     when the package was added with `--branch`). Local packages have
    //     `source_branch == None`, so this naturally never fires for them.
    //
    //     The `--branch is only valid for git packages` error must only be
    //     raised when the user *explicitly* passed `--branch` for a non-git
    //     package — not when the branch comes from `pkg.source_branch`.
    if branch_override.is_some() && pkg.source_type != SourceType::Git {
        anyhow::bail!(
            "`--branch` is only valid for git packages; `{}` is a local package",
            pkg.identifier
        );
    }
    let effective_branch: Option<&str> =
        resolve_target_branch(branch_override, pkg.source_branch.as_deref());

    // If an effective branch is resolved AND it differs from the package's
    // current branch, check it out (saving the current HEAD state so we can
    // restore it after the harness runs, including the case where the
    // original HEAD was detached).
    let repo_path = Path::new(&pkg.path);
    let original_head: Option<HeadState> = if let Some(target_branch) = effective_branch {
        let current = git::current_head(repo_path).await.map_err(|e| {
            anyhow::anyhow!(
                "failed to determine current HEAD for `{}`: {}",
                pkg.identifier,
                e
            )
        })?;

        // If we're already on the requested branch there's nothing to do —
        // and nothing to restore. Detached HEAD always needs a checkout.
        let already_on_target = matches!(&current, HeadState::Branch(b) if b == target_branch);

        if already_on_target {
            None
        } else {
            let current_desc = match &current {
                HeadState::Branch(b) => b.clone(),
                HeadState::Detached(sha) => format!("detached at {}", &sha[..sha.len().min(8)]),
            };
            eprintln!(
                "checking out `{}` (was on `{}`) ...",
                target_branch, current_desc
            );
            git::checkout(repo_path, target_branch).await?;
            Some(current)
        }
    } else {
        None
    };

    // Run the post-checkout body inside an inner async block so we can run
    // `restore_head` on every exit path (including the `?` errors below)
    // before propagating the result. A Drop guard would not work here
    // because restoration is async.
    let result = run_after_checkout(RunAfterCheckout {
        pkg: &pkg,
        question,
        harness_name,
        harness_config,
        effective_model,
        timeout,
        no_pull,
        effective_branch_some: effective_branch.is_some(),
        context,
        repo_path,
        db_path: db_path.clone(),
        conn,
    })
    .await;

    // Restore the original HEAD (if we changed it) regardless of how the
    // post-checkout body finished. Failures are logged but never override
    // the inner result, since the harness has already produced its answer.
    if let Some(head) = original_head {
        restore_head(repo_path, &head).await;
    }

    result
}

/// Inputs for the post-checkout phase of `run`.
///
/// Bundled into a struct so the helper signature stays readable — `run` itself
/// uses these to spawn the harness, persist the log, and update the history
/// index after a successful (or no-op) `--branch` checkout.
struct RunAfterCheckout<'a> {
    pkg: &'a Package,
    question: &'a str,
    harness_name: String,
    harness_config: crate::config::HarnessConfig,
    effective_model: Option<String>,
    timeout: u64,
    no_pull: bool,
    /// True when an effective branch (CLI override or pinned `pkg.source_branch`)
    /// was resolved for this run. Drives the auto-pull decision so the user
    /// gets the latest commits on the branch they asked about.
    effective_branch_some: bool,
    context: u32,
    repo_path: &'a Path,
    db_path: std::path::PathBuf,
    conn: rusqlite::Connection,
}

/// The post-checkout body of `kcl ask`: pull, build prompt, run harness, log.
///
/// Extracted so `run` can guarantee `restore_head` runs on every exit path
/// from this function (including any propagated `?` errors).
async fn run_after_checkout(args: RunAfterCheckout<'_>) -> Result<i32> {
    let RunAfterCheckout {
        pkg,
        question,
        harness_name,
        harness_config,
        effective_model,
        timeout,
        no_pull,
        effective_branch_some,
        context,
        repo_path,
        db_path,
        conn,
    } = args;

    // 3b. Auto-pull if applicable. --no-pull always wins. When an effective
    //     branch was resolved (either via `--branch` or via the package's
    //     pinned `source_branch`) we default to pulling so the user gets
    //     the latest commits on that branch; `--no-pull` still suppresses it.
    let should_pull = if no_pull {
        false
    } else if effective_branch_some {
        true
    } else {
        pkg.auto_pull && pkg.source_type == SourceType::Git
    };
    let mut pull_error: Option<String> = None;
    if should_pull && pkg.source_type == SourceType::Git {
        eprintln!("pulling {} ...", pkg.identifier);
        match git::pull(repo_path).await {
            Ok(msg) => eprintln!("{}: {}", pkg.identifier, msg),
            Err(e) => {
                let msg = format!("pull failed for {}: {}", pkg.identifier, e);
                eprintln!("warning: {}", msg);
                pull_error = Some(msg);
            }
        }
    }

    // 4. Capture provenance (pre-harness) and load prepared map + semantic
    //    similar memories. The pre-harness state is what the harness will
    //    actually see at invocation; it drives the prepared-map lookup, the
    //    staleness calculation, and the prompt header. The conversation log
    //    later records the post-harness state (see below) so that a harness
    //    which mutates HEAD does not get a misleading "answered at commit X"
    //    record. The two are compared after the run with a stderr warning on
    //    divergence.
    let pre_harness_sha: Option<String> = if pkg.source_type == SourceType::Git {
        git::current_commit_sha(repo_path).await.ok()
    } else {
        None
    };
    let pre_harness_branch: Option<String> = if pkg.source_type == SourceType::Git {
        git::current_branch(repo_path).await.ok().flatten()
    } else {
        None
    };
    // Aliases kept under the original names so the rest of the function (which
    // legitimately needs the pre-harness state — prepared-map lookup, prompt
    // header, staleness math) reads unchanged.
    let current_head_sha = pre_harness_sha.clone();
    let current_branch = pre_harness_branch.clone();

    // Load the applicable prepared orientation map (global vs per-branch)
    // using the getters provided by the model. Errors are ignored (graceful).
    let prepared_context: Option<PreparedContext> = if pkg.wants_per_branch_prepare() {
        if let Some(ref br) = current_branch {
            PreparedContext::get_latest_for_package_and_branch(&conn, &pkg.id, br)
                .ok()
                .flatten()
        } else {
            PreparedContext::get_latest_for_package(&conn, &pkg.id)
                .ok()
                .flatten()
        }
    } else {
        PreparedContext::get_latest_for_package(&conn, &pkg.id)
            .ok()
            .flatten()
    };

    // Pre-compute staleness distance while we are still async (git helper).
    //
    // Hot path: when the prepared map was recorded on the exact commit that is
    // currently checked out, the map is definitionally fresh (0 commits
    // behind). Compute that equality here and short-circuit so we do NOT spawn
    // a `git rev-list` subprocess at all. (The git helper already returns 0
    // for an equal range, but only after being awaited/spawned — this avoids
    // the process entirely in the common prepared+unchanged case.) Behavior
    // for the non-equal case is unchanged: we still ask git for the count.
    let recorded_sha = prepared_context
        .as_ref()
        .and_then(|p| p.git_commit_sha.as_deref());
    let commits_behind: Option<usize> =
        match staleness_plan(recorded_sha, current_head_sha.as_deref()) {
            StalenessPlan::None => None,
            StalenessPlan::Fresh => Some(0),
            StalenessPlan::AskGit { base, head } => {
                git::commit_count_between(repo_path, base, head).await.ok()
            }
        };

    // If embeddings configured, embed the current question and find similar
    // prior conversations (same model+dim). Top 3 above 0.80 cosine. The
    // computed embedding is retained in `cached_embedding` so the post-harness
    // storage step can reuse it instead of calling the embeddings API a second
    // time per `ask` (one API call, one network round-trip).
    let (similar_memories, cached_embedding): (
        Vec<SimilarMemory>,
        Option<(EmbeddingConfig, Vec<f32>)>,
    ) = if let Some(emb_cfg) = EmbeddingConfig::load() {
        match embed_question(&emb_cfg, question).await {
            Ok(emb) => {
                let dim = emb.len();
                let sims =
                    find_similar_questions(&conn, &pkg.id, &emb, &emb_cfg.model, dim, 3, 0.80);
                (sims, Some((emb_cfg, emb)))
            }
            Err(e) => {
                eprintln!(
                    "warning: failed to embed question for similarity search: {:#}",
                    e
                );
                (Vec::new(), None)
            }
        }
    } else {
        (Vec::new(), None)
    };

    // 5. Build prompt (now receives prepared map + similar-memory hints).
    let recent_questions = if context > 0 {
        Conversation::recent_questions(&conn, &pkg.id, context)?
    } else {
        Vec::new()
    };

    let context_slice = if recent_questions.is_empty() {
        None
    } else {
        Some(recent_questions.as_slice())
    };

    let prompt = harness::build_prompt(
        &pkg.display_name,
        &pkg.identifier,
        question,
        context_slice,
        prepared_context.as_ref(),
        current_head_sha.as_deref(),
        commits_behind,
        &similar_memories,
    );

    // Release the DB connection before the long-running harness invocation so
    // it doesn't hold WAL locks (or `busy_timeout` slots) while other `kcl`
    // processes try to write.
    drop(conn);

    // 5. Spawn harness subprocess.
    let started_at = Utc::now();

    let cwd = std::path::PathBuf::from(&pkg.path);

    // When a prepared orientation map was injected into the prompt, allow the
    // harness config to append its `prepared_args` (e.g. `--max-turns`, a
    // restricted `--allowedTools`) so exploration is mechanically capped even
    // if the model ignores the prompt's "trust the map" guidance. With no map
    // present this is `false` and the harness argv is unchanged (matching
    // `kcl prepare`, which never gets `prepared_args`).
    let map_present = prepared_context.is_some();

    let harness_result = harness::run_harness_with_prepared_args(
        &harness_config,
        &prompt,
        &cwd,
        timeout,
        true, // stream stdout to caller
        effective_model.as_deref(),
        map_present,
    )
    .await;

    let finished_at = Utc::now();

    // Re-capture provenance now that the harness has finished. Some harnesses
    // can mutate HEAD (checkout, reset) during their exploration; recording
    // *only* the pre-harness state would then attribute the answer to a commit
    // the harness was no longer on. If the post-harness state differs from
    // pre, we surface a single stderr warning (so users can spot
    // misbehaving harnesses) and persist the post-harness values as
    // authoritative. Best-effort: a git failure here falls back to the pre
    // values rather than silently dropping provenance.
    let post_harness_sha: Option<String> = if pkg.source_type == SourceType::Git {
        git::current_commit_sha(repo_path).await.ok()
    } else {
        None
    };
    let post_harness_branch: Option<String> = if pkg.source_type == SourceType::Git {
        git::current_branch(repo_path).await.ok().flatten()
    } else {
        None
    };
    let logged_commit_sha = post_harness_sha.clone().or_else(|| pre_harness_sha.clone());
    let logged_branch = post_harness_branch
        .clone()
        .or_else(|| pre_harness_branch.clone());
    if pre_harness_sha != post_harness_sha || pre_harness_branch != post_harness_branch {
        let fmt = |sha: &Option<String>, br: &Option<String>| -> String {
            let s = sha.as_deref().unwrap_or("none");
            let b = br.as_deref().unwrap_or("none");
            format!("commit `{}` on branch `{}`", s, b)
        };
        eprintln!(
            "warning: harness `{}` changed HEAD during the run (pre: {}; post: {}); recording post-harness state on the conversation log",
            harness_name,
            fmt(&pre_harness_sha, &pre_harness_branch),
            fmt(&post_harness_sha, &post_harness_branch)
        );
    }

    // Handle harness execution result. Regardless of success or failure we
    // persist a conversation record so every invocation appears in `kcl history`.
    // `exit_code = None` means the child had no exit status (killed by a signal)
    // or kcl could not obtain one (spawn failure, timeout, interrupted wait).
    let (mut response_text, mut exit_code) = match harness_result {
        Ok(output) => (output.stdout, output.exit_code),
        Err(e) => {
            eprintln!("error: {}", e);
            (format!("[error] {}", e), None)
        }
    };

    // Guard: a harness that completed "successfully" (exit 0) but produced an
    // empty or whitespace-only answer is NOT a usable result. Previously this
    // was recorded as `exit_code=0, response=""` — a successful conversation
    // that would then be embedded and surfaced as a reusable answer by
    // `kcl remember`. Demote it to a failure: keep the conversation in the
    // on-disk log and the history index (so the attempt is still visible) but
    // record a non-zero exit code so it is never embedded, never treated as a
    // reusable answer, and the command exits non-zero. An empty answer from a
    // harness that *did* run to completion is closest to exit code 3.
    let empty_success = is_empty_success(exit_code, &response_text);
    if empty_success {
        eprintln!(
            "error: harness `{}` exited `0` but produced an empty response; treating as a failed answer (not recorded as reusable)",
            harness_name
        );
        // Make the persisted log self-explanatory rather than a silent "".
        response_text = "[error] harness exited 0 but produced an empty response".to_string();
        // Demote so every downstream consumer (log, history row, embedding
        // gate, exit code) sees a completed-but-failed run.
        exit_code = Some(3);
    }

    // 6. Save conversation log as JSON file.
    let conv_id = uuid::Uuid::new_v4().to_string();
    let short_id = &conv_id[..8];
    let timestamp = started_at.format("%Y%m%d-%H%M%S");
    let log_filename = format!("{}-{}.json", timestamp, short_id);
    let relative_log_path = format!("{}/{}", pkg.identifier, log_filename);

    let log_dir = paths::log_dir()?;
    let pkg_log_dir = log_dir.join(&pkg.identifier);

    // Only record the model in the log if the harness actually accepted it.
    // (model_args being non-empty is the signal that the model was forwarded.)
    let logged_model = effective_model
        .clone()
        .filter(|_| !harness_config.model_args.is_empty());

    let log = ConversationLog {
        id: conv_id.clone(),
        package_id: pkg.id.clone(),
        package_identifier: pkg.identifier.clone(),
        question: question.to_string(),
        harness: harness_name.clone(),
        model: logged_model,
        started_at: started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        exit_code,
        response: response_text,
        pull_error,
        git_commit_sha: logged_commit_sha.clone(),
        git_branch: logged_branch.clone(),
        prepare_scope: pkg.prepare_scope.clone(),
    };

    // Best-effort: the user has already received the harness response, so a
    // failure to persist the log should not fail the command.
    let log_path = pkg_log_dir.join(&log_filename);
    if let Err(e) = write_log_file(&pkg_log_dir, &log_path, &log) {
        eprintln!(
            "warning: failed to write conversation log to {}: {:#}",
            log_path.display(),
            e
        );
    }

    // 7. Build the conversation row we will (best-effort) persist, plus the
    //    optional embedding for successful runs. The actual write is delegated
    //    to `record_conversation_and_embedding` below so the main flow stays
    //    readable. The embedding must be computed before we open any DB write
    //    transaction (see the helper for rationale).
    let conversation = Conversation {
        id: conv_id.clone(),
        package_id: pkg.id.clone(),
        question: question.to_string(),
        harness: harness_name,
        exit_code,
        log_path: relative_log_path,
        created_at: started_at,
        git_commit_sha: logged_commit_sha,
        git_branch: logged_branch,
    };

    // Reuse the embedding computed pre-harness for the similarity search. We
    // do NOT re-embed here: that would double the API charges and the network
    // round-trips per `ask` and could store a vector that differs from the one
    // searched against. If the pre-harness embed failed (or embeddings are
    // disabled), we simply skip storage — degrading gracefully matches the
    // search path's behavior. `dim as i64` below is safe: embedding
    // dimensions are tiny.
    let embedding_row = if matches!(exit_code, Some(0)) {
        cached_embedding.map(|(emb_cfg, emb)| {
            let dim = emb.len();
            let blob = crate::embeddings::embedding_to_blob(&emb);
            let created_at = Utc::now().to_rfc3339();
            (emb_cfg.model, dim, blob, created_at)
        })
    } else {
        None
    };

    // 7. Best-effort persistence of the conversation (and embedding for
    //    successful runs) into the SQLite index. Extracted so the main flow
    //    stays linear; all errors are warnings only.
    record_conversation_and_embedding(&db_path, conversation, &conv_id, embedding_row);

    // Exit code semantics:
    //   0 → harness succeeded
    //   2 → harness terminated without a normal exit status (signal-killed,
    //       spawn failure, timeout). Distinguished from harness errors because
    //       the child did not get to report its own result.
    //   3 → harness ran to completion but exited non-zero.
    match exit_code {
        Some(0) => Ok(0),
        Some(_) => Ok(3),
        None => Ok(2),
    }
}

/// Serialize `log` and write it to `log_path`, creating `pkg_log_dir` first.
fn write_log_file(pkg_log_dir: &Path, log_path: &Path, log: &ConversationLog) -> Result<()> {
    std::fs::create_dir_all(pkg_log_dir)
        .with_context(|| format!("failed to create log directory {}", pkg_log_dir.display()))?;
    let log_json = serde_json::to_string_pretty(log)?;
    std::fs::write(log_path, log_json)
        .with_context(|| format!("failed to write conversation log to {}", log_path.display()))?;
    Ok(())
}

/// Best-effort write of the `Conversation` row (and optional embedding row for
/// successful runs) into the SQLite index used by `kcl history` / `kcl remember`.
///
/// All failures produce only a warning on stderr; the user's harness answer is
/// never affected. The embedding (if present) must already have been computed by
/// the caller so that the short write transaction contains no network I/O.
fn record_conversation_and_embedding(
    db_path: &Path,
    conversation: Conversation,
    conv_id: &str,
    embedding_row: Option<(String, usize, Vec<u8>, String)>,
) {
    // We already ran migrations once at the top of `run` — skip them here so a
    // single ask doesn't poll `user_version` twice per invocation. `open_no_migrate`
    // is documented as exactly this use case.
    match db::open_no_migrate(db_path) {
        Ok(mut conn) => match conn.transaction() {
            Ok(tx) => {
                if let Err(e) = conversation.insert(&tx) {
                    eprintln!("warning: failed to record conversation in history: {:#}", e);
                } else {
                    if let Some((model, dim, blob, created_at)) = embedding_row {
                        const INSERT_EMBEDDING_SQL: &str = "INSERT OR REPLACE INTO conversation_embeddings (conversation_id, model, dim, embedding, created_at) VALUES (?1, ?2, ?3, ?4, ?5)";
                        if let Err(e) = tx.execute(
                            INSERT_EMBEDDING_SQL,
                            rusqlite::params![conv_id, &model, dim as i64, blob, created_at],
                        ) {
                            eprintln!("warning: failed to store question embedding: {:#}", e);
                        }
                    }

                    if let Err(e) = tx.commit() {
                        eprintln!(
                            "warning: failed to commit conversation transaction: {:#}",
                            e
                        );
                        // On failure the consumed Transaction's Drop performs a
                        // ROLLBACK (default DropBehavior::Rollback). This is the
                        // same effect the old manual ROLLBACK on error paths had.
                    }
                }
            }
            Err(e) => {
                eprintln!("warning: failed to start conversation transaction: {:#}", e);
            }
        },
        Err(e) => {
            eprintln!(
                "warning: failed to reopen database to record conversation: {:#}",
                e
            );
        }
    }
}

/// What `kcl ask` must do to learn how stale the prepared map is.
///
/// Splitting this decision out keeps the SHA-equal hot-path optimization
/// unit-testable without spawning git.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StalenessPlan<'a> {
    /// No prepared map, or no recorded/current SHA — staleness is unknown.
    None,
    /// Recorded SHA equals current HEAD: the map is definitionally fresh
    /// (0 commits behind). No git subprocess is spawned.
    Fresh,
    /// SHAs differ: git must be asked for the commit count of `base..head`.
    AskGit { base: &'a str, head: &'a str },
}

/// Decide how to compute "commits behind" for the prepared map.
///
/// Hot path: if the map's recorded commit equals the current HEAD we return
/// [`StalenessPlan::Fresh`] so the caller can short-circuit to `Some(0)`
/// *without* spawning `git rev-list`. Only when the SHAs genuinely differ do
/// we ask git (preserving the previous behavior for that case exactly).
pub(crate) fn staleness_plan<'a>(
    recorded_sha: Option<&'a str>,
    current_sha: Option<&'a str>,
) -> StalenessPlan<'a> {
    match (recorded_sha, current_sha) {
        (Some(rec), Some(cur)) => {
            if rec == cur {
                StalenessPlan::Fresh
            } else {
                StalenessPlan::AskGit {
                    base: rec,
                    head: cur,
                }
            }
        }
        _ => StalenessPlan::None,
    }
}

/// True when the harness ran to completion successfully (exit code `Some(0)`)
/// yet produced an empty or whitespace-only answer. Such a result must NOT be
/// recorded as a reusable success (it would otherwise be embedded and surfaced
/// by `kcl remember`); the caller demotes it to a failed run.
pub(crate) fn is_empty_success(exit_code: Option<i32>, response: &str) -> bool {
    matches!(exit_code, Some(0)) && response.trim().is_empty()
}

/// Resolve the branch `kcl ask` (or `prepare`) should check out for this run.
///
/// CLI `--branch` always wins over the package's pinned `source_branch`. If
/// neither is set, returns `None` and the package's current HEAD is left
/// untouched.
pub(crate) fn resolve_target_branch<'a>(
    branch_override: Option<&'a str>,
    pkg_source_branch: Option<&'a str>,
) -> Option<&'a str> {
    branch_override.or(pkg_source_branch)
}

/// Try to restore `repo_path` to its original `HeadState`.
///
/// Failures are logged to stderr but never propagated — the harness has
/// already produced its result and the user shouldn't see a successful
/// answer turn into a failed exit code just because git was unhappy.
pub(crate) async fn restore_head(repo_path: &Path, head: &HeadState) {
    let target_desc = match head {
        HeadState::Branch(name) => format!("branch `{}`", name),
        HeadState::Detached(sha) => format!("detached at `{}`", &sha[..sha.len().min(8)]),
    };
    eprintln!("restoring {} ...", target_desc);
    if let Err(e) = git::restore_head(repo_path, head).await {
        eprintln!("warning: failed to restore {}: {}", target_desc, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::package::{Package, SourceType};

    #[test]
    fn ask_nonexistent_package_returns_error() {
        let conn = db::open_memory().unwrap();
        let result = Package::get_by_identifier(&conn, "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn conversation_log_serializes_correctly() {
        let log = ConversationLog {
            id: "abc123".to_string(),
            package_id: "pkg-uuid".to_string(),
            package_identifier: "axum".to_string(),
            question: "How does routing work?".to_string(),
            harness: "claude".to_string(),
            model: Some("claude-sonnet-4.6".to_string()),
            started_at: "2026-04-07T10:30:00+00:00".to_string(),
            finished_at: "2026-04-07T10:30:45+00:00".to_string(),
            exit_code: Some(0),
            response: "Routing in axum uses...".to_string(),
            pull_error: None,
            git_commit_sha: None,
            git_branch: None,
            prepare_scope: "global".to_string(),
        };

        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["id"], "abc123");
        assert_eq!(parsed["package_identifier"], "axum");
        assert_eq!(parsed["question"], "How does routing work?");
        assert_eq!(parsed["harness"], "claude");
        assert_eq!(parsed["model"], "claude-sonnet-4.6");
        assert_eq!(parsed["exit_code"], 0);
        assert_eq!(parsed["response"], "Routing in axum uses...");
    }

    #[test]
    fn conversation_log_with_null_exit_code() {
        let log = ConversationLog {
            id: "abc123".to_string(),
            package_id: "pkg-uuid".to_string(),
            package_identifier: "test".to_string(),
            question: "test?".to_string(),
            harness: "claude".to_string(),
            model: None,
            started_at: "2026-04-07T10:30:00+00:00".to_string(),
            finished_at: "2026-04-07T10:30:45+00:00".to_string(),
            exit_code: None,
            response: "output".to_string(),
            pull_error: None,
            git_commit_sha: None,
            git_branch: None,
            prepare_scope: "global".to_string(),
        };

        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["exit_code"].is_null());
        // model is omitted from the serialized JSON when None
        assert!(parsed.get("model").is_none());
    }

    #[test]
    fn recent_questions_with_context() {
        let conn = db::open_memory().unwrap();
        let pkg = Package::new(
            "test-ctx".to_string(),
            "Test Context".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/test-ctx".to_string(),
            false,
            None,
            false,
            "global".to_string(),
        );
        pkg.insert(&conn).unwrap();

        // Insert some conversations.
        for i in 0..5 {
            let conv = Conversation::new(
                pkg.id.clone(),
                format!("Question {}", i),
                "claude".to_string(),
                Some(0),
                format!("/tmp/logs/conv{}.json", i),
                None,
                None,
            );
            conv.insert(&conn).unwrap();
        }

        // Fetch recent questions with limit.
        let questions = Conversation::recent_questions(&conn, &pkg.id, 3).unwrap();
        assert_eq!(questions.len(), 3);
        // Most recent first.
        assert_eq!(questions[0], "Question 4");
        assert_eq!(questions[1], "Question 3");
        assert_eq!(questions[2], "Question 2");
    }

    #[test]
    fn conversation_log_captures_harness_error() {
        let log = ConversationLog {
            id: "err-123".to_string(),
            package_id: "pkg-uuid".to_string(),
            package_identifier: "broken-pkg".to_string(),
            question: "Will this fail?".to_string(),
            harness: "claude".to_string(),
            model: None,
            started_at: "2026-04-07T10:30:00+00:00".to_string(),
            finished_at: "2026-04-07T10:30:05+00:00".to_string(),
            exit_code: None,
            response: "[error] harness timed out after 120s".to_string(),
            pull_error: None,
            git_commit_sha: None,
            git_branch: None,
            prepare_scope: "global".to_string(),
        };

        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["id"], "err-123");
        assert_eq!(parsed["package_identifier"], "broken-pkg");
        assert!(parsed["exit_code"].is_null());
        assert!(parsed["response"].as_str().unwrap().starts_with("[error]"));
    }

    #[test]
    fn failed_harness_creates_db_record() {
        let conn = db::open_memory().unwrap();
        let pkg = Package::new(
            "fail-pkg".to_string(),
            "Fail Package".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/fail-pkg".to_string(),
            false,
            None,
            false,
            "global".to_string(),
        );
        pkg.insert(&conn).unwrap();

        let conv = Conversation::new(
            pkg.id.clone(),
            "question that fails".to_string(),
            "claude".to_string(),
            None,
            "/tmp/logs/fail.json".to_string(),
            None,
            None,
        );
        conv.insert(&conn).unwrap();

        let retrieved = Conversation::get_by_id(&conn, &conv.id)
            .unwrap()
            .expect("failed conversation should be persisted");
        assert_eq!(retrieved.exit_code, None);
        assert_eq!(retrieved.question, "question that fails");
    }

    #[test]
    fn conversation_log_records_pull_error() {
        let log = ConversationLog {
            id: "pull-err-1".to_string(),
            package_id: "pkg-uuid".to_string(),
            package_identifier: "axum".to_string(),
            question: "How does routing work?".to_string(),
            harness: "claude".to_string(),
            model: None,
            started_at: "2026-04-07T10:30:00+00:00".to_string(),
            finished_at: "2026-04-07T10:30:45+00:00".to_string(),
            exit_code: Some(0),
            response: "Routing in axum uses...".to_string(),
            pull_error: Some("pull failed for axum: remote unreachable".to_string()),
            git_commit_sha: None,
            git_branch: None,
            prepare_scope: "global".to_string(),
        };

        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(
            parsed["pull_error"].as_str().unwrap(),
            "pull failed for axum: remote unreachable"
        );

        // Round-trips cleanly.
        let reparsed: ConversationLog = serde_json::from_str(&json).unwrap();
        assert_eq!(
            reparsed.pull_error.as_deref(),
            Some("pull failed for axum: remote unreachable")
        );
    }

    #[test]
    fn conversation_log_omits_pull_error_when_none() {
        let log = ConversationLog {
            id: "no-pull-err".to_string(),
            package_id: "pkg-uuid".to_string(),
            package_identifier: "axum".to_string(),
            question: "q".to_string(),
            harness: "claude".to_string(),
            model: None,
            started_at: "2026-04-07T10:30:00+00:00".to_string(),
            finished_at: "2026-04-07T10:30:01+00:00".to_string(),
            exit_code: Some(0),
            response: "ok".to_string(),
            pull_error: None,
            git_commit_sha: None,
            git_branch: None,
            prepare_scope: "global".to_string(),
        };

        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("pull_error").is_none());
    }

    #[test]
    fn resolve_target_branch_prefers_override() {
        // Construct a Package with a pinned source_branch to mirror real usage.
        let pkg = Package::new(
            "hono".to_string(),
            "Hono".to_string(),
            SourceType::Git,
            Some("https://github.com/honojs/hono.git".to_string()),
            Some("next".to_string()),
            "/tmp/hono".to_string(),
            true,
            None,
            false,
            "global".to_string(),
        );

        // No CLI override: fall back to the package's pinned branch.
        assert_eq!(
            resolve_target_branch(None, pkg.source_branch.as_deref()),
            Some("next")
        );

        // CLI override wins over the pinned branch.
        assert_eq!(
            resolve_target_branch(Some("main"), pkg.source_branch.as_deref()),
            Some("main")
        );

        // Neither set: returns None (current HEAD left untouched).
        assert_eq!(resolve_target_branch(None, None), None);

        // Override set, no pinned branch.
        assert_eq!(resolve_target_branch(Some("dev"), None), Some("dev"));
    }

    #[test]
    fn recent_questions_empty_when_none_exist() {
        let conn = db::open_memory().unwrap();
        let pkg = Package::new(
            "empty-pkg".to_string(),
            "Empty Package".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/empty-pkg".to_string(),
            false,
            None,
            false,
            "global".to_string(),
        );
        pkg.insert(&conn).unwrap();

        let questions = Conversation::recent_questions(&conn, &pkg.id, 5).unwrap();
        assert!(questions.is_empty());
    }

    #[test]
    fn staleness_plan_sha_equal_is_fresh_and_skips_git() {
        // Hot path: identical SHAs => Fresh, no AskGit (no subprocess).
        assert_eq!(
            staleness_plan(Some("abc123"), Some("abc123")),
            StalenessPlan::Fresh
        );
    }

    #[test]
    fn staleness_plan_sha_differs_asks_git() {
        assert_eq!(
            staleness_plan(Some("aaaa"), Some("bbbb")),
            StalenessPlan::AskGit {
                base: "aaaa",
                head: "bbbb"
            }
        );
    }

    #[test]
    fn staleness_plan_none_when_sha_missing() {
        assert_eq!(staleness_plan(None, Some("bbbb")), StalenessPlan::None);
        assert_eq!(staleness_plan(Some("aaaa"), None), StalenessPlan::None);
        assert_eq!(staleness_plan(None, None), StalenessPlan::None);
    }

    #[test]
    fn empty_success_guard_flags_blank_zero_exit() {
        // Exit 0 + empty/whitespace => must be treated as failure.
        assert!(is_empty_success(Some(0), ""));
        assert!(is_empty_success(Some(0), "   \n\t  "));
    }

    #[test]
    fn empty_success_guard_allows_real_answer() {
        assert!(!is_empty_success(Some(0), "Routing lives in src/router.rs"));
    }

    #[test]
    fn empty_success_guard_ignores_non_zero_and_signal() {
        // Non-zero / signal exits are handled by the existing exit-code
        // mapping; the empty-success guard must not also fire for them.
        assert!(!is_empty_success(Some(3), ""));
        assert!(!is_empty_success(Some(1), "   "));
        assert!(!is_empty_success(None, ""));
    }
}
