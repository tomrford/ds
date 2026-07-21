use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::io::Read as _;

use blake2::{Blake2b512, Digest as _};
use devspace_machine::{MachineConfig, RepositoryId, RepositoryIncarnation, encode_lower_hex};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_lib::ref_name::WorkspaceNameBuf;

pub(crate) const CHECKOUT_OWNER_FILE: &str = "devspace-checkout-owner";
const DESTINATION_HASH_BYTES: usize = 12;
const NAMED_WORKSPACE_HASH_BYTES: usize = 6;
const MAX_OWNER_MARKER_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckoutOwner {
    repository_id: String,
    incarnation: String,
    workspace_name: String,
}

impl CheckoutOwner {
    pub(crate) fn new(
        repository_id: impl Into<String>,
        incarnation: impl Into<String>,
        workspace_name: impl Into<String>,
    ) -> Self {
        Self {
            repository_id: repository_id.into(),
            incarnation: incarnation.into(),
            workspace_name: workspace_name.into(),
        }
    }

    pub(crate) fn repository_id(&self) -> &str {
        &self.repository_id
    }

    pub(crate) fn incarnation(&self) -> &str {
        &self.incarnation
    }

    pub(crate) fn workspace_name(&self) -> &str {
        &self.workspace_name
    }

    fn is_valid(&self) -> bool {
        RepositoryId::parse(self.repository_id.clone()).is_ok()
            && RepositoryIncarnation::parse(self.incarnation.clone()).is_ok()
            && !self.workspace_name.is_empty()
    }
}

pub(crate) fn reject_unsupported_global_options(
    command: &CommandHelper,
    subcommand: &str,
) -> Result<(), CommandError> {
    let global = command.global_args();
    if global.no_integrate_operation {
        return Err(user_error(format!(
            "`ds {subcommand}` does not support `--no-integrate-operation`"
        )));
    }
    if global.ignore_working_copy {
        return Err(user_error(format!(
            "`ds {subcommand}` does not support `--ignore-working-copy`"
        )));
    }
    if global.at_operation.is_some() {
        return Err(user_error(format!(
            "`ds {subcommand}` does not support `--at-operation`"
        )));
    }
    Ok(())
}

pub(crate) fn absolute_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

pub(crate) fn ensure_destination_parent(requested: &Path) -> Result<(), CommandError> {
    requested.file_name().ok_or_else(|| {
        user_error(format!(
            "Checkout destination {} has no directory name",
            requested.display()
        ))
    })?;
    let parent = requested.parent().ok_or_else(|| {
        user_error(format!(
            "Checkout destination {} has no parent directory",
            requested.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        user_error(format!(
            "Failed to create checkout parent {}: {error}",
            parent.display()
        ))
    })?;
    Ok(())
}

pub(crate) fn canonical_destination_path(requested: &Path) -> Result<PathBuf, CommandError> {
    let name = requested.file_name().ok_or_else(|| {
        user_error(format!(
            "Checkout destination {} has no directory name",
            requested.display()
        ))
    })?;
    let parent = requested.parent().ok_or_else(|| {
        user_error(format!(
            "Checkout destination {} has no parent directory",
            requested.display()
        ))
    })?;
    let parent = dunce::canonicalize(parent).map_err(|error| {
        user_error(format!(
            "Failed to canonicalize checkout parent {}: {error}",
            parent.display()
        ))
    })?;
    Ok(parent.join(name))
}

pub(crate) fn destination_hash(path: &Path) -> String {
    let encoded = encode_path(path);
    let digest = Blake2b512::digest(encoded.as_bytes());
    encode_lower_hex(&digest[..DESTINATION_HASH_BYTES])
}

pub(crate) fn workspace_name(config: &MachineConfig, path: &Path) -> WorkspaceNameBuf {
    let machine_id = config.machine_id().as_str();
    let Some(machine_name) = config.machine_name() else {
        return WorkspaceNameBuf::from(format!("{machine_id}-{}", destination_hash(path)));
    };
    let encoded_path = encode_path(path);
    let mut hasher = Blake2b512::new();
    hasher.update(machine_id.as_bytes());
    hasher.update(encoded_path.as_bytes());
    let digest = hasher.finalize();
    WorkspaceNameBuf::from(format!(
        "{machine_name}-{}",
        encode_lower_hex(&digest[..NAMED_WORKSPACE_HASH_BYTES])
    ))
}

pub(crate) fn read_checkout_owner(path: &Path) -> Result<CheckoutOwner, String> {
    let owner = read_checkout_owner_impl(path)?
        .ok_or_else(|| "not a Devspace checkout; nothing was touched".to_owned())?;
    if !owner.is_valid() {
        return Err("not a Devspace checkout; nothing was touched".to_owned());
    }
    Ok(owner)
}

pub(crate) fn owned_directory_matches(path: &Path, owner: &CheckoutOwner) -> Result<bool, String> {
    Ok(read_checkout_owner_impl(path)?.is_some_and(|actual| actual == *owner))
}

#[cfg(unix)]
fn read_checkout_owner_impl(path: &Path) -> Result<Option<CheckoutOwner>, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "owned checkout path has no parent".to_owned())?;
    let name = path
        .file_name()
        .ok_or_else(|| "owned checkout path has no file name".to_owned())?;
    let parent = fs::File::open(parent)
        .map_err(|error| format!("failed to open checkout parent: {error}"))?;
    let Some(root) = openat_nofollow(&parent, name, true)? else {
        return Ok(None);
    };
    let Some(jj_dir) = openat_nofollow(&root, std::ffi::OsStr::new(".jj"), true)? else {
        return Ok(None);
    };
    let Some(marker) = openat_nofollow(&jj_dir, std::ffi::OsStr::new(CHECKOUT_OWNER_FILE), false)?
    else {
        return Ok(None);
    };
    let marker = fs::File::from(marker);
    if !marker
        .metadata()
        .map_err(|error| format!("failed to inspect checkout ownership marker: {error}"))?
        .is_file()
    {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    std::io::Read::take(marker, MAX_OWNER_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read checkout ownership marker: {error}"))?;
    if bytes.len() as u64 > MAX_OWNER_MARKER_BYTES {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

#[cfg(unix)]
fn openat_nofollow(
    directory: impl rustix::fd::AsFd,
    name: &std::ffi::OsStr,
    is_directory: bool,
) -> Result<Option<rustix::fd::OwnedFd>, String> {
    let mut flags =
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW;
    if is_directory {
        flags |= rustix::fs::OFlags::DIRECTORY;
    }
    match rustix::fs::openat(directory, name, flags, rustix::fs::Mode::empty()) {
        Ok(file) => Ok(Some(file)),
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
            Ok(None)
        }
        Err(error) => Err(format!(
            "failed to inspect owned checkout component: {error}"
        )),
    }
}

#[cfg(not(unix))]
fn read_checkout_owner_impl(path: &Path) -> Result<Option<CheckoutOwner>, String> {
    // Checkout ownership prevents accidental replacement, not attacks by another local process.
    for component in [path.to_owned(), path.join(".jj")] {
        let metadata = match fs::symlink_metadata(&component) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("failed to inspect checkout ownership: {error}")),
        };
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Ok(None);
        }
    }
    let marker = path.join(".jj").join(CHECKOUT_OWNER_FILE);
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "failed to inspect checkout ownership marker: {error}"
            ));
        }
    };
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_OWNER_MARKER_BYTES
    {
        return Ok(None);
    }
    let bytes = fs::read(&marker)
        .map_err(|error| format!("failed to read checkout ownership marker: {error}"))?;
    Ok(serde_json::from_slice(&bytes).ok())
}

#[cfg(unix)]
fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    format!("unix:{}", encode_lower_hex(path.as_os_str().as_bytes()))
}

#[cfg(windows)]
fn encode_path(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt as _;
    let bytes = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    format!("windows:{}", encode_lower_hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use devspace_machine::{MachineId, SharedSecret};

    fn config(machine_id: &str, machine_name: Option<&str>) -> MachineConfig {
        let config = MachineConfig::new(
            "https://worker.example.test",
            MachineId::parse(machine_id).unwrap(),
            SharedSecret::new("development-secret").unwrap(),
        )
        .unwrap();
        match machine_name {
            Some(machine_name) => config.with_machine_name(machine_name).unwrap(),
            None => config,
        }
    }

    #[test]
    fn unnamed_workspace_keeps_the_full_machine_and_path_hash_form() {
        let machine_id = "12121212121212121212121212121212";
        let path = Path::new("/tmp/checkout");

        assert_eq!(
            workspace_name(&config(machine_id, None), path).as_str(),
            format!("{machine_id}-{}", destination_hash(path))
        );
    }

    #[test]
    fn named_workspace_is_deterministic_and_mixes_the_machine_id() {
        let path = Path::new("/tmp/checkout");
        let first = config("12121212121212121212121212121212", Some("shared-name"));
        let second = config("34343434343434343434343434343434", Some("shared-name"));

        let first_name = workspace_name(&first, path);
        assert_eq!(first_name, workspace_name(&first, path));
        assert!(first_name.as_str().starts_with("shared-name-"));
        assert_eq!(first_name.as_str().len(), "shared-name-".len() + 12);
        assert_ne!(first_name, workspace_name(&second, path));
    }

    #[cfg(unix)]
    #[test]
    fn named_workspace_hash_has_a_stable_vector() {
        let config = config("12121212121212121212121212121212", Some("macbook"));

        assert_eq!(
            workspace_name(&config, Path::new("/tmp/checkout")).as_str(),
            "macbook-28954720fc48"
        );
    }
}
