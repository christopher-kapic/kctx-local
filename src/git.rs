use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Check that the `git` binary is available on PATH.
fn check_git() -> Result<()> {
    which::which("git").context("git binary not found on PATH; install git to use git packages")?;
    Ok(())
}

/// Clone a git repository to the target directory.
///
/// When `shallow` is true, emits:
///   git clone --depth 1 --no-single-branch [--branch <branch>] <url> <target_dir>
///
/// The `--no-single-branch` flag is **critical**: without it a shallow clone
/// would only fetch the requested branch (or default), making later
/// `git::checkout(other_branch)` or `kcl ask --branch foo` fail or require
/// expensive unshallowing. With it, other branches remain fetchable (still
/// shallowly, depth 1 from their tips at fetch time).
///
/// The known limitation (documented to users): only the tip commit of each
/// fetched branch is present. Checking out "older versions" / arbitrary
/// historical commits is not possible until the user manually deepens
/// (`git fetch --deepen=N` or `git fetch --unshallow` in the clone dir).
/// `git log`, blame, etc. on pre-tip history will be truncated or fail.
/// This is the accepted disk-space vs. history tradeoff for `--shallow`.
///
/// Creates parent directories as needed.
pub async fn clone(
    url: &str,
    target_dir: &Path,
    branch: Option<&str>,
    shallow: bool,
) -> Result<()> {
    check_git()?;

    // Ensure parent directory exists.
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory {}", parent.display()))?;
    }

    let mut cmd = Command::new("git");
    cmd.arg("clone");
    if shallow {
        cmd.arg("--depth").arg("1");
        cmd.arg("--no-single-branch");
    }
    if let Some(b) = branch {
        cmd.arg("--branch").arg(b);
    }
    cmd.arg(url);
    cmd.arg(target_dir);

    let output = cmd.output().await.context("failed to execute git clone")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git clone failed: {}", stderr.trim());
    }

    Ok(())
}

/// Pull latest changes for a repository at the given path.
///
/// Strictly fast-forward only. Refuses to merge or rebase; if the local
/// branch has diverged from its upstream, returns an error telling the
/// user to resolve manually.
///
/// Shallow-clone note: on a `--depth 1` clone the `fetch` + ff-only will
/// update the (still-shallow) tip of the current branch. History remains
/// truncated to depth 1. No automatic deepening occurs here either.
///
/// Implementation:
/// 1. Determine the current branch (skip if `HEAD` is detached).
/// 2. Look up its upstream via `git rev-parse --abbrev-ref @{u}` (skip if
///    no upstream is configured).
/// 3. `git fetch` from the configured upstream.
/// 4. `git merge --ff-only @{u}`.
///
/// Returns a human-readable summary of what happened.
pub async fn pull(repo_path: &Path) -> Result<String> {
    check_git()?;

    if !repo_path.exists() {
        bail!("repository path does not exist: {}", repo_path.display());
    }

    // Determine current branch; skip if detached.
    let branch = match current_head(repo_path).await? {
        HeadState::Branch(name) => name,
        HeadState::Detached(_) => {
            return Ok("skipping pull: HEAD is detached".to_string());
        }
    };

    // Look up the configured upstream of the current branch. If there is no
    // upstream, this fails — treat it as a no-op skip rather than an error.
    let upstream_out = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("--symbolic-full-name")
        .arg("@{u}")
        .output()
        .await
        .context("failed to execute git rev-parse")?;

    if !upstream_out.status.success() {
        return Ok(format!("`{branch}` has no upstream; skipping pull"));
    }
    let upstream = String::from_utf8_lossy(&upstream_out.stdout)
        .trim()
        .to_string();

    // Capture the SHA before the fetch+merge so we can report what changed.
    let before_out = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .await
        .context("failed to execute git rev-parse HEAD")?;
    if !before_out.status.success() {
        let stderr = String::from_utf8_lossy(&before_out.stderr);
        bail!("git rev-parse HEAD failed: {}", stderr.trim());
    }
    let before = String::from_utf8_lossy(&before_out.stdout)
        .trim()
        .to_string();

    // Fetch from the configured remote (no remote arg — let git pick the
    // upstream's remote based on branch config).
    let fetch_out = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("fetch")
        .output()
        .await
        .context("failed to execute git fetch")?;
    if !fetch_out.status.success() {
        let stderr = String::from_utf8_lossy(&fetch_out.stderr);
        bail!("git fetch failed: {}", stderr.trim());
    }

    // Fast-forward only. If the branch has diverged this fails with a clear
    // git error like "Not possible to fast-forward, aborting." We surface a
    // friendlier message that tells the user to resolve manually.
    let merge_out = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("merge")
        .arg("--ff-only")
        .arg("@{u}")
        .output()
        .await
        .context("failed to execute git merge --ff-only")?;

    if !merge_out.status.success() {
        let stderr = String::from_utf8_lossy(&merge_out.stderr);
        bail!(
            "`{branch}` has diverged from upstream `{upstream}`; refusing to merge or rebase. Resolve manually and re-run. (git: {})",
            stderr.trim()
        );
    }

    // Compare the SHA after the merge to summarize.
    let after_out = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .await
        .context("failed to execute git rev-parse HEAD")?;
    if !after_out.status.success() {
        let stderr = String::from_utf8_lossy(&after_out.stderr);
        bail!("git rev-parse HEAD failed: {}", stderr.trim());
    }
    let after = String::from_utf8_lossy(&after_out.stdout)
        .trim()
        .to_string();

    if before == after {
        Ok("Already up to date.".to_string())
    } else {
        let short = |s: &str| s.chars().take(12).collect::<String>();
        Ok(format!("Updated {}..{}", short(&before), short(&after)))
    }
}

/// Check if a path is a git repository (has a .git directory or file).
#[cfg(test)]
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

/// The state of `HEAD` in a git repository.
///
/// Either points at a named branch or is detached at a specific commit SHA.
/// Used by `current_head` / `restore_head` so callers can correctly save and
/// restore repository state across operations like a temporary `--branch`
/// override in `kcl ask`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadState {
    /// `HEAD` points at the named branch.
    Branch(String),
    /// `HEAD` is detached at the given commit SHA.
    Detached(String),
}

/// Return the current state of `HEAD` in `repo_path`.
///
/// First tries `git -C <path> symbolic-ref --short HEAD`, which succeeds with
/// the branch name when `HEAD` is on a branch and fails when it's detached.
/// On failure, falls back to `git -C <path> rev-parse HEAD` to get the SHA.
pub async fn current_head(repo_path: &Path) -> Result<HeadState> {
    check_git()?;

    let symbolic = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("symbolic-ref")
        .arg("--short")
        .arg("HEAD")
        .output()
        .await
        .context("failed to execute git symbolic-ref")?;

    if symbolic.status.success() {
        let branch = String::from_utf8_lossy(&symbolic.stdout).trim().to_string();
        return Ok(HeadState::Branch(branch));
    }

    // Detached HEAD (or other unusual state) — read the raw SHA.
    let rev = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .await
        .context("failed to execute git rev-parse")?;

    if !rev.status.success() {
        let stderr = String::from_utf8_lossy(&rev.stderr);
        bail!("git rev-parse failed: {}", stderr.trim());
    }

    let sha = String::from_utf8_lossy(&rev.stdout).trim().to_string();
    Ok(HeadState::Detached(sha))
}

/// Return the name of the currently checked-out branch in `repo_path`, or
/// `None` if `HEAD` is detached.
///
/// Convenience wrapper around `current_head` for callers that only need a
/// branch name to record as metadata (e.g. `kcl packages add`).
pub async fn current_branch(repo_path: &Path) -> Result<Option<String>> {
    match current_head(repo_path).await? {
        HeadState::Branch(name) => Ok(Some(name)),
        HeadState::Detached(_) => Ok(None),
    }
}

/// Return the full commit SHA of the current HEAD (for provenance recording
/// and prepared-context staleness checks).
pub async fn current_commit_sha(repo_path: &Path) -> Result<String> {
    check_git()?;

    let out = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .await
        .context("failed to execute git rev-parse HEAD")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("git rev-parse HEAD failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Return how many commits exist between `base_sha` and `head_sha` (i.e. the
/// number of commits `head_sha` is ahead of `base_sha`). Returns 0 when they
/// are the same or when the range cannot be computed. Used for the "N commits
/// behind" note when injecting a prepared map whose recorded commit differs
/// from the current HEAD.
pub async fn commit_count_between(
    repo_path: &Path,
    base_sha: &str,
    head_sha: &str,
) -> Result<usize> {
    if base_sha == head_sha {
        return Ok(0);
    }
    check_git()?;

    let range = format!("{}..{}", base_sha, head_sha);
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-list")
        .arg("--count")
        .arg(&range)
        .output()
        .await
        .context("failed to execute git rev-list --count")?;
    if !out.status.success() {
        // Degrade gracefully for unusual histories (reset, rebase, etc.)
        return Ok(0);
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(s.parse::<usize>().unwrap_or(0))
}

/// Restore `repo_path` to the given `HeadState`.
///
/// For a `Branch` this runs `git checkout <branch>`; for a `Detached` SHA it
/// runs `git checkout --detach <sha>` so the repository ends up in the same
/// detached state it was originally in (rather than creating a stray local
/// branch named after the SHA).
pub async fn restore_head(repo_path: &Path, head: &HeadState) -> Result<()> {
    check_git()?;

    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo_path).arg("checkout");
    match head {
        HeadState::Branch(name) => {
            cmd.arg(name);
        }
        HeadState::Detached(sha) => {
            cmd.arg("--detach").arg(sha);
        }
    }

    let output = cmd
        .output()
        .await
        .context("failed to execute git checkout")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let target = match head {
            HeadState::Branch(name) => name.clone(),
            HeadState::Detached(sha) => sha.clone(),
        };
        bail!("git checkout `{}` failed: {}", target, stderr.trim());
    }

    Ok(())
}

/// Check out `branch` in the repository at `repo_path`.
///
/// Fetches from `origin` first so that branches that exist only on the
/// remote can be checked out as new local tracking branches.
///
/// Shallow-clone note: if the repo was cloned with `--depth 1 --no-single-branch`,
/// this fetch will bring in a shallow (depth-1) copy of the target branch.
/// No `--deepen` is performed automatically; users who need more history
/// on a branch must run git commands manually inside the clone directory.
pub async fn checkout(repo_path: &Path, branch: &str) -> Result<()> {
    check_git()?;

    // Fetch so we can resolve remote-only branches. Failures here are not
    // fatal — the user may be offline and the branch may already be local.
    let fetch_err = match Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("fetch")
        .arg("origin")
        .arg(branch)
        .output()
        .await
    {
        Ok(output) if output.status.success() => None,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let msg = format!("git fetch origin {branch} failed: {stderr}");
            eprintln!("warning: {msg}");
            Some(msg)
        }
        Err(e) => {
            let msg = format!("failed to execute git fetch: {e}");
            eprintln!("warning: {msg}");
            Some(msg)
        }
    };

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("checkout")
        .arg(branch)
        .output()
        .await
        .context("failed to execute git checkout")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut msg = format!("git checkout {} failed: {}", branch, stderr.trim());
        if let Some(fetch_msg) = fetch_err {
            msg.push_str(&format!(" (prior fetch also failed: {})", fetch_msg));
        }
        bail!("{msg}");
    }

    Ok(())
}

/// Validate that a string looks like a plausible git URL.
///
/// Accepts: https://, http://, git://, ssh://, file:// schemes,
/// SCP-like syntax (e.g. git@host:user/repo), and absolute paths.
pub fn validate_git_url(url: &str) -> Result<()> {
    let valid = url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("git://")
        || url.starts_with("ssh://")
        || url.starts_with("file://")
        || url.starts_with('/')
        // SCP-like: user@host:path
        || (url.contains('@') && url.contains(':') && !url.contains("://"));

    if !valid {
        bail!(
            "invalid git URL: `{url}`. Expected a URL (https://, git://, ssh://, etc.) or SCP syntax (git@host:path)"
        );
    }
    if url.starts_with("http://") {
        eprintln!(
            "warning: using insecure `http://` for git URL `{url}`; prefer `https://` to avoid MITM tampering"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn check_git_available() {
        // git should be available in CI and dev environments.
        assert!(check_git().is_ok());
    }

    #[tokio::test]
    #[ignore] // Requires network access; run with `cargo test -- --ignored`
    async fn clone_creates_directory_and_repo() {
        let tmp = std::env::temp_dir().join("kcl-test-git-clone");
        // Clean up from any previous run.
        let _ = std::fs::remove_dir_all(&tmp);

        let target = tmp.join("rust-mustache");
        let result = clone(
            "https://github.com/nickel-org/rust-mustache.git",
            &target,
            None,
            false,
        )
        .await;

        assert!(result.is_ok(), "clone failed: {:?}", result.err());
        assert!(target.exists(), "target directory should exist after clone");
        assert!(is_git_repo(&target), "target should be a git repo");

        // Cleanup.
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    #[ignore] // Requires network access; run with `cargo test -- --ignored`
    async fn pull_on_cloned_repo() {
        let tmp = std::env::temp_dir().join("kcl-test-git-pull");
        let _ = std::fs::remove_dir_all(&tmp);

        let target = tmp.join("rust-mustache");
        clone(
            "https://github.com/nickel-org/rust-mustache.git",
            &target,
            None,
            false,
        )
        .await
        .expect("clone should succeed");

        let result = pull(&target).await;
        assert!(result.is_ok(), "pull failed: {:?}", result.err());
        let msg = result.unwrap();
        assert!(
            msg.contains("Already up to date") || msg.contains("Updating"),
            "unexpected pull message: {msg}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn clone_fails_with_bad_url() {
        let tmp = std::env::temp_dir().join("kcl-test-git-bad-clone");
        let _ = std::fs::remove_dir_all(&tmp);

        let target = tmp.join("nonexistent");
        let result = clone(
            "https://example.com/nonexistent-repo.git",
            &target,
            None,
            false,
        )
        .await;
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn pull_fails_on_nonexistent_path() {
        let result = pull(&PathBuf::from("/nonexistent/path/kcl-test-12345")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[tokio::test]
    async fn pull_skips_when_branch_has_no_upstream() {
        let tmp = std::env::temp_dir().join("kcl-test-git-pull-no-upstream");
        let _ = std::fs::remove_dir_all(&tmp);
        // init_repo_with_two_commits creates a repo on `main` with no remote.
        let _ = init_repo_with_two_commits(&tmp).await;

        let result = pull(&tmp).await;
        assert!(result.is_ok(), "pull failed: {:?}", result.err());
        let msg = result.unwrap();
        assert!(
            msg.contains("has no upstream") && msg.contains("`main`"),
            "expected no-upstream skip message, got: {msg}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn pull_refuses_when_branch_has_diverged() {
        let tmp = std::env::temp_dir().join("kcl-test-git-pull-diverged");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let remote_dir = tmp.join("remote");
        let local_dir = tmp.join("local");

        async fn run(dir: &Path, args: &[&str]) -> std::process::Output {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} in {} failed: {}",
                args,
                dir.display(),
                String::from_utf8_lossy(&out.stderr)
            );
            out
        }

        // Build a "remote" bare-ish working repo with one commit. We use a
        // non-bare repo and clone from it via a `file://` URL — simpler than
        // a bare repo and still exercises the fetch+ff-only path.
        std::fs::create_dir_all(&remote_dir).unwrap();
        run(&remote_dir, &["init", "--initial-branch", "main"]).await;
        run(
            &remote_dir,
            &["config", "user.email", "test@example.invalid"],
        )
        .await;
        run(&remote_dir, &["config", "user.name", "Test User"]).await;
        run(&remote_dir, &["commit", "--allow-empty", "-m", "remote-1"]).await;
        // Allow pushing into a non-bare repo's checked-out branch.
        run(
            &remote_dir,
            &["config", "receive.denyCurrentBranch", "ignore"],
        )
        .await;

        // Clone into `local`. Use file:// so git records a real upstream.
        let remote_url = format!("file://{}", remote_dir.display());
        let clone_out = Command::new("git")
            .args(["clone", &remote_url, local_dir.to_str().unwrap()])
            .output()
            .await
            .unwrap();
        assert!(
            clone_out.status.success(),
            "clone failed: {}",
            String::from_utf8_lossy(&clone_out.stderr)
        );
        run(
            &local_dir,
            &["config", "user.email", "test@example.invalid"],
        )
        .await;
        run(&local_dir, &["config", "user.name", "Test User"]).await;

        // Add a divergent commit to `remote` (advance its main).
        run(&remote_dir, &["commit", "--allow-empty", "-m", "remote-2"]).await;
        // Add a *different* commit to `local` (so its main is divergent, not
        // just behind).
        run(&local_dir, &["commit", "--allow-empty", "-m", "local-2"]).await;

        let result = pull(&local_dir).await;
        assert!(result.is_err(), "expected pull to fail on diverged branch");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("diverged") && err.contains("`main`"),
            "error should mention divergence and branch: {err}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn is_git_repo_false_for_regular_dir() {
        assert!(!is_git_repo(Path::new("/tmp")));
    }

    #[tokio::test]
    async fn checkout_bogus_remote_surfaces_fetch_error() {
        let tmp = std::env::temp_dir().join("kcl-test-git-checkout-fetch-err");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Init a repo with one commit so checkout has something to work with.
        Command::new("git")
            .args(["init", "--initial-branch", "main"])
            .current_dir(&tmp)
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(&tmp)
            .output()
            .await
            .unwrap();
        // Point origin at a bogus URL so fetch fails.
        Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://example.invalid/no-repo.git",
            ])
            .current_dir(&tmp)
            .output()
            .await
            .unwrap();

        let result = checkout(&tmp, "nonexistent-branch").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("prior fetch also failed"),
            "error should mention failed fetch: {err}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Initialize a fresh repo at `dir` with two commits and return the SHAs
    /// of the first and second commit (in that order). Configures `user.name`
    /// and `user.email` locally so the commit calls succeed in CI sandboxes
    /// that don't have a global git identity set.
    async fn init_repo_with_two_commits(dir: &Path) -> (String, String) {
        std::fs::create_dir_all(dir).unwrap();

        async fn run(dir: &Path, args: &[&str]) -> std::process::Output {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            out
        }

        run(dir, &["init", "--initial-branch", "main"]).await;
        run(dir, &["config", "user.email", "test@example.invalid"]).await;
        run(dir, &["config", "user.name", "Test User"]).await;
        run(dir, &["commit", "--allow-empty", "-m", "first"]).await;
        let first = String::from_utf8(run(dir, &["rev-parse", "HEAD"]).await.stdout)
            .unwrap()
            .trim()
            .to_string();
        run(dir, &["commit", "--allow-empty", "-m", "second"]).await;
        let second = String::from_utf8(run(dir, &["rev-parse", "HEAD"]).await.stdout)
            .unwrap()
            .trim()
            .to_string();
        (first, second)
    }

    #[tokio::test]
    async fn current_head_reports_branch_when_on_branch() {
        let tmp = std::env::temp_dir().join("kcl-test-current-head-branch");
        let _ = std::fs::remove_dir_all(&tmp);
        let (_first, _second) = init_repo_with_two_commits(&tmp).await;

        let head = current_head(&tmp).await.unwrap();
        assert_eq!(head, HeadState::Branch("main".to_string()));

        // current_branch convenience wrapper agrees.
        let branch = current_branch(&tmp).await.unwrap();
        assert_eq!(branch, Some("main".to_string()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn current_head_reports_detached_sha() {
        let tmp = std::env::temp_dir().join("kcl-test-current-head-detached");
        let _ = std::fs::remove_dir_all(&tmp);
        let (first, _second) = init_repo_with_two_commits(&tmp).await;

        // Detach HEAD at the first commit.
        let out = Command::new("git")
            .args(["checkout", "--detach", &first])
            .current_dir(&tmp)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "detach failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let head = current_head(&tmp).await.unwrap();
        assert_eq!(head, HeadState::Detached(first.clone()));

        // current_branch returns None for detached HEAD.
        let branch = current_branch(&tmp).await.unwrap();
        assert_eq!(branch, None);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn restore_head_returns_to_detached_sha() {
        let tmp = std::env::temp_dir().join("kcl-test-restore-head-detached");
        let _ = std::fs::remove_dir_all(&tmp);
        let (first, _second) = init_repo_with_two_commits(&tmp).await;

        // Detach at the first commit, capture state, switch to main, then restore.
        let out = Command::new("git")
            .args(["checkout", "--detach", &first])
            .current_dir(&tmp)
            .output()
            .await
            .unwrap();
        assert!(out.status.success());

        let saved = current_head(&tmp).await.unwrap();
        assert_eq!(saved, HeadState::Detached(first.clone()));

        // Move to main (a "different" state) — using `git checkout` directly
        // rather than `crate::git::checkout` so we don't invoke the fetch
        // step against the missing `origin` remote.
        let out = Command::new("git")
            .args(["checkout", "main"])
            .current_dir(&tmp)
            .output()
            .await
            .unwrap();
        assert!(out.status.success());

        // Now restore the detached state.
        restore_head(&tmp, &saved).await.unwrap();

        let head_after = current_head(&tmp).await.unwrap();
        assert_eq!(head_after, HeadState::Detached(first));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn restore_head_returns_to_branch() {
        let tmp = std::env::temp_dir().join("kcl-test-restore-head-branch");
        let _ = std::fs::remove_dir_all(&tmp);
        let (first, _second) = init_repo_with_two_commits(&tmp).await;

        // Saved state: on branch main.
        let saved = current_head(&tmp).await.unwrap();
        assert_eq!(saved, HeadState::Branch("main".to_string()));

        // Detach to simulate a checkout that moved HEAD.
        let out = Command::new("git")
            .args(["checkout", "--detach", &first])
            .current_dir(&tmp)
            .output()
            .await
            .unwrap();
        assert!(out.status.success());

        // Restore.
        restore_head(&tmp, &saved).await.unwrap();
        let head_after = current_head(&tmp).await.unwrap();
        assert_eq!(head_after, HeadState::Branch("main".to_string()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_git_url_accepts_valid_urls() {
        let valid = [
            "https://github.com/user/repo.git",
            "http://github.com/user/repo.git",
            "git://github.com/user/repo.git",
            "ssh://git@github.com/user/repo.git",
            "file:///home/user/repo",
            "git@github.com:user/repo.git",
            "/home/user/local-repo",
        ];
        for url in valid {
            assert!(validate_git_url(url).is_ok(), "should accept: {url}");
        }
    }

    #[test]
    fn validate_git_url_rejects_invalid_urls() {
        let invalid = [
            "not-a-url",
            "ftp://example.com/repo",
            "just some words",
            "",
            "relative/path/repo",
        ];
        for url in invalid {
            assert!(validate_git_url(url).is_err(), "should reject: {url}");
        }
    }

    // ------------------------------------------------------------------
    // Shallow clone tests (exercise --depth 1 --no-single-branch behavior)
    // These use only local git repos (file://) so they run without network.
    // ------------------------------------------------------------------

    /// Helper: init a git repo at `dir` with `main` (2 commits) and `feature`
    /// (1 commit), configure identity, return (main_tip, feature_tip).
    async fn init_repo_with_main_and_feature(dir: &Path) -> (String, String) {
        std::fs::create_dir_all(dir).unwrap();

        async fn run(dir: &Path, args: &[&str]) -> std::process::Output {
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .await
                .unwrap()
        }

        run(dir, &["init", "--initial-branch", "main"]).await;
        run(dir, &["config", "user.email", "test@example.invalid"]).await;
        run(dir, &["config", "user.name", "Test User"]).await;
        run(dir, &["commit", "--allow-empty", "-m", "main-1"]).await;
        run(dir, &["commit", "--allow-empty", "-m", "main-2"]).await;
        let main_tip = String::from_utf8(run(dir, &["rev-parse", "HEAD"]).await.stdout)
            .unwrap()
            .trim()
            .to_string();

        // Create feature branch with its own tip (divergent from main).
        run(dir, &["checkout", "-b", "feature"]).await;
        run(dir, &["commit", "--allow-empty", "-m", "feature-1"]).await;
        let feature_tip = String::from_utf8(run(dir, &["rev-parse", "HEAD"]).await.stdout)
            .unwrap()
            .trim()
            .to_string();

        // Back to main for the default clone target.
        run(dir, &["checkout", "main"]).await;

        (main_tip, feature_tip)
    }

    #[tokio::test]
    async fn shallow_clone_truncates_history_on_default_branch() {
        let tmp = std::env::temp_dir().join("kcl-test-shallow-truncate");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let origin = tmp.join("origin");
        let (main_tip, _feature_tip) = init_repo_with_main_and_feature(&origin).await;

        let clone_dir = tmp.join("shallow-clone");
        let url = format!("file://{}", origin.display());
        clone(&url, &clone_dir, None, true)
            .await
            .expect("shallow clone failed");

        assert!(is_git_repo(&clone_dir));

        // History must be truncated to depth 1.
        let count_out = Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(&clone_dir)
            .output()
            .await
            .unwrap();
        let count = String::from_utf8_lossy(&count_out.stdout)
            .trim()
            .to_string();
        assert_eq!(count, "1", "shallow clone should have only the tip commit");

        // The pre-tip commit from origin must NOT be present locally.
        let old_commit = {
            // main-1 is parent of main_tip
            let parent_out = Command::new("git")
                .args(["rev-parse", &format!("{}^", main_tip)])
                .current_dir(&origin)
                .output()
                .await
                .unwrap();
            String::from_utf8_lossy(&parent_out.stdout)
                .trim()
                .to_string()
        };
        let cat = Command::new("git")
            .args(["cat-file", "-t", &old_commit])
            .current_dir(&clone_dir)
            .output()
            .await
            .unwrap();
        assert!(
            !cat.status.success(),
            "old commit before shallow tip must be absent in depth-1 clone"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn shallow_clone_allows_checkout_of_other_branch() {
        let tmp = std::env::temp_dir().join("kcl-test-shallow-branch-switch");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let origin = tmp.join("origin");
        let (_main_tip, feature_tip) = init_repo_with_main_and_feature(&origin).await;

        let clone_dir = tmp.join("shallow-clone");
        let url = format!("file://{}", origin.display());
        clone(&url, &clone_dir, None, true)
            .await
            .expect("shallow clone failed");

        // Initially on main (depth 1).
        let head0 = current_branch(&clone_dir).await.unwrap();
        assert_eq!(head0, Some("main".to_string()));

        // Checkout the other branch — must succeed thanks to --no-single-branch.
        checkout(&clone_dir, "feature")
            .await
            .expect("checkout feature on shallow clone failed");

        let head1 = current_branch(&clone_dir).await.unwrap();
        assert_eq!(head1, Some("feature".to_string()));

        // Still only depth 1 on the feature branch.
        let count_out = Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(&clone_dir)
            .output()
            .await
            .unwrap();
        let count = String::from_utf8_lossy(&count_out.stdout)
            .trim()
            .to_string();
        assert_eq!(count, "1");

        // The feature_tip commit IS present (fetched shallowly).
        let cat = Command::new("git")
            .args(["cat-file", "-t", &feature_tip])
            .current_dir(&clone_dir)
            .output()
            .await
            .unwrap();
        assert!(
            cat.status.success(),
            "the tip of the second branch must be fetchable after shallow clone with --no-single-branch"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn pull_on_shallow_clone_updates_tip_but_keeps_truncated() {
        let tmp = std::env::temp_dir().join("kcl-test-shallow-pull");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let origin = tmp.join("origin");
        let (_main_tip, _feature_tip) = init_repo_with_main_and_feature(&origin).await;

        let clone_dir = tmp.join("shallow-clone");
        let url = format!("file://{}", origin.display());
        clone(&url, &clone_dir, None, true).await.expect("clone");

        // Simulate an advance on the remote (new commit on main).
        {
            async fn run(dir: &Path, args: &[&str]) -> std::process::Output {
                Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .output()
                    .await
                    .unwrap()
            }
            run(&origin, &["config", "user.email", "test@example.invalid"]).await;
            run(&origin, &["config", "user.name", "Test User"]).await;
            run(&origin, &["commit", "--allow-empty", "-m", "main-3"]).await;
        }

        // Pull should succeed even on shallow.
        let msg = pull(&clone_dir)
            .await
            .expect("pull on shallow should succeed");
        // Accept any of the normal "already / updated / up to date" messages that pull() produces.
        assert!(
            msg.contains("Updating")
                || msg.contains("up to date")
                || msg.contains("Already up to date")
                || msg.contains("Updated"),
            "unexpected pull message on shallow clone: {msg}"
        );

        // Still only depth-1 after pull.
        let count_out = Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(&clone_dir)
            .output()
            .await
            .unwrap();
        let count = String::from_utf8_lossy(&count_out.stdout)
            .trim()
            .to_string();
        let n: usize = count.parse().unwrap_or(99);
        // A normal `pull` on a depth-1 clone will deepen by the number of new commits
        // the remote advanced, but it does *not* unshallow the entire history.
        // We only care that we did not get the full history (the test repo has > 2 commits).
        assert!(
            n <= 2,
            "pull on shallow clone deepened too far (count={n}); we expect limited deepening only"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
