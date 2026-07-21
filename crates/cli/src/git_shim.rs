//! Best-effort `.git` index shim for local checkout consumers that look for
//! Git metadata without treating Git as Devspace's local VCS surface.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::process::Command;

use blake2::{Blake2b512, Digest as _};
use jj_lib::backend::TreeValue;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::StoreFactories;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::settings::UserSettings;
use jj_lib::workspace::Workspace;

const PRIVATE_EXCLUDES_BEGIN: &str = "# devspace private paths";
const PRIVATE_EXCLUDES_END: &str = "# /devspace private paths";
const MAX_PATHS_PER_GIT_COMMAND: usize = 128;
const MAX_PATH_BYTES_PER_GIT_COMMAND: usize = 32 * 1024;
const AFTER_EXCLUSION_FAILPOINT: &str = "git_shim_after_exclusion";
const LOCK_FILE: &str = "devspace-git-shim.lock";
const STATE_FILE: &str = "devspace-git-shim.state";
const TREE_IDENTITY_DOMAIN: &[u8] = b"devspace-git-shim-tree-v1";
const POLICY_IDENTITY_DOMAIN: &[u8] = b"devspace-hidden-set-v1";

pub const SETTING: &str = "devspace.git-shim";

pub fn ensure(checkout_root: &Path, settings: &UserSettings) {
    if let Err(err) = ensure_inner(checkout_root, settings) {
        tracing::warn!(
            checkout = %checkout_root.display(),
            "git index shim refresh failed: {err}"
        );
    }
}

pub struct RemovalGuard {
    _lock: Option<CheckoutLock>,
}

pub fn remove_guard(checkout_root: &Path) -> RemovalGuard {
    match CheckoutLock::acquire(checkout_root).and_then(|lock| {
        make_git_dirs_writable(checkout_root)?;
        Ok(lock)
    }) {
        Ok(lock) => RemovalGuard { _lock: Some(lock) },
        Err(err) => {
            tracing::warn!(
                checkout = %checkout_root.display(),
                "git index shim unlock failed: {err}"
            );
            RemovalGuard { _lock: None }
        }
    }
}

fn ensure_inner(checkout_root: &Path, settings: &UserSettings) -> Result<(), String> {
    let _checkout_lock = CheckoutLock::acquire(checkout_root)?;
    let git_dir = checkout_root.join(".git");
    if git_dir.exists() && !git_dir.is_dir() {
        return Err(format!(
            "{} exists but is not a directory",
            git_dir.display()
        ));
    }
    // An interrupted process can leave the guard relaxed. Repair it before
    // resolving any refresh inputs so errors cannot extend that window.
    make_git_dirs_read_only(checkout_root)?;

    // Resolve everything that does not need a writable .git first. In
    // particular, an unrelated invalid filesystem path must not leave the
    // guard relaxed or partially rewrite the index.
    let base_ignores =
        crate::working_copy::base_ignores(checkout_root).map_err(|error| error.to_string())?;
    let discovered = crate::working_copy::discover_shim_paths(checkout_root, &base_ignores)
        .map_err(|error| error.to_string())?;
    let canonical = canonical_paths(checkout_root, settings)?;
    let refresh_state = canonical.refresh_state(&discovered);
    if git_dir.is_dir()
        && (git_dir.join("index").is_file() || !canonical.has_public_paths())
        && read_refresh_state(checkout_root).as_deref() == Some(refresh_state.as_str())
    {
        return if canonical.policy_errors.is_empty() {
            Ok(())
        } else {
            Err(canonical.policy_errors.join("; "))
        };
    }
    let excluded_paths = discovered
        .hidden_paths
        .iter()
        .chain(&discovered.base_ignored_paths)
        .chain(&canonical.hidden_paths)
        .chain(&canonical.fail_closed_roots)
        .cloned()
        .collect::<BTreeSet<_>>();

    // info/exclude is line-based with no escape for newlines, so such a path
    // cannot be kept out of `git add -A`; a carriage return survives only
    // until Git strips it at a line ending. Fail closed before touching .git
    // rather than let the path into the index.
    if let Some(path) = excluded_paths
        .iter()
        .find(|path| path.as_internal_file_string().contains(['\n', '\r']))
    {
        return Err(format!(
            "cannot exclude {:?}: Git exclude patterns cannot represent newlines",
            path.as_internal_file_string()
        ));
    }

    // A base ignore only excludes untracked paths from jj's snapshot. Restore
    // canonical public files after clearing an ignored root, including on the
    // first shim build and after repairing an index written by an older ds.
    // An invalid policy excludes its complete subtree instead: no materialized
    // conflict or symlink target is allowed to invent projection policy.
    let canonical_public = canonical
        .tracked_paths
        .iter()
        .filter(|path| {
            discovered
                .base_ignored_paths
                .iter()
                .any(|ignored| at_or_below(path, ignored))
                && !canonical.hidden_paths.contains(*path)
                && !canonical
                    .fail_closed_roots
                    .iter()
                    .any(|root| at_or_below(path, root))
        })
        .collect::<Vec<_>>();

    let guard = GitDirGuard::acquire(checkout_root)?;
    let refresh = (|| {
        if !git_dir.exists() {
            require_success(git(checkout_root).args(["init", "-q"]), "git init")?;
        }
        remove_stale_index_lock(&git_dir)?;
        ensure_info_exclude(&git_dir, excluded_paths.iter().map(|path| path.as_ref()))?;

        // Remove exclusions before adding anything. Once removed, info/exclude
        // prevents `add -A` from reacquiring them, so a later error can omit
        // public files but cannot leak hidden files.
        remove_from_index(checkout_root, &excluded_paths)?;
        if crate::git::failpoint_enabled(AFTER_EXCLUSION_FAILPOINT) {
            return Err(format!("injected failure at {AFTER_EXCLUSION_FAILPOINT}"));
        }
        require_success(git(checkout_root).args(["add", "-A"]), "git add -A")?;
        add_to_index(checkout_root, canonical_public)?;
        write_refresh_state(checkout_root, &refresh_state)?;

        if canonical.policy_errors.is_empty() {
            Ok(())
        } else {
            Err(canonical.policy_errors.join("; "))
        }
    })();
    finish_guard(refresh, guard)
}

fn finish_guard(result: Result<(), String>, guard: GitDirGuard<'_>) -> Result<(), String> {
    match (result, guard.finish()) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(lock_error)) => Err(format!(
            "{error}; also failed to restore Git guard: {lock_error}"
        )),
    }
}

struct CanonicalPaths {
    tree_identity: String,
    policy_identity: String,
    tracked_paths: BTreeSet<RepoPathBuf>,
    hidden_paths: BTreeSet<RepoPathBuf>,
    fail_closed_roots: BTreeSet<RepoPathBuf>,
    policy_errors: Vec<String>,
}

impl CanonicalPaths {
    fn refresh_state(&self, discovered: &crate::working_copy::ShimDiscovery) -> String {
        let mut hasher = Blake2b512::new();
        hasher.update(self.tree_identity.as_bytes());
        for (kind, paths) in [
            (b'h', &discovered.hidden_paths),
            (b'i', &discovered.base_ignored_paths),
        ] {
            for path in paths {
                let path = path.as_internal_file_string();
                hasher.update([kind]);
                hasher.update((path.len() as u64).to_le_bytes());
                hasher.update(path.as_bytes());
            }
        }
        format!(
            "{}\n{}\n",
            hex_bytes(&hasher.finalize()),
            self.policy_identity
        )
    }

    fn has_public_paths(&self) -> bool {
        self.tracked_paths.iter().any(|path| {
            !self.hidden_paths.contains(path)
                && !self
                    .fail_closed_roots
                    .iter()
                    .any(|root| at_or_below(path, root))
        })
    }
}

fn canonical_paths(
    checkout_root: &Path,
    settings: &UserSettings,
) -> Result<CanonicalPaths, String> {
    let workspace = Workspace::load(
        settings,
        checkout_root,
        &StoreFactories::default(),
        &crate::working_copy::devspace_working_copy_factories(),
    )
    .map_err(|error| format!("load checkout metadata: {error}"))?;
    let tree = workspace
        .working_copy()
        .tree()
        .map_err(|error| format!("load working-copy tree: {error}"))?
        .clone();
    futures::executor::block_on(resolve_canonical_paths(tree))
}

async fn resolve_canonical_paths(tree: MergedTree) -> Result<CanonicalPaths, String> {
    let tree_identity = tree_identity(&tree);
    let store = tree.store();
    let mut policy_paths = BTreeSet::new();
    for tree_id in tree.tree_ids().iter() {
        let mut pending = vec![(RepoPathBuf::root(), tree_id.clone())];
        while let Some((dir, tree_id)) = pending.pop() {
            let backend_tree = store
                .get_tree(dir.clone(), &tree_id)
                .await
                .map_err(|error| format!("read canonical tree: {error}"))?;
            for entry in backend_tree.entries_non_recursive() {
                let path = dir.join(entry.name());
                if is_dsprivate(&path) {
                    policy_paths.insert(path);
                } else if let TreeValue::Tree(child_id) = entry.value() {
                    pending.push((path, child_id.clone()));
                }
            }
        }
    }

    let mut matcher = GitIgnoreFile::empty();
    let mut policy_files = Vec::new();
    let mut fail_closed_roots = BTreeSet::new();
    let mut policy_errors = Vec::new();
    for path in &policy_paths {
        let value = tree.path_value(path).await.map_err(|error| {
            format!("read canonical {}: {error}", path.as_internal_file_string())
        })?;
        let id = match value.into_resolved() {
            Ok(Some(TreeValue::File { id, .. })) => id,
            Ok(_) => {
                fail_closed_policy(
                    path,
                    "is not a regular file",
                    &mut fail_closed_roots,
                    &mut policy_errors,
                );
                continue;
            }
            Err(_) => {
                fail_closed_policy(
                    path,
                    "is conflicted",
                    &mut fail_closed_roots,
                    &mut policy_errors,
                );
                continue;
            }
        };
        policy_files.push((path.clone(), id.clone()));
        let contents = store.read_file(path, &id).await.map_err(|error| {
            format!("read canonical {}: {error}", path.as_internal_file_string())
        })?;
        let mut bytes = Vec::new();
        jj_lib::file_util::copy_async_to_sync(contents, &mut bytes)
            .await
            .map_err(|error| {
                format!("read canonical {}: {error}", path.as_internal_file_string())
            })?;
        matcher = matcher
            .chain(
                path.parent().expect(".dsprivate has a parent directory"),
                Path::new(".dsprivate"),
                &bytes,
            )
            .map_err(|error| error.to_string())?;
    }

    let tracked_paths = tree
        .entries()
        .map(|(path, value)| {
            value
                .map(|_| path)
                .map_err(|error| format!("read working-copy tree: {error}"))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut hidden_paths = tracked_paths
        .iter()
        .filter(|path| {
            policy_paths.contains(*path)
                || matcher.matches_file(path)
                || path
                    .ancestors()
                    .skip(1)
                    .any(|ancestor| matcher.matches_dir(ancestor))
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    hidden_paths.extend(policy_paths);

    Ok(CanonicalPaths {
        tree_identity,
        policy_identity: policy_identity(&policy_files),
        tracked_paths,
        hidden_paths,
        fail_closed_roots,
        policy_errors,
    })
}

fn tree_identity(tree: &MergedTree) -> String {
    let mut hasher = Blake2b512::new();
    hasher.update(TREE_IDENTITY_DOMAIN);
    for id in tree.tree_ids().iter() {
        hasher.update((id.as_bytes().len() as u64).to_le_bytes());
        hasher.update(id.as_bytes());
    }
    for label in tree.labels().as_slice() {
        hasher.update((label.len() as u64).to_le_bytes());
        hasher.update(label.as_bytes());
    }
    hex_bytes(&hasher.finalize())
}

fn policy_identity(files: &[(RepoPathBuf, jj_lib::backend::FileId)]) -> String {
    if files.is_empty() {
        return "none".to_owned();
    }
    let mut hasher = Blake2b512::new();
    hasher.update(POLICY_IDENTITY_DOMAIN);
    for (path, id) in files {
        let path = path.as_internal_file_string();
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update(id.as_bytes());
    }
    hex_bytes(&hasher.finalize())
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_refresh_state(checkout_root: &Path) -> Option<String> {
    fs::read_to_string(checkout_root.join(".jj").join(STATE_FILE)).ok()
}

fn write_refresh_state(checkout_root: &Path, state: &str) -> Result<(), String> {
    let path = checkout_root.join(".jj").join(STATE_FILE);
    fs::write(&path, state).map_err(|error| format!("write {}: {error}", path.display()))
}

fn remove_stale_index_lock(git_dir: &Path) -> Result<(), String> {
    let path = git_dir.join("index.lock");
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove stale {}: {error}", path.display())),
    }
}

fn fail_closed_policy(
    path: &RepoPath,
    reason: &str,
    roots: &mut BTreeSet<RepoPathBuf>,
    errors: &mut Vec<String>,
) {
    roots.insert(
        path.parent()
            .expect(".dsprivate has a parent directory")
            .to_owned(),
    );
    errors.push(format!(
        "canonical policy {} {reason}",
        path.as_internal_file_string()
    ));
}

fn at_or_below(path: &RepoPath, root: &RepoPath) -> bool {
    path == root || path.starts_with(root)
}

fn is_dsprivate(path: &RepoPath) -> bool {
    path.as_internal_file_string()
        .rsplit('/')
        .next()
        .is_some_and(|name| name == ".dsprivate")
}

fn remove_from_index(checkout_root: &Path, paths: &BTreeSet<RepoPathBuf>) -> Result<(), String> {
    for_path_batches(paths.iter(), |batch| {
        let mut command = git(checkout_root);
        command.args(["rm", "-r", "--cached", "-q", "--ignore-unmatch", "--"]);
        command.args(batch.iter().map(|path| git_pathspec(path)));
        require_success(&mut command, "git rm --cached")
    })
}

fn add_to_index<'a>(
    checkout_root: &Path,
    paths: impl IntoIterator<Item = &'a RepoPathBuf>,
) -> Result<(), String> {
    for_path_batches(paths, |batch| {
        let mut command = git(checkout_root);
        command.args(["add", "-f", "--"]);
        command.args(batch.iter().map(|path| git_pathspec(path)));
        require_success(&mut command, "git add -f")
    })
}

fn for_path_batches<'a>(
    paths: impl IntoIterator<Item = &'a RepoPathBuf>,
    mut run: impl FnMut(&[&'a RepoPathBuf]) -> Result<(), String>,
) -> Result<(), String> {
    let paths = paths.into_iter().collect::<Vec<_>>();
    if let Some(path) = paths
        .iter()
        .find(|path| path.as_internal_file_string().len() + 1 > MAX_PATH_BYTES_PER_GIT_COMMAND)
    {
        return Err(format!(
            "Git pathspec is too long: {}",
            path.as_internal_file_string()
        ));
    }

    let mut batch = Vec::new();
    let mut bytes = 0;
    for path in paths {
        let path_bytes = path.as_internal_file_string().len() + 1;
        if !batch.is_empty()
            && (batch.len() >= MAX_PATHS_PER_GIT_COMMAND
                || bytes + path_bytes > MAX_PATH_BYTES_PER_GIT_COMMAND)
        {
            run(&batch)?;
            batch.clear();
            bytes = 0;
        }
        batch.push(path);
        bytes += path_bytes;
    }
    if !batch.is_empty() {
        run(&batch)?;
    }
    Ok(())
}

fn git_pathspec(path: &RepoPath) -> &str {
    if path.is_root() {
        "."
    } else {
        path.as_internal_file_string()
    }
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
    if path.is_root() {
        return "/*".to_owned();
    }
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
        // Index paths are exact repo paths, never patterns. Literal pathspecs
        // keep names starting with `:` or containing glob characters from
        // being parsed as pathspec magic.
        .env("GIT_LITERAL_PATHSPECS", "1")
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

struct CheckoutLock {
    file: fs::File,
}

impl CheckoutLock {
    fn acquire(checkout_root: &Path) -> Result<Self, String> {
        let path = checkout_root.join(".jj").join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("open {}: {error}", path.display()))?;
        file.lock()
            .map_err(|error| format!("lock {}: {error}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for CheckoutLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

struct GitDirGuard<'a> {
    checkout_root: &'a Path,
    active: bool,
}

impl<'a> GitDirGuard<'a> {
    fn new(checkout_root: &'a Path) -> Self {
        Self {
            checkout_root,
            active: true,
        }
    }

    fn acquire(checkout_root: &'a Path) -> Result<Self, String> {
        let guard = Self::new(checkout_root);
        make_git_dirs_writable(checkout_root)?;
        Ok(guard)
    }

    fn finish(mut self) -> Result<(), String> {
        make_git_dirs_read_only(self.checkout_root)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for GitDirGuard<'_> {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = make_git_dirs_read_only(self.checkout_root)
        {
            tracing::warn!(
                checkout = %self.checkout_root.display(),
                "failed to restore Git index shim guard: {error}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_batches_respect_count_limit() {
        let paths = (0..1_000)
            .map(|index| {
                RepoPathBuf::from_internal_string(format!("ignored/{index:04}-{}", "x".repeat(200)))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let mut batch_sizes = Vec::new();
        for_path_batches(paths.iter(), |batch| {
            batch_sizes.push(batch.len());
            Ok(())
        })
        .unwrap();

        assert_eq!(batch_sizes.iter().sum::<usize>(), paths.len());
        assert_eq!(batch_sizes[0], MAX_PATHS_PER_GIT_COMMAND);
        assert!(
            batch_sizes
                .iter()
                .all(|size| *size <= MAX_PATHS_PER_GIT_COMMAND)
        );
    }

    #[test]
    fn path_batches_split_at_byte_limit_before_count_limit() {
        let paths = (0..300)
            .map(|index| {
                RepoPathBuf::from_internal_string(format!(
                    "ignored/{}/{index:04}-{}",
                    "x".repeat(200),
                    "y".repeat(200)
                ))
                .unwrap()
            })
            .collect::<Vec<_>>();
        let mut batches = Vec::new();
        for_path_batches(paths.iter(), |batch| {
            batches.push((
                batch.len(),
                batch
                    .iter()
                    .map(|path| path.as_internal_file_string().len() + 1)
                    .sum::<usize>(),
            ));
            Ok(())
        })
        .unwrap();

        assert_eq!(
            batches.iter().map(|(count, _)| count).sum::<usize>(),
            paths.len()
        );
        assert!(batches.len() > 1);
        assert!(
            batches
                .iter()
                .all(|(count, bytes)| *count < MAX_PATHS_PER_GIT_COMMAND
                    && *bytes <= MAX_PATH_BYTES_PER_GIT_COMMAND)
        );
        let next_path_bytes = paths[batches[0].0].as_internal_file_string().len() + 1;
        assert!(batches[0].1 + next_path_bytes > MAX_PATH_BYTES_PER_GIT_COMMAND);
    }
}
