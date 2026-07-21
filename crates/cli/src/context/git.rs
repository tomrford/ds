//! Git plumbing for the context store: bare remote caches, snapshot
//! materialization, and input validation. All transport goes through the
//! user's `git` CLI so private-repo auth works unchanged.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use super::{Context as _, Result, ensure_dir_mode, run_command, unique_path};

#[derive(Clone, Debug)]
pub struct Git {
    program: OsString,
}

#[derive(Clone, Debug)]
pub enum ResolveSpec {
    DefaultBranch,
    Ref(String),
}

impl Git {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// Stable content key via `git hash-object` (used for cache paths).
    pub fn hash_string(&self, value: &str) -> Result<String> {
        let args = vec![OsString::from("hash-object"), OsString::from("--stdin")];
        let output = run_command(&self.program, &args, Some(value.as_bytes()))?.check()?;
        Ok(output.stdout.trim().to_string())
    }

    pub fn ensure_remote_cache(&self, remote_dir: &Path, url: &str) -> Result<()> {
        let parent = remote_dir.parent().with_context(|| {
            format!("remote cache path has no parent: {}", remote_dir.display())
        })?;
        ensure_dir_mode(parent, 0o700)?;

        if remote_dir.exists() {
            if self.remote_origin_matches(remote_dir, url)? {
                return Ok(());
            }
            remove_path(remote_dir)?;
        }

        let temp_remote_dir = unique_path(parent, ".devspace-remote");
        if let Err(error) = self.initialize_remote_cache(&temp_remote_dir, url) {
            let _ = remove_path(&temp_remote_dir);
            return Err(error);
        }
        fs::rename(&temp_remote_dir, remote_dir).with_context(|| {
            format!(
                "move remote cache into place {} -> {}",
                temp_remote_dir.display(),
                remote_dir.display()
            )
        })?;
        Ok(())
    }

    pub fn resolve_spec(&self, remote_dir: &Path, spec: ResolveSpec) -> Result<String> {
        match spec {
            ResolveSpec::DefaultBranch => self.fetch_default_head(remote_dir),
            ResolveSpec::Ref(ref_name) => self.fetch_ref(remote_dir, &ref_name),
        }
    }

    pub fn ensure_commit_available(&self, remote_dir: &Path, commit: &str) -> Result<()> {
        if self.has_commit(remote_dir, commit)? {
            return Ok(());
        }
        let fetch_args = self.git_dir_args(
            remote_dir,
            [
                OsString::from("fetch"),
                OsString::from("--no-tags"),
                OsString::from("origin"),
                OsString::from(commit),
            ],
        );
        run_command(&self.program, &fetch_args, None)?.check()?;
        Ok(())
    }

    /// Check out `commit` into a fresh tree with `.git` stripped, then move
    /// it (or `subdir` of it) into `target_dir`.
    pub fn materialize_snapshot(
        &self,
        remote_dir: &Path,
        commit: &str,
        target_dir: &Path,
        subdir: Option<&str>,
    ) -> Result<()> {
        let parent = target_dir
            .parent()
            .with_context(|| format!("snapshot path has no parent: {}", target_dir.display()))?;
        ensure_dir_mode(parent, 0o700)?;
        let temp_checkout = unique_path(parent, ".devspace-checkout");

        let clone_args = self.base_args([
            OsString::from("clone"),
            OsString::from("--shared"),
            OsString::from("--no-checkout"),
            OsString::from("--no-tags"),
            remote_dir.as_os_str().to_os_string(),
            temp_checkout.as_os_str().to_os_string(),
        ]);
        run_command(&self.program, &clone_args, None)?.check()?;

        let checkout_args = self.base_args([
            OsString::from("-C"),
            temp_checkout.as_os_str().to_os_string(),
            OsString::from("checkout"),
            OsString::from("--detach"),
            OsString::from("--force"),
            OsString::from(commit),
        ]);
        if let Err(error) = run_command(&self.program, &checkout_args, None)?.check() {
            let _ = remove_path(&temp_checkout);
            return Err(error);
        }

        let git_dir = temp_checkout.join(".git");
        fs::remove_dir_all(&git_dir)
            .with_context(|| format!("strip .git from {}", temp_checkout.display()))?;

        let final_source = match subdir {
            Some(sub) => {
                validate_subdir(sub)?;
                let sub_path = temp_checkout.join(sub);
                if !sub_path.is_dir() {
                    let _ = remove_path(&temp_checkout);
                    bail!("subdir {sub} is not a directory at commit {commit}");
                }
                sub_path
            }
            None => temp_checkout.clone(),
        };

        fs::rename(&final_source, target_dir).with_context(|| {
            format!(
                "move snapshot into place {} -> {}",
                final_source.display(),
                target_dir.display()
            )
        })?;

        if subdir.is_some() {
            let _ = remove_path(&temp_checkout);
        }
        Ok(())
    }

    fn initialize_remote_cache(&self, remote_dir: &Path, url: &str) -> Result<()> {
        let init_args = self.base_args([
            OsString::from("init"),
            OsString::from("--bare"),
            remote_dir.as_os_str().to_os_string(),
        ]);
        run_command(&self.program, &init_args, None)?.check()?;
        ensure_dir_mode(remote_dir, 0o700)?;

        let add_remote_args = self.git_dir_args(
            remote_dir,
            [
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                OsString::from(url),
            ],
        );
        run_command(&self.program, &add_remote_args, None)?.check()?;
        Ok(())
    }

    fn remote_origin_matches(&self, remote_dir: &Path, expected_url: &str) -> Result<bool> {
        let args = self.git_dir_args(
            remote_dir,
            [
                OsString::from("config"),
                OsString::from("--get"),
                OsString::from("remote.origin.url"),
            ],
        );
        let output = run_command(&self.program, &args, None)?;
        if !output.status.success() {
            return Ok(false);
        }
        Ok(output.stdout.trim() == expected_url)
    }

    fn fetch_default_head(&self, remote_dir: &Path) -> Result<String> {
        let fetch_args = self.git_dir_args(
            remote_dir,
            [
                OsString::from("fetch"),
                OsString::from("--prune"),
                OsString::from("--no-tags"),
                OsString::from("origin"),
                OsString::from("+HEAD:refs/heads/devspace-head"),
            ],
        );
        run_command(&self.program, &fetch_args, None)?.check()?;

        let rev_parse_args = self.git_dir_args(
            remote_dir,
            [
                OsString::from("rev-parse"),
                OsString::from("refs/heads/devspace-head"),
            ],
        );
        let output = run_command(&self.program, &rev_parse_args, None)?.check()?;
        Ok(output.stdout.trim().to_string())
    }

    fn fetch_ref(&self, remote_dir: &Path, ref_name: &str) -> Result<String> {
        let fetch_args = self.git_dir_args(
            remote_dir,
            [
                OsString::from("fetch"),
                OsString::from("--no-tags"),
                OsString::from("origin"),
                OsString::from(ref_name),
            ],
        );
        run_command(&self.program, &fetch_args, None)?.check()?;

        let rev_parse_args = self.git_dir_args(
            remote_dir,
            [OsString::from("rev-parse"), OsString::from("FETCH_HEAD")],
        );
        let output = run_command(&self.program, &rev_parse_args, None)?.check()?;
        Ok(output.stdout.trim().to_string())
    }

    fn has_commit(&self, remote_dir: &Path, commit: &str) -> Result<bool> {
        let probe = format!("{commit}^{{commit}}");
        let args = self.git_dir_args(
            remote_dir,
            [
                OsString::from("cat-file"),
                OsString::from("-e"),
                OsString::from(probe),
            ],
        );
        let output = run_command(&self.program, &args, None)?;
        Ok(output.status.success())
    }

    /// Hooks are disabled on every invocation: snapshot trees come from
    /// arbitrary remotes and must never execute repo-provided code.
    fn base_args(&self, tail: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("-c"),
            OsString::from("core.hooksPath=/dev/null"),
        ];
        args.extend(tail);
        args
    }

    fn git_dir_args(
        &self,
        git_dir: &Path,
        tail: impl IntoIterator<Item = OsString>,
    ) -> Vec<OsString> {
        let mut args = self.base_args([OsString::from(format!("--git-dir={}", git_dir.display()))]);
        args.extend(tail);
        args
    }
}

pub fn validate_ref_name(ref_name: &str) -> Result<()> {
    let invalid = ref_name.is_empty()
        || ref_name.starts_with('-')
        || ref_name.starts_with('/')
        || ref_name.ends_with('/')
        || ref_name.ends_with('.')
        || ref_name.contains("//")
        || ref_name.contains("..")
        || ref_name.contains("@{")
        || ref_name.as_bytes().iter().any(|byte| {
            byte.is_ascii_control()
                || matches!(
                    *byte,
                    b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\'
                )
        })
        || ref_name.split('/').any(|component| {
            component.is_empty()
                || component.starts_with('.')
                || component.ends_with(".lock")
                || component.ends_with('.')
        });
    if invalid {
        bail!("invalid ref name {ref_name:?}");
    }
    Ok(())
}

pub fn validate_subdir(subdir: &str) -> Result<()> {
    use std::path::Component;
    let path = Path::new(subdir);
    let invalid = subdir.is_empty()
        || path.components().any(|component| match component {
            Component::Normal(part) => part.to_string_lossy().starts_with('.'),
            _ => true,
        });
    if invalid {
        bail!("invalid subdir {subdir:?}");
    }
    Ok(())
}

pub fn validate_commit_oid(commit: &str) -> Result<()> {
    let valid_length = matches!(commit.len(), 40 | 64);
    if !valid_length || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid commit id {commit:?}");
    }
    Ok(())
}

pub(crate) fn remove_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
    }
}
