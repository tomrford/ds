use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{Matcher, Visit, VisitDirs, VisitFiles};
use jj_lib::repo_path::RepoPath;

use crate::checkout::{read_checkout_owner, reject_unsupported_global_options};

const DSHIDE: &str = ".dshide";

#[derive(clap::Args)]
pub(crate) struct HiddenArgs {
    #[command(subcommand)]
    command: HiddenCommand,
}

#[derive(clap::Subcommand)]
enum HiddenCommand {
    /// Add a gitignore pattern to the working-copy .dshide file.
    Add { pattern: String },
    /// Remove an exact pattern line from the working-copy .dshide file.
    Remove { pattern: String },
    /// Print the working-copy .dshide file as-is.
    List,
}

pub(crate) async fn run_hidden(
    ui: &mut Ui,
    command: &CommandHelper,
    args: HiddenArgs,
) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "hidden")?;
    match args.command {
        HiddenCommand::Add { pattern } => add(ui, command, pattern).await,
        HiddenCommand::Remove { pattern } => remove(ui, command, pattern).await,
        HiddenCommand::List => list(ui, command).await,
    }
}

async fn add(ui: &mut Ui, command: &CommandHelper, pattern: String) -> Result<(), CommandError> {
    validate_pattern_argument(&pattern)?;
    let root = checkout_root(ui, command).await?;
    let policy_path = root.join(DSHIDE);
    let mut contents = read_policy(&policy_path)?;
    if contains_line(&contents, pattern.as_bytes()) {
        writeln!(ui.status(), "Pattern `{pattern}` is already in .dshide.")?;
        return Ok(());
    }
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        contents.push(b'\n');
    }
    contents.extend_from_slice(pattern.as_bytes());
    contents.push(b'\n');
    fs::write(&policy_path, contents).map_err(|error| {
        user_error(format!(
            "Failed to write {}: {error}",
            policy_path.display()
        ))
    })?;

    let matcher = pattern_matcher(&pattern);
    let mut workspace_command = command.workspace_helper(ui).await?;
    let covered = pattern_is_gitignored(
        workspace_command.workspace_root(),
        &matcher,
        workspace_command.base_ignores()?,
    )?;
    if !covered {
        writeln!(
            ui.warning_default(),
            "Pattern `{pattern}` is not covered by gitignore in this working copy."
        )?;
    }

    let track_matcher = HiddenTrackMatcher { matcher };
    let mut options =
        workspace_command.snapshot_options_with_start_tracking_matcher(&track_matcher)?;
    options.force_tracking_matcher = &track_matcher;
    let mut transaction = workspace_command.start_transaction().into_inner();
    let (mut locked_workspace, _) = workspace_command.start_working_copy_mutation().await?;
    locked_workspace.locked_wc().snapshot(&options).await?;
    transaction.repo_mut().rebase_descendants().await?;
    let repo = transaction.commit("add hidden pattern").await?;
    locked_workspace.finish(repo.op_id().clone()).await?;
    writeln!(ui.status(), "Added `{pattern}` to .dshide.")?;
    Ok(())
}

async fn remove(ui: &mut Ui, command: &CommandHelper, pattern: String) -> Result<(), CommandError> {
    validate_pattern_argument(&pattern)?;
    let root = checkout_root(ui, command).await?;
    let policy_path = root.join(DSHIDE);
    let contents = read_policy(&policy_path)?;
    let Some(contents) = remove_line(&contents, pattern.as_bytes()) else {
        return Err(user_error(format!(
            "Pattern `{pattern}` is not present in .dshide."
        )));
    };
    fs::write(&policy_path, contents).map_err(|error| {
        user_error(format!(
            "Failed to write {}: {error}",
            policy_path.display()
        ))
    })?;
    command.workspace_helper(ui).await?;
    writeln!(ui.status(), "Removed `{pattern}` from .dshide.")?;
    writeln!(
        ui.warning_default(),
        "Content matching `{pattern}` is eligible for future Git publication from descendants."
    )?;
    Ok(())
}

async fn list(ui: &mut Ui, command: &CommandHelper) -> Result<(), CommandError> {
    let root = checkout_root(ui, command).await?;
    let contents = read_policy(&root.join(DSHIDE))?;
    ui.stdout().write_all(&contents)?;
    Ok(())
}

async fn checkout_root(ui: &Ui, command: &CommandHelper) -> Result<PathBuf, CommandError> {
    let workspace = command.workspace_helper_no_snapshot(ui).await?;
    let root = workspace.workspace_root().to_owned();
    read_checkout_owner(&root)
        .map_err(|_| user_error("`ds hidden` is available only inside a Devspace checkout."))?;
    Ok(root)
}

fn validate_pattern_argument(pattern: &str) -> Result<(), CommandError> {
    if pattern.contains(['\n', '\r']) {
        Err(user_error("A hidden pattern must be exactly one line."))
    } else {
        Ok(())
    }
}

fn read_policy(path: &Path) -> Result<Vec<u8>, CommandError> {
    match fs::read(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(user_error(format!(
            "Failed to read {}: {error}",
            path.display()
        ))),
    }
}

fn contains_line(contents: &[u8], pattern: &[u8]) -> bool {
    contents
        .split(|byte| *byte == b'\n')
        .any(|line| line == pattern)
}

fn remove_line(contents: &[u8], pattern: &[u8]) -> Option<Vec<u8>> {
    let mut start = 0;
    while start <= contents.len() {
        let end = contents[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(contents.len(), |offset| start + offset);
        if &contents[start..end] == pattern {
            let remove_end = if end < contents.len() { end + 1 } else { end };
            let mut updated = Vec::with_capacity(contents.len() - (remove_end - start));
            updated.extend_from_slice(&contents[..start]);
            updated.extend_from_slice(&contents[remove_end..]);
            return Some(updated);
        }
        if end == contents.len() {
            break;
        }
        start = end + 1;
    }
    None
}

fn pattern_matcher(pattern: &str) -> Arc<GitIgnoreFile> {
    let mut bytes = pattern.as_bytes().to_vec();
    bytes.push(b'\n');
    GitIgnoreFile::empty()
        .chain(RepoPath::root(), Path::new(DSHIDE), &bytes)
        .expect("in-memory gitignore patterns have no parse errors")
}

#[derive(Debug)]
struct HiddenTrackMatcher {
    matcher: Arc<GitIgnoreFile>,
}

impl Matcher for HiddenTrackMatcher {
    fn matches(&self, file: &RepoPath) -> bool {
        file.as_internal_file_string() == DSHIDE || hidden_file_matches(&self.matcher, file)
    }

    fn visit(&self, _dir: &RepoPath) -> Visit {
        Visit::Specific {
            dirs: VisitDirs::All,
            files: VisitFiles::All,
        }
    }
}

fn hidden_file_matches(matcher: &GitIgnoreFile, path: &RepoPath) -> bool {
    matcher.matches_file(path)
        || path
            .ancestors()
            .skip(1)
            .filter(|ancestor| !ancestor.is_root())
            .any(|ancestor| matcher.matches_dir(ancestor))
}

fn pattern_is_gitignored(
    root: &Path,
    dshide: &GitIgnoreFile,
    base_ignores: Arc<GitIgnoreFile>,
) -> Result<bool, CommandError> {
    let mut matched = false;
    let mut uncovered = false;
    inspect_gitignore_coverage(
        root,
        RepoPath::root(),
        dshide,
        base_ignores,
        &mut matched,
        &mut uncovered,
    )?;
    Ok(matched && !uncovered)
}

fn inspect_gitignore_coverage(
    root: &Path,
    dir: &RepoPath,
    dshide: &GitIgnoreFile,
    ignores: Arc<GitIgnoreFile>,
    matched: &mut bool,
    uncovered: &mut bool,
) -> Result<(), CommandError> {
    let internal_dir = dir.as_internal_file_string();
    let filesystem_dir = root.join(internal_dir);
    let ignores = ignores
        .chain_with_file(dir, filesystem_dir.join(".gitignore"))
        .map_err(CommandError::from)?;
    let entries = fs::read_dir(&filesystem_dir).map_err(|error| {
        user_error(format!(
            "Failed to inspect gitignore coverage in {}: {error}",
            filesystem_dir.display()
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            user_error(format!(
                "Failed to inspect gitignore coverage in {}: {error}",
                filesystem_dir.display()
            ))
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if dir.is_root() && matches!(name.as_str(), ".git" | ".jj" | DSHIDE) {
            continue;
        }
        let Ok(name) = jj_lib::repo_path::RepoPathComponentBuf::new(name) else {
            continue;
        };
        let entry_path = dir.join(&name);
        let file_type = entry.file_type().map_err(|error| {
            user_error(format!(
                "Failed to inspect {}: {error}",
                entry.path().display()
            ))
        })?;
        if file_type.is_dir() {
            if dshide.matches_dir(&entry_path) {
                *matched = true;
                *uncovered |= !ignores.matches_dir(&entry_path);
                continue;
            }
            inspect_gitignore_coverage(
                root,
                &entry_path,
                dshide,
                ignores.clone(),
                matched,
                uncovered,
            )?;
        } else if hidden_file_matches(dshide, &entry_path) {
            *matched = true;
            *uncovered |= !ignores.matches_file(&entry_path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_edits_preserve_unrelated_bytes() {
        let contents = b"# comment\nfirst\nsecond";
        assert!(contains_line(contents, b"first"));
        assert_eq!(
            remove_line(contents, b"first").unwrap(),
            b"# comment\nsecond"
        );
        assert!(remove_line(contents, b"missing").is_none());
    }

    #[test]
    fn force_tracking_includes_children_of_an_excluded_directory() {
        let matcher = HiddenTrackMatcher {
            matcher: pattern_matcher("private/"),
        };
        assert!(matcher.matches(RepoPath::from_internal_string("private/secret").unwrap()));
    }
}
