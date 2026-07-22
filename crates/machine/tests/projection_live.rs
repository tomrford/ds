//! Live projection recovery across a real bare Git remote and Worker journal.
//!
//! This manual probe requires an explicitly supplied live repository authority.

use devspace_machine::{
    CommitMapping, ExportMappings, ExportMode, GitProjection, HttpTransport, ImportMappings,
    MachineConfig, MachineId, MachineRepository, PackOptions, ProjectionObservation,
    ProjectionState, ProjectionUpdate, SharedSecret, SyncTransport, build_packs,
};
use jj_lib::backend::{
    ChangeId, Commit as BackendCommit, CommitId, CopyId, MillisSinceEpoch, Signature, Timestamp,
    Tree as BackendTree, TreeValue,
};
use jj_lib::merge::Merge;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf, RepoPathComponentBuf};
use jj_lib::store::Store;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

mod common;

use common::settings;

fn signature(seed: i64) -> Signature {
    Signature {
        name: "Devspace Test".to_owned(),
        email: "devspace@example.invalid".to_owned(),
        timestamp: Timestamp {
            timestamp: MillisSinceEpoch(seed),
            tz_offset: 60,
        },
    }
}

async fn write_file(store: &Arc<Store>, path: &RepoPath, bytes: &[u8]) -> TreeValue {
    let mut reader = bytes;
    let id = store.write_file(path, &mut reader).await.unwrap();
    TreeValue::File {
        id,
        executable: false,
        copy_id: CopyId::placeholder(),
    }
}

async fn write_private_history(store: &Arc<Store>) -> (CommitId, Vec<Vec<u8>>) {
    let hidden_path = RepoPathBuf::from_internal_string("secrets/.env").unwrap();
    let visible_path = RepoPathBuf::from_internal_string("visible.txt").unwrap();
    let hidden_values = vec![
        b"LIVE_PROJECTION_PRIVATE=first\0\xff".to_vec(),
        b"LIVE_PROJECTION_PRIVATE=second\0\xfe".to_vec(),
    ];
    let dsprivate_path = RepoPathBuf::from_internal_string(".dsprivate").unwrap();
    let dsprivate = write_file(store, &dsprivate_path, b"/secrets/.env\n").await;
    let mut parent = store.root_commit_id().clone();
    for (index, hidden) in hidden_values.iter().enumerate() {
        let hidden_value = write_file(store, &hidden_path, hidden).await;
        let secrets = store
            .write_tree(
                RepoPath::from_internal_string("secrets").unwrap(),
                BackendTree::from_sorted_entries(vec![(
                    RepoPathComponentBuf::new(".env".to_owned()).unwrap(),
                    hidden_value,
                )]),
            )
            .await
            .unwrap();
        let visible = write_file(
            store,
            &visible_path,
            format!("visible revision {index}\n").as_bytes(),
        )
        .await;
        let tree = store
            .write_tree(
                RepoPath::root(),
                BackendTree::from_sorted_entries(vec![
                    (
                        RepoPathComponentBuf::new(".dsprivate".to_owned()).unwrap(),
                        dsprivate.clone(),
                    ),
                    (
                        RepoPathComponentBuf::new("secrets".to_owned()).unwrap(),
                        TreeValue::Tree(secrets.id().clone()),
                    ),
                    (
                        RepoPathComponentBuf::new("visible.txt".to_owned()).unwrap(),
                        visible,
                    ),
                ]),
            )
            .await
            .unwrap();
        parent = store
            .write_commit(
                BackendCommit {
                    parents: vec![parent],
                    predecessors: Vec::new(),
                    root_tree: Merge::resolved(tree.id().clone()),
                    conflict_labels: Merge::resolved(String::new()),
                    change_id: ChangeId::new(vec![index as u8 + 1; store.change_id_length()]),
                    description: format!("live projection fixture {index}"),
                    author: signature(index as i64),
                    committer: signature(index as i64),
                    secure_sig: None,
                },
                None,
            )
            .await
            .unwrap()
            .id()
            .clone();
    }
    (parent, hidden_values)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Worker credentials, repository authority, and Git remote"]
async fn another_machine_recovers_remote_move_and_rebuilds_an_empty_sidecar() {
    let base_url = std::env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
    let shared_secret =
        std::env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
    let repository_id =
        std::env::var("DEVSPACE_REPOSITORY_ID").expect("set DEVSPACE_REPOSITORY_ID");
    let incarnation = parse_incarnation(
        &std::env::var("DEVSPACE_INCARNATION").expect("set DEVSPACE_INCARNATION"),
    );
    let first_machine = [0x11; 16];
    let recovery_machine = [0x22; 16];
    let batch_id = [0x33; 16];
    let first_config = MachineConfig::new(
        &base_url,
        MachineId::parse("11".repeat(16)).unwrap(),
        SharedSecret::new(&shared_secret).unwrap(),
    )
    .unwrap();
    let recovery_config = MachineConfig::new(
        &base_url,
        MachineId::parse("22".repeat(16)).unwrap(),
        SharedSecret::new(&shared_secret).unwrap(),
    )
    .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let first = MachineRepository::init(temp.path().join("machine-a/native"), &settings)
        .await
        .unwrap();
    let (private_head, hidden_values) = write_private_history(first.repo().store()).await;
    let mut cloud = HttpTransport::new(&first_config, &repository_id, incarnation).unwrap();
    cloud.probe_access().await.unwrap();
    let journal = HttpTransport::new(&first_config, &repository_id, incarnation).unwrap();

    let sidecar = temp.path().join("machine-a/projection");
    let projection = GitProjection::init(&sidecar, &settings).unwrap();
    let hidden_set = projection
        .hidden_set_for_commit(first.repo().store(), &private_head)
        .await
        .unwrap();
    let exported = projection
        .export_reachable(
            first.repo().store(),
            std::slice::from_ref(&private_head),
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap();
    let git_head = exported.git_heads[0].clone();
    let imported = projection
        .import_reachable(
            first.repo().store(),
            std::slice::from_ref(&git_head),
            &mut ImportMappings::default(),
        )
        .await
        .unwrap();
    let public_by_git = imported
        .new_mappings
        .iter()
        .map(|mapping| (mapping.git_id.clone(), mapping.canonical_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut states = Vec::with_capacity(exported.new_mappings.len());
    for mapping in &exported.new_mappings {
        let state_hidden_set = projection
            .hidden_set_for_commit(first.repo().store(), &mapping.canonical_id)
            .await
            .unwrap();
        states.push(ProjectionState {
            git_oid: mapping.git_id.as_bytes().try_into().unwrap(),
            canonical_commit_id: mapping.canonical_id.as_bytes().try_into().unwrap(),
            public_commit_id: public_by_git[&mapping.git_id]
                .as_bytes()
                .try_into()
                .unwrap(),
            hidden_set_id: state_hidden_set.identity().to_projection_id(),
        });
    }
    let proposed_state = states
        .iter()
        .position(|state| state.git_oid.as_slice() == git_head.as_bytes())
        .unwrap();

    let mut durable_heads = vec![private_head.clone()];
    durable_heads.extend(imported.canonical_heads.iter().cloned());
    let closure = first.commit_closure(&durable_heads).unwrap();
    let packs = build_packs(
        &closure,
        &BTreeSet::new(),
        temp.path().join("packs"),
        PackOptions::default(),
    )
    .unwrap();
    for pack in &packs.packs {
        cloud
            .upload_manifest(
                pack.id,
                &fs::read(pack.directory.join("manifest.bin")).unwrap(),
            )
            .await
            .unwrap();
        for position in 0..pack.manifest.chunks().len() {
            cloud
                .upload_chunk(
                    pack.id,
                    position,
                    &fs::read(pack.directory.join(format!("{position:08}.chunk"))).unwrap(),
                )
                .await
                .unwrap();
        }
        cloud.install_pack(pack.id).await.unwrap();
    }

    let begun = journal
        .begin_push(
            batch_id,
            first_machine,
            "origin",
            &[ProjectionUpdate {
                bookmark: "main".to_owned(),
                expected_old_oid: None,
                states: states.clone(),
                proposed_state: Some(proposed_state),
            }],
        )
        .await
        .unwrap();
    assert!(begun.pending);
    assert!(
        projection
            .scan_hidden_paths(&git_head, &hidden_set)
            .await
            .unwrap()
            .is_empty()
    );

    let origin = temp.path().join("origin.git");
    git(&["init", "--bare", origin.to_str().unwrap()], None);
    git(
        &["update-ref", "refs/heads/main", &git_head.hex()],
        Some(projection.git_repo_path()),
    );
    git(
        &[
            "push",
            "--atomic",
            "--force-with-lease=refs/heads/main:",
            origin.to_str().unwrap(),
            "refs/heads/main",
        ],
        Some(projection.git_repo_path()),
    );
    assert_hidden_bytes_absent(&origin, &hidden_values);
    drop(projection);
    drop(first);

    let recovery = HttpTransport::new(&recovery_config, &repository_id, incarnation).unwrap();
    let claimed = recovery
        .claim_push(batch_id, recovery_machine)
        .await
        .unwrap();
    assert!(claimed.fence > begun.fence);
    let replay = recovery.get_push_replay(batch_id).await.unwrap();
    assert_eq!(replay.owner_machine, recovery_machine);
    assert_eq!(replay.fence, claimed.fence);
    assert_eq!(replay.updates.len(), 1);
    assert_eq!(
        replay.updates[0]
            .proposed_state
            .map(|index| replay.updates[0].states[index].git_oid),
        Some(git_head.as_bytes().try_into().unwrap())
    );

    let second = MachineRepository::init(temp.path().join("machine-b/native"), &settings)
        .await
        .unwrap();
    let mut second_cloud =
        HttpTransport::new(&recovery_config, &repository_id, incarnation).unwrap();
    let catalog = second_cloud.list_packs(0, None).await.unwrap();
    for entry in catalog.packs {
        let pack = second_cloud.download_pack(entry.id).await.unwrap();
        second
            .install_pack(pack.id, &pack.manifest, &pack.chunks)
            .unwrap();
    }
    let replay_rows = replay.updates.iter().flat_map(|update| {
        update.states.iter().map(|state| CommitMapping {
            canonical_id: CommitId::new(state.canonical_commit_id.to_vec()),
            git_id: CommitId::new(state.git_oid.to_vec()),
        })
    });
    let rebuilt = GitProjection::init(temp.path().join("machine-b/projection"), &settings).unwrap();
    let rebuilt_head = rebuilt
        .export_reachable(
            second.repo().store(),
            std::slice::from_ref(&private_head),
            &mut ExportMappings::from_rows(replay_rows).unwrap(),
            ExportMode::Replay,
        )
        .await
        .unwrap();
    assert_eq!(
        rebuilt_head.git_heads.as_slice(),
        std::slice::from_ref(&git_head)
    );
    assert_hidden_bytes_absent(rebuilt.git_repo_path(), &hidden_values);

    let live_oid: [u8; 20] = parse_git_oid(&git_output(
        &["rev-parse", "refs/heads/main"],
        Some(&origin),
    ));
    let pending_snapshot = recovery.get(0, None).await.unwrap();
    let pending = pending_snapshot
        .pending
        .iter()
        .find(|batch| batch.batch_id == batch_id)
        .unwrap();
    assert_eq!(pending.refs[0].proposed_git_oid, Some(live_oid));
    let recovered = recovery
        .recover_push(
            batch_id,
            recovery_machine,
            claimed.fence,
            &[ProjectionObservation {
                bookmark: pending.refs[0].bookmark.clone(),
                live_oid: Some(live_oid),
            }],
        )
        .await
        .unwrap();
    assert_eq!(recovered.outcome.as_deref(), Some("accepted"));
    let mut snapshot = recovery.get(0, None).await.unwrap();
    let through = snapshot.through;
    while snapshot.has_more {
        let page = recovery
            .get(snapshot.next_after, Some(through))
            .await
            .unwrap();
        snapshot.mappings.extend(page.mappings);
        snapshot.next_after = page.next_after;
        snapshot.has_more = page.has_more;
    }
    assert!(snapshot.pending.is_empty());
    assert_eq!(snapshot.cursors[0].git_oid, live_oid);
    assert_eq!(snapshot.mappings.len(), exported.new_mappings.len());

    let replay_batch_id = [0x44; 16];
    let replay_begun = journal
        .begin_push(
            replay_batch_id,
            first_machine,
            "origin",
            &[ProjectionUpdate {
                bookmark: "replay".to_owned(),
                expected_old_oid: None,
                states,
                proposed_state: Some(proposed_state),
            }],
        )
        .await
        .unwrap();
    let replay_claimed = recovery
        .claim_push(replay_batch_id, recovery_machine)
        .await
        .unwrap();
    assert!(replay_claimed.fence > replay_begun.fence);
    let unchanged = recovery
        .recover_push(
            replay_batch_id,
            recovery_machine,
            replay_claimed.fence,
            &[ProjectionObservation {
                bookmark: "replay".to_owned(),
                live_oid: None,
            }],
        )
        .await
        .unwrap_err();
    assert!(
        unchanged
            .to_string()
            .contains("replay the exact push before recovery")
    );

    let replay_request = recovery.get_push_replay(replay_batch_id).await.unwrap();
    assert_eq!(replay_request.owner_machine, recovery_machine);
    assert_eq!(replay_request.fence, replay_claimed.fence);
    assert_eq!(replay_request.updates[0].bookmark, "replay");
    assert_eq!(
        replay_request.updates[0]
            .proposed_state
            .map(|index| replay_request.updates[0].states[index].git_oid),
        Some(git_head.as_bytes().try_into().unwrap())
    );
    git(
        &["update-ref", "refs/heads/replay", &git_head.hex()],
        Some(rebuilt.git_repo_path()),
    );
    git(
        &[
            "push",
            "--atomic",
            "--force-with-lease=refs/heads/replay:",
            origin.to_str().unwrap(),
            "refs/heads/replay",
        ],
        Some(rebuilt.git_repo_path()),
    );
    assert_hidden_bytes_absent(&origin, &hidden_values);
    let replay_live_oid = parse_git_oid(&git_output(
        &["rev-parse", "refs/heads/replay"],
        Some(&origin),
    ));
    let replay_recovered = recovery
        .recover_push(
            replay_batch_id,
            recovery_machine,
            replay_claimed.fence,
            &[ProjectionObservation {
                bookmark: "replay".to_owned(),
                live_oid: Some(replay_live_oid),
            }],
        )
        .await
        .unwrap();
    assert_eq!(replay_recovered.outcome.as_deref(), Some("accepted"));
    let replay_snapshot = recovery.get(0, None).await.unwrap();
    assert!(replay_snapshot.pending.is_empty());
    assert!(replay_snapshot.cursors.iter().any(|cursor| {
        cursor.remote == "origin"
            && cursor.bookmark == "replay"
            && cursor.git_oid == replay_live_oid
    }));
}

fn git(args: &[&str], git_dir: Option<&Path>) {
    let output = git_command(args, git_dir).output().unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output(args: &[&str], git_dir: Option<&Path>) -> String {
    let output = git_command(args, git_dir).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn git_command(args: &[&str], git_dir: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(git_dir) = git_dir {
        command.arg("--git-dir").arg(git_dir);
    }
    command.args(args);
    command
}

fn parse_git_oid(value: &str) -> [u8; 20] {
    let value = value.trim();
    std::array::from_fn(|index| u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).unwrap())
}

fn parse_incarnation(value: &str) -> [u8; 16] {
    assert_eq!(
        value.len(),
        32,
        "DEVSPACE_INCARNATION must be 32 hex characters"
    );
    std::array::from_fn(|index| {
        u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .expect("DEVSPACE_INCARNATION must be lowercase hex")
    })
}

fn assert_hidden_bytes_absent(git_dir: &Path, hidden_values: &[Vec<u8>]) {
    let objects = git_output(
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
        Some(git_dir),
    );
    for line in objects.lines() {
        let Some((id, "blob")) = line.split_once(' ') else {
            continue;
        };
        let output = git_command(&["cat-file", "blob", id], Some(git_dir))
            .output()
            .unwrap();
        assert!(output.status.success());
        for hidden in hidden_values {
            assert!(
                !output
                    .stdout
                    .windows(hidden.len())
                    .any(|window| window == hidden),
                "hidden bytes entered Git object storage"
            );
        }
    }
}
