use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;

use devspace_machine::{
    CatalogEntry, CommitMapping, FetchReceipt, FetchRef, FetchedGitRef, GitLiftError, GitOid,
    GitProcessEnvironment, GitProjection, HttpTransport, ImportMappings, LiftResult,
    MachineRepository, MachineStore, PackOptions, ProjectionCursor, ProjectionSnapshot,
    ProjectionState, ProjectionTransport, QualifiedRef, RemoteUrl, upload_object_closure,
};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteName, RemoteRefSymbol};
use jj_lib::repo::Repo as _;

use crate::checkout::{read_checkout_owner, reject_unsupported_global_options};
use crate::sync::{LockedSyncRun, run_sync_entry_locked};

use super::projection_sidecar::open_or_create_projection;
use super::push::{
    find_remote, load_projection_snapshot, overlapping_pending, recover_pending_batch,
};

const FAILPOINT_ENV: &str = "DEVSPACE_FAILPOINT";
const AFTER_FETCH_RECORD_FAILPOINT: &str = "after_fetch_record_before_view";
const LOST_FETCH_RECORD_RESPONSE_FAILPOINT: &str = "lost_fetch_record_response_once";

pub(super) async fn fetch_bookmarks(
    ui: &mut Ui,
    command: &CommandHelper,
    bookmark_names: Vec<String>,
    remote_name: String,
) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "git fetch")?;
    validate_requested_bookmarks(&bookmark_names)?;

    let workspace = command.workspace_helper(ui).await?;
    let owner = read_checkout_owner(workspace.workspace_root())
        .map_err(|_| user_error("`ds git fetch` is available only inside a Devspace checkout."))?;
    let store = MachineStore::platform_default().map_err(display_error)?;
    let entry = store
        .entries()
        .map_err(display_error)?
        .into_iter()
        .find(|entry| {
            entry.identity.repository_id.as_str() == owner.repository_id()
                && entry.identity.incarnation.as_str() == owner.incarnation()
        })
        .ok_or_else(|| {
            user_error(
                "repository-not-registered: This checkout's repository is not registered on this machine.",
            )
        })?;
    drop(workspace);

    let sync_guard = match run_sync_entry_locked(&store, &entry, command.settings()).await {
        Ok(LockedSyncRun::Completed(guard)) => guard,
        Ok(LockedSyncRun::AlreadyLocked) => {
            return Err(user_error(format!(
                "Repository `{}` is already being synchronized; retry the fetch after it finishes.",
                entry.name
            )));
        }
        Err(error) => return Err(user_error(error)),
    };

    let repository = MachineRepository::open(&entry.native_repository_path, command.settings())
        .await
        .map_err(display_error)?;
    let config = store.load_config().map_err(display_error)?;
    let incarnation = parse_hex(
        entry.identity.incarnation.as_str(),
        "repository incarnation",
    )?;
    let machine_id = parse_hex(config.machine_id().as_str(), "machine ID")?;
    let journal =
        ProjectionTransport::new(&config, entry.identity.repository_id.as_str(), incarnation)
            .map_err(display_error)?;
    let cloud = HttpTransport::new(&config, entry.identity.repository_id.as_str(), incarnation)
        .map_err(display_error)?;
    let (projection, rebuilt_projection) = open_or_create_projection(
        &store.repository_projection_path(&entry.identity),
        command.settings(),
    )
    .map_err(user_error)?;
    if rebuilt_projection {
        writeln!(
            ui.warning_default(),
            "Rebuilt the local Git projection sidecar after it failed validation."
        )?;
    }

    let runtime = cloud_runtime()?;
    let outcome = runtime
        .block_on(fetch_with_cloud(
            &store,
            &entry,
            &repository,
            &projection,
            journal,
            cloud,
            machine_id,
            &remote_name,
            bookmark_names,
        ))
        .map_err(user_error)?;
    if !outcome.polluted_paths.is_empty() {
        writeln!(
            ui.warning_default(),
            "WARNING: fetched public history conflicts with hidden paths: {}. The public bytes remain on the remote until its history is rewritten externally.",
            outcome.polluted_paths.join(", ")
        )?;
    }
    for line in outcome.lines {
        writeln!(ui.status(), "{line}")?;
    }
    drop(sync_guard);
    Ok(())
}

struct FetchOutcome {
    lines: Vec<String>,
    polluted_paths: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
async fn fetch_with_cloud(
    store: &MachineStore,
    entry: &CatalogEntry,
    repository: &MachineRepository,
    projection: &GitProjection,
    journal: ProjectionTransport,
    mut cloud: HttpTransport,
    machine_id: [u8; 16],
    remote_name: &str,
    mut bookmark_names: Vec<String>,
) -> Result<FetchOutcome, String> {
    let remotes = journal
        .list_remotes()
        .await
        .map_err(|error| error.to_string())?;
    let remote = find_remote(&remotes, remote_name)?;
    let remote_url = RemoteUrl::new(remote.url.clone());
    if bookmark_names.is_empty() {
        bookmark_names =
            devspace_machine::ls_remote_heads(&remote_url, &GitProcessEnvironment::default())
                .map_err(|error| error.to_string())?
                .into_keys()
                .collect();
    }
    validate_requested_bookmarks_text(&bookmark_names)?;
    if bookmark_names.is_empty() {
        return Ok(FetchOutcome {
            lines: Vec::new(),
            polluted_paths: Vec::new(),
        });
    }

    let requested_names = bookmark_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut snapshot = load_projection_snapshot(&journal).await?;
    while let Some(pending) = overlapping_pending(&snapshot, remote_name, &requested_names) {
        recover_pending_batch(
            repository, projection, &journal, &mut cloud, machine_id, remote, &snapshot, &pending,
        )
        .await?;
        snapshot = load_projection_snapshot(&journal).await?;
    }

    let report = devspace_machine::fetch(
        projection.git_repo_path(),
        remote_name,
        &bookmark_names,
        &remote_url,
        &GitProcessEnvironment::default(),
    )
    .map_err(|error| error.to_string())?;
    let cursors = cursors_by_bookmark(&snapshot, remote_name);
    let mut lines = Vec::with_capacity(bookmark_names.len());
    let mut changed = Vec::new();
    for bookmark in &bookmark_names {
        let observed = report
            .heads
            .get(bookmark)
            .copied()
            .ok_or_else(|| format!("Git did not report fetched bookmark `{bookmark}`"))?;
        match cursors.get(bookmark.as_str()).copied() {
            Some(cursor) if cursor.git_oid == observed.0 => {
                lines.push(format!("up to date {bookmark} from {remote_name}"));
            }
            Some(cursor) => {
                lines.push(format!(
                    "fetched {bookmark} from {remote_name}: {} -> {}",
                    short_oid(GitOid(cursor.git_oid)),
                    short_oid(observed)
                ));
                changed.push(FetchedGitRef {
                    remote: remote_name.to_owned(),
                    bookmark: bookmark.clone(),
                    head: observed.0,
                });
            }
            None => {
                lines.push(format!("new bookmark {bookmark} from {remote_name}"));
                changed.push(FetchedGitRef {
                    remote: remote_name.to_owned(),
                    bookmark: bookmark.clone(),
                    head: observed.0,
                });
            }
        }
    }

    let mut polluted_paths = BTreeSet::new();
    if !changed.is_empty() {
        let receipts = snapshot_receipts(&snapshot)?;
        let selection = devspace_machine::select_seeds(projection, &changed, &snapshot, &receipts)
            .await
            .map_err(lift_error)?;
        let mut import_mappings = ImportMappings::default();
        let git_heads = changed
            .iter()
            .map(|fetched| CommitId::new(fetched.head.to_vec()))
            .collect::<Vec<_>>();
        let imported = projection
            .import_reachable_with_stops(
                repository.repo().store(),
                &git_heads,
                selection.stop_set(),
                &mut import_mappings,
            )
            .await
            .map_err(|error| error.to_string())?;
        let lifted =
            devspace_machine::lift_imported(repository, &imported.new_mappings, &selection)
                .await
                .map_err(lift_error)?;
        polluted_paths.extend(lifted.polluted_paths.iter().cloned());
        let fetch_refs =
            assemble_fetch_refs(remote_name, &changed, &snapshot, &selection, &lifted)?;
        let new_receipts = imported
            .new_mappings
            .iter()
            .map(|mapping| {
                Ok(FetchReceipt {
                    git_oid: git_oid(&mapping.git_id)?,
                    public_commit_id: object_id(&mapping.canonical_id)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        upload_heads(
            repository,
            imported
                .new_mappings
                .iter()
                .map(|mapping| mapping.canonical_id.clone())
                .collect(),
            store,
            entry,
            &mut cloud,
        )
        .await?;
        upload_heads(
            repository,
            fetch_refs
                .iter()
                .flat_map(|fetch_ref| fetch_ref.states.iter())
                .map(|state| CommitId::new(state.canonical_commit_id.to_vec()))
                .collect(),
            store,
            entry,
            &mut cloud,
        )
        .await?;

        let fetch_id = new_fetch_id()?;
        record_fetch_with_retry(
            &journal,
            fetch_id,
            machine_id,
            remote_name,
            &fetch_refs,
            &new_receipts,
        )
        .await?;
        if failpoint_enabled(AFTER_FETCH_RECORD_FAILPOINT) {
            std::process::exit(86);
        }
        snapshot = load_projection_snapshot(&journal).await?;
    }

    lines.extend(
        update_view_from_journal(repository, &snapshot, remote_name, &bookmark_names).await?,
    );
    Ok(FetchOutcome {
        lines,
        polluted_paths: polluted_paths
            .into_iter()
            .map(|path| path.as_internal_file_string().to_owned())
            .collect(),
    })
}

async fn record_fetch_with_retry(
    journal: &ProjectionTransport,
    fetch_id: [u8; 16],
    machine_id: [u8; 16],
    remote: &str,
    refs: &[FetchRef],
    receipts: &[FetchReceipt],
) -> Result<(), String> {
    let first = journal
        .record_fetch(fetch_id, machine_id, remote, refs, receipts)
        .await;
    if first.is_ok() && !failpoint_enabled(LOST_FETCH_RECORD_RESPONSE_FAILPOINT) {
        return Ok(());
    }
    journal
        .record_fetch(fetch_id, machine_id, remote, refs, receipts)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn validate_requested_bookmarks(bookmarks: &[String]) -> Result<(), CommandError> {
    validate_requested_bookmarks_text(bookmarks).map_err(user_error)
}

fn validate_requested_bookmarks_text(bookmarks: &[String]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for bookmark in bookmarks {
        QualifiedRef::from_bookmark(bookmark).map_err(|error| error.to_string())?;
        if !seen.insert(bookmark.as_str()) {
            return Err(format!(
                "Bookmark `{bookmark}` was requested more than once."
            ));
        }
    }
    Ok(())
}

fn snapshot_receipts(snapshot: &ProjectionSnapshot) -> Result<ImportMappings, String> {
    ImportMappings::from_rows(snapshot.mappings.iter().map(|mapping| CommitMapping {
        git_id: CommitId::new(mapping.git_oid.to_vec()),
        canonical_id: CommitId::new(mapping.public_commit_id.to_vec()),
    }))
    .map_err(|error| error.to_string())
}

fn assemble_fetch_refs(
    remote: &str,
    changed: &[FetchedGitRef],
    snapshot: &ProjectionSnapshot,
    selection: &devspace_machine::SeedSelection,
    lifted: &LiftResult,
) -> Result<Vec<FetchRef>, String> {
    let cursors = cursors_by_bookmark(snapshot, remote);
    changed
        .iter()
        .map(|fetched| {
            let mut states = lifted
                .states
                .iter()
                .filter(|state| selection.reaches(remote, &fetched.bookmark, &state.git_oid))
                .map(projection_state)
                .collect::<Result<Vec<_>, _>>()?;
            let proposed_state = states
                .iter()
                .position(|state| state.git_oid == fetched.head);
            let proposed_state = if proposed_state.is_some() {
                proposed_state
            } else if snapshot.mappings.iter().rev().any(|mapping| {
                mapping.remote == remote
                    && mapping.bookmark == fetched.bookmark
                    && mapping.git_oid == fetched.head
            }) {
                None
            } else {
                let seed = selection.seed_for(&fetched.head).ok_or_else(|| {
                    format!(
                        "fetched head {} has no lifted or active projection state",
                        GitOid(fetched.head)
                    )
                })?;
                states.push(ProjectionState {
                    git_oid: seed.git_oid,
                    canonical_commit_id: object_id(&seed.canonical_commit_id)?,
                    public_commit_id: object_id(&seed.public_commit_id)?,
                    hidden_set_id: seed.hidden_set_id.to_projection_id(),
                });
                Some(states.len() - 1)
            };
            Ok(FetchRef {
                bookmark: fetched.bookmark.clone(),
                observed_git_oid: fetched.head,
                expected_cursor_oid: cursors
                    .get(fetched.bookmark.as_str())
                    .map(|cursor| cursor.git_oid),
                states,
                proposed_state,
            })
        })
        .collect()
}

fn projection_state(
    state: &devspace_machine::LiftedCommitState,
) -> Result<ProjectionState, String> {
    Ok(ProjectionState {
        git_oid: state.git_oid,
        canonical_commit_id: object_id(&state.canonical_commit_id)?,
        public_commit_id: object_id(&state.public_commit_id)?,
        hidden_set_id: state.hidden_set_id.to_projection_id(),
    })
}

async fn upload_heads(
    repository: &MachineRepository,
    heads: Vec<CommitId>,
    store: &MachineStore,
    entry: &CatalogEntry,
    cloud: &mut HttpTransport,
) -> Result<(), String> {
    let heads = heads.into_iter().collect::<BTreeSet<_>>();
    if heads.is_empty() {
        return Ok(());
    }
    let closure = repository
        .commit_closure(&heads.into_iter().collect::<Vec<_>>())
        .map_err(|error| error.to_string())?;
    upload_object_closure(
        &closure,
        store.repository_packs_path(&entry.identity),
        PackOptions::default(),
        cloud,
    )
    .await
    .map_err(|error| error.to_string())
}

async fn update_view_from_journal(
    repository: &MachineRepository,
    snapshot: &ProjectionSnapshot,
    remote: &str,
    bookmarks: &[String],
) -> Result<Vec<String>, String> {
    let cursors = cursors_by_bookmark(snapshot, remote);
    let mut updates = Vec::new();
    for bookmark in bookmarks {
        let cursor = cursors.get(bookmark.as_str()).ok_or_else(|| {
            format!("projection journal has no cursor for {remote}/{bookmark} after fetch")
        })?;
        let name = RefName::new(bookmark);
        let symbol = RemoteRefSymbol {
            name,
            remote: RemoteName::new(remote),
        };
        let old_remote = repository.repo().view().get_remote_bookmark(symbol).clone();
        let new_target = RefTarget::normal(CommitId::new(cursor.canonical_commit_id.to_vec()));
        if old_remote.target != new_target {
            updates.push((bookmark.clone(), old_remote, new_target));
        }
    }
    if updates.is_empty() {
        return Ok(Vec::new());
    }

    let mut transaction = repository.repo().start_transaction();
    let mut commits = Vec::with_capacity(updates.len());
    for (_, _, target) in &updates {
        let id = target
            .as_normal()
            .expect("journal cursors always select one canonical commit");
        commits.push(
            repository
                .repo()
                .store()
                .get_commit_async(id)
                .await
                .map_err(|error| error.to_string())?,
        );
    }
    transaction
        .repo_mut()
        .index_commits(&commits)
        .await
        .map_err(|error| error.to_string())?;
    transaction
        .repo_mut()
        .add_heads(&commits)
        .await
        .map_err(|error| error.to_string())?;

    let mut lines = Vec::new();
    for (bookmark, old_remote, new_target) in updates {
        let name = RefName::new(&bookmark);
        let symbol = RemoteRefSymbol {
            name,
            remote: RemoteName::new(remote),
        };
        if old_remote.is_tracked() {
            transaction
                .repo_mut()
                .merge_local_bookmark(name, &old_remote.target, &new_target)
                .map_err(|error| error.to_string())?;
            lines.push(format!("bookmark: {bookmark}@{remote} [updated] tracked"));
        }
        let state = if old_remote.is_present() {
            old_remote.state
        } else {
            RemoteRefState::New
        };
        transaction.repo_mut().set_remote_bookmark(
            symbol,
            RemoteRef {
                target: new_target,
                state,
            },
        );
    }
    transaction
        .commit(format!("fetch from {remote}"))
        .await
        .map_err(|error| error.to_string())?;
    Ok(lines)
}

fn cursors_by_bookmark<'a>(
    snapshot: &'a ProjectionSnapshot,
    remote: &str,
) -> BTreeMap<&'a str, &'a ProjectionCursor> {
    snapshot
        .cursors
        .iter()
        .filter(|cursor| cursor.remote == remote)
        .map(|cursor| (cursor.bookmark.as_str(), cursor))
        .collect()
}

fn lift_error(error: GitLiftError) -> String {
    match error {
        GitLiftError::RefRewritten { bookmark, .. } => format!(
            "remote history for {bookmark} was rewritten outside devspace; fetching rewritten history is not supported yet"
        ),
        GitLiftError::AmbiguousSeed { git_oid } => format!(
            "Git object {} has ambiguous private seed lineage",
            GitOid(git_oid)
        ),
        other => other.to_string(),
    }
}

fn object_id(id: &CommitId) -> Result<[u8; 64], String> {
    id.as_bytes()
        .try_into()
        .map_err(|_| format!("canonical commit {id} is not a 64-byte object ID"))
}

fn git_oid(id: &CommitId) -> Result<[u8; 20], String> {
    id.as_bytes()
        .try_into()
        .map_err(|_| format!("Git commit {id} is not a SHA-1 object ID"))
}

fn new_fetch_id() -> Result<[u8; 16], String> {
    let mut id = [0; 16];
    getrandom::fill(&mut id).map_err(|_| "failed to generate a projection fetch ID".to_owned())?;
    Ok(id)
}

fn failpoint_enabled(name: &str) -> bool {
    std::env::var_os(FAILPOINT_ENV).as_deref() == Some(std::ffi::OsStr::new(name))
}

fn short_oid(oid: GitOid) -> String {
    oid.to_string()[..12].to_owned()
}

fn parse_hex<const N: usize>(value: &str, label: &str) -> Result<[u8; N], CommandError> {
    if value.len() != N * 2 {
        return Err(user_error(format!("{label} has an invalid length")));
    }
    let mut bytes = [0; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| user_error(format!("{label} is not lowercase hexadecimal")))?;
    }
    Ok(bytes)
}

fn cloud_runtime() -> Result<tokio::runtime::Runtime, CommandError> {
    tokio::runtime::Runtime::new()
        .map_err(|_| user_error("failed to start the cloud transport runtime"))
}

fn display_error(error: impl std::fmt::Display) -> CommandError {
    user_error(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
    use jj_lib::ref_name::RemoteRefSymbol;
    use jj_lib::settings::UserSettings;
    use std::io::Read as _;
    use std::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn journal_view_update_fast_forwards_conflicts_and_leaves_new_refs_untracked() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("native");
        let repository = MachineRepository::init(&path, &settings()).await.unwrap();
        let root = repository.repo().store().root_commit_id().clone();
        let tree = repository.repo().store().empty_merged_tree();
        let mut transaction = repository.repo().start_transaction();
        let base = transaction
            .repo_mut()
            .new_commit(vec![root], tree.clone())
            .write()
            .await
            .unwrap();
        let remote = transaction
            .repo_mut()
            .new_commit(vec![base.id().clone()], tree.clone())
            .write()
            .await
            .unwrap();
        let local = transaction
            .repo_mut()
            .new_commit(vec![base.id().clone()], tree)
            .write()
            .await
            .unwrap();
        for bookmark in ["ff", "diverged"] {
            let local_target = if bookmark == "ff" {
                base.id().clone()
            } else {
                local.id().clone()
            };
            transaction
                .repo_mut()
                .set_local_bookmark_target(RefName::new(bookmark), RefTarget::normal(local_target));
            transaction.repo_mut().set_remote_bookmark(
                remote_symbol(bookmark),
                RemoteRef {
                    target: RefTarget::normal(base.id().clone()),
                    state: RemoteRefState::Tracked,
                },
            );
        }
        transaction
            .repo_mut()
            .set_local_bookmark_target(RefName::new("new"), RefTarget::normal(base.id().clone()));
        transaction.commit("fixture refs").await.unwrap();
        let remote_id = remote.id().clone();
        let base_id = base.id().clone();
        drop(repository);

        let repository = MachineRepository::open(&path, &settings()).await.unwrap();
        let snapshot = snapshot_with_cursors(["ff", "diverged", "new"], &remote_id);
        let lines = update_view_from_journal(
            &repository,
            &snapshot,
            "origin",
            &["ff".to_owned(), "diverged".to_owned(), "new".to_owned()],
        )
        .await
        .unwrap();
        assert_eq!(
            lines,
            [
                "bookmark: ff@origin [updated] tracked",
                "bookmark: diverged@origin [updated] tracked"
            ]
        );
        drop(repository);

        let repository = MachineRepository::open(&path, &settings()).await.unwrap();
        assert_eq!(
            repository
                .repo()
                .view()
                .get_local_bookmark(RefName::new("ff"))
                .as_normal(),
            Some(&remote_id)
        );
        assert!(
            repository
                .repo()
                .view()
                .get_local_bookmark(RefName::new("diverged"))
                .has_conflict()
        );
        assert_eq!(
            repository
                .repo()
                .view()
                .get_local_bookmark(RefName::new("new"))
                .as_normal(),
            Some(&base_id)
        );
        let new_remote = repository
            .repo()
            .view()
            .get_remote_bookmark(remote_symbol("new"));
        assert_eq!(new_remote.target.as_normal(), Some(&remote_id));
        assert_eq!(new_remote.state, RemoteRefState::New);
    }

    #[tokio::test]
    async fn lost_record_fetch_response_retries_the_exact_random_id_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().unwrap();
            let first = read_request(&mut first_stream);
            drop(first_stream);
            let (mut second_stream, _) = listener.accept().unwrap();
            let second = read_request(&mut second_stream);
            let body = format!(
                r#"{{"fetchId":"{}","activationCursor":1}}"#,
                "22".repeat(16)
            );
            write!(
                second_stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            (first, second)
        });
        let config = devspace_machine::MachineConfig::new(
            format!("http://{address}"),
            devspace_machine::MachineId::parse("11".repeat(16)).unwrap(),
            devspace_machine::SharedSecret::new("fetch-retry-secret").unwrap(),
        )
        .unwrap();
        let journal = ProjectionTransport::new(&config, &"ab".repeat(32), [0xcd; 16]).unwrap();
        let fetch_ref = FetchRef {
            bookmark: "main".to_owned(),
            observed_git_oid: [3; 20],
            expected_cursor_oid: None,
            states: vec![ProjectionState {
                git_oid: [3; 20],
                canonical_commit_id: [4; 64],
                public_commit_id: [5; 64],
                hidden_set_id: None,
            }],
            proposed_state: Some(0),
        };

        record_fetch_with_retry(
            &journal,
            [0x22; 16],
            [0x11; 16],
            "origin",
            &[fetch_ref],
            &[FetchReceipt {
                git_oid: [3; 20],
                public_commit_id: [5; 64],
            }],
        )
        .await
        .unwrap();

        let (first, second) = server.join().unwrap();
        assert_eq!(request_body(&first), request_body(&second));
        assert!(request_body(&second).contains(&"22".repeat(16)));
    }

    fn snapshot_with_cursors<const N: usize>(
        bookmarks: [&str; N],
        target: &CommitId,
    ) -> ProjectionSnapshot {
        ProjectionSnapshot {
            activation_cursor: N as u64,
            cursors: bookmarks
                .into_iter()
                .enumerate()
                .map(|(index, bookmark)| ProjectionCursor {
                    remote: "origin".to_owned(),
                    bookmark: bookmark.to_owned(),
                    git_oid: [index as u8 + 1; 20],
                    canonical_commit_id: target.as_bytes().try_into().unwrap(),
                    public_commit_id: [index as u8 + 1; 64],
                    hidden_set_id: None,
                    activation_sequence: index as u64 + 1,
                })
                .collect(),
            mappings: Vec::new(),
            next_after: N as u64,
            through: N as u64,
            has_more: false,
            pending: Vec::new(),
        }
    }

    fn remote_symbol(bookmark: &str) -> RemoteRefSymbol<'_> {
        RemoteRefSymbol {
            name: RefName::new(bookmark),
            remote: RemoteName::new("origin"),
        }
    }

    fn settings() -> UserSettings {
        let mut config = StackedConfig::with_defaults();
        config.add_layer(
            ConfigLayer::parse(
                ConfigSource::User,
                r#"
                    [user]
                    name = "Devspace Test"
                    email = "devspace@example.invalid"
                "#,
            )
            .unwrap(),
        );
        UserSettings::from_config(config).unwrap()
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0; 8_192];
        loop {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
            let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let body_start = header_end + 4;
            let headers = String::from_utf8_lossy(&bytes[..body_start]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                })
                .unwrap();
            if bytes.len() >= body_start + length {
                return String::from_utf8(bytes[..body_start + length].to_vec()).unwrap();
            }
        }
    }

    fn request_body(request: &str) -> &str {
        request.split_once("\r\n\r\n").unwrap().1
    }
}
