//! Shared cache of remote clones and read-only snapshots, plus the gc-root
//! registry that lets `gc` find every project lockfile that still pins a
//! snapshot.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::git::Git;
use super::lock::{FileLock, MutationLock};
use super::manifest::{LockEntry, Lockfile};
use super::{Context as _, Result, ensure_dir_mode};

pub struct Store {
    cache_root: PathBuf,
    state_root: PathBuf,
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub removed_snapshots: Vec<PathBuf>,
    pub removed_remotes: Vec<PathBuf>,
    pub removed_roots: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

pub struct StoreMutationLock {
    _lock: FileLock,
}

impl Store {
    pub fn new(cache_root: PathBuf, state_root: PathBuf) -> Self {
        Self {
            cache_root,
            state_root,
        }
    }

    fn snapshots_dir(&self) -> PathBuf {
        self.cache_root.join("snapshots")
    }

    fn remotes_dir(&self) -> PathBuf {
        self.cache_root.join("remotes")
    }

    fn roots_dir(&self) -> PathBuf {
        self.state_root.join("roots")
    }

    fn locks_dir(&self) -> PathBuf {
        self.state_root.join("locks")
    }

    pub fn prepare(&self) -> Result<()> {
        ensure_dir_mode(&self.cache_root, 0o700)?;
        ensure_dir_mode(&self.state_root, 0o700)?;
        ensure_dir_mode(&self.snapshots_dir(), 0o700)?;
        ensure_dir_mode(&self.remotes_dir(), 0o700)?;
        ensure_dir_mode(&self.roots_dir(), 0o700)?;
        ensure_dir_mode(&self.locks_dir(), 0o700)
    }

    pub fn lock_mutation(&self) -> Result<StoreMutationLock> {
        let path = self.locks_dir().join("store.lock");
        let lock = FileLock::try_acquire(&path)?
            .with_context(|| format!("context store is busy: {}", self.state_root.display()))?;
        Ok(StoreMutationLock { _lock: lock })
    }

    /// Serialize mutations of one project's `.repos/`, keyed like the gc
    /// roots. Keeping the lock file here instead of inside the project means
    /// checkouts never carry lock artifacts; the cost is that Grepo, which
    /// locks `.repos/.mutate.lock`, no longer excludes against `ds context`.
    pub fn lock_project_mutation(&self, git: &Git, project_dir: &Path) -> Result<MutationLock> {
        let canonical = project_dir
            .canonicalize()
            .with_context(|| format!("canonicalize {}", project_dir.display()))?;
        let key = git.hash_string(&canonical.display().to_string())?;
        let path = self.locks_dir().join(format!("{key}.mutate.lock"));
        MutationLock::acquire(&path, project_dir)
    }

    pub fn with_remote_cache<T>(
        &self,
        git: &Git,
        url: &str,
        f: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        let remote_key = git.hash_string(url)?;
        self.with_remote_cache_for_key(git, url, &remote_key, f)
    }

    /// Where the snapshot for this (url, commit, subdir) lives, materialized
    /// or not.
    pub fn snapshot_path(
        &self,
        git: &Git,
        url: &str,
        commit: &str,
        subdir: Option<&str>,
    ) -> Result<PathBuf> {
        let remote_key = git.hash_string(url)?;
        let snapshot_key = self.snapshot_key(git, url, commit, subdir)?;
        Ok(self.snapshot_dir_for_keys(&remote_key, &snapshot_key))
    }

    pub fn ensure_snapshot_for_commit(
        &self,
        git: &Git,
        url: &str,
        commit: &str,
        subdir: Option<&str>,
    ) -> Result<PathBuf> {
        let remote_key = git.hash_string(url)?;
        let snapshot_key = self.snapshot_key(git, url, commit, subdir)?;
        let snapshot_dir = self.snapshot_dir_for_keys(&remote_key, &snapshot_key);
        self.with_remote_cache_for_key(git, url, &remote_key, |remote_dir| {
            if snapshot_dir.exists() {
                return Ok(snapshot_dir.clone());
            }
            git.ensure_commit_available(remote_dir, commit)?;
            git.materialize_snapshot(remote_dir, commit, &snapshot_dir, subdir)?;
            make_read_only(&snapshot_dir)?;
            Ok(snapshot_dir)
        })
    }

    /// Register `lock_path` as a gc root (a symlink back to the lockfile).
    pub fn refresh_root(&self, git: &Git, lock_path: &Path) -> Result<PathBuf> {
        let canonical = lock_path
            .canonicalize()
            .with_context(|| format!("canonicalize {}", lock_path.display()))?;
        let root_key = git.hash_string(&canonical.display().to_string())?;
        let root_link = self.roots_dir().join(format!("{root_key}.lock"));
        if root_link.exists() {
            fs::remove_file(&root_link)
                .with_context(|| format!("remove {}", root_link.display()))?;
        }
        symlink(&canonical, &root_link).with_context(|| {
            format!(
                "create symlink {} -> {}",
                root_link.display(),
                canonical.display()
            )
        })?;
        Ok(root_link)
    }

    /// Sweep snapshots and remote caches unreachable from any rooted
    /// lockfile. Foreign entries hold nothing in this store, so they
    /// contribute no reachability.
    pub fn gc(&self, git: &Git) -> Result<GcReport> {
        let mut report = GcReport::default();
        let mut reachable_snapshots = BTreeSet::new();
        let mut reachable_remotes = BTreeSet::new();

        for entry in read_dir_paths(&self.roots_dir())? {
            let metadata = fs::symlink_metadata(&entry)
                .with_context(|| format!("inspect {}", entry.display()))?;
            if !metadata.file_type().is_symlink() {
                continue;
            }
            let lock_path = match fs::canonicalize(&entry) {
                Ok(path) => path,
                Err(_) => {
                    fs::remove_file(&entry)
                        .with_context(|| format!("remove {}", entry.display()))?;
                    report.removed_roots.push(entry);
                    continue;
                }
            };
            let lockfile = match Lockfile::load(&lock_path) {
                Ok(lockfile) => lockfile,
                Err(error) => {
                    report.warnings.push(format!(
                        "skipped rooted lockfile {}: {error:#}",
                        lock_path.display()
                    ));
                    continue;
                }
            };
            for entry in lockfile.entries() {
                let LockEntry::Git(git_entry) = entry else {
                    continue;
                };
                let Some(commit) = &git_entry.commit else {
                    continue;
                };
                let remote_key = git.hash_string(&git_entry.url)?;
                let snapshot_key =
                    self.snapshot_key(git, &git_entry.url, commit, git_entry.subdir.as_deref())?;
                reachable_snapshots.insert(self.snapshot_dir_for_keys(&remote_key, &snapshot_key));
                reachable_remotes.insert(self.remote_dir_for_key(&remote_key));
            }
        }

        for url_dir in read_dir_paths(&self.snapshots_dir())? {
            if !url_dir.is_dir() {
                continue;
            }
            let mut has_remaining_entries = false;
            for snapshot_dir in read_dir_paths(&url_dir)? {
                if !snapshot_dir.is_dir() || reachable_snapshots.contains(&snapshot_dir) {
                    has_remaining_entries = true;
                    continue;
                }
                make_writable(&snapshot_dir)?;
                fs::remove_dir_all(&snapshot_dir)
                    .with_context(|| format!("remove {}", snapshot_dir.display()))?;
                report.removed_snapshots.push(snapshot_dir);
            }
            if !has_remaining_entries {
                fs::remove_dir(&url_dir)
                    .with_context(|| format!("remove {}", url_dir.display()))?;
            }
        }

        for remote_dir in read_dir_paths(&self.remotes_dir())? {
            if !remote_dir.is_dir() || reachable_remotes.contains(&remote_dir) {
                continue;
            }
            fs::remove_dir_all(&remote_dir)
                .with_context(|| format!("remove {}", remote_dir.display()))?;
            report.removed_remotes.push(remote_dir);
        }

        Ok(report)
    }

    fn snapshot_key(
        &self,
        git: &Git,
        url: &str,
        commit: &str,
        subdir: Option<&str>,
    ) -> Result<String> {
        let payload = format!("{url}\n{commit}\n{}", subdir.unwrap_or(""));
        git.hash_string(&payload)
    }

    fn remote_dir_for_key(&self, remote_key: &str) -> PathBuf {
        self.remotes_dir().join(format!("{remote_key}.git"))
    }

    fn snapshot_dir_for_keys(&self, remote_key: &str, snapshot_key: &str) -> PathBuf {
        self.snapshots_dir().join(remote_key).join(snapshot_key)
    }

    fn with_remote_cache_for_key<T>(
        &self,
        git: &Git,
        url: &str,
        remote_key: &str,
        f: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        let _lock = FileLock::acquire(&self.locks_dir().join(format!("remote-{remote_key}.lock")))?;
        let remote_dir = self.remote_dir_for_key(remote_key);
        git.ensure_remote_cache(&remote_dir, url)?;
        f(&remote_dir)
    }
}

pub fn replace_symlink(link_path: &Path, target: &Path) -> Result<()> {
    if let Some(metadata) = symlink_metadata_if_exists(link_path)? {
        if !metadata.file_type().is_symlink() {
            bail!(
                "{} exists and is not a devspace-managed symlink",
                link_path.display()
            );
        }
        fs::remove_file(link_path).with_context(|| format!("remove {}", link_path.display()))?;
    }
    let parent = link_path
        .parent()
        .with_context(|| format!("link path has no parent: {}", link_path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    symlink(target, link_path).with_context(|| {
        format!(
            "create symlink {} -> {}",
            link_path.display(),
            target.display()
        )
    })
}

pub fn remove_managed_symlink(path: &Path) -> Result<()> {
    let Some(metadata) = symlink_metadata_if_exists(path)? else {
        return Ok(());
    };
    if !metadata.file_type().is_symlink() {
        bail!(
            "{} exists and is not a devspace-managed symlink",
            path.display()
        );
    }
    fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
}

fn make_read_only(root: &Path) -> Result<()> {
    rewrite_tree_modes(root, |is_dir, mode| {
        if is_dir || mode & 0o100 != 0 {
            0o500
        } else {
            0o400
        }
    })
}

fn make_writable(root: &Path) -> Result<()> {
    rewrite_tree_modes(root, |is_dir, mode| {
        if is_dir || mode & 0o100 != 0 {
            0o700
        } else {
            0o600
        }
    })
}

fn rewrite_tree_modes(root: &Path, rewrite: impl Fn(bool, u32) -> u32) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let metadata =
            fs::symlink_metadata(&path).with_context(|| format!("inspect {}", path.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(rewrite(file_type.is_dir(), permissions.mode()));
        fs::set_permissions(&path, permissions)
            .with_context(|| format!("set permissions on {}", path.display()))?;
        if file_type.is_dir() {
            for entry in fs::read_dir(&path).with_context(|| format!("read {}", path.display()))? {
                stack.push(
                    entry
                        .with_context(|| format!("read {}", path.display()))?
                        .path(),
                );
            }
        }
    }
    Ok(())
}

pub fn read_dir_paths(path: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
        entries.push(
            entry
                .with_context(|| format!("read {}", path.display()))?
                .path(),
        );
    }
    entries.sort();
    Ok(entries)
}

pub fn symlink_metadata_if_exists(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("inspect {}", path.display())),
    }
}
