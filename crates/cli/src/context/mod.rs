//! `ds context`: project-local read-only reference repos.
//!
//! A trimmed port of Grepo's git-backed core (see `.repos/grepo`). The
//! project directory is `.repos/`; `.repos/.lock` is the tracked source of
//! truth and each `.repos/<alias>` is a generated symlink into a shared
//! cached snapshot (a read-only tree with `.git` stripped). Devspace only
//! owns raw-URL entries created through `ds context add`. Entries with source
//! metadata or non-Git backends are unsupported, preserved defensively, and
//! skipped.
//!
//! Projects must ignore generated `.repos/<alias>` links while keeping
//! `.repos/.lock` tracked, conventionally with `.repos/*` followed by
//! `!.repos/.lock`. Otherwise the absolute, machine-local symlink targets can
//! be snapshotted as ordinary symlink objects. `ds context` warns when a
//! configured alias is not ignored; it does not override project ignore
//! policy.
//!
//! Caches live under the platform cache dir + `devspace/context`, state
//! (gc roots, store locks) under the devspace data dir + `context`. Both
//! remain outside the project checkout.

macro_rules! bail {
    ($($arg:tt)*) => {
        return Err(format!($($arg)*))
    };
}

pub(crate) type Result<T> = std::result::Result<T, String>;

pub(crate) trait Context<T> {
    fn context(self, message: impl std::fmt::Display) -> Result<T>;
    fn with_context(self, message: impl FnOnce() -> String) -> Result<T>;
}

impl<T, E: std::fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, message: impl std::fmt::Display) -> Result<T> {
        self.map_err(|error| format!("{message}: {error}"))
    }

    fn with_context(self, message: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|error| format!("{}: {error}", message()))
    }
}

impl<T> Context<T> for Option<T> {
    fn context(self, message: impl std::fmt::Display) -> Result<T> {
        self.ok_or_else(|| message.to_string())
    }

    fn with_context(self, message: impl FnOnce() -> String) -> Result<T> {
        self.ok_or_else(message)
    }
}

mod git;
mod lock;
mod manifest;
mod store;

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitStatus, Stdio};

use devspace_machine::MachineStore;
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;

use crate::checkout::{read_checkout_owner, reject_unsupported_global_options};
use git::{Git, ResolveSpec};
use manifest::{GitLockEntry, LockEntry, LockMode, Lockfile};
use store::Store;

pub const PROJECT_DIR: &str = ".repos";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SyncMessageKind {
    Output,
    Diagnostic,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SyncMessage {
    pub kind: SyncMessageKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SyncReport {
    pub warned: bool,
}

#[derive(clap::Args)]
pub(crate) struct ContextArgs {
    #[command(subcommand)]
    command: ContextCommand,
}

#[derive(clap::Subcommand)]
/// Pin read-only Git reference repos under .repos/.
enum ContextCommand {
    /// Create .repos/ and an empty lockfile in the current directory.
    Init,
    /// Register an alias and materialize it immediately.
    Add {
        /// Symlink name under .repos/ and lockfile key.
        alias: String,
        /// Git URL (anything `git clone` accepts).
        #[arg(long, value_name = "URL")]
        url: String,
        /// Track a named branch or tag on the remote.
        #[arg(long = "ref", value_name = "REF", conflicts_with = "commit")]
        ref_name: Option<String>,
        /// Pin to an exact commit; `update` leaves it alone.
        #[arg(long, value_name = "COMMIT")]
        commit: Option<String>,
        /// Snapshot only this subdirectory of the source.
        #[arg(long, value_name = "PATH")]
        subdir: Option<String>,
        /// Replace an existing alias.
        #[arg(long)]
        force: bool,
    },
    /// Print the configured aliases and how they track upstream.
    List,
    /// Materialize the commits already recorded in the lockfile.
    Sync,
    /// Advance tracking aliases to their current upstream.
    Update {
        /// Aliases to update; omit to update every tracking alias.
        #[arg(value_name = "ALIAS")]
        aliases: Vec<String>,
    },
    /// Drop aliases from the lockfile and delete their symlinks.
    Remove {
        #[arg(required = true, value_name = "ALIAS")]
        aliases: Vec<String>,
    },
    /// Delete cached snapshots and remotes not referenced by rooted projects.
    Gc {
        /// Print each deleted path, not just the summary.
        #[arg(long)]
        verbose: bool,
    },
}

pub(crate) async fn run_context(
    ui: &mut Ui,
    command: &CommandHelper,
    args: ContextArgs,
) -> std::result::Result<(), CommandError> {
    reject_unsupported_global_options(command, "context")?;
    let workspace = command.workspace_helper_no_snapshot(ui).await?;
    let checkout_root = workspace.workspace_root();
    read_checkout_owner(checkout_root).map_err(|_| {
        user_error("`ds context` commands are available only inside a Devspace checkout.")
    })?;
    let app = App::from_dir(checkout_root.to_owned()).map_err(user_error)?;
    let warned = app.execute(args.command).map_err(user_error)?;
    app.warn_if_aliases_not_ignored();
    if warned {
        Err(user_error("context command completed with warnings"))
    } else {
        Ok(())
    }
}

impl App {
    fn execute(&self, cmd: ContextCommand) -> Result<bool> {
        let warned = match cmd {
            ContextCommand::Init => self.init()?,
            ContextCommand::Add {
                alias,
                url,
                ref_name,
                commit,
                subdir,
                force,
            } => {
                validate_alias(&alias)?;
                if let Some(ref_name) = &ref_name {
                    git::validate_ref_name(ref_name)?;
                }
                if let Some(commit) = &commit {
                    git::validate_commit_oid(commit)?;
                }
                if let Some(subdir) = &subdir {
                    git::validate_subdir(subdir)?;
                }
                let mode = match (&ref_name, &commit) {
                    (Some(ref_name), None) => LockMode::Ref {
                        ref_name: ref_name.clone(),
                    },
                    (None, Some(_)) => LockMode::Exact,
                    (None, None) => LockMode::Default,
                    (Some(_), Some(_)) => unreachable!("clap conflicts_with"),
                };
                self.add(
                    GitLockEntry {
                        alias,
                        url,
                        subdir,
                        mode,
                        commit,
                    },
                    force,
                )?
            }
            ContextCommand::List => self.list()?,
            ContextCommand::Sync => {
                self.sync_with(&mut |message| match message.kind {
                    SyncMessageKind::Output => println!("{}", message.text),
                    SyncMessageKind::Diagnostic => eprintln!("{}", message.text),
                })?
                .warned
            }
            ContextCommand::Update { aliases } => {
                for alias in &aliases {
                    validate_alias(alias)?;
                }
                self.update(&aliases)?
            }
            ContextCommand::Remove { aliases } => {
                for alias in &aliases {
                    validate_alias(alias)?;
                }
                self.remove(&aliases)?
            }
            ContextCommand::Gc { verbose } => self.gc(verbose)?,
        };
        Ok(warned)
    }
}

/// Emit a warning and remember that the run should exit nonzero.
fn warn(warned: &mut bool, message: impl std::fmt::Display) {
    *warned = true;
    eprintln!("warning: {message}");
}

struct App {
    cwd: PathBuf,
    git: Git,
    store: Store,
}

struct ProjectRoot {
    dir: PathBuf,
    lock_path: PathBuf,
}

impl ProjectRoot {
    fn discover(start: &Path) -> Option<Self> {
        for ancestor in start.ancestors() {
            let dir = ancestor.join(PROJECT_DIR);
            let lock_path = dir.join(".lock");
            if lock_path.is_file() {
                return Some(Self { dir, lock_path });
            }
        }
        None
    }

    fn create_at(cwd: &Path) -> Result<Self> {
        let dir = cwd.join(PROJECT_DIR);
        if dir.exists() && !dir.is_dir() {
            bail!("{} exists and is not a directory", dir.display());
        }
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

        let lock_path = dir.join(".lock");
        if !lock_path.exists() {
            write_atomic_str(&lock_path, "")?;
        }
        write_atomic_str(&dir.join(".gitignore"), "*\n!.lock\n")?;
        Ok(Self { dir, lock_path })
    }

    fn load_lockfile(&self) -> Result<Lockfile> {
        Lockfile::load(&self.lock_path)
    }

    fn link_path(&self, alias: &str) -> PathBuf {
        self.dir.join(alias)
    }
}

impl App {
    fn from_dir(cwd: PathBuf) -> Result<Self> {
        let cache_root = match std::env::var_os("DEVSPACE_CACHE_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => platform_cache_directory()
                .context("cannot determine the platform cache directory")?
                .join("devspace"),
        };
        let machine_store = MachineStore::platform_default().map_err(|error| error.to_string())?;
        let store = Store::new(
            cache_root.join("context"),
            machine_store.root().join("context"),
        );
        Ok(Self {
            cwd,
            git: Git::new("git"),
            store,
        })
    }

    fn warn_if_aliases_not_ignored(&self) {
        let Some(root) = ProjectRoot::discover(&self.cwd) else {
            return;
        };
        let Ok(lockfile) = root.load_lockfile() else {
            return;
        };
        if lockfile.aliases().is_empty() {
            return;
        }
        let Some(worktree) = root.dir.parent() else {
            return;
        };
        let path = root.link_path(".devspace-ignore-probe");
        if matches!(path_is_ignored(worktree, &path), Ok(false)) {
            eprintln!(
                "warning: .repos/ aliases are not ignored; absolute cache symlinks may be committed (add `.repos/*` and `!.repos/.lock` to .gitignore)"
            );
        }
    }

    fn required_root(&self) -> Result<ProjectRoot> {
        ProjectRoot::discover(&self.cwd).with_context(|| {
            format!(
                "no {PROJECT_DIR}/.lock found from {}; run `ds context init`",
                self.cwd.display()
            )
        })
    }

    fn prepared_store(&self) -> Result<&Store> {
        self.store.prepare()?;
        Ok(&self.store)
    }

    fn init(&self) -> Result<bool> {
        let existed = self.cwd.join(PROJECT_DIR).join(".lock").is_file();
        let root = ProjectRoot::create_at(&self.cwd)?;
        let store = self.prepared_store()?;
        let _lock = store.lock_project_mutation(&self.git, &root.dir)?;
        let _store_lock = store.lock_mutation()?;
        store.refresh_root(&self.git, &root.lock_path)?;
        let status = if existed {
            "already initialized"
        } else {
            "initialized"
        };
        println!("{status} {}", root.dir.display());
        Ok(false)
    }

    fn add(&self, entry: GitLockEntry, force: bool) -> Result<bool> {
        let root = match ProjectRoot::discover(&self.cwd) {
            Some(root) => root,
            None => ProjectRoot::create_at(&self.cwd)?,
        };
        let store = self.prepared_store()?;
        let _lock = store.lock_project_mutation(&self.git, &root.dir)?;
        let _store_lock = store.lock_mutation()?;

        let mut lockfile = root.load_lockfile()?;
        let existing = lockfile.get(&entry.alias).cloned();
        if existing.is_some() && !force {
            bail!(
                "alias {:?} already exists (use --force to replace)",
                entry.alias
            );
        }

        let realized = self.realize(store, entry, false)?;
        let snapshot = self.apply(&root, &mut lockfile, &realized)?;
        lockfile.write(&root.lock_path)?;
        store.refresh_root(&self.git, &root.lock_path)?;

        let verb = if existing.is_some() {
            "replaced"
        } else {
            "added"
        };
        println!("{verb} {} -> {}", realized.alias, snapshot.display());
        Ok(false)
    }

    fn list(&self) -> Result<bool> {
        let root = self.required_root()?;
        let lockfile = root.load_lockfile()?;
        let mut sections = Vec::new();
        for entry in lockfile.entries() {
            match entry {
                LockEntry::Git(entry) => {
                    let mut lines = vec![format!("[repos.{}]", entry.alias)];
                    lines.push(format!("url = {:?}", entry.url));
                    if let Some(subdir) = &entry.subdir {
                        lines.push(format!("subdir = {subdir:?}"));
                    }
                    match &entry.mode {
                        LockMode::Default => lines.push("mode = \"default\"".to_string()),
                        LockMode::Ref { ref_name } => {
                            lines.push("mode = \"ref\"".to_string());
                            lines.push(format!("ref = {ref_name:?}"));
                        }
                        LockMode::Exact => {
                            lines.push("mode = \"exact\"".to_string());
                            if let Some(commit) = &entry.commit {
                                lines.push(format!("commit = {commit:?}"));
                            }
                        }
                    }
                    sections.push(lines.join("\n"));
                }
                LockEntry::Foreign(entry) => {
                    sections.push(format!(
                        "[repos.{}]\n(unsupported by ds context)",
                        entry.alias
                    ));
                }
            }
        }
        if !sections.is_empty() {
            println!("{}", sections.join("\n\n"));
        }
        Ok(false)
    }

    fn sync_with(&self, emit: &mut dyn FnMut(SyncMessage)) -> Result<SyncReport> {
        let root = self.required_root()?;
        let store = self.prepared_store()?;
        let _lock = store.lock_project_mutation(&self.git, &root.dir)?;
        let _store_lock = store.lock_mutation()?;
        let mut lockfile = root.load_lockfile()?;
        let mut dirty = false;
        let mut warned = false;

        for alias in lockfile.aliases() {
            let entry = match lockfile.get(&alias) {
                Some(LockEntry::Git(entry)) => entry.clone(),
                Some(LockEntry::Foreign(_)) => {
                    emit(SyncMessage {
                        kind: SyncMessageKind::Diagnostic,
                        text: format!(
                            "note: skipped {alias}: source-bearing entry or unsupported backend"
                        ),
                    });
                    continue;
                }
                None => continue,
            };
            match self.realize(store, entry.clone(), false) {
                Ok(realized) => match self.apply(&root, &mut lockfile, &realized) {
                    Ok(snapshot) => {
                        if realized != entry {
                            dirty = true;
                        }
                        emit(SyncMessage {
                            kind: SyncMessageKind::Output,
                            text: format!("synced {} -> {}", realized.alias, snapshot.display()),
                        });
                    }
                    Err(error) => {
                        warned = true;
                        emit(SyncMessage {
                            kind: SyncMessageKind::Diagnostic,
                            text: format!("warning: failed to sync {alias}: {error:#}"),
                        });
                    }
                },
                Err(error) => {
                    warned = true;
                    emit(SyncMessage {
                        kind: SyncMessageKind::Diagnostic,
                        text: format!("warning: failed to sync {alias}: {error:#}"),
                    });
                }
            }
        }

        let keep: BTreeSet<String> = lockfile.aliases().into_iter().collect();
        self.prune_leftover_links(&root, &keep)?;
        if dirty {
            lockfile.write(&root.lock_path)?;
        }
        store.refresh_root(&self.git, &root.lock_path)?;
        Ok(SyncReport { warned })
    }

    fn update(&self, aliases: &[String]) -> Result<bool> {
        let root = self.required_root()?;
        let store = self.prepared_store()?;
        let _lock = store.lock_project_mutation(&self.git, &root.dir)?;
        let _store_lock = store.lock_mutation()?;
        let mut lockfile = root.load_lockfile()?;
        let selected = lockfile.select(aliases)?;
        let explicit = !aliases.is_empty();
        let mut dirty = false;
        let mut warned = false;

        for alias in selected {
            let entry = match lockfile.get(&alias) {
                Some(LockEntry::Git(entry)) => entry.clone(),
                Some(LockEntry::Foreign(_)) => {
                    if explicit {
                        eprintln!(
                            "note: skipped {alias}: source-bearing entry or unsupported backend"
                        );
                    }
                    continue;
                }
                None => continue,
            };
            if matches!(entry.mode, LockMode::Exact) {
                if explicit {
                    println!("skipped {alias}: exact pin");
                }
                continue;
            }
            match self.realize(store, entry.clone(), true) {
                Ok(realized) => match self.apply(&root, &mut lockfile, &realized) {
                    Ok(snapshot) => {
                        if realized != entry {
                            dirty = true;
                        }
                        println!("updated {} -> {}", realized.alias, snapshot.display());
                    }
                    Err(error) => {
                        warn(&mut warned, format!("failed to update {alias}: {error:#}"));
                    }
                },
                Err(error) => warn(&mut warned, format!("failed to update {alias}: {error:#}")),
            }
        }

        if dirty {
            lockfile.write(&root.lock_path)?;
            store.refresh_root(&self.git, &root.lock_path)?;
        }
        Ok(warned)
    }

    fn remove(&self, aliases: &[String]) -> Result<bool> {
        let root = self.required_root()?;
        let store = self.prepared_store()?;
        let _lock = store.lock_project_mutation(&self.git, &root.dir)?;
        let _store_lock = store.lock_mutation()?;
        let mut lockfile = root.load_lockfile()?;
        let selected = lockfile.select(aliases)?;

        for alias in &selected {
            lockfile.remove(alias);
        }
        lockfile.write(&root.lock_path)?;
        store.refresh_root(&self.git, &root.lock_path)?;

        let mut warned = false;
        for alias in &selected {
            match store::remove_managed_symlink(&root.link_path(alias)) {
                Ok(()) => println!("removed {alias}"),
                Err(error) => warn(&mut warned, format!("{error:#}")),
            }
        }
        Ok(warned)
    }

    fn gc(&self, verbose: bool) -> Result<bool> {
        let store = self.prepared_store()?;
        let _store_lock = store.lock_mutation()?;
        let report = store.gc(&self.git)?;
        let mut warned = false;

        let total = report.removed_snapshots.len()
            + report.removed_remotes.len()
            + report.removed_roots.len();
        if total > 0 {
            println!(
                "deleted {} snapshot(s), {} remote(s), {} stale root(s)",
                report.removed_snapshots.len(),
                report.removed_remotes.len(),
                report.removed_roots.len()
            );
        } else {
            println!("gc: nothing to remove");
        }
        for warning in &report.warnings {
            warn(&mut warned, warning);
        }
        if verbose {
            for path in &report.removed_snapshots {
                println!("deleted snapshot {}", path.display());
            }
            for path in &report.removed_remotes {
                println!("deleted remote {}", path.display());
            }
            for path in &report.removed_roots {
                println!("deleted stale root {}", path.display());
            }
        }
        Ok(warned)
    }

    /// Resolve an entry's commit (when tracking) and materialize its
    /// snapshot; returns the possibly-updated entry.
    fn realize(
        &self,
        store: &Store,
        mut entry: GitLockEntry,
        refresh: bool,
    ) -> Result<GitLockEntry> {
        if entry.url.is_empty() {
            bail!("entry has no URL");
        }
        if refresh || entry.commit.is_none() {
            match &entry.mode {
                LockMode::Exact => {
                    if entry.commit.is_none() {
                        bail!("exact pin has no commit recorded");
                    }
                }
                LockMode::Default => {
                    entry.commit =
                        Some(self.resolve_tracking(store, &entry, ResolveSpec::DefaultBranch)?);
                }
                LockMode::Ref { ref_name } => {
                    git::validate_ref_name(ref_name)?;
                    entry.commit = Some(self.resolve_tracking(
                        store,
                        &entry,
                        ResolveSpec::Ref(ref_name.clone()),
                    )?);
                }
            }
        }
        if let Some(commit) = &entry.commit {
            git::validate_commit_oid(commit)?;
        }
        if let Some(subdir) = &entry.subdir {
            git::validate_subdir(subdir)?;
        }
        entry
            .commit
            .as_deref()
            .context("entry has no commit")
            .and_then(|commit| {
                store.ensure_snapshot_for_commit(
                    &self.git,
                    &entry.url,
                    commit,
                    entry.subdir.as_deref(),
                )
            })?;
        Ok(entry)
    }

    fn resolve_tracking(
        &self,
        store: &Store,
        entry: &GitLockEntry,
        spec: ResolveSpec,
    ) -> Result<String> {
        store.with_remote_cache(&self.git, &entry.url, |remote_dir| {
            self.git.resolve_spec(remote_dir, spec)
        })
    }

    /// Point the project symlink at the entry's snapshot and record the
    /// entry in the lockfile. Returns the snapshot path.
    fn apply(
        &self,
        root: &ProjectRoot,
        lockfile: &mut Lockfile,
        entry: &GitLockEntry,
    ) -> Result<PathBuf> {
        let commit = entry.commit.as_deref().context("entry has no commit")?;
        let snapshot =
            self.store
                .snapshot_path(&self.git, &entry.url, commit, entry.subdir.as_deref())?;
        store::replace_symlink(&root.link_path(&entry.alias), &snapshot)?;
        lockfile.upsert(LockEntry::Git(entry.clone()));
        Ok(snapshot)
    }

    /// Remove managed symlinks whose alias is no longer in the lockfile.
    fn prune_leftover_links(&self, root: &ProjectRoot, keep: &BTreeSet<String>) -> Result<()> {
        for path in store::read_dir_paths(&root.dir)? {
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if name.starts_with('.') || !is_valid_alias(name) || keep.contains(name) {
                continue;
            }
            if store::symlink_metadata_if_exists(&path)?
                .is_some_and(|metadata| metadata.file_type().is_symlink())
            {
                store::remove_managed_symlink(&path)?;
                println!("removed {}", path.display());
            }
        }
        Ok(())
    }
}

fn path_is_ignored(worktree: &Path, path: &Path) -> Result<bool> {
    let relative = path.strip_prefix(worktree).with_context(|| {
        format!(
            "{} is outside worktree {}",
            path.display(),
            worktree.display()
        )
    })?;
    let args = vec![
        OsString::from("-c"),
        OsString::from("core.hooksPath=/dev/null"),
        OsString::from("-C"),
        worktree.as_os_str().to_os_string(),
        OsString::from("check-ignore"),
        OsString::from("--no-index"),
        OsString::from("--quiet"),
        OsString::from("--"),
        relative.as_os_str().to_os_string(),
    ];
    let output = run_command(OsStr::new("git"), &args, None)?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => output.check().map(|_| false),
    }
}

// ─── shared helpers ───────────────────────────────────────────────────────────

pub(crate) fn is_valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && !alias.starts_with('.')
        && alias
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

fn validate_alias(alias: &str) -> Result<()> {
    if is_valid_alias(alias) {
        Ok(())
    } else {
        bail!("invalid alias {alias:?}");
    }
}

fn platform_cache_directory() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches"))
    }
    #[cfg(target_os = "windows")]
    {
        Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
            return Some(PathBuf::from(cache));
        }
        Some(PathBuf::from(std::env::var_os("HOME")?).join(".cache"))
    }
}

pub(crate) fn ensure_dir_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("set permissions on {}", path.display()))
}

pub(crate) fn write_atomic_str(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temp = unique_path(parent, ".devspace-write");
    fs::write(&temp, contents).with_context(|| format!("write {}", temp.display()))?;
    fs::rename(&temp, path)
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))
}

pub(crate) fn unique_path(parent: &Path, prefix: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!("{prefix}-{pid}-{nanos}"))
}

pub(crate) struct CommandOutput {
    pub cmd: String,
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn check(self) -> Result<Self> {
        if self.status.success() {
            return Ok(self);
        }
        let stderr = self.stderr.trim();
        let detail = if stderr.is_empty() {
            format!("status {}", self.status)
        } else {
            stderr.to_string()
        };
        bail!("{} failed: {detail}", self.cmd);
    }
}

pub(crate) fn run_command(
    program: &OsStr,
    args: &[OsString],
    stdin_data: Option<&[u8]>,
) -> Result<CommandOutput> {
    let cmd = format!(
        "{} {}",
        program.to_string_lossy(),
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut command = ProcessCommand::new(program);
    command.args(args);
    if stdin_data.is_some() {
        command.stdin(Stdio::piped());
    }
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn().with_context(|| format!("spawn {cmd}"))?;
    if let Some(input) = stdin_data {
        child
            .stdin
            .take()
            .with_context(|| format!("open stdin for {cmd}"))?
            .write_all(input)
            .with_context(|| format!("write stdin for {cmd}"))?;
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("wait for {cmd}"))?;
    Ok(CommandOutput {
        cmd,
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}
