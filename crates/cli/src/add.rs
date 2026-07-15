use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::io::Read as _;

use devspace_machine::{
    CatalogEntry, CheckoutCreationIntent, CheckoutCreationPhase, CheckoutCreationTarget,
    CheckoutCreationToken, MachineRepository, MachineStore, RepositoryName,
};
use jj_cli::cli_util::{CommandHelper, RevisionArg};
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo::Repo as _;
use jj_lib::repo::StoreFactories;
use jj_lib::workspace::{Workspace, default_working_copy_factories, default_working_copy_factory};
use jj_lib::workspace_store::{SimpleWorkspaceStore, WorkspaceStore as _};

use crate::bare_workspace::{is_stock_bare_repository, workspace_for_repository};

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

    let _creation_guard = store
        .lock_checkout_creation()
        .map_err(|error| user_error(error.to_string()))?;
    let existing = store
        .checkout_creation_intent(&requested_path)
        .map_err(|error| user_error(error.to_string()))?;
    if let Some(intent) = &existing
        && (intent.target().repository_name() != &name
            || intent.target().repository_identity() != &entry.identity
            || intent.target().revision() != args.revision.as_ref())
    {
        return Err(user_error(format!(
            "Checkout creation at {} belongs to a different repository or revision intent and will not be adopted",
            requested_path.display()
        )));
    }
    let is_retry = existing.is_some();
    if !is_retry {
        require_absent(&requested_path)?;
    }
    let settings = if is_retry {
        command.settings().clone()
    } else {
        command.settings_for_new_workspace(ui, &requested_path)?.0
    };
    let repository = MachineRepository::open(&entry.native_repository_path, &settings)
        .await
        .map_err(|error| user_error(error.to_string()))?;
    let (base_commit_id, refresh_source_workspace, mut intent) = if let Some(intent) = existing {
        (
            CommitId::new(intent.target().base_commit().to_vec()),
            matches!(args.revision.as_ref(), "@" | "@-" | "@+"),
            intent,
        )
    } else {
        let (base_commit_id, refresh_source_workspace) =
            resolve_base_revision(ui, command, &entry, &args.revision).await?;
        let workspace_name =
            allocate_workspace_name(machine.machine_id().as_str(), repository.repo().as_ref())?;
        let mut token = [0_u8; 16];
        getrandom::fill(&mut token)
            .map_err(|_| user_error("failed to generate a checkout ownership token"))?;
        let target = CheckoutCreationTarget::new(
            requested_path.clone(),
            name.clone(),
            entry.identity.clone(),
            base_commit_id.as_bytes().to_vec(),
            args.revision.as_ref().to_owned(),
        );
        let intent = store
            .begin_checkout_creation(
                target,
                CheckoutCreationToken::new(token),
                workspace_name.as_str().to_owned(),
            )
            .map_err(|error| user_error(error.to_string()))?;
        (base_commit_id, refresh_source_workspace, intent)
    };
    let workspace_name = WorkspaceNameBuf::from(intent.workspace_id().to_owned());
    ensure_destination_parent(&requested_path)?;
    let operation_id = resume_checkout_creation(
        &store,
        &mut intent,
        &entry,
        &settings,
        &base_commit_id,
        &workspace_name,
    )
    .await
    .map_err(user_error)?;
    if refresh_source_workspace {
        update_source_operation(command, operation_id)
            .await
            .map_err(|error| {
                user_error(format!(
                    "Checkout was published, but the source checkout could not be refreshed ({error}); retry `ds add` to finish recovery"
                ))
            })?;
    }
    intent = store
        .advance_checkout_creation(&intent, CheckoutCreationPhase::Complete)
        .map_err(|error| user_error(error.to_string()))?;
    debug_assert_eq!(intent.phase(), CheckoutCreationPhase::Complete);

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
        writeln!(
            ui.status(),
            "Created workspace {} for `{name}` at {}.",
            workspace_name.as_symbol(),
            requested_path.display()
        )?;
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

fn require_absent(path: &Path) -> Result<(), CommandError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(user_error(format!(
            "Checkout destination {} already exists; existing files and directories are never adopted or replaced",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(user_error(format!(
            "Failed to inspect checkout destination {}: {error}",
            path.display()
        ))),
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

fn allocate_workspace_name(
    machine_id: &str,
    repo: &dyn jj_lib::repo::Repo,
) -> Result<WorkspaceNameBuf, CommandError> {
    for _ in 0..8 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|_| user_error("failed to generate a workspace identity"))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let workspace = WorkspaceNameBuf::from(format!("{machine_id}-{suffix}"));
        if repo.view().get_wc_commit_id(&workspace).is_none() {
            return Ok(workspace);
        }
    }
    Err(user_error(
        "failed to allocate a unique workspace identity after 8 attempts",
    ))
}

const CHECKOUT_OWNER_FILE: &str = "devspace-checkout-owner";

async fn resume_checkout_creation(
    store: &MachineStore,
    intent: &mut CheckoutCreationIntent,
    entry: &CatalogEntry,
    settings: &jj_lib::settings::UserSettings,
    base_commit_id: &CommitId,
    workspace_name: &WorkspaceName,
) -> Result<OperationId, String> {
    let destination = intent.target().destination().to_owned();
    let parent = destination
        .parent()
        .ok_or_else(|| "checkout destination has no parent directory".to_owned())?;
    let staging = parent.join(intent.staging_name());
    let token = intent.token().hex();

    if path_exists(&destination) {
        if !checkout_is_owned(&destination, &token)? {
            return Err(format!(
                "Checkout destination {} appeared or was replaced by another process; it was not adopted or modified",
                destination.display()
            ));
        }
        if intent.phase() < CheckoutCreationPhase::Published {
            *intent = store
                .advance_checkout_creation(intent, CheckoutCreationPhase::Published)
                .map_err(|error| error.to_string())?;
        }
    }

    if intent.phase() < CheckoutCreationPhase::Staged {
        ensure_owned_staging(&staging, &token)?;
        failpoint("before_workspace_registration");
        let (mut workspace, working_copy_commit, registered_operation) =
            register_or_resume_workspace(
                store,
                intent,
                &staging,
                entry,
                settings,
                base_commit_id,
                workspace_name,
            )
            .await?;
        failpoint("after_workspace_registration");
        if intent.phase() < CheckoutCreationPhase::WorkspaceRegistered {
            *intent = store
                .advance_checkout_creation(intent, CheckoutCreationPhase::WorkspaceRegistered)
                .map_err(|error| error.to_string())?;
        }
        workspace
            .check_out(registered_operation, None, &working_copy_commit)
            .await
            .map_err(|error| format!("failed to materialize checkout files: {error}"))?;
        sync_directory(&staging)
            .map_err(|error| format!("failed to sync staged checkout: {error}"))?;
        *intent = store
            .advance_checkout_creation(intent, CheckoutCreationPhase::Staged)
            .map_err(|error| error.to_string())?;
        failpoint("after_checkout_staging");
    }

    if intent.phase() < CheckoutCreationPhase::Complete {
        require_exact_registered_workspace(intent, entry, settings, workspace_name).await?;
    }

    if intent.phase() < CheckoutCreationPhase::Published {
        if path_exists(&destination) {
            return Err(format!(
                "Checkout destination {} appeared before publication; it was not adopted or modified",
                destination.display()
            ));
        }
        if !staging_is_owned(&staging, &token)? {
            return Err(format!(
                "Checkout staging path {} is missing or no longer owned by this creation intent",
                staging.display()
            ));
        }
        publish_directory_noclobber(&staging, &destination)?;
        if !checkout_is_owned(&destination, &token)? {
            return Err(
                "checkout parent or destination was replaced during atomic publication; no replacement path was modified"
                    .to_owned(),
            );
        }
        failpoint("after_final_publication");
        *intent = store
            .advance_checkout_creation(intent, CheckoutCreationPhase::Published)
            .map_err(|error| error.to_string())?;
    }

    if !checkout_is_owned(&destination, &token)? {
        return Err(format!(
            "Published checkout {} is missing its ownership marker or was replaced",
            destination.display()
        ));
    }
    record_workspace_destination(
        &entry.native_repository_path,
        workspace_name,
        &staging,
        &destination,
        intent,
    )?;
    let operation_id = MachineRepository::open(&entry.native_repository_path, settings)
        .await
        .map_err(|error| error.to_string())?
        .repo()
        .op_id()
        .clone();
    Ok(operation_id)
}

async fn require_exact_registered_workspace(
    intent: &CheckoutCreationIntent,
    entry: &CatalogEntry,
    settings: &jj_lib::settings::UserSettings,
    workspace_name: &WorkspaceName,
) -> Result<(), String> {
    let expected = intent
        .working_copy_commit()
        .ok_or_else(|| "checkout intent has no planned working-copy commit".to_owned())?;
    let repository = MachineRepository::open(&entry.native_repository_path, settings)
        .await
        .map_err(|error| error.to_string())?;
    let current = repository
        .repo()
        .view()
        .get_wc_commit_id(workspace_name)
        .ok_or_else(|| {
            format!(
                "workspace {} was forgotten while checkout creation was pending; it will not be silently re-registered",
                workspace_name.as_symbol()
            )
        })?;
    if current.as_bytes() != expected {
        return Err(format!(
            "workspace {} moved while checkout creation was pending; it will not be adopted",
            workspace_name.as_symbol()
        ));
    }
    Ok(())
}

async fn register_or_resume_workspace(
    store: &MachineStore,
    intent: &mut CheckoutCreationIntent,
    staging: &Path,
    entry: &CatalogEntry,
    settings: &jj_lib::settings::UserSettings,
    base_commit_id: &CommitId,
    workspace_name: &WorkspaceName,
) -> Result<(Workspace, jj_lib::commit::Commit, OperationId), String> {
    let repository_path = dunce::canonicalize(&entry.native_repository_path)
        .map_err(|error| format!("failed to resolve machine repository path: {error}"))?;
    let repository = MachineRepository::open(&repository_path, settings)
        .await
        .map_err(|error| error.to_string())?;
    let repo = repository.repo();
    let base_commit = repo
        .store()
        .get_commit_async(base_commit_id)
        .await
        .map_err(|error| format!("failed to load checkout base commit: {error}"))?;
    let expected_id = if let Some(id) = intent.working_copy_commit() {
        CommitId::new(id.to_vec())
    } else {
        let mut transaction = repo.start_transaction();
        let commit = transaction
            .repo_mut()
            .new_commit(vec![base_commit.id().clone()], base_commit.tree())
            .write()
            .await
            .map_err(|error| format!("failed to create checkout working-copy commit: {error}"))?;
        let id = commit.id().clone();
        *intent = store
            .record_checkout_working_copy_commit(intent, id.as_bytes().to_vec())
            .map_err(|error| error.to_string())?;
        id
    };
    let working_copy_commit = repo
        .store()
        .get_commit_async(&expected_id)
        .await
        .map_err(|error| format!("failed to load planned checkout commit: {error}"))?;
    if working_copy_commit.parent_ids() != [base_commit_id.clone()]
        || working_copy_commit.tree_ids() != base_commit.tree_ids()
    {
        return Err("planned checkout commit no longer matches its durable base".to_owned());
    }

    ensure_repository_pointer(staging, &repository_path)?;
    let repository = MachineRepository::open(&repository_path, settings)
        .await
        .map_err(|error| error.to_string())?;
    let repo = repository.repo();
    let current_id = repo.view().get_wc_commit_id(workspace_name).cloned();
    let repo = match current_id {
        None => {
            let mut transaction = repo.start_transaction();
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
            transaction
                .commit(format!(
                    "create initial working-copy commit in workspace {}",
                    workspace_name.as_symbol()
                ))
                .await
                .map_err(|error| format!("failed to publish checkout registration: {error}"))?
        }
        Some(current_id) if current_id == expected_id => repo.clone(),
        Some(_) => {
            return Err(format!(
                "workspace {} moved while checkout creation was pending; it will not be adopted",
                workspace_name.as_symbol()
            ));
        }
    };
    let workspace = ensure_working_copy_state(staging, repo.as_ref(), workspace_name.to_owned())?;
    if workspace.workspace_name() != workspace_name {
        return Err("staged checkout belongs to a different workspace identity".to_owned());
    }
    SimpleWorkspaceStore::load(&repository_path)
        .and_then(|store| store.add(workspace.workspace_name(), workspace.workspace_root()))
        .map_err(|error| format!("failed to record staged checkout location: {error}"))?;
    Ok((workspace, working_copy_commit, repo.op_id().clone()))
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
    let expected = jj_lib::file_util::path_to_bytes(&path_to_store)
        .map_err(|error| format!("failed to encode machine repository path: {error}"))?;
    let pointer = jj_dir.join("repo");
    match fs::symlink_metadata(&pointer) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            let actual = fs::read(&pointer)
                .map_err(|error| format!("failed to read checkout repository pointer: {error}"))?;
            if actual != expected {
                return Err("staged checkout points at a different repository".to_owned());
            }
        }
        Ok(_) => return Err("staged checkout repository pointer is not a regular file".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut pointer_file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&pointer)
                .map_err(|error| {
                    format!("failed to create checkout repository pointer: {error}")
                })?;
            pointer_file
                .write_all(expected)
                .and_then(|()| pointer_file.sync_all())
                .map_err(|error| format!("failed to write checkout repository pointer: {error}"))?;
            sync_directory(&jj_dir)
                .map_err(|error| format!("failed to sync checkout repository pointer: {error}"))?;
        }
        Err(error) => {
            return Err(format!(
                "failed to inspect checkout repository pointer: {error}"
            ));
        }
    }
    Ok(())
}

fn ensure_working_copy_state(
    staging: &Path,
    repo: &jj_lib::repo::ReadonlyRepo,
    workspace_name: WorkspaceNameBuf,
) -> Result<Workspace, String> {
    let jj_dir = staging.join(".jj");
    let state_path = jj_dir.join("working_copy");
    match fs::symlink_metadata(&state_path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            let type_metadata = fs::symlink_metadata(state_path.join("type")).map_err(|error| {
                format!("failed to inspect checkout working-copy type: {error}")
            })?;
            if !type_metadata.file_type().is_file() || type_metadata.file_type().is_symlink() {
                return Err("checkout working-copy type is not a regular file".to_owned());
            }
        }
        Ok(_) => return Err("checkout working-copy state is not a directory".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let temporary = create_temporary_directory(&jj_dir, "working-copy")?;
            failpoint("after_working_copy_state_directory_created");
            errorpoint("after_working_copy_state_directory_created")?;
            let working_copy_factory = default_working_copy_factory();
            let working_copy = working_copy_factory
                .init_working_copy(
                    repo.store().clone(),
                    staging.to_owned(),
                    temporary.clone(),
                    repo.op_id().clone(),
                    workspace_name,
                    repo.settings(),
                )
                .map_err(|error| format!("failed to initialize working-copy state: {error}"))?;
            fs::write(temporary.join("type"), working_copy.name())
                .map_err(|error| format!("failed to write working-copy type: {error}"))?;
            drop(working_copy);
            sync_directory(&temporary)
                .map_err(|error| format!("failed to sync temporary working-copy state: {error}"))?;
            rename_directory_noclobber(&temporary, &state_path, None)?;
        }
        Err(error) => {
            return Err(format!(
                "failed to inspect checkout working-copy state: {error}"
            ));
        }
    }
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

fn ensure_owned_staging(staging: &Path, token: &str) -> Result<(), String> {
    if path_exists(staging) {
        return staging_is_owned(staging, token)?.then_some(()).ok_or_else(|| {
            format!(
                "Checkout staging path {} already exists without this intent's ownership marker",
                staging.display()
            )
        });
    }
    let parent = staging
        .parent()
        .ok_or_else(|| "checkout staging path has no parent".to_owned())?;
    let reservation = create_temporary_directory(parent, "checkout-reservation")?;
    failpoint("after_staging_reservation_directory_created");
    errorpoint("after_staging_reservation_directory_created")?;
    let jj_dir = reservation.join(".jj");
    fs::create_dir(&jj_dir)
        .map_err(|error| format!("failed to create owned checkout metadata directory: {error}"))?;
    let marker = jj_dir.join(CHECKOUT_OWNER_FILE);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .map_err(|error| format!("failed to mark checkout staging ownership: {error}"))?;
    file.write_all(token.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("failed to persist checkout staging ownership: {error}"))?;
    sync_directory(&jj_dir)
        .and_then(|()| sync_directory(&reservation))
        .map_err(|error| format!("failed to sync checkout staging reservation: {error}"))?;
    rename_directory_noclobber(&reservation, staging, None)
}

fn staging_is_owned(staging: &Path, token: &str) -> Result<bool, String> {
    owned_directory_matches(staging, token)
}

fn checkout_is_owned(destination: &Path, token: &str) -> Result<bool, String> {
    owned_directory_matches(destination, token)
}

#[cfg(unix)]
fn owned_directory_matches(path: &Path, token: &str) -> Result<bool, String> {
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
    let mut bytes = Vec::with_capacity(token.len() + 1);
    std::io::Read::take(marker, (token.len() + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read checkout ownership marker: {error}"))?;
    Ok(bytes == token.as_bytes())
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
fn owned_directory_matches(path: &Path, token: &str) -> Result<bool, String> {
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
    fs::read(&marker)
        .map(|bytes| bytes == token.as_bytes())
        .map_err(|error| format!("failed to read checkout ownership marker: {error}"))
}

fn record_workspace_destination(
    repository_path: &Path,
    workspace_name: &WorkspaceName,
    staging: &Path,
    destination: &Path,
    intent: &CheckoutCreationIntent,
) -> Result<(), String> {
    let store = SimpleWorkspaceStore::load(repository_path).map_err(|error| error.to_string())?;
    if let Some(existing) = store
        .get_workspace_path(workspace_name)
        .map_err(|error| error.to_string())?
    {
        let existing = if existing.is_absolute() {
            existing
        } else {
            repository_path.join(existing)
        };
        let expected_staging_name = intent.staging_name();
        let expected = existing == destination
            || existing == staging
            || existing
                .file_name()
                .is_some_and(|name| name == std::ffi::OsStr::new(&expected_staging_name));
        if !expected {
            return Err(format!(
                "workspace {} location changed while checkout creation was pending; it will not be replaced",
                workspace_name.as_symbol()
            ));
        }
    }
    store
        .add(workspace_name, destination)
        .map_err(|error| format!("failed to record checkout location: {error}"))
}

#[cfg(unix)]
fn publish_directory_noclobber(staging: &Path, destination: &Path) -> Result<(), String> {
    rename_directory_noclobber(staging, destination, Some("before_directory_rename"))
}

#[cfg(unix)]
fn rename_directory_noclobber(
    staging: &Path,
    destination: &Path,
    failpoint_name: Option<&str>,
) -> Result<(), String> {
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
    if let Some(name) = failpoint_name {
        failpoint(name);
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
    rename_directory_noclobber(staging, destination, None)
}

#[cfg(not(unix))]
fn rename_directory_noclobber(
    staging: &Path,
    destination: &Path,
    _failpoint_name: Option<&str>,
) -> Result<(), String> {
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

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn create_temporary_directory(parent: &Path, purpose: &str) -> Result<PathBuf, String> {
    for _ in 0..8 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|_| format!("failed to generate temporary {purpose} identity"))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = parent.join(format!(".devspace-{purpose}-{suffix}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("failed to create temporary {purpose}: {error}")),
        }
    }
    Err(format!(
        "failed to reserve a unique temporary {purpose} directory"
    ))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn errorpoint(name: &str) -> Result<(), String> {
    if std::env::var_os("DEVSPACE_TEST_CHECKOUT_ERRORPOINT").as_deref()
        == Some(std::ffi::OsStr::new(name))
    {
        Err(format!("injected checkout failure at {name}"))
    } else {
        Ok(())
    }
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
