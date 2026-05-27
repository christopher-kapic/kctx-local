//! Embeddings client for semantic question memory in kcl.
//!
//! Supports `openai` and `openrouter` providers (both use the standard
//! OpenAI-compatible `/v1/embeddings` POST endpoint and response shape).
//!
//! - Configuration (provider + model) lives in the main `Config` (see `config.rs`).
//! - API keys are **never** stored in config.json; they are always read at call
//!   time from `OPENAI_API_KEY` or `OPENROUTER_API_KEY`.
//! - On missing key (or other runtime failure) the caller is expected to degrade
//!   gracefully — `kcl ask` continues to work, just without similarity hints.
//!
//! ## BLOB storage format (for `conversation_embeddings.embedding`)
//!
//! Vectors are stored as `BLOB` columns containing the raw little-endian
//! representation of `f32` values:
//!
//! - byte 0..3   = f32[0] as little-endian IEEE-754 single
//! - byte 4..7   = f32[1]
//! - ...
//! - total len   = dim * 4  (no length prefix, no magic, no padding)
//!
//! This is the exact format expected by sqlite-vec's `vec_distance_cosine(blob, blob)`
//! and friends, as well as by `vec0` virtual tables.  Use
//! [`embedding_to_blob`] / [`blob_to_embedding`] for (de)serialization.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::Deserialize;

use crate::paths;

// Re-export the core runtime types (defined in config.rs per the plan's file ownership)
// so the embeddings module presents a clean, self-contained public API:
//   use crate::embeddings::{EmbeddingConfig, EmbeddingProvider, embed_question, find_similar_questions, ...};
pub use crate::config::{EmbeddingConfig, EmbeddingProvider};

impl EmbeddingConfig {
    /// Loads the optional `embeddings` subsection from `~/.config/kcl/config.json`.
    ///
    /// Returns `None` when:
    /// - the file does not exist,
    /// - it is empty / whitespace-only,
    /// - the `embeddings` key is absent, or
    /// - any I/O or parse error occurs (we degrade silently so that `kcl ask`
    ///   never hard-fails because of a bad config for the optional memory feature).
    #[allow(dead_code)]
    pub fn load() -> Option<Self> {
        (|| -> Result<Self> {
            let path = paths::config_file()?;
            if !path.exists() {
                bail!("config file does not exist");
            }
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            if contents.trim().is_empty() {
                bail!("config file is empty");
            }

            #[derive(Deserialize)]
            struct Wrapper {
                #[serde(default)]
                embeddings: Option<EmbeddingConfig>,
            }

            let w: Wrapper = serde_json::from_str(&contents)
                .with_context(|| format!("parsing {}", path.display()))?;
            w.embeddings
                .ok_or_else(|| anyhow::anyhow!("no `embeddings` key present"))
        })()
        .ok()
    }
}

/// Asynchronously obtain an embedding vector for `text` using the configured
/// provider and model.
///
/// The correct environment variable is read *at call time*:
/// - `OPENAI_API_KEY` for provider `openai`
/// - `OPENROUTER_API_KEY` for provider `openrouter`
///
/// The function never falls back to reading a key from disk or from the
/// `Config` struct.
///
/// On any error (missing key, HTTP failure, rate limit, dimension problems,
/// bad JSON, …) a richly formatted `anyhow::Error` is returned.  All user-facing
/// literals (env var names, provider names, model names, URLs) are wrapped in
/// backticks so they render nicely in terminals and agent output.
pub async fn embed_question(cfg: &EmbeddingConfig, text: &str) -> Result<Vec<f32>> {
    let base_url = default_base_url_for_provider(cfg.provider);
    embed_question_against(cfg, text, base_url).await
}

/// The production `/v1` base URL for `provider`. Pulled out so tests (and
/// any future ambient-config consumers) can point at a different host without
/// duplicating the match.
fn default_base_url_for_provider(provider: EmbeddingProvider) -> &'static str {
    match provider {
        EmbeddingProvider::Openai => "https://api.openai.com/v1",
        EmbeddingProvider::Openrouter => "https://openrouter.ai/api/v1",
    }
}

/// Same behavior as [`embed_question`] but with the OpenAI-compatible base URL
/// supplied by the caller (e.g. `http://127.0.0.1:PORT/v1` for HTTP-mocked
/// tests). Public only inside the crate; production code should call
/// [`embed_question`] which fills in the standard provider URL.
pub(crate) async fn embed_question_against(
    cfg: &EmbeddingConfig,
    text: &str,
    base_url: &str,
) -> Result<Vec<f32>> {
    if text.trim().is_empty() {
        bail!("cannot embed an empty or whitespace-only question");
    }

    let key_var = match cfg.provider {
        EmbeddingProvider::Openai => "OPENAI_API_KEY",
        EmbeddingProvider::Openrouter => "OPENROUTER_API_KEY",
    };

    let api_key = std::env::var(key_var)
        .ok()
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "embedding provider `{}` is configured but the `{}` environment variable is not set (or empty). \
                 Export it (`export {}=\"sk-...\"` or the OpenRouter equivalent) and re-run. \
                 `kcl ask` will continue without semantic memory for this run.",
                cfg.provider,
                key_var,
                key_var
            )
        })?;

    let url = format!("{}/embeddings", base_url);
    let provider_label = cfg.provider.as_str();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("kcl/", env!("CARGO_PKG_VERSION"), " (embeddings)"))
        .build()
        .context("building HTTP client for embeddings request")?;

    let resp = client
        .post(&url)
        .bearer_auth(&api_key)
        .json(&serde_json::json!({
            "model": &cfg.model,
            "input": text,
        }))
        .send()
        .await
        .with_context(|| {
            format!(
                "failed to POST to `{}/embeddings` (model `{}`)",
                base_url, cfg.model
            )
        })?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<no response body>".to_string());
        let short: String = body.chars().take(400).collect();

        match status.as_u16() {
            401 | 403 => bail!(
                "authentication failed (HTTP {}) calling `{}` embeddings API for model `{}`. \
                 Check that the `{}` environment variable is valid and has not been revoked.",
                status,
                provider_label,
                cfg.model,
                key_var
            ),
            429 => bail!(
                "rate limit (HTTP 429) from `{}` embeddings API. Wait a short while or switch \
                 to a different provider / model via `kcl config set embeddings.model ...`.",
                provider_label
            ),
            400 => bail!(
                "bad request (HTTP 400) to `{}` embeddings endpoint for model `{}`: `{}`. \
                 Double-check the exact model identifier in your embeddings config.",
                provider_label,
                cfg.model,
                short
            ),
            _ => bail!(
                "embeddings API call to `{}` (model `{}`) failed with status {}: `{}`",
                provider_label,
                cfg.model,
                status,
                short
            ),
        }
    }

    #[derive(Deserialize)]
    struct EmbeddingData {
        embedding: Vec<f32>,
    }

    #[derive(Deserialize)]
    struct EmbeddingResponse {
        data: Vec<EmbeddingData>,
    }

    let parsed: EmbeddingResponse = resp
        .json()
        .await
        .context("decoding JSON body from embeddings API response")?;

    let vector = parsed
        .data
        .into_iter()
        .next()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "embeddings response from `{}` for model `{}` contained no `data[0].embedding` entry",
                provider_label, cfg.model
            )
        })?
        .embedding;

    if vector.is_empty() {
        bail!(
            "received empty embedding vector (len=0) from `{}` model `{}`",
            provider_label,
            cfg.model
        );
    }

    Ok(vector)
}

/// Computes the cosine similarity of two vectors (range `[-1.0, 1.0]`).
///
/// Returns `0.0` when the vectors have different lengths, are empty, or either
/// has (near) zero L2 norm.  The implementation is pure Rust, allocation-free
/// for the hot loop, and is used both for the fallback path in
/// `find_similar_questions` and for unit tests.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    if norm_a < 1e-12 || norm_b < 1e-12 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Batch version: returns the cosine similarity of `query` with every vector
/// in `candidates`, in the same order.  Mismatched-length or zero-norm entries
/// yield `0.0`.
#[allow(dead_code)]
pub fn cosine_similarities(query: &[f32], candidates: &[Vec<f32>]) -> Vec<f32> {
    candidates
        .iter()
        .map(|c| cosine_similarity(query, c))
        .collect()
}

/// Serialize an embedding to the little-endian `f32` byte blob format used
/// by the `conversation_embeddings` table and by sqlite-vec.
pub fn embedding_to_blob(emb: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(emb.len() * 4);
    for &f in emb {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Deserialize a little-endian `f32` byte blob back into a vector.
///
/// Returns `Err` (rather than panicking) when `bytes.len()` is not a multiple
/// of 4 so a single corrupt row in `conversation_embeddings` cannot crash
/// `kcl ask` mid-similarity-search — callers are expected to log + skip the
/// offending row.
pub fn blob_to_embedding(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        anyhow::bail!(
            "embedding BLOB length {} is not a multiple of 4 (corrupt row); skipping",
            bytes.len()
        );
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| {
            let arr: [u8; 4] = chunk.try_into().expect("chunks_exact guarantees 4 bytes");
            f32::from_le_bytes(arr)
        })
        .collect())
}

/// A semantically similar past question discovered via embeddings.
#[derive(Debug, Clone, PartialEq)]
pub struct SimilarMemory {
    /// Conversation ID (short prefix or full).  Safe to pass to `kcl remember`.
    pub id: String,
    /// The original question text that was asked.
    pub question: String,
    /// Cosine similarity in `[0.0, 1.0]` (higher = more similar to current query).
    pub similarity: f32,
}

/// Look up past questions for `package_id` whose embeddings (under the exact
/// same `model` + `dim`) are sufficiently close to `query_embedding`.
///
/// The lookup prefers the fast `vec_distance_cosine` SQL function provided by
/// the sqlite-vec extension (when it has been registered via `db::open`).  If
/// the function is unavailable the routine transparently falls back to a
/// pure-Rust brute-force implementation over the (small) result set.
///
/// Only rows whose stored `model` and `dim` exactly match the supplied values
/// participate in the search (this gives the documented “model change = old
/// memories ignored” behaviour).
///
/// Returned vector is sorted by descending similarity and truncated to at most
/// `top_k` entries.  Only entries with `similarity >= threshold` are kept.
pub fn find_similar_questions(
    conn: &Connection,
    package_id: &str,
    query_embedding: &[f32],
    model: &str,
    dim: usize,
    top_k: usize,
    threshold: f32,
) -> Vec<SimilarMemory> {
    if top_k == 0 || query_embedding.is_empty() || query_embedding.len() != dim {
        return Vec::new();
    }
    if model.trim().is_empty() {
        return Vec::new();
    }

    let query_blob = embedding_to_blob(query_embedding);

    // Fast path via sqlite-vec (if the auto-extension registered the functions)
    if let Ok(results) =
        try_find_with_vec_distance(conn, package_id, &query_blob, model, dim, top_k, threshold)
    {
        return results;
    }

    // Pure-Rust fallback (always works, even when the extension is absent)
    try_find_with_rust_cosine(
        conn,
        package_id,
        query_embedding,
        model,
        dim,
        top_k,
        threshold,
    )
    .unwrap_or_default()
}

/// Internal: attempt the search using sqlite-vec's `vec_distance_cosine`.
/// Returns Err on any problem (missing function, wrong blob size for a row,
/// query failure, …) so the caller can fall back.
fn try_find_with_vec_distance(
    conn: &Connection,
    package_id: &str,
    query_blob: &[u8],
    model: &str,
    dim: usize,
    top_k: usize,
    threshold: f32,
) -> Result<Vec<SimilarMemory>> {
    // We deliberately fetch more than top_k so that the threshold filter
    // still has candidates even when the very closest ones are below it.
    let fetch_limit = (top_k.max(20) * 3) as i64;

    let sql = "
        SELECT
            c.id,
            c.question,
            vec_distance_cosine(e.embedding, ?)
        FROM conversation_embeddings e
        JOIN conversations c ON c.id = e.conversation_id
        WHERE c.package_id = ?
          AND e.model = ?
          AND e.dim = ?
        ORDER BY vec_distance_cosine(e.embedding, ?) ASC
        LIMIT ?
    ";

    let mut stmt = conn.prepare(sql)?;

    let dim_i64 = dim as i64;
    let rows = stmt.query_map(
        params![
            query_blob,
            package_id,
            model,
            dim_i64,
            query_blob,
            fetch_limit
        ],
        |row| {
            let id: String = row.get(0)?;
            let question: String = row.get(1)?;
            let dist: f64 = row.get(2)?;
            let sim = (1.0_f32 - dist as f32).clamp(0.0, 1.0);
            Ok((id, question, sim))
        },
    )?;

    let mut out = Vec::new();
    for r in rows {
        let (id, question, sim) = r?;
        if sim >= threshold {
            out.push(SimilarMemory {
                id,
                question,
                similarity: sim,
            });
        }
    }

    out.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(top_k);
    Ok(out)
}

/// Internal: pure-Rust implementation used when sqlite-vec functions are absent.
fn try_find_with_rust_cosine(
    conn: &Connection,
    package_id: &str,
    query: &[f32],
    model: &str,
    dim: usize,
    top_k: usize,
    threshold: f32,
) -> Result<Vec<SimilarMemory>> {
    let sql = "
        SELECT c.id, c.question, e.embedding
        FROM conversation_embeddings e
        JOIN conversations c ON c.id = e.conversation_id
        WHERE c.package_id = ?
          AND e.model = ?
          AND e.dim = ?
    ";

    let dim_i64 = dim as i64;
    let mut stmt = conn.prepare(sql)?;
    // We fetch the raw blob here and decode it inside the loop so a single
    // corrupt row can be logged and skipped without aborting the whole search.
    let rows = stmt.query_map(params![package_id, model, dim_i64], |row| {
        let id: String = row.get(0)?;
        let question: String = row.get(1)?;
        let bytes: Vec<u8> = row.get(2)?;
        Ok((id, question, bytes))
    })?;

    let mut scored: Vec<SimilarMemory> = Vec::new();
    for r in rows {
        let (id, question, bytes) = r?;
        let emb = match blob_to_embedding(&bytes) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warning: skipping conversation `{id}` in similarity search: {e:#}");
                continue;
            }
        };
        if emb.len() != dim {
            continue; // defensive: row stored under a different model dim
        }
        let sim = cosine_similarity(query, &emb);
        if sim >= threshold {
            scored.push(SimilarMemory {
                id,
                question,
                similarity: sim,
            });
        }
    }

    scored.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(top_k);
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    // ---------- cosine math (pure, no I/O) ----------

    #[test]
    fn cosine_identical_is_one() {
        let v = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_opposite_is_minus_one() {
        let a = vec![1.0, 2.0, 3.0];
        let b: Vec<f32> = a.iter().map(|x| -x).collect();
        let sim = cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 1e-5, "got {}", sim);
    }

    #[test]
    fn cosine_mismatched_dims_yields_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![1.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_zero_vector_yields_zero() {
        let a = vec![0.0, 0.0];
        let b = vec![1.0, 1.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_similarities_batch_matches_single() {
        let q = vec![1.0, 0.0, 0.0];
        // Third candidate is at 45° in the x-y plane → cosine exactly 1/sqrt(2) ≈ 0.707
        let cands = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.5, 0.5, 0.0],
        ];
        let batch = cosine_similarities(&q, &cands);
        assert_eq!(batch.len(), 3);
        assert!((batch[0] - 1.0).abs() < 1e-6);
        assert_eq!(batch[1], 0.0);
        assert!(batch[2] > 0.6 && batch[2] < 0.8, "got {}", batch[2]);
    }

    // ---------- blob round-trip (documents the storage format) ----------

    #[test]
    fn blob_roundtrip_is_exact() {
        let original = vec![0.0_f32, -1.5, std::f32::consts::PI, 1e-10, 999.999];
        let blob = embedding_to_blob(&original);
        assert_eq!(blob.len(), original.len() * 4);
        let restored = blob_to_embedding(&blob).expect("valid blob should decode");
        for (a, b) in original.iter().zip(restored.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn blob_to_embedding_rejects_bad_length() {
        let err = blob_to_embedding(&[0u8, 0, 0]).unwrap_err();
        assert!(err.to_string().contains("not a multiple of 4"));
    }

    #[test]
    fn find_similar_uses_registered_sqlite_vec_extension() {
        // Exercise the fast (sqlite-vec) path against the *real* migrated
        // schema. `db::open_memory` registers the extension; the resulting
        // connection's `vec_distance_cosine` is what `find_similar_questions`
        // will dispatch to. This complements the fallback-only test above.
        use crate::db;
        use crate::models::conversation::Conversation;
        use crate::models::package::{Package, SourceType};

        let conn = db::open_memory().unwrap();
        // Sanity: the extension's marker function must be callable here so we
        // know we are exercising the fast path and not the Rust fallback.
        let _: String = conn
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .expect("sqlite-vec functions should be available via db::open_memory");

        let pkg = Package::new(
            "vec-pkg".to_string(),
            "vec-pkg".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/vec-pkg".to_string(),
            false,
            None,
            false,
            "global".to_string(),
        );
        pkg.insert(&conn).unwrap();

        // Two real conversations + their embeddings via the actual schema.
        for (q, emb) in [("near", vec![0.99_f32, 0.1]), ("far", vec![0.0_f32, 1.0])] {
            let conv = Conversation::new(
                pkg.id.clone(),
                q.to_string(),
                "claude".to_string(),
                Some(0),
                format!("/tmp/{}.json", q),
                None,
                None,
            );
            conv.insert(&conn).unwrap();
            conn.execute(
                "INSERT INTO conversation_embeddings (conversation_id, model, dim, embedding, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![conv.id, "m", 2_i64, embedding_to_blob(&emb), "2026-01-01T00:00:00Z"],
            )
            .unwrap();
        }

        let results = find_similar_questions(&conn, &pkg.id, &[1.0_f32, 0.0], "m", 2, 5, 0.5);
        // Only the near vector should clear the 0.5 threshold.
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].question, "near");
        assert!(results[0].similarity > 0.95);
    }

    #[test]
    fn find_similar_skips_corrupt_blob_row() {
        // Seed one good and one corrupt row; the corrupt one must be skipped
        // rather than crashing the search.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE packages (id TEXT PRIMARY KEY);
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY,
                package_id TEXT NOT NULL,
                question TEXT NOT NULL
            );
            CREATE TABLE conversation_embeddings (
                conversation_id TEXT PRIMARY KEY,
                model TEXT NOT NULL,
                dim INTEGER NOT NULL,
                embedding BLOB NOT NULL,
                created_at TEXT
            );
            ",
        )
        .unwrap();
        conn.execute("INSERT INTO packages (id) VALUES ('pkg1')", [])
            .unwrap();
        // Good row.
        conn.execute(
            "INSERT INTO conversations (id, package_id, question) VALUES ('good', 'pkg1', 'q-good')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_embeddings (conversation_id, model, dim, embedding) \
             VALUES ('good', 'm', 2, ?)",
            params![embedding_to_blob(&[1.0_f32, 0.0])],
        )
        .unwrap();
        // Corrupt row: dim says 2 (8 bytes) but blob has 3 bytes.
        conn.execute(
            "INSERT INTO conversations (id, package_id, question) VALUES ('bad', 'pkg1', 'q-bad')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_embeddings (conversation_id, model, dim, embedding) \
             VALUES ('bad', 'm', 2, ?)",
            params![vec![0u8, 0, 0]],
        )
        .unwrap();

        let results = find_similar_questions(&conn, "pkg1", &[1.0_f32, 0.0], "m", 2, 5, 0.5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "good");
    }

    // ---------- error paths that do not require the network ----------

    #[tokio::test]
    async fn embed_missing_key_reports_actionable_message() {
        // Make sure the var is absent for the duration of the test.
        let _guard = RemoveEnvVar::new("OPENAI_API_KEY");

        let cfg = EmbeddingConfig {
            provider: EmbeddingProvider::Openai,
            model: "text-embedding-3-small".to_string(),
        };

        let err = embed_question(&cfg, "does rust have async?")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("`OPENAI_API_KEY`"));
        assert!(msg.contains("`openai`"));
        assert!(msg.contains("export OPENAI_API_KEY"));
    }

    #[tokio::test]
    async fn embed_empty_text_is_rejected() {
        let cfg = EmbeddingConfig {
            provider: EmbeddingProvider::Openrouter,
            model: "openai/text-embedding-3-small".to_string(),
        };
        let err = embed_question(&cfg, "   \n\t  ").await.unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn provider_from_str_roundtrips_and_errors_with_backticks() {
        assert_eq!(
            "openai".parse::<EmbeddingProvider>().unwrap(),
            EmbeddingProvider::Openai
        );
        assert_eq!(
            "openrouter".parse::<EmbeddingProvider>().unwrap(),
            EmbeddingProvider::Openrouter
        );

        let err = "foo".parse::<EmbeddingProvider>().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("`foo`"));
        assert!(msg.contains("`openai` or `openrouter`"));
    }

    // ---------- DB similarity (requires an in-memory schema + sqlite-vec registration) ----------

    #[test]
    fn find_similar_uses_rust_fallback_when_no_vec_extension() {
        // We deliberately do NOT register the sqlite-vec extension here.
        // The function must still return sensible results via the pure-Rust path.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // Minimal schema so the JOINs succeed (the real migrations create the tables).
        conn.execute_batch(
            "
            CREATE TABLE packages (id TEXT PRIMARY KEY);
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY,
                package_id TEXT NOT NULL,
                question TEXT NOT NULL
            );
            CREATE TABLE conversation_embeddings (
                conversation_id TEXT PRIMARY KEY,
                model TEXT NOT NULL,
                dim INTEGER NOT NULL,
                embedding BLOB NOT NULL,
                created_at TEXT
            );
            ",
        )
        .unwrap();

        // Seed one conversation + embedding (dim = 2)
        conn.execute("INSERT INTO packages (id) VALUES ('pkg1')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO conversations (id, package_id, question) VALUES ('conv-1', 'pkg1', 'How do I build?')",
            [],
        )
        .unwrap();
        let emb = vec![1.0_f32, 0.0];
        let blob = embedding_to_blob(&emb);
        conn.execute(
            "INSERT INTO conversation_embeddings (conversation_id, model, dim, embedding) \
             VALUES ('conv-1', 'text-embedding-3-small', 2, ?)",
            params![blob],
        )
        .unwrap();

        let query = vec![0.99_f32, 0.01]; // very close
        let results =
            find_similar_questions(&conn, "pkg1", &query, "text-embedding-3-small", 2, 5, 0.5);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "conv-1");
        assert!(results[0].similarity > 0.98);
        assert_eq!(results[0].question, "How do I build?");
    }

    // ---------- HTTP-mocked tests for `embed_question_against` ----------

    /// Spawn a minimal one-shot HTTP server that responds with `status_line`
    /// (e.g. `"200 OK"`) and `body` to a single request, then closes. Returns
    /// the base URL (without the trailing `/embeddings`) that the embeddings
    /// client should be pointed at.
    ///
    /// Implemented with `tokio::net::TcpListener` and raw `AsyncReadExt` /
    /// `AsyncWriteExt` so we don't pull in a mock-HTTP dependency. Adequate
    /// for the small surface we need to cover.
    async fn spawn_one_shot_http_server(status_line: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{}/v1", addr);

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                // Read request headers (up to 8KB is plenty for our payloads).
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await; // best-effort; we don't validate the body
                let response = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status_line,
                    body.len(),
                    body
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });

        base
    }

    /// Set `OPENAI_API_KEY` for the duration of the returned guard so a test
    /// can exercise the authenticated request path without depending on the
    /// host's real env.
    struct SetEnvVar {
        key: &'static str,
        prev: Option<String>,
    }

    impl SetEnvVar {
        fn new(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: test-only env mutation; restored on Drop.
            unsafe {
                std::env::set_var(key, value);
            }
            SetEnvVar { key, prev }
        }
    }

    impl Drop for SetEnvVar {
        fn drop(&mut self) {
            // SAFETY: matches the unsafe set in `new`.
            unsafe {
                if let Some(v) = &self.prev {
                    std::env::set_var(self.key, v);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[tokio::test]
    async fn embed_against_mock_200_returns_vector() {
        let _key = SetEnvVar::new("OPENAI_API_KEY", "sk-test");
        let body = r#"{"data":[{"embedding":[0.1,0.2,0.3,0.4]}]}"#;
        let base = spawn_one_shot_http_server("200 OK", body).await;

        let cfg = EmbeddingConfig {
            provider: EmbeddingProvider::Openai,
            model: "text-embedding-3-small".to_string(),
        };
        let v = embed_question_against(&cfg, "hello", &base).await.unwrap();
        assert_eq!(v.len(), 4);
        assert!((v[0] - 0.1).abs() < 1e-6);
        assert!((v[3] - 0.4).abs() < 1e-6);
    }

    #[tokio::test]
    async fn embed_against_mock_401_surfaces_auth_error() {
        let _key = SetEnvVar::new("OPENAI_API_KEY", "sk-test");
        let body = r#"{"error":{"message":"bad key"}}"#;
        let base = spawn_one_shot_http_server("401 Unauthorized", body).await;

        let cfg = EmbeddingConfig {
            provider: EmbeddingProvider::Openai,
            model: "text-embedding-3-small".to_string(),
        };
        let err = embed_question_against(&cfg, "q", &base).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("authentication failed"));
        assert!(msg.contains("`OPENAI_API_KEY`"));
        assert!(msg.contains("401"));
    }

    #[tokio::test]
    async fn embed_against_mock_429_surfaces_rate_limit() {
        let _key = SetEnvVar::new("OPENROUTER_API_KEY", "sk-or-test");
        let body = r#"{"error":{"message":"slow down"}}"#;
        let base = spawn_one_shot_http_server("429 Too Many Requests", body).await;

        let cfg = EmbeddingConfig {
            provider: EmbeddingProvider::Openrouter,
            model: "openai/text-embedding-3-small".to_string(),
        };
        let err = embed_question_against(&cfg, "q", &base).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("rate limit"));
        assert!(msg.contains("429"));
        assert!(msg.contains("`openrouter`"));
    }

    #[tokio::test]
    async fn embed_against_mock_500_surfaces_generic_error() {
        let _key = SetEnvVar::new("OPENAI_API_KEY", "sk-test");
        let body = "internal server error";
        let base = spawn_one_shot_http_server("500 Internal Server Error", body).await;

        let cfg = EmbeddingConfig {
            provider: EmbeddingProvider::Openai,
            model: "text-embedding-3-small".to_string(),
        };
        let err = embed_question_against(&cfg, "q", &base).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("500"));
        assert!(msg.contains("`openai`"));
    }

    #[tokio::test]
    async fn embed_against_mock_empty_data_array_errors() {
        let _key = SetEnvVar::new("OPENAI_API_KEY", "sk-test");
        let body = r#"{"data":[]}"#;
        let base = spawn_one_shot_http_server("200 OK", body).await;

        let cfg = EmbeddingConfig {
            provider: EmbeddingProvider::Openai,
            model: "text-embedding-3-small".to_string(),
        };
        let err = embed_question_against(&cfg, "q", &base).await.unwrap_err();
        assert!(err.to_string().contains("no `data[0].embedding` entry"));
    }

    // Helper to temporarily remove an env var and restore it afterwards.
    struct RemoveEnvVar {
        key: &'static str,
        prev: Option<String>,
    }

    impl RemoveEnvVar {
        fn new(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: test-only helper; we are single-threaded in the test and
            // immediately restore the previous value (or remove) in Drop.
            unsafe {
                std::env::remove_var(key);
            }
            RemoveEnvVar { key, prev }
        }
    }

    impl Drop for RemoveEnvVar {
        fn drop(&mut self) {
            // SAFETY: same justification as above — test helper restoring env.
            unsafe {
                if let Some(v) = &self.prev {
                    std::env::set_var(self.key, v);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }
}
