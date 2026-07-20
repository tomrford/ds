use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use devspace_machine::{
    ExportMappings, GitProjection, ImportMappings, MachineRepository, ProjectionError,
};
use jj_lib::backend::{
    ChangeId, Commit as BackendCommit, CommitId, CopyId, MillisSinceEpoch, Signature, Timestamp,
    Tree as BackendTree, TreeId, TreeValue,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::merge::Merge;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf, RepoPathComponentBuf};
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;

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
    let dshide_path = path(".dshide");
    let hidden_values = vec![
        b"DEVSPACE_PRIVATE_SENTINEL=first\0\xff".to_vec(),
        b"DEVSPACE_PRIVATE_SENTINEL=second\0\xfe".to_vec(),
    ];
    let dshide = write_file(store, &dshide_path, b"/secrets/.env\n").await;
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
                (".dshide", dshide.clone()),
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
        projected_value(&projection, &exported.git_heads[0], ".dshide")
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
    let hide_a = write_file(store, &path(".dshide"), b"secrets/secret-a\n").await;
    let second_tree = write_tree(
        store,
        RepoPath::root(),
        vec![
            (".dshide", hide_a),
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
    let hide_b = write_file(store, &path(".dshide"), b"secrets/secret-b\n").await;
    let third_tree = write_tree(
        store,
        RepoPath::root(),
        vec![
            (".dshide", hide_b),
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
            .file_id()
            .is_none()
    );
    assert!(
        projection
            .hidden_set_for_commit(store, &second)
            .await
            .unwrap()
            .identity()
            .file_id()
            .is_some()
    );
    let exported = projection
        .export_reachable(
            store,
            std::slice::from_ref(&third),
            &mut ExportMappings::default(),
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
            projected_value(&projection, git_id, ".dshide")
                .await
                .is_none()
        );
    }
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
                ".dshide",
                write_file(store, &path(".dshide"), b"private/\n!private/keep.txt\n").await,
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
        .export_reachable(store, &[head], &mut ExportMappings::default())
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
async fn conflicted_dshide_fails_closed_with_the_commit_id() {
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
            ".dshide",
            write_file(store, &path(".dshide"), b"left\n").await,
        )],
    )
    .await;
    let right = write_tree(
        store,
        RepoPath::root(),
        vec![(
            ".dshide",
            write_file(store, &path(".dshide"), b"right\n").await,
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
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ProjectionError::ConflictedDshide(id) if id == head));
}

#[tokio::test]
async fn directory_at_dshide_fails_closed_with_the_commit_id() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let store = repository.repo().store();
    let policy_tree = write_tree(
        store,
        RepoPath::from_internal_string(".dshide").unwrap(),
        vec![(
            "pattern",
            write_file(store, &path(".dshide/pattern"), b"secret").await,
        )],
    )
    .await;
    let root = write_tree(
        store,
        RepoPath::root(),
        vec![(".dshide", TreeValue::Tree(policy_tree))],
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
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ProjectionError::InvalidDshideEntry(id) if id == head));
}

#[tokio::test]
async fn sidecar_rebuilds_from_durable_mapping_rows() {
    let (temp, repository, settings, head, hidden_values) = fixture().await;
    let sidecar = temp.path().join("projection");
    let first = GitProjection::init(&sidecar, &settings).unwrap();
    let first_export = first
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &mut ExportMappings::default(),
        )
        .await
        .unwrap();
    let rows = first_export.new_mappings.clone();
    let first_head = first_export.git_heads[0].clone();
    drop(first);
    fs::remove_dir_all(&sidecar).unwrap();

    let rebuilt = GitProjection::init(&sidecar, &settings).unwrap();
    let mut mappings = ExportMappings::from_rows(rows).unwrap();
    let rebuilt_export = rebuilt
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &mut mappings,
        )
        .await
        .unwrap();
    assert_eq!(rebuilt_export.git_heads, [first_head]);
    assert!(rebuilt_export.new_mappings.is_empty());
    assert_hidden_bytes_absent(rebuilt.git_repo_path(), &hidden_values);
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
                ".dshide",
                write_file(projection.store(), &path(".dshide"), b"also planted").await,
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
        [path(".dshide"), path("secrets/.env")]
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
        )
        .await
        .unwrap();
}
