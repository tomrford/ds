use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use devspace_kernel::hidden::{HiddenPath, HiddenPathSet};
use devspace_machine::{
    ExactPathFilter, ExportMappings, GitProjection, ImportMappings, MachineRepository,
    ProjectionError,
};
use jj_lib::backend::{
    ChangeId, Commit as BackendCommit, CommitId, CopyId, MillisSinceEpoch, Signature, Timestamp,
    Tree as BackendTree, TreeValue,
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

async fn write_history(store: &Arc<Store>) -> (CommitId, Vec<Vec<u8>>) {
    let hidden_path = RepoPathBuf::from_internal_string("secrets/.env").unwrap();
    let visible_path = RepoPathBuf::from_internal_string("visible.txt").unwrap();
    let hidden_values = vec![
        b"DEVSPACE_PRIVATE_SENTINEL=first\0\xff".to_vec(),
        b"DEVSPACE_PRIVATE_SENTINEL=second\0\xfe".to_vec(),
    ];
    let mut parent = store.root_commit_id().clone();
    for (index, hidden) in hidden_values.iter().enumerate() {
        let hidden_value = write_file(store, &hidden_path, hidden).await;
        let secrets_tree = store
            .write_tree(
                RepoPath::from_internal_string("secrets").unwrap(),
                BackendTree::from_sorted_entries(vec![(component(".env"), hidden_value)]),
            )
            .await
            .unwrap();
        let visible = format!("visible revision {index}\n");
        let visible_value = write_file(store, &visible_path, visible.as_bytes()).await;
        let root_tree = store
            .write_tree(
                RepoPath::root(),
                BackendTree::from_sorted_entries(vec![
                    (
                        component("secrets"),
                        TreeValue::Tree(secrets_tree.id().clone()),
                    ),
                    (component("visible.txt"), visible_value),
                ]),
            )
            .await
            .unwrap();
        parent = store
            .write_commit(
                BackendCommit {
                    parents: vec![parent],
                    predecessors: Vec::new(),
                    root_tree: Merge::resolved(root_tree.id().clone()),
                    conflict_labels: Merge::resolved(String::new()),
                    change_id: ChangeId::new(vec![index as u8 + 1; store.change_id_length()]),
                    description: format!("projection fixture {index}"),
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
    let (head, hidden) = write_history(repository.repo().store()).await;
    (temp, repository, settings, head, hidden)
}

#[tokio::test]
async fn hidden_bytes_never_enter_fresh_sidecar() {
    let (temp, repository, settings, head, hidden_values) = fixture().await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let hidden = HiddenPathSet::from_paths([HiddenPath::parse("secrets/.env").unwrap()]);
    let filter = ExactPathFilter::from_hidden_paths(&hidden).unwrap();
    let exported = projection
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &filter,
            &mut ExportMappings::default(),
        )
        .await
        .unwrap();

    assert_eq!(head.as_bytes().len(), 64);
    assert_eq!(exported.git_heads[0].as_bytes().len(), 20);
    assert_eq!(exported.new_mappings.len(), 2);
    assert_hidden_bytes_absent(projection.git_repo_path(), &hidden_values);

    let projected = projection
        .store()
        .get_commit_async(&exported.git_heads[0])
        .await
        .unwrap();
    assert!(
        projected
            .tree()
            .path_value(&RepoPathBuf::from_internal_string("secrets/.env").unwrap())
            .await
            .unwrap()
            .into_resolved()
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn sidecar_rebuilds_from_durable_mapping_rows() {
    let (temp, repository, settings, head, hidden_values) = fixture().await;
    let sidecar = temp.path().join("projection");
    let hidden = HiddenPathSet::from_paths([HiddenPath::parse("secrets/.env").unwrap()]);
    let filter = ExactPathFilter::from_hidden_paths(&hidden).unwrap();
    let first = GitProjection::init(&sidecar, &settings).unwrap();
    let first_export = first
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &filter,
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
            &filter,
            &mut mappings,
        )
        .await
        .unwrap();
    assert_eq!(rebuilt_export.git_heads, [first_head]);
    assert!(rebuilt_export.new_mappings.is_empty());
    assert_hidden_bytes_absent(rebuilt.git_repo_path(), &hidden_values);
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
    let gitlink_path = RepoPathBuf::from_internal_string("vendor/dependency").unwrap();
    let vendor_tree = projection
        .store()
        .write_tree(
            RepoPath::from_internal_string("vendor").unwrap(),
            BackendTree::from_sorted_entries(vec![(
                component("dependency"),
                TreeValue::GitSubmodule(CommitId::new(vec![0x11; 20])),
            )]),
        )
        .await
        .unwrap();
    let root_tree = projection
        .store()
        .write_tree(
            RepoPath::root(),
            BackendTree::from_sorted_entries(vec![(
                component("vendor"),
                TreeValue::Tree(vendor_tree.id().clone()),
            )]),
        )
        .await
        .unwrap();
    let git_head = projection
        .store()
        .write_commit(
            BackendCommit {
                parents: vec![projection.store().root_commit_id().clone()],
                predecessors: Vec::new(),
                root_tree: Merge::resolved(root_tree.id().clone()),
                conflict_labels: Merge::resolved(String::new()),
                change_id: ChangeId::new(vec![7; projection.store().change_id_length()]),
                description: "gitlink fixture".to_owned(),
                author: signature(7),
                committer: signature(7),
                secure_sig: None,
            },
            None,
        )
        .await
        .unwrap()
        .id()
        .clone();

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
async fn policy_cutover_rejects_stale_mapping_and_creates_public_child() {
    let (temp, repository, settings, head, _) = fixture().await;
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    let initial = projection
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &ExactPathFilter::default(),
            &mut ExportMappings::default(),
        )
        .await
        .unwrap();
    let public_tip = initial.git_heads[0].clone();
    let hidden = HiddenPathSet::from_paths([HiddenPath::parse("secrets/.env").unwrap()]);
    let filter = ExactPathFilter::from_hidden_paths(&hidden).unwrap();
    assert_eq!(
        projection
            .scan_hidden_paths(&public_tip, &filter)
            .await
            .unwrap(),
        [RepoPathBuf::from_internal_string("secrets/.env").unwrap()]
    );

    let error = projection
        .export_reachable(
            repository.repo().store(),
            std::slice::from_ref(&head),
            &filter,
            &mut ExportMappings::from_rows(initial.new_mappings).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ProjectionError::StaleMapping { .. }));

    let cutover = projection
        .project_snapshot_after(repository.repo().store(), &head, &public_tip, &filter, true)
        .await
        .unwrap();
    assert_ne!(cutover, public_tip);
    assert_eq!(
        projection
            .store()
            .get_commit_async(&cutover)
            .await
            .unwrap()
            .parent_ids(),
        [public_tip]
    );
    assert!(
        projection
            .scan_hidden_paths(&cutover, &filter)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn hidden_leaf_is_filtered_before_its_bytes_are_read() {
    let (temp, repository, settings, head, _) = fixture().await;
    let hidden_path = RepoPathBuf::from_internal_string("secrets/.env").unwrap();
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
    let hidden = HiddenPathSet::from_paths([HiddenPath::parse("secrets/.env").unwrap()]);
    let projection = GitProjection::init(temp.path().join("projection"), &settings).unwrap();
    projection
        .export_reachable(
            repository.repo().store(),
            &[head],
            &ExactPathFilter::from_hidden_paths(&hidden).unwrap(),
            &mut ExportMappings::default(),
        )
        .await
        .unwrap();
}
