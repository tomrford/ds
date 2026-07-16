use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::io::Read as _;

use blake2::{Blake2b512, Digest as _};
use devspace_machine::{CatalogEntry, MachineRepository, MachineStore, RepositoryName};
use jj_cli::cli_util::{CommandHelper, RevisionArg};
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo::Repo as _;
use jj_lib::repo::StoreFactories;
use jj_lib::workspace::{Workspace, default_working_copy_factories, default_working_copy_factory};
use jj_lib::workspace_store::{SimpleWorkspaceStore, WorkspaceStore as _};

use crate::bare_workspace::{is_stock_bare_repository, workspace_for_repository};

const CHECKOUT_OWNER_FILE: &str = "devspace-checkout-owner";
const DESTINATION_HASH_BYTES: usize = 12;
const MAX_OWNER_MARKER_BYTES: u64 = 16 * 1024;

#[derive(clap::Args)]
pub(crate) struct AddArgs {
    /// Repository name in the local machine catalog.
    repo: String,

    /// Directory to create the checkout at.
    #[arg(value_hint = clap::ValueHint::DirPath)]
    path: PathBuf,

    /// Base revision, resolved against the local accepted repository heads.
    #[arg(short = 'r', long = "rev", alias = "revision", value_name = "REV")]
    revision: RevisionArg,

    /// Print the checkout identity as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(serde::Serialize)]
struct AddedCheckout<'a> {
    root: &'a Path,
    repo: &'a str,
    workspace_id: &'a str,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct CheckoutOwner {
    repository_id: String,
    incarnation: String,
    workspace_name: String,
}

#[derive(Clone, Copy)]
enum AddOutcome {
    Created,
    Rebuilt,
    AlreadyExists,
}

pub(crate) async fn add_checkout(
    ui: &mut Ui,
    command: &CommandHelper,
    args: AddArgs,
) -> Result<(), CommandError> {
    reject_unsupported_global_options(command)?;
    let requested_path = absolute_path(command.cwd(), &args.path);
    if args.json && requested_path.to_str().is_none() {
        return Err(user_error(
            "`ds add --json` requires a checkout path representable as UTF-8",
        ));
    }
    let name = RepositoryName::parse(args.repo).map_err(|error| user_error(error.to_string()))?;
    let store = MachineStore::platform_default().map_err(|error| user_error(error.to_string()))?;
    let machine = store
        .load_config()
        .map_err(|error| user_error(error.to_string()))?;
    let entry = store
        .resolve(&name)
        .map_err(|error| user_error(error.to_string()))?
        .ok_or_else(|| {
            user_error(format!(
                "Repository `{name}` is not present in this machine store. Cloud first-use is unavailable until production machine enrolment exists."
            ))
        })?;
    if !is_stock_bare_repository(&entry.native_repository_path) {
        return Err(user_error(format!(
            "Repository `{name}` is registered locally, but its native repository is missing or invalid. Cloud first-use is unavailable until production machine enrolment exists."
        )));
    }

    ensure_destination_parent(&requested_path)?;
    let requested_path = canonical_destination_path(&requested_path)?;
    let destination_hash = destination_hash(&requested_path);
    let workspace_name = WorkspaceNameBuf::from(format!(
        "{}-{destination_hash}",
        machine.machine_id().as_str()
    ));
    let owner = CheckoutOwner {
        repository_id: entry.identity.repository_id.as_str().to_owned(),
        incarnation: entry.identity.incarnation.as_str().to_owned(),
        workspace_name: workspace_name.as_str().to_owned(),
    };
    let _destination_guard = store
        .try_lock_checkout_destination(&destination_hash)
        .map_err(|error| user_error(error.to_string()))?;
    let destination_exists = inspect_destination(&requested_path, &owner)?;
    let settings = if destination_exists {
        command.settings().clone()
    } else {
        command.settings_for_new_workspace(ui, &requested_path)?.0
    };
    let (base_commit_id, refresh_source_workspace) =
        resolve_base_revision(ui, command, &entry, &args.revision).await?;
    let repository = MachineRepository::open(&entry.native_repository_path, &settings)
        .await
        .map_err(|error| user_error(error.to_string()))?;
    let current_workspace_commit = repository
        .repo()
        .view()
        .get_wc_commit_id(&workspace_name)
        .cloned();

    let (outcome, operation_id) = match (destination_exists, current_workspace_commit) {
        (true, Some(current_id)) => {
            require_requested_parent(
                repository.repo().as_ref(),
                &workspace_name,
                &current_id,
                &base_commit_id,
            )
            .await
            .map_err(user_error)?;
            record_workspace_destination(
                &entry.native_repository_path,
                &workspace_name,
                &requested_path,
            )
            .map_err(user_error)?;
            (AddOutcome::AlreadyExists, repository.repo().op_id().clone())
        }
        (true, None) => {
            return Err(user_error(format!(
                "Checkout destination {} has this repository's ownership marker, but workspace {} is not registered",
                requested_path.display(),
                workspace_name.as_symbol()
            )));
        }
        (false, Some(current_id)) => {
            let working_copy_commit = require_requested_parent(
                repository.repo().as_ref(),
                &workspace_name,
                &current_id,
                &base_commit_id,
            )
            .await
            .map_err(user_error)?;
            rebuild_checkout(
                &entry,
                &requested_path,
                &destination_hash,
                &workspace_name,
                &owner,
                repository.repo().clone(),
                &working_copy_commit,
            )
            .await
            .map_err(user_error)?;
            (AddOutcome::Rebuilt, repository.repo().op_id().clone())
        }
        (false, None) => {
            let (repo, working_copy_commit) =
                register_workspace(repository.repo(), &workspace_name, &base_commit_id)
                    .await
                    .map_err(user_error)?;
            failpoint("after_workspace_registration");
            let operation_id = repo.op_id().clone();
            rebuild_checkout(
                &entry,
                &requested_path,
                &destination_hash,
                &workspace_name,
                &owner,
                repo,
                &working_copy_commit,
            )
            .await
            .map_err(user_error)?;
            (AddOutcome::Created, operation_id)
        }
    };

    if refresh_source_workspace {
        update_source_operation(command, operation_id)
            .await
            .map_err(|error| {
                user_error(format!(
                    "Checkout was published, but the source checkout could not be refreshed ({error}); retry `ds add` to finish recovery"
                ))
            })?;
    }

    let checkout = AddedCheckout {
        root: &requested_path,
        repo: name.as_str(),
        workspace_id: workspace_name.as_str(),
    };
    if args.json {
        serde_json::to_writer_pretty(ui.stdout(), &checkout)
            .map_err(|error| user_error(format!("failed to encode checkout identity: {error}")))?;
        writeln!(ui.stdout())?;
    } else {
        match outcome {
            AddOutcome::Created => writeln!(
                ui.status(),
                "Created workspace {} for `{name}` at {}.",
                workspace_name.as_symbol(),
                requested_path.display()
            )?,
            AddOutcome::Rebuilt => writeln!(
                ui.status(),
                "Rebuilt workspace {} for `{name}` at {}.",
                workspace_name.as_symbol(),
                requested_path.display()
            )?,
            AddOutcome::AlreadyExists => writeln!(
                ui.status(),
                "Workspace {} for `{name}` already exists at {}.",
                workspace_name.as_symbol(),
                requested_path.display()
            )?,
        }
    }
    Ok(())
}

fn reject_unsupported_global_options(command: &CommandHelper) -> Result<(), CommandError> {
    let global = command.global_args();
    if global.no_integrate_operation {
        return Err(user_error(
            "`ds add` does not support `--no-integrate-operation`",
        ));
    }
    if global.ignore_working_copy {
        return Err(user_error(
            "`ds add` does not support `--ignore-working-copy`",
        ));
    }
    if global.at_operation.is_some() {
        return Err(user_error("`ds add` does not support `--at-operation`"));
    }
    Ok(())
}

fn absolute_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

fn ensure_destination_parent(requested: &Path) -> Result<(), CommandError> {
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

fn canonical_destination_path(requested: &Path) -> Result<PathBuf, CommandError> {
    let name = requested
        .file_name()
        .expect("destination name was checked before canonicalization");
    let parent = requested
        .parent()
        .expect("destination parent was checked before canonicalization");
    let parent = dunce::canonicalize(parent).map_err(|error| {
        user_error(format!(
            "Failed to canonicalize checkout parent {}: {error}",
            parent.display()
        ))
    })?;
    Ok(parent.join(name))
}

async fn resolve_base_revision(
    ui: &Ui,
    command: &CommandHelper,
    entry: &CatalogEntry,
    revision: &RevisionArg,
) -> Result<(CommitId, bool), CommandError> {
    if matches!(revision.as_ref(), "@" | "@-" | "@+") {
        if command.workspace_loader().is_err() {
            return Err(unavailable_at_revision(&entry.name, revision));
        }
        let workspace = command.workspace_helper(ui).await?;
        let source_repo = dunce::canonicalize(workspace.repo_path()).map_err(|error| {
            user_error(format!(
                "Failed to resolve current checkout repository: {error}"
            ))
        })?;
        let target_repo = dunce::canonicalize(&entry.native_repository_path).map_err(|error| {
            user_error(format!(
                "Failed to resolve machine repository for `{}`: {error}",
                entry.name
            ))
        })?;
        if source_repo != target_repo {
            return Err(unavailable_at_revision(&entry.name, revision));
        }
        return Ok((
            workspace
                .resolve_single_rev(ui, revision)
                .await?
                .id()
                .clone(),
            true,
        ));
    }

    let repository = MachineRepository::open(&entry.native_repository_path, command.settings())
        .await
        .map_err(|error| user_error(error.to_string()))?;
    let repo = repository.repo().clone();
    let workspace = workspace_for_repository(entry.native_repository_path.clone(), repo.clone());
    let helper = command.for_workable_repo(ui, workspace, repo)?;
    Ok((
        helper.resolve_single_rev(ui, revision).await?.id().clone(),
        false,
    ))
}

fn unavailable_at_revision(name: &RepositoryName, revision: &RevisionArg) -> CommandError {
    user_error(format!(
        "Revision {revision:?} only resolves inside a checkout of `{name}`; use a bookmark, commit ID, or `<workspace-id>@` revision instead"
    ))
}

async fn register_workspace(
    repo: &std::sync::Arc<jj_lib::repo::ReadonlyRepo>,
    workspace_name: &WorkspaceName,
    base_commit_id: &CommitId,
) -> Result<(std::sync::Arc<jj_lib::repo::ReadonlyRepo>, Commit), String> {
    let base_commit = repo
        .store()
        .get_commit_async(base_commit_id)
        .await
        .map_err(|error| format!("failed to load checkout base commit: {error}"))?;
    let mut transaction = repo.start_transaction();
    let working_copy_commit = transaction
        .repo_mut()
        .new_commit(vec![base_commit.id().clone()], base_commit.tree())
        .write()
        .await
        .map_err(|error| format!("failed to create checkout working-copy commit: {error}"))?;
    transaction
        .repo_mut()
        .edit(workspace_name.to_owned(), &working_copy_commit)
        .await
        .map_err(|error| format!("failed to register checkout commit: {error}"))?;
    transaction
        .repo_mut()
        .rebase_descendants()
        .await
        .map_err(|error| format!("failed to rebase checkout descendants: {error}"))?;
    let repo = transaction
        .commit(format!(
            "create initial working-copy commit in workspace {}",
            workspace_name.as_symbol()
        ))
        .await
        .map_err(|error| format!("failed to publish checkout registration: {error}"))?;
    Ok((repo, working_copy_commit))
}

async fn require_requested_parent(
    repo: &jj_lib::repo::ReadonlyRepo,
    workspace_name: &WorkspaceName,
    current_id: &CommitId,
    requested_base: &CommitId,
) -> Result<Commit, String> {
    let current = repo
        .store()
        .get_commit_async(current_id)
        .await
        .map_err(|error| {
            format!(
                "failed to load current position of workspace {}: {error}",
                workspace_name.as_symbol()
            )
        })?;
    if current.parent_ids() == [requested_base.clone()] {
        return Ok(current);
    }
    let parents = current
        .parent_ids()
        .iter()
        .map(|id| id.hex())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "workspace {} is registered at working-copy commit {}, whose parent position is [{}], not requested base {}; pass the matching parent commit to `--revision`",
        workspace_name.as_symbol(),
        current.id().hex(),
        parents,
        requested_base.hex()
    ))
}

async fn rebuild_checkout(
    entry: &CatalogEntry,
    destination: &Path,
    destination_hash: &str,
    workspace_name: &WorkspaceName,
    owner: &CheckoutOwner,
    repo: std::sync::Arc<jj_lib::repo::ReadonlyRepo>,
    working_copy_commit: &Commit,
) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| "checkout destination has no parent directory".to_owned())?;
    let staging = parent.join(format!(".devspace-staging-{destination_hash}"));
    create_owned_staging(&staging, owner)?;
    let repository_path = dunce::canonicalize(&entry.native_repository_path)
        .map_err(|error| format!("failed to resolve machine repository path: {error}"))?;
    ensure_repository_pointer(&staging, &repository_path)?;
    let mut workspace =
        initialize_working_copy(&staging, repo.as_ref(), workspace_name.to_owned())?;
    workspace
        .check_out(repo.op_id().clone(), None, working_copy_commit)
        .await
        .map_err(|error| format!("failed to materialize checkout files: {error}"))?;
    sync_directory(&staging).map_err(|error| format!("failed to sync staged checkout: {error}"))?;
    failpoint("after_checkout_staging");
    publish_directory_noclobber(&staging, destination)?;
    failpoint("after_final_publication");
    if !owned_directory_matches(destination, owner)? {
        return Err(
            "checkout parent or destination was replaced during atomic publication; no replacement path was modified"
                .to_owned(),
        );
    }
    record_workspace_destination(&repository_path, workspace_name, destination)?;
    Ok(())
}

fn inspect_destination(path: &Path, owner: &CheckoutOwner) -> Result<bool, CommandError> {
    match fs::symlink_metadata(path) {
        Ok(_) if owned_directory_matches(path, owner).map_err(user_error)? => Ok(true),
        Ok(_) => Err(user_error(format!(
            "Checkout destination {} already exists without the matching Devspace ownership marker; existing files and directories are never adopted or replaced",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(user_error(format!(
            "Failed to inspect checkout destination {}: {error}",
            path.display()
        ))),
    }
}

fn create_owned_staging(staging: &Path, owner: &CheckoutOwner) -> Result<(), String> {
    match fs::symlink_metadata(staging) {
        Ok(_) if owned_directory_matches(staging, owner)? => {
            fs::remove_dir_all(staging).map_err(|error| {
                format!(
                    "failed to delete disposable checkout staging at {}: {error}",
                    staging.display()
                )
            })?;
        }
        Ok(_) => {
            return Err(format!(
                "checkout staging path {} already exists without the matching Devspace ownership marker",
                staging.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "failed to inspect checkout staging path {}: {error}",
                staging.display()
            ));
        }
    }
    fs::create_dir(staging)
        .map_err(|error| format!("failed to create checkout staging directory: {error}"))?;
    let jj_dir = staging.join(".jj");
    fs::create_dir(&jj_dir)
        .map_err(|error| format!("failed to create checkout metadata directory: {error}"))?;
    let marker = jj_dir.join(CHECKOUT_OWNER_FILE);
    let mut bytes = serde_json::to_vec_pretty(owner)
        .map_err(|error| format!("failed to encode checkout ownership marker: {error}"))?;
    bytes.push(b'\n');
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .map_err(|error| format!("failed to create checkout ownership marker: {error}"))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("failed to persist checkout ownership marker: {error}"))?;
    sync_directory(&jj_dir)
        .and_then(|()| sync_directory(staging))
        .map_err(|error| format!("failed to sync checkout ownership marker: {error}"))
}

fn ensure_repository_pointer(staging: &Path, repository_path: &Path) -> Result<(), String> {
    let jj_dir = staging.join(".jj");
    let jj_dir_abs = dunce::canonicalize(&jj_dir)
        .map_err(|error| format!("failed to resolve checkout metadata path: {error}"))?;
    let path_to_store = jj_lib::file_util::relative_path(&jj_dir_abs, repository_path);
    let path_to_store = if path_to_store.is_relative() {
        jj_lib::file_util::slash_path(&path_to_store).into_owned()
    } else {
        path_to_store
    };
    let encoded = jj_lib::file_util::path_to_bytes(&path_to_store)
        .map_err(|error| format!("failed to encode machine repository path: {error}"))?;
    let pointer = jj_dir.join("repo");
    let mut pointer_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pointer)
        .map_err(|error| format!("failed to create checkout repository pointer: {error}"))?;
    pointer_file
        .write_all(encoded)
        .and_then(|()| pointer_file.sync_all())
        .map_err(|error| format!("failed to write checkout repository pointer: {error}"))?;
    sync_directory(&jj_dir)
        .map_err(|error| format!("failed to sync checkout repository pointer: {error}"))
}

fn initialize_working_copy(
    staging: &Path,
    repo: &jj_lib::repo::ReadonlyRepo,
    workspace_name: WorkspaceNameBuf,
) -> Result<Workspace, String> {
    let state_path = staging.join(".jj/working_copy");
    fs::create_dir(&state_path)
        .map_err(|error| format!("failed to create checkout working-copy state: {error}"))?;
    let working_copy_factory = default_working_copy_factory();
    let working_copy = working_copy_factory
        .init_working_copy(
            repo.store().clone(),
            staging.to_owned(),
            state_path.clone(),
            repo.op_id().clone(),
            workspace_name,
            repo.settings(),
        )
        .map_err(|error| format!("failed to initialize working-copy state: {error}"))?;
    fs::write(state_path.join("type"), working_copy.name())
        .map_err(|error| format!("failed to write working-copy type: {error}"))?;
    drop(working_copy);
    sync_directory(&state_path)
        .map_err(|error| format!("failed to sync working-copy state: {error}"))?;
    Workspace::load(
        repo.settings(),
        staging,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .map_err(|error| format!("failed to load checkout metadata: {error}"))
}

async fn update_source_operation(
    command: &CommandHelper,
    operation_id: OperationId,
) -> Result<(), String> {
    let mut workspace = command
        .load_workspace()
        .map_err(|error| error.error.to_string())?;
    let locked = workspace
        .start_working_copy_mutation()
        .await
        .map_err(|error| error.to_string())?;
    locked
        .finish(operation_id)
        .await
        .map_err(|error| error.to_string())
}

#[cfg(unix)]
fn owned_directory_matches(path: &Path, owner: &CheckoutOwner) -> Result<bool, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "owned checkout path has no parent".to_owned())?;
    let name = path
        .file_name()
        .ok_or_else(|| "owned checkout path has no file name".to_owned())?;
    let parent = fs::File::open(parent)
        .map_err(|error| format!("failed to open checkout parent: {error}"))?;
    let Some(root) = openat_nofollow(&parent, name, true)? else {
        return Ok(false);
    };
    let Some(jj_dir) = openat_nofollow(&root, std::ffi::OsStr::new(".jj"), true)? else {
        return Ok(false);
    };
    let Some(marker) = openat_nofollow(&jj_dir, std::ffi::OsStr::new(CHECKOUT_OWNER_FILE), false)?
    else {
        return Ok(false);
    };
    let marker = fs::File::from(marker);
    if !marker
        .metadata()
        .map_err(|error| format!("failed to inspect checkout ownership marker: {error}"))?
        .is_file()
    {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    std::io::Read::take(marker, MAX_OWNER_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read checkout ownership marker: {error}"))?;
    if bytes.len() as u64 > MAX_OWNER_MARKER_BYTES {
        return Ok(false);
    }
    Ok(serde_json::from_slice::<CheckoutOwner>(&bytes).is_ok_and(|actual| actual == *owner))
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
fn owned_directory_matches(path: &Path, owner: &CheckoutOwner) -> Result<bool, String> {
    // Checkout creation protects against accidental collisions and stale leftovers, not local
    // adversaries, so non-Unix platforms use best-effort path-based ownership checks.
    for component in [path.to_owned(), path.join(".jj")] {
        let metadata = match fs::symlink_metadata(&component) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("failed to inspect checkout ownership: {error}")),
        };
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Ok(false);
        }
    }
    let marker = path.join(".jj").join(CHECKOUT_OWNER_FILE);
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "failed to inspect checkout ownership marker: {error}"
            ));
        }
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Ok(false);
    }
    if metadata.len() > MAX_OWNER_MARKER_BYTES {
        return Ok(false);
    }
    let bytes = fs::read(&marker)
        .map_err(|error| format!("failed to read checkout ownership marker: {error}"))?;
    Ok(serde_json::from_slice::<CheckoutOwner>(&bytes).is_ok_and(|actual| actual == *owner))
}

fn record_workspace_destination(
    repository_path: &Path,
    workspace_name: &WorkspaceName,
    destination: &Path,
) -> Result<(), String> {
    let repository_path = dunce::canonicalize(repository_path)
        .map_err(|error| format!("failed to resolve machine repository path: {error}"))?;
    SimpleWorkspaceStore::load(&repository_path)
        .and_then(|store| store.add(workspace_name, destination))
        .map_err(|error| format!("failed to record checkout location: {error}"))
}

#[cfg(unix)]
fn publish_directory_noclobber(staging: &Path, destination: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;

    let parent = destination
        .parent()
        .ok_or_else(|| "checkout destination has no parent directory".to_owned())?;
    if staging.parent() != Some(parent) {
        return Err("checkout staging and destination are not siblings".to_owned());
    }
    let parent_file = fs::File::open(parent)
        .map_err(|error| format!("failed to open checkout parent: {error}"))?;
    let handle_metadata = parent_file
        .metadata()
        .map_err(|error| format!("failed to inspect checkout parent handle: {error}"))?;
    let path_metadata = fs::metadata(parent)
        .map_err(|error| format!("failed to inspect checkout parent path: {error}"))?;
    if handle_metadata.dev() != path_metadata.dev() || handle_metadata.ino() != path_metadata.ino()
    {
        return Err("checkout parent was replaced before publication".to_owned());
    }
    rustix::fs::renameat_with(
        &parent_file,
        staging
            .file_name()
            .expect("checkout staging has a file name"),
        &parent_file,
        destination
            .file_name()
            .expect("checkout destination has a file name"),
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(|error| {
        let error = std::io::Error::from_raw_os_error(error.raw_os_error());
        format!(
            "failed to publish checkout at {} without replacing existing data: {error}",
            destination.display()
        )
    })?;
    parent_file
        .sync_all()
        .map_err(|error| format!("failed to sync checkout parent: {error}"))
}

#[cfg(not(unix))]
fn publish_directory_noclobber(staging: &Path, destination: &Path) -> Result<(), String> {
    fs::rename(staging, destination).map_err(|error| {
        format!(
            "failed to publish checkout at {} without replacing existing data: {error}",
            destination.display()
        )
    })?;
    sync_directory(
        destination
            .parent()
            .expect("checkout destination has a parent"),
    )
    .map_err(|error| format!("failed to sync checkout parent: {error}"))
}

fn destination_hash(path: &Path) -> String {
    let encoded = encode_path(path);
    let digest = Blake2b512::digest(encoded.as_bytes());
    hex_bytes(&digest[..DESTINATION_HASH_BYTES])
}

#[cfg(unix)]
fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    format!("unix:{}", hex_bytes(path.as_os_str().as_bytes()))
}

#[cfg(windows)]
fn encode_path(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt as _;
    let bytes = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    format!("windows:{}", hex_bytes(&bytes))
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn failpoint(name: &str) {
    if std::env::var_os("DEVSPACE_TEST_CHECKOUT_FAILPOINT").as_deref()
        != Some(std::ffi::OsStr::new(name))
    {
        return;
    }
    if let Some(path) = std::env::var_os("DEVSPACE_TEST_CHECKOUT_FAILPOINT_READY") {
        let _ = fs::write(path, name);
    }
    if let Some(path) = std::env::var_os("DEVSPACE_TEST_CHECKOUT_FAILPOINT_CONTINUE") {
        while !Path::new(&path).exists() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        return;
    }
    loop {
        std::thread::park();
    }
}
