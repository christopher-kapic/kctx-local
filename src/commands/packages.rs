use std::path::Path;

use anyhow::{Result, bail};

use crate::cli::PackagesCommand;
use crate::config::Config;
use crate::db;
use crate::dirs;
use crate::git;
use crate::models::package::{Package, SourceType};

/// Helper to open the database from the default location.
fn open_db() -> Result<rusqlite::Connection> {
    let db_path = dirs::db_file()?;
    db::open(&db_path)
}

pub fn run(command: &PackagesCommand) -> Result<()> {
    match command {
        PackagesCommand::List { verbose, json } => cmd_list(*verbose, *json),
        PackagesCommand::Add {
            identifier,
            path,
            git,
            branch,
        } => cmd_add(
            identifier,
            path.as_deref(),
            git.as_deref(),
            branch.as_deref(),
        ),
        PackagesCommand::Remove { identifier } => cmd_remove(identifier),
        PackagesCommand::Show { identifier, json } => cmd_show(identifier, *json),
        PackagesCommand::Pull { identifier, all } => cmd_pull(identifier.as_deref(), *all),
        PackagesCommand::Set {
            identifier,
            key,
            value,
            unset,
        } => cmd_set(identifier, key, value.as_deref(), *unset),
    }
}

fn cmd_list(verbose: bool, json: bool) -> Result<()> {
    let conn = open_db()?;
    let packages = Package::list_all(&conn)?;

    if json {
        let out = serde_json::to_string_pretty(&packages)?;
        println!("{out}");
    } else if verbose {
        for pkg in &packages {
            println!(
                "{}\t{}\t{}",
                pkg.identifier,
                pkg.source_type.as_str(),
                pkg.path,
            );
        }
    } else {
        for pkg in &packages {
            println!("{}", pkg.identifier);
        }
    }

    Ok(())
}

fn cmd_add(
    identifier: &str,
    path: Option<&str>,
    git: Option<&str>,
    branch: Option<&str>,
) -> Result<()> {
    // Validate: must supply --path or --git (or both for tracking existing clone with remote).
    if path.is_none() && git.is_none() {
        bail!(
            "Must specify --path or --git (or both). Examples:\n  kcl packages add {identifier} --path /path/to/codebase\n  kcl packages add {identifier} --git https://github.com/user/repo.git"
        );
    }

    let conn = open_db()?;

    // Check for duplicate identifier.
    if Package::get_by_identifier(&conn, identifier)?.is_some() {
        bail!(
            "Package '{identifier}' already exists. Use `kcl packages show {identifier}` to view it or choose a different identifier."
        );
    }

    let (source_type, source_url, source_branch, resolved_path, auto_pull) = if let Some(git_url) =
        git
    {
        // Git package — may or may not have an explicit --path.
        if let Some(p) = path {
            // Existing clone with remote tracking. If the user didn't specify
            // a branch, record whatever branch the existing clone is on.
            let abs = resolve_and_validate_path(p)?;
            let recorded_branch = match branch {
                Some(b) => Some(b.to_string()),
                None => git::current_branch(Path::new(&abs)).ok(),
            };
            (
                SourceType::Git,
                Some(git_url.to_string()),
                recorded_branch,
                abs,
                true,
            )
        } else if let Some(existing) = Package::get_by_source_url(&conn, git_url)? {
            // A previously-registered package already points at this git URL —
            // reuse its on-disk clone instead of cloning a second time. This
            // lets monorepos be registered under multiple identifiers.
            let pkg_dir = std::path::PathBuf::from(&existing.path);
            eprintln!(
                "reusing existing clone at {} (already registered as '{}')",
                existing.path, existing.identifier
            );
            let recorded_branch = match branch {
                Some(b) => Some(b.to_string()),
                None => git::current_branch(&pkg_dir).ok(),
            };
            (
                SourceType::Git,
                Some(git_url.to_string()),
                recorded_branch,
                existing.path,
                true,
            )
        } else {
            // Clone the repo to clone_dir/<identifier>. When the user didn't
            // pass --branch we let git pick the remote's default branch
            // instead of hard-coding "main".
            let config = Config::load_or_default()?;
            let clone_dir = expand_tilde(&config.clone_dir);
            let pkg_dir = clone_dir.join(identifier);

            if pkg_dir.exists() {
                bail!(
                    "clone target already exists: {}; remove it first or use --path to track it",
                    pkg_dir.display()
                );
            }

            eprintln!("cloning {} ...", git_url);
            git::clone(git_url, &pkg_dir, branch)?;
            eprintln!("cloned to {}", pkg_dir.display());

            // Record the actual branch we ended up on (either the explicit
            // --branch value or the remote's default).
            let recorded_branch = match branch {
                Some(b) => Some(b.to_string()),
                None => git::current_branch(&pkg_dir).ok(),
            };

            (
                SourceType::Git,
                Some(git_url.to_string()),
                recorded_branch,
                pkg_dir.to_string_lossy().to_string(),
                true,
            )
        }
    } else {
        // Local package — --path is required (already checked above).
        let p = path.unwrap();
        let abs = resolve_and_validate_path(p)?;
        (SourceType::Local, None, None, abs, false)
    };

    let pkg = Package::new(
        identifier.to_string(),
        identifier.to_string(),
        source_type,
        source_url,
        source_branch,
        resolved_path,
        auto_pull,
        None,
    );

    pkg.insert(&conn)?;
    eprintln!("added package '{identifier}'");
    Ok(())
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = ::dirs::home_dir() {
            return home.join(rest);
        }
    } else if path == "~"
        && let Some(home) = ::dirs::home_dir()
    {
        return home;
    }
    std::path::PathBuf::from(path)
}

/// Resolve a path to absolute form and validate it exists as a directory.
fn resolve_and_validate_path(p: &str) -> Result<String> {
    let path = Path::new(p);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    if !abs.exists() {
        bail!("path does not exist: {}", abs.display());
    }
    if !abs.is_dir() {
        bail!("path is not a directory: {}", abs.display());
    }

    Ok(abs.to_string_lossy().to_string())
}

fn cmd_remove(identifier: &str) -> Result<()> {
    let conn = open_db()?;

    let pkg = Package::get_by_identifier(&conn, identifier)?;
    match pkg {
        Some(pkg) => {
            Package::delete(&conn, &pkg.id)?;
            eprintln!("removed package '{identifier}'");
        }
        None => {
            bail!("Package '{identifier}' not found. Run `kcl list` to see available packages.");
        }
    }

    Ok(())
}

fn cmd_show(identifier: &str, json: bool) -> Result<()> {
    let conn = open_db()?;

    let pkg = Package::get_by_identifier(&conn, identifier)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Package '{identifier}' not found. Run `kcl list` to see available packages."
        )
    })?;

    if json {
        let out = serde_json::to_string_pretty(&pkg)?;
        println!("{out}");
    } else {
        println!("identifier:    {}", pkg.identifier);
        println!("display_name:  {}", pkg.display_name);
        println!("source_type:   {}", pkg.source_type.as_str());
        if let Some(url) = &pkg.source_url {
            println!("source_url:    {url}");
        }
        if let Some(branch) = &pkg.source_branch {
            println!("source_branch: {branch}");
        }
        println!("path:          {}", pkg.path);
        println!("auto_pull:     {}", pkg.auto_pull);
        if let Some(harness) = &pkg.harness {
            println!("harness:       {harness}");
        }
        println!("created_at:    {}", pkg.created_at);
        println!("updated_at:    {}", pkg.updated_at);
    }

    Ok(())
}

fn cmd_pull(identifier: Option<&str>, all: bool) -> Result<()> {
    let conn = open_db()?;

    if let Some(id) = identifier {
        // Pull a single package.
        let pkg = Package::get_by_identifier(&conn, id)?.ok_or_else(|| {
            anyhow::anyhow!("Package '{id}' not found. Run `kcl list` to see available packages.")
        })?;

        if pkg.source_type != SourceType::Git {
            bail!("Package '{id}' is not a git package. Only git packages can be pulled.");
        }

        let repo_path = Path::new(&pkg.path);
        eprintln!("pulling {} ...", pkg.identifier);
        let msg = git::pull(repo_path)?;
        eprintln!("{}: {}", pkg.identifier, msg);
    } else if all {
        // Pull all auto-pull-enabled git packages.
        let packages = Package::list_all(&conn)?;
        let mut pulled = 0;
        let mut errors = 0;

        for pkg in &packages {
            if pkg.source_type != SourceType::Git || !pkg.auto_pull {
                continue;
            }

            let repo_path = Path::new(&pkg.path);
            eprintln!("pulling {} ...", pkg.identifier);
            match git::pull(repo_path) {
                Ok(msg) => {
                    eprintln!("{}: {}", pkg.identifier, msg);
                    pulled += 1;
                }
                Err(e) => {
                    eprintln!("{}: error: {}", pkg.identifier, e);
                    errors += 1;
                }
            }
        }

        eprintln!("pulled {pulled} package(s), {errors} error(s)");
    } else {
        bail!("Specify a package identifier or use --all to pull all auto-pull packages.");
    }

    Ok(())
}

fn cmd_set(identifier: &str, key: &str, value: Option<&str>, unset: bool) -> Result<()> {
    let conn = open_db()?;

    let mut pkg = Package::get_by_identifier(&conn, identifier)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Package '{identifier}' not found. Run `kcl list` to see available packages."
        )
    })?;

    match key {
        "auto-pull" => {
            if unset {
                bail!("cannot unset auto-pull; use 'true' or 'false'");
            }
            let val = value.ok_or_else(|| anyhow::anyhow!("missing value for auto-pull"))?;
            match val {
                "true" => pkg.auto_pull = true,
                "false" => pkg.auto_pull = false,
                other => {
                    bail!("invalid value for auto-pull: '{other}' (expected 'true' or 'false')")
                }
            }
        }
        "harness" => {
            if unset {
                pkg.harness = None;
            } else {
                let val = value
                    .ok_or_else(|| anyhow::anyhow!("missing value for harness (or use --unset)"))?;
                pkg.harness = Some(val.to_string());
            }
        }
        other => {
            bail!("Unknown property '{other}'. Valid properties: auto-pull, harness.");
        }
    }

    pkg.update(&conn)?;
    eprintln!("updated '{identifier}'");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::package::{Package, SourceType};

    /// Helper: create an in-memory DB with a test package.
    fn setup() -> (rusqlite::Connection, Package) {
        let conn = db::open_memory().unwrap();
        let pkg = Package::new(
            "testpkg".to_string(),
            "testpkg".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/testpkg".to_string(),
            false,
            None,
        );
        pkg.insert(&conn).unwrap();
        (conn, pkg)
    }

    #[test]
    fn add_local_validates_path_exists() {
        // resolve_and_validate_path should fail for nonexistent paths.
        let result = resolve_and_validate_path("/nonexistent/path/12345");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn add_local_validates_path_is_directory() {
        // A regular file should be rejected.
        let tmp = std::env::temp_dir().join("kcl_test_file");
        std::fs::write(&tmp, "test").unwrap();
        let result = resolve_and_validate_path(tmp.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not a directory"));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn add_local_accepts_valid_directory() {
        let result = resolve_and_validate_path("/tmp");
        assert!(result.is_ok());
    }

    #[test]
    fn duplicate_identifier_rejected() {
        let (conn, _pkg) = setup();
        // Try inserting a package with the same identifier.
        let dup = Package::new(
            "testpkg".to_string(),
            "testpkg".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/other".to_string(),
            false,
            None,
        );
        let result = dup.insert(&conn);
        assert!(result.is_err());
    }

    #[test]
    fn remove_nonexistent_package() {
        let conn = db::open_memory().unwrap();
        let result = Package::get_by_identifier(&conn, "nope").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn set_auto_pull() {
        let (conn, _pkg) = setup();
        let mut pkg = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        assert!(!pkg.auto_pull);

        pkg.auto_pull = true;
        pkg.update(&conn).unwrap();

        let updated = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        assert!(updated.auto_pull);
    }

    #[test]
    fn set_harness() {
        let (conn, _pkg) = setup();
        let mut pkg = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        assert!(pkg.harness.is_none());

        pkg.harness = Some("claude".to_string());
        pkg.update(&conn).unwrap();

        let updated = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        assert_eq!(updated.harness.as_deref(), Some("claude"));
    }

    #[test]
    fn unset_harness() {
        let (conn, _pkg) = setup();
        let mut pkg = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        pkg.harness = Some("claude".to_string());
        pkg.update(&conn).unwrap();

        let mut pkg = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        pkg.harness = None;
        pkg.update(&conn).unwrap();

        let updated = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        assert!(updated.harness.is_none());
    }

    #[test]
    fn second_git_package_with_same_url_reuses_path() {
        // Simulates `kcl packages add app-b --git <url>` after app-a already
        // exists for the same URL: lookup returns the existing path so the
        // caller can skip cloning.
        let conn = db::open_memory().unwrap();
        let url = "https://github.com/acme/monorepo.git";

        let first = Package::new(
            "app-a".to_string(),
            "app-a".to_string(),
            SourceType::Git,
            Some(url.to_string()),
            Some("main".to_string()),
            "/clones/monorepo".to_string(),
            true,
            None,
        );
        first.insert(&conn).unwrap();

        // The cmd_add code path queries by source_url before cloning.
        let existing = Package::get_by_source_url(&conn, url).unwrap();
        assert!(existing.is_some());
        let existing = existing.unwrap();
        assert_eq!(existing.path, "/clones/monorepo");

        // Insert the second package reusing that path.
        let second = Package::new(
            "app-b".to_string(),
            "app-b".to_string(),
            SourceType::Git,
            Some(url.to_string()),
            Some("main".to_string()),
            existing.path.clone(),
            true,
            None,
        );
        second.insert(&conn).unwrap();

        let app_a = Package::get_by_identifier(&conn, "app-a").unwrap().unwrap();
        let app_b = Package::get_by_identifier(&conn, "app-b").unwrap().unwrap();
        assert_eq!(app_a.path, app_b.path);
        assert_ne!(app_a.id, app_b.id);
    }

    #[test]
    fn list_json_serialization() {
        let (conn, _pkg) = setup();
        let packages = Package::list_all(&conn).unwrap();
        let json = serde_json::to_string_pretty(&packages).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["identifier"], "testpkg");
    }

    #[test]
    fn show_json_serialization() {
        let (conn, _pkg) = setup();
        let pkg = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        let json = serde_json::to_string_pretty(&pkg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_object());
        assert_eq!(parsed["identifier"], "testpkg");
        assert_eq!(parsed["source_type"], "local");
        assert_eq!(parsed["auto_pull"], false);
        assert_eq!(parsed["path"], "/tmp/testpkg");
    }
}
