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

use anyhow::{Context as _, Result, bail};
use devspace_machine::MachineStore;
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::file_util::persist_temp_file;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};

use crate::checkout::{read_checkout_owner, reject_unsupported_global_options};
use git::{Git, ResolveSpec};
use manifest::{GitLockEntry, LockEntry, LockMode, Lockfile};
use store::Store;

pub const PROJECT_DIR: &str = ".repos";
pub(crate) const ALIASES_NOT_IGNORED_WARNING: &str = ".repos/ aliases are not ignored; absolute cache symlinks may be committed (add `.repos/*` and `!.repos/.lock` to .gitignore)";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SyncMessageKind {
    Output,
    Diagnostic,
    Warning,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
    let app = App::from_dir(checkout_root.to_owned()).map_err(context_error)?;
    let warned = app.execute(ui, args.command).map_err(context_error)?;
    app.warn_if_aliases_not_ignored(ui).map_err(context_error)?;
    if warned {
        Err(user_error("context command completed with warnings"))
    } else {
        Ok(())
    }
}

/// Materialize the context lockfile discovered from `cwd` without changing
/// process state. Messages are emitted as each entry completes so callers can
/// choose their destination without capturing a child process's output.
pub(crate) fn sync_at(
    cwd: &Path,
    emit: &mut dyn FnMut(SyncMessage) -> Result<()>,
) -> Result<SyncReport> {
    App::from_dir(cwd.to_owned())?.sync_with(emit, false)
}

pub(crate) fn clear_at(cwd: &Path) -> Result<Vec<SyncMessage>> {
    App::from_dir(cwd.to_owned())?.clear()
}

pub(crate) fn aliases_not_ignored_at(cwd: &Path) -> Result<bool> {
    App::from_dir(cwd.to_owned())?.aliases_not_ignored()
}

impl App {
    fn execute(&self, ui: &mut Ui, cmd: ContextCommand) -> Result<bool> {
        let warned = match cmd {
            ContextCommand::Init => self.init(ui)?,
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
                    ui,
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
            ContextCommand::List => self.list(ui)?,
            ContextCommand::Sync => self.sync(ui)?,
            ContextCommand::Update { aliases } => {
                for alias in &aliases {
                    validate_alias(alias)?;
                }
                self.update(ui, &aliases)?
            }
            ContextCommand::Remove { aliases } => {
                for alias in &aliases {
                    validate_alias(alias)?;
                }
                self.remove(ui, &aliases)?
            }
            ContextCommand::Gc { verbose } => self.gc(ui, verbose)?,
        };
        Ok(warned)
    }
}

/// Emit a warning and remember that the run should exit nonzero.
fn warn(ui: &mut Ui, warned: &mut bool, message: impl std::fmt::Display) -> Result<()> {
    *warned = true;
    writeln!(
        ui.warning_default(),
        "{}",
        redact_url_userinfo(&message.to_string())
    )?;
    Ok(())
}

fn context_error(error: anyhow::Error) -> CommandError {
    user_error(redact_url_userinfo(&format!("{error:#}")))
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
    fn at(cwd: &Path) -> Option<Self> {
        let dir = cwd.join(PROJECT_DIR);
        if !fs::symlink_metadata(&dir).is_ok_and(|metadata| metadata.is_dir()) {
            return None;
        }
        let lock_path = dir.join(".lock");
        fs::symlink_metadata(&lock_path)
            .is_ok_and(|metadata| metadata.is_file())
            .then_some(Self { dir, lock_path })
    }

    fn discover(start: &Path) -> Option<Self> {
        start.ancestors().find_map(Self::at)
    }

    fn create_at(cwd: &Path) -> Result<Self> {
        let dir = cwd.join(PROJECT_DIR);
        match fs::symlink_metadata(&dir) {
            Ok(metadata) if !metadata.is_dir() => {
                bail!("{} exists and is not a directory", dir.display());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", dir.display()));
            }
        }

        let lock_path = dir.join(".lock");
        match fs::symlink_metadata(&lock_path) {
            Ok(metadata) if !metadata.is_file() => {
                bail!("{} exists and is not a regular file", lock_path.display());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_atomic_str(&lock_path, "")?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", lock_path.display()));
            }
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

pub(crate) fn has_project_lock(cwd: &Path) -> bool {
    ProjectRoot::at(cwd).is_some()
}

impl App {
    fn from_dir(cwd: PathBuf) -> Result<Self> {
        let cache_root = match std::env::var_os("DEVSPACE_CACHE_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => platform_cache_directory()
                .context("cannot determine the platform cache directory")?
                .join("devspace"),
        };
        let machine_store = MachineStore::platform_default()?;
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

    fn warn_if_aliases_not_ignored(&self, ui: &mut Ui) -> Result<()> {
        if self.aliases_not_ignored()? {
            writeln!(ui.warning_default(), "{ALIASES_NOT_IGNORED_WARNING}")?;
        }
        Ok(())
    }

    fn aliases_not_ignored(&self) -> Result<bool> {
        let Some(root) = ProjectRoot::discover(&self.cwd) else {
            return Ok(false);
        };
        let Ok(lockfile) = root.load_lockfile() else {
            return Ok(false);
        };
        let aliases = lockfile.aliases();
        if aliases.is_empty() {
            return Ok(false);
        }
        let Some(worktree) = root.dir.parent() else {
            return Ok(false);
        };
        Ok(aliases.into_iter().any(|alias| {
            matches!(
                path_is_ignored(worktree, &root.link_path(&alias)),
                Ok(false)
            )
        }))
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

    fn init(&self, ui: &mut Ui) -> Result<bool> {
        let existed = ProjectRoot::at(&self.cwd).is_some();
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
        writeln!(ui.stdout(), "{status} {}", root.dir.display())?;
        Ok(false)
    }

    fn add(&self, ui: &mut Ui, entry: GitLockEntry, force: bool) -> Result<bool> {
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
        writeln!(
            ui.stdout(),
            "{verb} {} -> {}",
            realized.alias,
            snapshot.display()
        )?;
        Ok(false)
    }

    fn list(&self, ui: &mut Ui) -> Result<bool> {
        let root = self.required_root()?;
        let lockfile = root.load_lockfile()?;
        let mut sections = Vec::new();
        for entry in lockfile.entries() {
            match entry {
                LockEntry::Git(entry) => {
                    let mut lines = vec![format!("[repos.{}]", entry.alias)];
                    lines.push(format!("url = {:?}", redact_url_userinfo(&entry.url)));
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
            writeln!(ui.stdout(), "{}", sections.join("\n\n"))?;
        }
        Ok(false)
    }

    fn sync(&self, ui: &mut Ui) -> Result<bool> {
        let report = self.sync_with(
            &mut |message| {
                match message.kind {
                    SyncMessageKind::Output => writeln!(ui.stdout(), "{}", message.text)?,
                    SyncMessageKind::Diagnostic => writeln!(ui.status(), "{}", message.text)?,
                    SyncMessageKind::Warning => writeln!(ui.warning_default(), "{}", message.text)?,
                }
                Ok(())
            },
            true,
        )?;
        Ok(report.warned)
    }

    fn sync_with(
        &self,
        emit: &mut dyn FnMut(SyncMessage) -> Result<()>,
        prune_leftovers: bool,
    ) -> Result<SyncReport> {
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
                    })?;
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
                        })?;
                    }
                    Err(error) => {
                        warned = true;
                        emit(SyncMessage {
                            kind: SyncMessageKind::Warning,
                            text: redact_url_userinfo(&format!(
                                "failed to sync {alias}: {error:#}"
                            )),
                        })?;
                    }
                },
                Err(error) => {
                    warned = true;
                    emit(SyncMessage {
                        kind: SyncMessageKind::Warning,
                        text: redact_url_userinfo(&format!("failed to sync {alias}: {error:#}")),
                    })?;
                }
            }
        }

        let keep: BTreeSet<String> = lockfile.aliases().into_iter().collect();
        if prune_leftovers {
            self.prune_leftover_links(&root, &keep, emit)?;
        }
        if dirty {
            lockfile.write(&root.lock_path)?;
        }
        store.refresh_root(&self.git, &root.lock_path)?;
        Ok(SyncReport { warned })
    }

    fn clear(&self) -> Result<Vec<SyncMessage>> {
        let Some(root) = ProjectRoot::at(&self.cwd) else {
            return Ok(Vec::new());
        };
        let store = self.prepared_store()?;
        let _lock = store.lock_project_mutation(&self.git, &root.dir)?;
        let lockfile = root.load_lockfile()?;
        let mut messages = Vec::new();

        for entry in lockfile.entries() {
            let LockEntry::Git(entry) = entry else {
                continue;
            };
            let path = root.link_path(&entry.alias);
            if !store::symlink_metadata_if_exists(&path)?
                .is_some_and(|metadata| metadata.file_type().is_symlink())
            {
                continue;
            }
            let Some(commit) = entry.commit.as_deref() else {
                messages.push(SyncMessage {
                    kind: SyncMessageKind::Warning,
                    text: format!(
                        "left {} unchanged before working-copy movement because lock entry {} has no commit; context state may need manual repair",
                        path.display(),
                        entry.alias
                    ),
                });
                continue;
            };
            let expected =
                store.snapshot_path(&self.git, &entry.url, commit, entry.subdir.as_deref())?;
            let actual =
                fs::read_link(&path).with_context(|| format!("read link {}", path.display()))?;
            if actual != expected {
                messages.push(SyncMessage {
                    kind: SyncMessageKind::Warning,
                    text: format!(
                        "left {} unchanged before working-copy movement because its target is not the snapshot recorded by `.repos/.lock`",
                        path.display()
                    ),
                });
                continue;
            }
            store::remove_managed_symlink(&path)?;
            messages.push(SyncMessage {
                kind: SyncMessageKind::Output,
                text: format!("removed {}", path.display()),
            });
        }

        Ok(messages)
    }

    fn update(&self, ui: &mut Ui, aliases: &[String]) -> Result<bool> {
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
                        writeln!(
                            ui.status(),
                            "note: skipped {alias}: source-bearing entry or unsupported backend"
                        )?;
                    }
                    continue;
                }
                None => continue,
            };
            if matches!(entry.mode, LockMode::Exact) {
                if explicit {
                    writeln!(ui.stdout(), "skipped {alias}: exact pin")?;
                }
                continue;
            }
            match self.realize(store, entry.clone(), true) {
                Ok(realized) => match self.apply(&root, &mut lockfile, &realized) {
                    Ok(snapshot) => {
                        if realized != entry {
                            dirty = true;
                        }
                        writeln!(
                            ui.stdout(),
                            "updated {} -> {}",
                            realized.alias,
                            snapshot.display()
                        )?;
                    }
                    Err(error) => {
                        warn(
                            ui,
                            &mut warned,
                            format!("failed to update {alias}: {error:#}"),
                        )?;
                    }
                },
                Err(error) => warn(
                    ui,
                    &mut warned,
                    format!("failed to update {alias}: {error:#}"),
                )?,
            }
        }

        if dirty {
            lockfile.write(&root.lock_path)?;
            store.refresh_root(&self.git, &root.lock_path)?;
        }
        Ok(warned)
    }

    fn remove(&self, ui: &mut Ui, aliases: &[String]) -> Result<bool> {
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
                Ok(()) => writeln!(ui.stdout(), "removed {alias}")?,
                Err(error) => warn(ui, &mut warned, format!("{error:#}"))?,
            }
        }
        Ok(warned)
    }

    fn gc(&self, ui: &mut Ui, verbose: bool) -> Result<bool> {
        let store = self.prepared_store()?;
        let _store_lock = store.lock_mutation()?;
        let report = store.gc(&self.git)?;
        let mut warned = false;

        let total = report.removed_snapshots.len()
            + report.removed_remotes.len()
            + report.removed_roots.len();
        if total > 0 {
            writeln!(
                ui.stdout(),
                "deleted {} snapshot(s), {} remote(s), {} stale root(s)",
                report.removed_snapshots.len(),
                report.removed_remotes.len(),
                report.removed_roots.len()
            )?;
        } else {
            writeln!(ui.stdout(), "gc: nothing to remove")?;
        }
        for warning in &report.warnings {
            warn(ui, &mut warned, warning)?;
        }
        if verbose {
            for path in &report.removed_snapshots {
                writeln!(ui.stdout(), "deleted snapshot {}", path.display())?;
            }
            for path in &report.removed_remotes {
                writeln!(ui.stdout(), "deleted remote {}", path.display())?;
            }
            for path in &report.removed_roots {
                writeln!(ui.stdout(), "deleted stale root {}", path.display())?;
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
    fn prune_leftover_links(
        &self,
        root: &ProjectRoot,
        keep: &BTreeSet<String>,
        emit: &mut dyn FnMut(SyncMessage) -> Result<()>,
    ) -> Result<()> {
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
                emit(SyncMessage {
                    kind: SyncMessageKind::Output,
                    text: format!("removed {}", path.display()),
                })?;
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
        _ => path_is_ignored_without_git(worktree, relative),
    }
}

fn path_is_ignored_without_git(worktree: &Path, relative: &Path) -> Result<bool> {
    let mut ignores = GitIgnoreFile::empty();
    let root_ignore = worktree.join(".gitignore");
    if root_ignore.is_file() {
        ignores = ignores.chain_with_file(RepoPath::root(), root_ignore)?;
    }
    let context_ignore = worktree.join(PROJECT_DIR).join(".gitignore");
    if context_ignore.is_file() {
        let context_path = RepoPathBuf::from_internal_string(PROJECT_DIR.to_owned())?;
        ignores = ignores.chain_with_file(&context_path, context_ignore)?;
    }
    let relative = RepoPathBuf::from_relative_path(relative)?;
    Ok(ignores.matches_file(&relative))
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
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary file in {}", parent.display()))?;
    temp.write_all(contents.as_bytes())
        .with_context(|| format!("write temporary file for {}", path.display()))?;
    persist_temp_file(temp, path)
        .with_context(|| format!("persist temporary file as {}", path.display()))?;
    Ok(())
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
        let stderr = redact_url_userinfo(self.stderr.trim());
        let detail = if stderr.is_empty() {
            format!("status {}", self.status)
        } else {
            stderr
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
            .map(|arg| redact_url_userinfo(&arg.to_string_lossy()))
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

pub(crate) fn redact_url_userinfo(value: &str) -> String {
    let mut redacted = value.to_owned();
    let mut search_from = 0;
    while let Some(relative_scheme_end) = redacted[search_from..].find("://") {
        let authority_start = search_from + relative_scheme_end + 3;
        let authority_end = redacted[authority_start..]
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '/' | '\\' | '?' | '#')
            })
            .map_or(redacted.len(), |offset| authority_start + offset);
        let Some(relative_at) = redacted[authority_start..authority_end].rfind('@') else {
            search_from = authority_end;
            continue;
        };
        let at = authority_start + relative_at;
        redacted.replace_range(authority_start..=at, "");
        search_from = authority_start;
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::redact_url_userinfo;

    #[test]
    fn strips_url_userinfo_from_commands_and_diagnostics() {
        assert_eq!(
            redact_url_userinfo(
                "fatal: unable to access 'https://user:secret@example.test/repository': denied"
            ),
            "fatal: unable to access 'https://example.test/repository': denied"
        );
        assert_eq!(
            redact_url_userinfo("git@example.test:repository"),
            "git@example.test:repository"
        );
    }
}
