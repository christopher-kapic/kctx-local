use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Check that the `git` binary is available on PATH.
fn check_git() -> Result<()> {
    which::which("git").context("git binary not found on PATH; install git to use git packages")?;
    Ok(())
}

/// Clone a git repository to the target directory.
///
/// Shells out to `git clone <url> [--branch <branch>] <target_dir>`.
/// Creates parent directories as needed.
pub fn clone(url: &str, target_dir: &Path, branch: Option<&str>) -> Result<()> {
    check_git()?;

    // Ensure parent directory exists.
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory {}", parent.display()))?;
    }

    let mut cmd = Command::new("git");
    cmd.arg("clone");
    if let Some(b) = branch {
        cmd.arg("--branch").arg(b);
    }
    cmd.arg(url);
    cmd.arg(target_dir);

    let output = cmd.output().context("failed to execute git clone")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git clone failed: {}", stderr.trim());
    }

    Ok(())
}

/// Pull latest changes for a repository at the given path.
///
/// Shells out to `git -C <path> pull`.
pub fn pull(repo_path: &Path) -> Result<String> {
    check_git()?;

    if !repo_path.exists() {
        bail!("repository path does not exist: {}", repo_path.display());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("pull")
        .output()
        .context("failed to execute git pull")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        bail!("git pull failed: {}", stderr.trim());
    }

    // Return stdout (e.g. "Already up to date." or merge summary).
    Ok(stdout.trim().to_string())
}

/// Check if a path is a git repository (has a .git directory or file).
#[allow(dead_code)]
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

/// Return the name of the currently checked-out branch in `repo_path`.
///
/// Shells out to `git -C <path> rev-parse --abbrev-ref HEAD`. If HEAD is
/// detached this returns the literal string "HEAD" — callers that need to
/// restore state should treat that as a special case.
pub fn current_branch(repo_path: &Path) -> Result<String> {
    check_git()?;

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .output()
        .context("failed to execute git rev-parse")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git rev-parse failed: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Check out `branch` in the repository at `repo_path`.
///
/// Fetches from `origin` first so that branches that exist only on the
/// remote can be checked out as new local tracking branches.
pub fn checkout(repo_path: &Path, branch: &str) -> Result<()> {
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
        bail!("invalid git URL: `{url}`. Expected a URL (https://, git://, ssh://, etc.) or SCP syntax (git@host:path)");
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

    #[test]
    #[ignore] // Requires network access; run with `cargo test -- --ignored`
    fn clone_creates_directory_and_repo() {
        let tmp = std::env::temp_dir().join("kcl-test-git-clone");
        // Clean up from any previous run.
        let _ = std::fs::remove_dir_all(&tmp);

        let target = tmp.join("rust-mustache");
        let result = clone(
            "https://github.com/nickel-org/rust-mustache.git",
            &target,
            None,
        );

        assert!(result.is_ok(), "clone failed: {:?}", result.err());
        assert!(target.exists(), "target directory should exist after clone");
        assert!(is_git_repo(&target), "target should be a git repo");

        // Cleanup.
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    #[ignore] // Requires network access; run with `cargo test -- --ignored`
    fn pull_on_cloned_repo() {
        let tmp = std::env::temp_dir().join("kcl-test-git-pull");
        let _ = std::fs::remove_dir_all(&tmp);

        let target = tmp.join("rust-mustache");
        clone(
            "https://github.com/nickel-org/rust-mustache.git",
            &target,
            None,
        )
        .expect("clone should succeed");

        let result = pull(&target);
        assert!(result.is_ok(), "pull failed: {:?}", result.err());
        let msg = result.unwrap();
        assert!(
            msg.contains("Already up to date") || msg.contains("Updating"),
            "unexpected pull message: {msg}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn clone_fails_with_bad_url() {
        let tmp = std::env::temp_dir().join("kcl-test-git-bad-clone");
        let _ = std::fs::remove_dir_all(&tmp);

        let target = tmp.join("nonexistent");
        let result = clone("https://example.com/nonexistent-repo.git", &target, None);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pull_fails_on_nonexistent_path() {
        let result = pull(&PathBuf::from("/nonexistent/path/kcl-test-12345"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn is_git_repo_false_for_regular_dir() {
        assert!(!is_git_repo(Path::new("/tmp")));
    }

    #[test]
    fn checkout_bogus_remote_surfaces_fetch_error() {
        let tmp = std::env::temp_dir().join("kcl-test-git-checkout-fetch-err");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Init a repo with one commit so checkout has something to work with.
        Command::new("git")
            .args(["init", "--initial-branch", "main"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(&tmp)
            .output()
            .unwrap();
        // Point origin at a bogus URL so fetch fails.
        Command::new("git")
            .args(["remote", "add", "origin", "https://example.invalid/no-repo.git"])
            .current_dir(&tmp)
            .output()
            .unwrap();

        let result = checkout(&tmp, "nonexistent-branch");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("prior fetch also failed"),
            "error should mention failed fetch: {err}"
        );

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
}
