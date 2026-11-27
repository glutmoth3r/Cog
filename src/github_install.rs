//! Install a GitHub checkout through the existing local package store.
use std::path::{Path, PathBuf};
use std::process::Command;

fn github_url(input: &str) -> Result<String, String> {
    let path = input.strip_prefix("https://github.com/").unwrap_or(input);
    let Some((owner, repository)) = path.split_once('/') else {
        return Err("expected a GitHub repository as owner/repository or https://github.com/owner/repository".into());
    };
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    if owner.is_empty()
        || owner.len() > 39
        || owner.starts_with('-')
        || owner.ends_with('-')
        || !owner
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        || repository.is_empty()
        || repository.len() > 100
        || matches!(repository, "." | "..")
        || !repository
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    {
        return Err("invalid GitHub repository; expected owner/repository or https://github.com/owner/repository".into());
    }
    Ok(format!("https://github.com/{owner}/{repository}.git"))
}

fn git(args: &[&std::ffi::OsStr], directory: Option<&Path>) -> Result<String, String> {
    let mut command = Command::new("git");
    command.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    let output = command
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git failed: {}", message.trim()));
    }
    String::from_utf8(output.stdout).map_err(|_| "git returned invalid UTF-8".into())
}

fn temporary_directory() -> Result<PathBuf, String> {
    for _ in 0..16 {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce)
            .map_err(|error| format!("cannot create temporary checkout: {error}"))?;
        let path =
            std::env::temp_dir().join(format!("cog-git-{:032x}", u128::from_ne_bytes(nonce)));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("cannot create temporary checkout: {error}")),
        }
    }
    Err("cannot find an unused temporary checkout directory".into())
}

fn validate_checkout(checkout: &Path) -> Result<(), String> {
    let root = checkout
        .canonicalize()
        .map_err(|error| format!("cannot resolve Git checkout: {error}"))?;
    let mut directories = vec![root.clone()];
    let mut entries = 0usize;
    let mut path_bytes = 0usize;
    while let Some(directory) = directories.pop() {
        let children = std::fs::read_dir(&directory)
            .map_err(|error| format!("cannot inspect Git checkout: {error}"))?;
        for entry in children {
            let entry = entry.map_err(|error| format!("cannot inspect Git checkout: {error}"))?;
            let path = entry.path();
            entries = entries
                .checked_add(1)
                .ok_or("Git checkout entry limit exceeded")?;
            path_bytes = path_bytes
                .checked_add(path.as_os_str().as_encoded_bytes().len())
                .ok_or("Git checkout path limit exceeded")?;
            if entries > 262_144 || path_bytes > 64 * 1024 * 1024 {
                return Err("Git checkout inspection limit exceeded".into());
            }
            let kind = entry
                .file_type()
                .map_err(|error| format!("cannot inspect Git checkout entry: {error}"))?;
            if kind.is_symlink() {
                return Err(format!(
                    "Git checkout contains a symbolic link: {}",
                    path.display()
                ));
            }
            if path == root.join(".git") {
                if !kind.is_dir() {
                    return Err("Git checkout metadata is not a directory".into());
                }
                continue;
            }
            if kind.is_dir() {
                directories
                    .try_reserve(1)
                    .map_err(|_| "Git checkout inspection limit exceeded")?;
                directories.push(path);
            } else if !kind.is_file() {
                return Err(format!(
                    "Git checkout contains a non-regular file: {}",
                    path.display()
                ));
            }
        }
    }

    let mut pending = vec![root.clone()];
    let mut visited = std::collections::HashSet::new();
    while let Some(package) = pending.pop() {
        if !visited.insert(package.clone()) {
            continue;
        }
        if visited.len() > 256 {
            return Err("Git checkout package limit (256) exceeded".into());
        }
        let dependencies = cog::config::dependencies(&package.join("dependencies.kdl"))
            .map_err(|errors| cog::frontend::render(&errors))?;
        for dependency in dependencies {
            if dependency.installed.is_some() {
                continue;
            }
            if dependency.archive {
                return Err("GitHub installation does not support archive dependencies; install the package locally first".into());
            }
            let mut relative = package
                .strip_prefix(&root)
                .map_err(|_| "Git checkout dependency escapes the checkout")?
                .to_path_buf();
            for component in dependency.path.components() {
                match component {
                    std::path::Component::Normal(part) => relative.push(part),
                    std::path::Component::CurDir => {}
                    std::path::Component::ParentDir if relative.pop() => {}
                    _ => {
                        return Err(format!(
                            "Git checkout dependency escapes the checkout: {}",
                            dependency.path.display()
                        ));
                    }
                }
            }
            let path = root.join(relative).canonicalize().map_err(|error| {
                format!(
                    "cannot resolve Git checkout dependency {}: {error}",
                    dependency.path.display()
                )
            })?;
            if !path.starts_with(&root) {
                return Err(format!(
                    "Git checkout dependency escapes the checkout: {}",
                    dependency.path.display()
                ));
            }
            if !path.is_dir() {
                return Err(format!(
                    "Git checkout dependency is not a directory: {}",
                    dependency.path.display()
                ));
            }
            pending
                .try_reserve(1)
                .map_err(|_| "Git checkout package limit exceeded")?;
            visited
                .try_reserve(1)
                .map_err(|_| "Git checkout package limit exceeded")?;
            pending.push(path);
        }
    }
    Ok(())
}

pub(crate) fn install(
    repository: &str,
    revision: Option<&str>,
    locked: bool,
) -> Result<(cog::package::InstallResult, String), String> {
    use std::ffi::OsStr;
    let url = github_url(repository)?;
    let revision = revision.unwrap_or("HEAD");
    if revision.is_empty()
        || revision.starts_with('-')
        || revision.len() > 4096
        || revision.chars().any(char::is_control)
    {
        return Err("invalid Git revision".into());
    }
    let temporary = temporary_directory()?;
    let checkout = temporary.join("checkout");
    let result = (|| {
        git(
            &[
                OsStr::new("clone"),
                OsStr::new("--quiet"),
                OsStr::new("--no-checkout"),
                OsStr::new("--"),
                OsStr::new(&url),
                checkout.as_os_str(),
            ],
            None,
        )?;
        let expression = format!("{revision}^{{commit}}");
        let commit = git(
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new("--end-of-options"),
                OsStr::new(&expression),
            ],
            Some(&checkout),
        )?;
        let commit = commit.trim();
        if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("git returned an invalid commit identity".into());
        }
        let hooks = format!("core.hooksPath={}", temporary.join("no-hooks").display());
        git(
            &[
                OsStr::new("-c"),
                OsStr::new(&hooks),
                OsStr::new("checkout"),
                OsStr::new("--quiet"),
                OsStr::new("--detach"),
                OsStr::new(commit),
                OsStr::new("--"),
            ],
            Some(&checkout),
        )?;
        validate_checkout(&checkout)?;
        let installed = cog::package::install_path(&checkout, locked)?;
        Ok((installed, commit.to_owned()))
    })();
    match (result, std::fs::remove_dir_all(&temporary)) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok((installed, _)), Err(error)) => Err(format!(
            "installed package at {}, but cannot remove temporary checkout: {error}",
            installed.path.display()
        )),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!(
            "{error}; cannot remove temporary checkout: {cleanup}"
        )),
    }
}
