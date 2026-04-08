use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

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
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("does not exist"));
    }

    #[test]
    fn is_git_repo_false_for_regular_dir() {
        assert!(!is_git_repo(Path::new("/tmp")));
    }
}
