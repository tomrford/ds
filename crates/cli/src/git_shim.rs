//! Best-effort `.git` index shim for local checkout consumers that look for
//! Git metadata without treating Git as Devspace's local VCS surface.

use std::fs;
use std::path::Path;
use std::process::Command;

use jj_lib::repo_path::RepoPath;

const PRIVATE_EXCLUDES_BEGIN: &str = "# devspace private paths";
const PRIVATE_EXCLUDES_END: &str = "# /devspace private paths";

pub fn ensure(checkout_root: &Path) {
    if let Err(err) = ensure_inner(checkout_root) {
        tracing::warn!(
            checkout = %checkout_root.display(),
            "git index shim refresh failed: {err}"
        );
    }
}

pub fn remove_guard(checkout_root: &Path) {
    if let Err(err) = make_git_dirs_writable(checkout_root) {
        tracing::warn!(
            checkout = %checkout_root.display(),
            "git index shim unlock failed: {err}"
        );
    }
}

fn ensure_inner(checkout_root: &Path) -> Result<(), String> {
    let git_dir = checkout_root.join(".git");
    if git_dir.exists() && !git_dir.is_dir() {
        return Err(format!(
            "{} exists but is not a directory",
            git_dir.display()
        ));
    }

    if !git_dir.exists() {
        require_success(git(checkout_root).args(["init", "-q"]), "git init")?;
    }

    make_git_dirs_writable(checkout_root)?;
    let base_ignores =
        crate::working_copy::base_ignores(checkout_root).map_err(|error| error.to_string())?;
    let hidden_paths = crate::working_copy::discover_hidden_paths(checkout_root, &base_ignores)
        .map_err(|error| error.to_string())?;
    ensure_info_exclude(&git_dir, hidden_paths.iter().map(|path| path.as_ref()))?;
    require_success(git(checkout_root).args(["add", "-A"]), "git add -A")?;
    if !hidden_paths.is_empty() {
        let mut command = git(checkout_root);
        command.args(["rm", "-r", "--cached", "-q", "--ignore-unmatch", "--"]);
        command.args(
            hidden_paths
                .iter()
                .map(|path| path.as_internal_file_string()),
        );
        require_success(&mut command, "git rm --cached")?;
    }
    make_git_dirs_read_only(checkout_root)?;
    Ok(())
}

fn ensure_info_exclude<'a>(
    git_dir: &Path,
    hidden_paths: impl IntoIterator<Item = &'a RepoPath>,
) -> Result<(), String> {
    let exclude_path = git_dir.join("info/exclude");
    let text = fs::read_to_string(&exclude_path).unwrap_or_default();
    let mut lines = Vec::new();
    let mut in_private_section = false;
    for line in text.lines() {
        if line == PRIVATE_EXCLUDES_BEGIN {
            in_private_section = true;
        } else if line == PRIVATE_EXCLUDES_END {
            in_private_section = false;
        } else if !in_private_section {
            lines.push(line.to_owned());
        }
    }
    if !lines.iter().any(|line| line.trim() == ".jj/") {
        lines.push(".jj/".to_owned());
    }
    let hidden_paths = hidden_paths.into_iter().collect::<Vec<_>>();
    if !hidden_paths.is_empty() {
        lines.push(PRIVATE_EXCLUDES_BEGIN.to_owned());
        lines.extend(hidden_paths.into_iter().map(exclude_pattern));
        lines.push(PRIVATE_EXCLUDES_END.to_owned());
    }
    fs::write(&exclude_path, format!("{}\n", lines.join("\n")))
        .map_err(|error| format!("write {}: {error}", exclude_path.display()))
}

fn exclude_pattern(path: &RepoPath) -> String {
    let mut pattern = String::from("/");
    for character in path.as_internal_file_string().chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | ' ') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern
}

fn git(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "0")
        .arg("-c")
        .arg("core.hooksPath=/dev/null");
    command
}

fn require_success(command: &mut Command, description: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|error| format!("run {description}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{description} failed"))
    }
}

fn make_git_dirs_writable(checkout_root: &Path) -> Result<(), String> {
    update_git_dir_modes(checkout_root, |mode| mode | 0o700)
}

fn make_git_dirs_read_only(checkout_root: &Path) -> Result<(), String> {
    update_git_dir_modes(checkout_root, |mode| mode & !0o222)
}

fn update_git_dir_modes(checkout_root: &Path, f: impl Fn(u32) -> u32 + Copy) -> Result<(), String> {
    let git_dir = checkout_root.join(".git");
    if !git_dir.exists() {
        return Ok(());
    }
    crate::tree_modes::rewrite(&git_dir, |is_dir, mode| is_dir.then(|| f(mode)))
        .map_err(|error| format!("update modes under {}: {error}", git_dir.display()))
}
