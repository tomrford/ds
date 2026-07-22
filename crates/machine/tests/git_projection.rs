use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use devspace_machine::{
    ExportMappings, ExportMode, GitProjection, ImportMappings, MachineRepository, ProjectionError,
};
use jj_lib::backend::{
    ChangeId, Commit as BackendCommit, CommitId, CopyId, MillisSinceEpoch, Signature, Timestamp,
    Tree as BackendTree, TreeId, TreeValue,
};
use jj_lib::merge::Merge;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf, RepoPathComponentBuf};
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;

mod common;

use common::settings;

fn component(name: &str) -> RepoPathComponentBuf {
    RepoPathComponentBuf::new(name.to_owned()).unwrap()
}

fn path(value: &str) -> RepoPathBuf {
    RepoPathBuf::from_internal_string(value).unwrap()
}

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

async fn write_tree(store: &Arc<Store>, dir: &RepoPath, entries: Vec<(&str, TreeValue)>) -> TreeId {
    let mut entries = entries
        .into_iter()
        .map(|(name, value)| (component(name), value))
        .collect::<Vec<_>>();
    entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
    store
        .write_tree(dir, BackendTree::from_sorted_entries(entries))
        .await
        .unwrap()
        .id()
        .clone()
}

async fn write_commit(
    store: &Arc<Store>,
    parent: CommitId,
    root_tree: Merge<TreeId>,
    conflict_labels: Merge<String>,
    seed: u8,
) -> CommitId {
    store
        .write_commit(
            BackendCommit {
                parents: vec![parent],
                predecessors: Vec::new(),
                root_tree,
                conflict_labels,
                change_id: ChangeId::new(vec![seed; store.change_id_length()]),
                description: format!("projection fixture {seed}"),
                author: signature(seed.into()),
                committer: signature(seed.into()),
                secure_sig: None,
            },
            None,
        )
        .await
        .unwrap()
        .id()
        .clone()
}

async fn write_private_history(store: &Arc<Store>) -> (CommitId, Vec<Vec<u8>>) {
    let hidden_path = path("secrets/.env");
    let dsprivate_path = path(".dsprivate");
    let hidden_values = vec![
        b"DEVSPACE_PRIVATE_SENTINEL=first\0\xff".to_vec(),
        b"DEVSPACE_PRIVATE_SENTINEL=second\0\xfe".to_vec(),
    ];
    let dsprivate = write_file(store, &dsprivate_path, b"/secrets/.env\n").await;
    let mut parent = store.root_commit_id().clone();
    for (index, hidden) in hidden_values.iter().enumerate() {
        let hidden_value = write_file(store, &hidden_path, hidden).await;
        let secrets_tree = write_tree(
            store,
            RepoPath::from_internal_string("secrets").unwrap(),
            vec![(".env", hidden_value)],
        )
        .await;
        let visible = format!("visible revision {index}\n");
        let visible_value = write_file(store, &path("visible.txt"), visible.as_bytes()).await;
        let root_tree = write_tree(
            store,
            RepoPath::root(),
            vec![
                (".dsprivate", dsprivate.clone()),
                ("secrets", TreeValue::Tree(secrets_tree)),
                ("visible.txt", visible_value),
            ],
        )
        .await;
        parent = write_commit(
            store,
            parent,
            Merge::resolved(root_tree),
            Merge::resolved(String::new()),
            index as u8 + 1,
        )
        .await;
    }
    (parent, hidden_values)
}

fn all_git_blob_contents(git_dir: &Path) -> Vec<Vec<u8>> {
    let listing = Command::new("git")
        .args([
            "--git-dir",
            git_dir.to_str().unwrap(),
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ])
        .output()
        .unwrap();
    assert!(
        listing.status.success(),
        "{}",
        String::from_utf8_lossy(&listing.stderr)
    );
    String::from_utf8(listing.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, kind)| *kind == "blob")
        .map(|(id, _)| {
            let output = Command::new("git")
                .args([
                    "--git-dir",
                    git_dir.to_str().unwrap(),
                    "cat-file",
                    "blob",
                    id,
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        })
        .collect()
}

fn assert_hidden_bytes_absent(git_dir: &Path, hidden_values: &[Vec<u8>]) {
    let blobs = all_git_blob_contents(git_dir);
    for hidden in hidden_values {
        assert!(
            blobs
                .iter()
                .all(|blob| !blob.windows(hidden.len()).any(|window| window == hidden)),
            "hidden bytes entered Git object storage"
        );
    }
}

async fn fixture() -> (
    tempfile::TempDir,
    MachineRepository,
    UserSettings,
    CommitId,
    Vec<Vec<u8>>,
) {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let (head, hidden) = write_private_history(repository.repo().store()).await;
    (temp, repository, settings, head, hidden)
}

async fn projected_value(
    projection: &GitProjection,
    commit_id: &CommitId,
    value_path: &str,
) -> Option<TreeValue> {
    projection
        .store()
        .get_commit_async(commit_id)
        .await
        .unwrap()
        .tree()
        .path_value(&path(value_path))
        .await
        .unwrap()
        .into_resolved()
        .unwrap()
}

#[tokio::test]
async fn hidden_bytes_never_enter_fresh_sidecar() {
    let (temp, repository, settings, head, hidden_values) = fixture().await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let exported = projection
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap();

    assert_eq!(head.as_bytes().len(), 64);
    assert_eq!(exported.git_heads[0].as_bytes().len(), 20);
    assert_eq!(exported.new_mappings.len(), 2);
    assert_hidden_bytes_absent(projection.git_repo_path(), &hidden_values);
    assert!(
        projected_value(&projection, &exported.git_heads[0], "secrets/.env")
            .await
            .is_none()
    );
    assert!(
        projected_value(&projection, &exported.git_heads[0], ".dsprivate")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn each_commit_uses_its_own_hidden_set() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let store = repository.repo().store();
    let secret_a = write_file(store, &path("secrets/secret-a"), b"a").await;
    let secret_b = write_file(store, &path("secrets/secret-b"), b"b").await;
    let secrets_tree = write_tree(
        store,
        RepoPath::from_internal_string("secrets").unwrap(),
        vec![("secret-a", secret_a), ("secret-b", secret_b)],
    )
    .await;
    let root_without_policy = write_tree(
        store,
        RepoPath::root(),
        vec![("secrets", TreeValue::Tree(secrets_tree.clone()))],
    )
    .await;
    let first = write_commit(
        store,
        store.root_commit_id().clone(),
        Merge::resolved(root_without_policy),
        Merge::resolved(String::new()),
        10,
    )
    .await;
    let hide_a = write_file(store, &path(".dsprivate"), b"secrets/secret-a\n").await;
    let second_tree = write_tree(
        store,
        RepoPath::root(),
        vec![
            (".dsprivate", hide_a),
            ("secrets", TreeValue::Tree(secrets_tree.clone())),
        ],
    )
    .await;
    let second = write_commit(
        store,
        first.clone(),
        Merge::resolved(second_tree),
        Merge::resolved(String::new()),
        11,
    )
    .await;
    let hide_b = write_file(store, &path(".dsprivate"), b"secrets/secret-b\n").await;
    let third_tree = write_tree(
        store,
        RepoPath::root(),
        vec![
            (".dsprivate", hide_b),
            ("secrets", TreeValue::Tree(secrets_tree)),
        ],
    )
    .await;
    let third = write_commit(
        store,
        second.clone(),
        Merge::resolved(third_tree),
        Merge::resolved(String::new()),
        12,
    )
    .await;

    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    assert!(
        projection
            .hidden_set_for_commit(store, &first)
            .await
            .unwrap()
            .identity()
            .to_projection_id()
            .is_none()
    );
    assert!(
        projection
            .hidden_set_for_commit(store, &second)
            .await
            .unwrap()
            .identity()
            .to_projection_id()
            .is_some()
    );
    let exported = projection
        .export_reachable(
            store,
            std::slice::from_ref(&third),
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap();
    let mapped = exported
        .new_mappings
        .iter()
        .map(|row| (row.canonical_id.clone(), row.git_id.clone()))
        .collect::<BTreeMap<_, _>>();

    assert!(
        projected_value(&projection, &mapped[&first], "secrets/secret-a")
            .await
            .is_some()
    );
    assert!(
        projected_value(&projection, &mapped[&first], "secrets/secret-b")
            .await
            .is_some()
    );
    assert!(
        projected_value(&projection, &mapped[&second], "secrets/secret-a")
            .await
            .is_none()
    );
    assert!(
        projected_value(&projection, &mapped[&second], "secrets/secret-b")
            .await
            .is_some()
    );
    assert!(
        projected_value(&projection, &mapped[&third], "secrets/secret-a")
            .await
            .is_some()
    );
    assert!(
        projected_value(&projection, &mapped[&third], "secrets/secret-b")
            .await
            .is_none()
    );
    for git_id in mapped.values() {
        assert!(
            projected_value(&projection, git_id, ".dsprivate")
                .await
                .is_none()
        );
    }
}

#[tokio::test]
async fn nested_dsprivate_is_anchored_to_its_directory_and_always_hidden() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let store = repository.repo().store();
    let secret = write_file(store, &path("sub/secret"), b"private").await;
    let sub_without_policy = write_tree(
        store,
        RepoPath::from_internal_string("sub").unwrap(),
        vec![("secret", secret.clone())],
    )
    .await;
    let root_policy = write_file(store, &path(".dsprivate"), b"/secret\n").await;
    let first_tree = write_tree(
        store,
        RepoPath::root(),
        vec![
            (".dsprivate", root_policy.clone()),
            ("sub", TreeValue::Tree(sub_without_policy)),
        ],
    )
    .await;
    let first = write_commit(
        store,
        store.root_commit_id().clone(),
        Merge::resolved(first_tree),
        Merge::resolved(String::new()),
        13,
    )
    .await;
    let nested_policy = write_file(store, &path("sub/.dsprivate"), b"secret\n").await;
    let sub_with_policy = write_tree(
        store,
        RepoPath::from_internal_string("sub").unwrap(),
        vec![(".dsprivate", nested_policy), ("secret", secret)],
    )
    .await;
    let second_tree = write_tree(
        store,
        RepoPath::root(),
        vec![
            (".dsprivate", root_policy),
            ("sub", TreeValue::Tree(sub_with_policy)),
        ],
    )
    .await;
    let second = write_commit(
        store,
        first.clone(),
        Merge::resolved(second_tree),
        Merge::resolved(String::new()),
        14,
    )
    .await;

    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let exported = projection
        .export_reachable(
            store,
            &[second],
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap();
    let mapped = exported
        .new_mappings
        .iter()
        .map(|row| (row.canonical_id.clone(), row.git_id.clone()))
        .collect::<BTreeMap<_, _>>();
    assert!(
        projected_value(&projection, &mapped[&first], "sub/secret")
            .await
            .is_some()
    );
    let second_public = exported.git_heads.last().unwrap();
    assert!(
        projected_value(&projection, second_public, "sub/secret")
            .await
            .is_none()
    );
    assert!(
        projected_value(&projection, second_public, "sub/.dsprivate")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn nested_negation_overrides_a_shallower_file_pattern() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let store = repository.repo().store();
    let sub = write_tree(
        store,
        RepoPath::from_internal_string("sub").unwrap(),
        vec![
            (
                ".dsprivate",
                write_file(store, &path("sub/.dsprivate"), b"!secret.txt\n").await,
            ),
            (
                "other.txt",
                write_file(store, &path("sub/other.txt"), b"hidden").await,
            ),
            (
                "secret.txt",
                write_file(store, &path("sub/secret.txt"), b"public").await,
            ),
        ],
    )
    .await;
    let root = write_tree(
        store,
        RepoPath::root(),
        vec![
            (
                ".dsprivate",
                write_file(store, &path(".dsprivate"), b"sub/*.txt\n").await,
            ),
            ("sub", TreeValue::Tree(sub)),
        ],
    )
    .await;
    let head = write_commit(
        store,
        store.root_commit_id().clone(),
        Merge::resolved(root),
        Merge::resolved(String::new()),
        15,
    )
    .await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let exported = projection
        .export_reachable(
            store,
            &[head],
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap();
    assert!(
        projected_value(&projection, &exported.git_heads[0], "sub/secret.txt")
            .await
            .is_some()
    );
    assert!(
        projected_value(&projection, &exported.git_heads[0], "sub/other.txt")
            .await
            .is_none()
    );
    assert!(
        projected_value(&projection, &exported.git_heads[0], "sub/.dsprivate")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn hidden_set_identity_covers_nested_policy_add_edit_and_remove() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let store = repository.repo().store();
    let empty_sub = write_tree(
        store,
        RepoPath::from_internal_string("sub").unwrap(),
        vec![],
    )
    .await;
    let no_policy_tree = write_tree(
        store,
        RepoPath::root(),
        vec![("sub", TreeValue::Tree(empty_sub))],
    )
    .await;
    let none = write_commit(
        store,
        store.root_commit_id().clone(),
        Merge::resolved(no_policy_tree.clone()),
        Merge::resolved(String::new()),
        16,
    )
    .await;
    let first_policy = write_file(store, &path("sub/.dsprivate"), b"first\n").await;
    let first_sub = write_tree(
        store,
        RepoPath::from_internal_string("sub").unwrap(),
        vec![(".dsprivate", first_policy)],
    )
    .await;
    let first_tree = write_tree(
        store,
        RepoPath::root(),
        vec![("sub", TreeValue::Tree(first_sub))],
    )
    .await;
    let added = write_commit(
        store,
        none.clone(),
        Merge::resolved(first_tree),
        Merge::resolved(String::new()),
        17,
    )
    .await;
    let second_policy = write_file(store, &path("sub/.dsprivate"), b"second\n").await;
    let second_sub = write_tree(
        store,
        RepoPath::from_internal_string("sub").unwrap(),
        vec![(".dsprivate", second_policy)],
    )
    .await;
    let second_tree = write_tree(
        store,
        RepoPath::root(),
        vec![("sub", TreeValue::Tree(second_sub))],
    )
    .await;
    let edited = write_commit(
        store,
        added.clone(),
        Merge::resolved(second_tree),
        Merge::resolved(String::new()),
        18,
    )
    .await;
    let removed = write_commit(
        store,
        edited.clone(),
        Merge::resolved(no_policy_tree),
        Merge::resolved(String::new()),
        19,
    )
    .await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let none_id = projection
        .hidden_set_for_commit(store, &none)
        .await
        .unwrap()
        .identity()
        .to_projection_id();
    let added_id = projection
        .hidden_set_for_commit(store, &added)
        .await
        .unwrap()
        .identity()
        .to_projection_id();
    let edited_id = projection
        .hidden_set_for_commit(store, &edited)
        .await
        .unwrap()
        .identity()
        .to_projection_id();
    let removed_id = projection
        .hidden_set_for_commit(store, &removed)
        .await
        .unwrap()
        .identity()
        .to_projection_id();
    assert!(none_id.is_none());
    assert!(added_id.is_some());
    assert_ne!(added_id, edited_id);
    assert!(removed_id.is_none());
}

#[tokio::test]
async fn hidden_directory_is_pruned_before_negation_can_reinclude_a_child() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let store = repository.repo().store();
    let private_tree = write_tree(
        store,
        RepoPath::from_internal_string("private").unwrap(),
        vec![
            (
                "drop.txt",
                write_file(store, &path("private/drop.txt"), b"drop").await,
            ),
            (
                "keep.txt",
                write_file(store, &path("private/keep.txt"), b"keep").await,
            ),
        ],
    )
    .await;
    let root = write_tree(
        store,
        RepoPath::root(),
        vec![
            (
                ".dsprivate",
                write_file(store, &path(".dsprivate"), b"private/\n!private/keep.txt\n").await,
            ),
            ("private", TreeValue::Tree(private_tree)),
            (
                "visible",
                write_file(store, &path("visible"), b"public").await,
            ),
        ],
    )
    .await;
    let head = write_commit(
        store,
        store.root_commit_id().clone(),
        Merge::resolved(root),
        Merge::resolved(String::new()),
        20,
    )
    .await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let exported = projection
        .export_reachable(
            store,
            &[head],
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap();

    assert!(
        projected_value(&projection, &exported.git_heads[0], "private")
            .await
            .is_none()
    );
    assert!(
        projected_value(&projection, &exported.git_heads[0], "visible")
            .await
            .is_some()
    );
}

#[tokio::test]
async fn conflicted_dsprivate_fails_closed_with_the_commit_id() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let store = repository.repo().store();
    let left = write_tree(
        store,
        RepoPath::root(),
        vec![(
            ".dsprivate",
            write_file(store, &path(".dsprivate"), b"left\n").await,
        )],
    )
    .await;
    let right = write_tree(
        store,
        RepoPath::root(),
        vec![(
            ".dsprivate",
            write_file(store, &path(".dsprivate"), b"right\n").await,
        )],
    )
    .await;
    let head = write_commit(
        store,
        store.root_commit_id().clone(),
        Merge::from_vec(vec![store.empty_tree_id().clone(), left, right]),
        Merge::from_vec(vec![String::new(), String::new(), String::new()]),
        30,
    )
    .await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let error = projection
        .export_reachable(
            store,
            std::slice::from_ref(&head),
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ProjectionError::ConflictedDsprivate { commit_id, path: policy_path }
            if commit_id == head && policy_path == path(".dsprivate")
    ));
}

#[tokio::test]
async fn conflicted_nested_dsprivate_fails_closed_with_commit_and_path() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let store = repository.repo().store();
    let left_sub = write_tree(
        store,
        RepoPath::from_internal_string("sub").unwrap(),
        vec![(
            ".dsprivate",
            write_file(store, &path("sub/.dsprivate"), b"left\n").await,
        )],
    )
    .await;
    let right_sub = write_tree(
        store,
        RepoPath::from_internal_string("sub").unwrap(),
        vec![(
            ".dsprivate",
            write_file(store, &path("sub/.dsprivate"), b"right\n").await,
        )],
    )
    .await;
    let left = write_tree(
        store,
        RepoPath::root(),
        vec![("sub", TreeValue::Tree(left_sub))],
    )
    .await;
    let right = write_tree(
        store,
        RepoPath::root(),
        vec![("sub", TreeValue::Tree(right_sub))],
    )
    .await;
    let head = write_commit(
        store,
        store.root_commit_id().clone(),
        Merge::from_vec(vec![store.empty_tree_id().clone(), left, right]),
        Merge::from_vec(vec![String::new(), String::new(), String::new()]),
        32,
    )
    .await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let error = projection
        .export_reachable(
            store,
            std::slice::from_ref(&head),
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ProjectionError::ConflictedDsprivate { commit_id, path: policy_path }
            if commit_id == head && policy_path == path("sub/.dsprivate")
    ));
}

#[tokio::test]
async fn shared_subtree_cache_is_partitioned_by_nested_chain_state() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let store = repository.repo().store();
    let payload = write_tree(
        store,
        RepoPath::from_internal_string("sub/payload").unwrap(),
        vec![
            (
                "secret-a",
                write_file(store, &path("sub/payload/secret-a"), b"a").await,
            ),
            (
                "secret-b",
                write_file(store, &path("sub/payload/secret-b"), b"b").await,
            ),
        ],
    )
    .await;
    let policy_a = write_file(store, &path("sub/.dsprivate"), b"payload/secret-a\n").await;
    let policy_b = write_file(store, &path("sub/.dsprivate"), b"payload/secret-b\n").await;
    let sub_a = write_tree(
        store,
        RepoPath::from_internal_string("sub").unwrap(),
        vec![
            (".dsprivate", policy_a),
            ("payload", TreeValue::Tree(payload.clone())),
        ],
    )
    .await;
    let sub_b = write_tree(
        store,
        RepoPath::from_internal_string("sub").unwrap(),
        vec![
            (".dsprivate", policy_b),
            ("payload", TreeValue::Tree(payload)),
        ],
    )
    .await;
    let root_a = write_tree(
        store,
        RepoPath::root(),
        vec![("sub", TreeValue::Tree(sub_a))],
    )
    .await;
    let root_b = write_tree(
        store,
        RepoPath::root(),
        vec![("sub", TreeValue::Tree(sub_b))],
    )
    .await;
    let first = write_commit(
        store,
        store.root_commit_id().clone(),
        Merge::resolved(root_a),
        Merge::resolved(String::new()),
        33,
    )
    .await;
    let second = write_commit(
        store,
        store.root_commit_id().clone(),
        Merge::resolved(root_b),
        Merge::resolved(String::new()),
        34,
    )
    .await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let exported = projection
        .export_reachable(
            store,
            &[first.clone(), second.clone()],
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap();
    let mapped = exported
        .new_mappings
        .iter()
        .map(|row| (row.canonical_id.clone(), row.git_id.clone()))
        .collect::<BTreeMap<_, _>>();
    assert!(
        projected_value(&projection, &mapped[&first], "sub/payload/secret-a")
            .await
            .is_none()
    );
    assert!(
        projected_value(&projection, &mapped[&first], "sub/payload/secret-b")
            .await
            .is_some()
    );
    assert!(
        projected_value(&projection, &mapped[&second], "sub/payload/secret-a")
            .await
            .is_some()
    );
    assert!(
        projected_value(&projection, &mapped[&second], "sub/payload/secret-b")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn directory_at_dsprivate_fails_closed_with_the_commit_id() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let store = repository.repo().store();
    let policy_tree = write_tree(
        store,
        RepoPath::from_internal_string(".dsprivate").unwrap(),
        vec![(
            "pattern",
            write_file(store, &path(".dsprivate/pattern"), b"secret").await,
        )],
    )
    .await;
    let root = write_tree(
        store,
        RepoPath::root(),
        vec![(".dsprivate", TreeValue::Tree(policy_tree))],
    )
    .await;
    let head = write_commit(
        store,
        store.root_commit_id().clone(),
        Merge::resolved(root),
        Merge::resolved(String::new()),
        31,
    )
    .await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let error = projection
        .export_reachable(
            store,
            std::slice::from_ref(&head),
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ProjectionError::InvalidDsprivateEntry { commit_id, path: policy_path }
            if commit_id == head && policy_path == path(".dsprivate")
    ));
}

#[tokio::test]
async fn export_fails_when_mapped_git_object_is_missing() {
    let (temp, repository, settings, head, _) = fixture().await;
    let sidecar = temp.path().join("projection");
    let first = GitProjection::init(&sidecar, &settings).unwrap();
    let first_export = first
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap();
    let rows = first_export.new_mappings.clone();
    let mapped_head = rows
        .iter()
        .find(|mapping| mapping.canonical_id == head)
        .unwrap()
        .git_id
        .clone();
    drop(first);
    fs::remove_dir_all(&sidecar).unwrap();

    let rebuilt = GitProjection::init(&sidecar, &settings).unwrap();
    let mut mappings = ExportMappings::from_rows(rows).unwrap();
    let error = rebuilt
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &mut mappings,
            ExportMode::Strict,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ProjectionError::MissingMappedObject {
            canonical_id,
            git_id
        } if canonical_id == head && git_id == mapped_head
    ));
}

#[tokio::test]
async fn replay_export_rederives_when_mapped_git_object_is_missing() {
    let (temp, repository, settings, head, _) = fixture().await;
    let sidecar = temp.path().join("projection");
    let first = GitProjection::init(&sidecar, &settings).unwrap();
    let first_export = first
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap();
    let rows = first_export.new_mappings;
    let expected_head = first_export.git_heads[0].clone();
    drop(first);
    fs::remove_dir_all(&sidecar).unwrap();

    let rebuilt = GitProjection::init(&sidecar, &settings).unwrap();
    let replayed = rebuilt
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &mut ExportMappings::from_rows(rows).unwrap(),
            ExportMode::Replay,
        )
        .await
        .unwrap();
    assert_eq!(replayed.git_heads, [expected_head]);
}

#[tokio::test]
async fn import_rederives_when_mapped_canonical_object_is_missing() {
    let (temp, repository, settings, head, _) = fixture().await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let exported = projection
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap();
    let git_head = exported.git_heads[0].clone();
    let first_target = MachineRepository::init(temp.path().join("first-target"), &settings)
        .await
        .unwrap();
    let first_import = projection
        .import_reachable(
            first_target.repo().store(),
            std::slice::from_ref(&git_head),
            &mut ImportMappings::default(),
        )
        .await
        .unwrap();
    let canonical_head = first_import.canonical_heads[0].clone();

    let rebuilt_target = MachineRepository::init(temp.path().join("rebuilt-target"), &settings)
        .await
        .unwrap();
    assert!(matches!(
        rebuilt_target
            .repo()
            .store()
            .backend()
            .read_commit(&canonical_head)
            .await,
        Err(jj_lib::backend::BackendError::ObjectNotFound { .. })
    ));
    let mut mappings = ImportMappings::from_rows(first_import.new_mappings).unwrap();
    let rebuilt_import = projection
        .import_reachable(
            rebuilt_target.repo().store(),
            std::slice::from_ref(&git_head),
            &mut mappings,
        )
        .await
        .unwrap();

    assert_eq!(rebuilt_import.canonical_heads, [canonical_head]);
}

#[tokio::test]
async fn export_stops_when_mapped_git_object_is_present() {
    let (temp, repository, settings, head, _) = fixture().await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let exported = projection
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap();
    let git_head = exported.git_heads[0].clone();
    let missing_canonical = CommitId::new(vec![0x55; 64]);
    let mut mappings = ExportMappings::from_rows([devspace_machine::CommitMapping {
        canonical_id: missing_canonical.clone(),
        git_id: git_head.clone(),
    }])
    .unwrap();

    let stopped = projection
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&missing_canonical),
            &mut mappings,
            ExportMode::Strict,
        )
        .await
        .unwrap();

    assert_eq!(stopped.git_heads, [git_head]);
    assert!(stopped.new_mappings.is_empty());
}

#[tokio::test]
async fn full_tree_scan_finds_a_planted_public_leak() {
    let (temp, repository, settings, head, _) = fixture().await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let hidden_set = projection
        .hidden_set_for_commit(repository.repo().store(), &head)
        .await
        .unwrap();
    let leaked_tree = write_tree(
        projection.store(),
        RepoPath::from_internal_string("secrets").unwrap(),
        vec![(
            ".env",
            write_file(projection.store(), &path("secrets/.env"), b"planted").await,
        )],
    )
    .await;
    let root = write_tree(
        projection.store(),
        RepoPath::root(),
        vec![
            (
                ".dsprivate",
                write_file(projection.store(), &path(".dsprivate"), b"also planted").await,
            ),
            ("secrets", TreeValue::Tree(leaked_tree)),
        ],
    )
    .await;
    let public = write_commit(
        projection.store(),
        projection.store().root_commit_id().clone(),
        Merge::resolved(root),
        Merge::resolved(String::new()),
        40,
    )
    .await;

    assert_eq!(
        projection
            .scan_hidden_paths(&public, &hidden_set)
            .await
            .unwrap(),
        [path(".dsprivate"), path("secrets/.env")]
    );
}

#[tokio::test]
async fn full_tree_scan_uses_nested_canonical_policy() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let store = repository.repo().store();
    let canonical_sub = write_tree(
        store,
        RepoPath::from_internal_string("sub").unwrap(),
        vec![
            (
                ".dsprivate",
                write_file(store, &path("sub/.dsprivate"), b"secret\n").await,
            ),
            (
                "secret",
                write_file(store, &path("sub/secret"), b"private").await,
            ),
        ],
    )
    .await;
    let canonical_root = write_tree(
        store,
        RepoPath::root(),
        vec![("sub", TreeValue::Tree(canonical_sub))],
    )
    .await;
    let canonical = write_commit(
        store,
        store.root_commit_id().clone(),
        Merge::resolved(canonical_root),
        Merge::resolved(String::new()),
        41,
    )
    .await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let hidden_set = projection
        .hidden_set_for_commit(store, &canonical)
        .await
        .unwrap();
    let public_sub = write_tree(
        projection.store(),
        RepoPath::from_internal_string("sub").unwrap(),
        vec![
            (
                ".dsprivate",
                write_file(
                    projection.store(),
                    &path("sub/.dsprivate"),
                    b"planted policy",
                )
                .await,
            ),
            (
                "secret",
                write_file(projection.store(), &path("sub/secret"), b"planted").await,
            ),
        ],
    )
    .await;
    let public_root = write_tree(
        projection.store(),
        RepoPath::root(),
        vec![("sub", TreeValue::Tree(public_sub))],
    )
    .await;
    let public = write_commit(
        projection.store(),
        projection.store().root_commit_id().clone(),
        Merge::resolved(public_root),
        Merge::resolved(String::new()),
        42,
    )
    .await;

    assert_eq!(
        projection
            .scan_hidden_paths(&public, &hidden_set)
            .await
            .unwrap(),
        [path("sub/.dsprivate"), path("sub/secret")]
    );
}

#[tokio::test]
async fn gitlink_import_fails_before_simple_encoding() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let initial_operation = repository.repo().op_id().clone();
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let gitlink_path = path("vendor/dependency");
    let vendor_tree = write_tree(
        projection.store(),
        RepoPath::from_internal_string("vendor").unwrap(),
        vec![(
            "dependency",
            TreeValue::GitSubmodule(CommitId::new(vec![0x11; 20])),
        )],
    )
    .await;
    let root_tree = write_tree(
        projection.store(),
        RepoPath::root(),
        vec![("vendor", TreeValue::Tree(vendor_tree))],
    )
    .await;
    let git_head = write_commit(
        projection.store(),
        projection.store().root_commit_id().clone(),
        Merge::resolved(root_tree),
        Merge::resolved(String::new()),
        50,
    )
    .await;

    let error = projection
        .import_reachable(
            repository.repo().store(),
            &[git_head],
            &mut ImportMappings::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ProjectionError::GitLink {
            path,
            operation: "import"
        } if path == gitlink_path
    ));
    assert_eq!(repository.repo().op_id(), &initial_operation);
    assert_eq!(
        repository
            .repo()
            .op_heads_store()
            .get_op_heads()
            .await
            .unwrap(),
        vec![initial_operation]
    );
}

#[tokio::test]
async fn hidden_leaf_is_filtered_before_its_bytes_are_read() {
    let (temp, repository, settings, head, _) = fixture().await;
    let hidden_path = path("secrets/.env");
    let hidden_id = match repository
        .repo()
        .store()
        .get_commit_async(&head)
        .await
        .unwrap()
        .tree()
        .path_value(&hidden_path)
        .await
        .unwrap()
        .into_resolved()
        .unwrap()
        .unwrap()
    {
        TreeValue::File { id, .. } => id,
        value => panic!("expected hidden file, got {value:?}"),
    };
    fs::remove_file(repository.path().join("store/files").join(hidden_id.hex())).unwrap();
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    projection
        .export_reachable(
            repository.repo().store(),
            &[head],
            &mut ExportMappings::default(),
            ExportMode::Strict,
        )
        .await
        .unwrap();
}
