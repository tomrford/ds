use std::collections::BTreeSet;
use std::fs;
use std::process::Command;

use blake2::{Blake2b512, Digest as _};
use devspace_kernel_git::{ObjectKind, Oid, validate};
use devspace_machine_git::{
    MachineGitRepository, ObjectKey, PackInstallError, PackOptions, build_packs,
};
use futures::executor::block_on;
use gix::objs::{Kind as GitObjectKind, Write as _};
use jj_lib::backend::{ChangeId, CommitId, CopyId, TreeValue};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;

fn settings() -> UserSettings {
    let mut config = StackedConfig::with_defaults();
    config.add_layer(
        ConfigLayer::parse(
            ConfigSource::User,
            r#"
                [user]
                name = "Machine Git Test"
                email = "machine-git@example.invalid"

                [git]
                write-change-id-header = true
            "#,
        )
        .unwrap(),
    );
    UserSettings::from_config(config).unwrap()
}

#[test]
fn initializes_git_odb_with_stock_operation_stores() {
    let temp = tempfile::tempdir().unwrap();
    let repository = block_on(MachineGitRepository::init(temp.path(), &settings())).unwrap();

    assert_eq!(
        repository.git_repo_path(),
        fs::canonicalize(temp.path().join("store/git")).unwrap()
    );
    assert!(repository.git_repo_path().join("objects").is_dir());
    assert!(repository.path().join("store/extra").is_dir());
    assert!(repository.operation_store_path().join("type").is_file());
    assert!(repository.operation_heads_path().join("type").is_file());
    assert!(
        gix::open(repository.git_repo_path())
            .unwrap()
            .workdir()
            .is_none()
    );
}

#[test]
fn discovers_packed_foreign_history_equal_to_git_rev_list() {
    let temp = tempfile::tempdir().unwrap();
    let repository = block_on(MachineGitRepository::init(temp.path(), &settings())).unwrap();
    let foreign = write_foreign_commit(&repository, b"packed closure\n");
    run_git(
        &repository,
        &["update-ref", "refs/heads/foreign", &oid_hex(foreign)],
    );
    run_git(&repository, &["repack", "-ad"]);
    let pack_directory = repository.git_repo_path().join("objects/pack");
    assert!(fs::read_dir(&pack_directory).unwrap().any(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|ext| ext == "pack")
    }));
    let foreign_hex = oid_hex(foreign);
    assert!(
        !repository
            .git_repo_path()
            .join("objects")
            .join(&foreign_hex[..2])
            .join(&foreign_hex[2..])
            .exists()
    );

    let closure = repository.object_closure([foreign]).unwrap();
    let output = run_git(&repository, &["rev-list", "--objects", &oid_hex(foreign)]);
    let expected = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| Oid::from_hex(line.split_whitespace().next().unwrap().as_bytes()).unwrap())
        .collect::<BTreeSet<_>>();
    let actual = closure
        .objects
        .iter()
        .map(|object| object.key.id)
        .collect::<BTreeSet<_>>();

    assert_eq!(actual, expected);
    assert_eq!(
        closure
            .objects
            .iter()
            .map(|object| object.key.kind)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([ObjectKind::Blob, ObjectKind::Tree, ObjectKind::Commit])
    );
}

#[test]
fn deterministic_pack_rebuilds_exact_objects_and_semantics_without_extras() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source");
    let source = block_on(MachineGitRepository::init(&source_path, &settings())).unwrap();
    let (conflict_commit, foreign_commit) = build_semantic_fixture(&source);
    let closure = source
        .object_closure([conflict_commit, foreign_commit])
        .unwrap();

    let first = build_packs(&source, &closure, &BTreeSet::new(), PackOptions::default()).unwrap();
    let second = build_packs(&source, &closure, &BTreeSet::new(), PackOptions::default()).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.packs.len(), 1);
    assert_eq!(first.metrics.packed_objects, closure.objects.len());
    let pack = &first.packs[0];
    assert_eq!(pack.manifest.encode(), pack.manifest_bytes);

    let destination_path = temp.path().join("destination");
    let destination = block_on(MachineGitRepository::init(&destination_path, &settings())).unwrap();
    let installed = destination
        .install_pack(pack.id, &pack.manifest_bytes, &pack.chunks)
        .unwrap();
    assert_eq!(installed.head_commits, closure.head_commits);
    assert_eq!(installed.inserted_objects, closure.objects.len());
    assert_eq!(installed.existing_objects, 0);

    for object in &closure.objects {
        assert_eq!(
            read_raw(&source, object.key),
            read_raw(&destination, object.key),
            "raw bytes differ for {:?}",
            object.key
        );
    }
    let retried = destination
        .install_pack(pack.id, &pack.manifest_bytes, &pack.chunks)
        .unwrap();
    assert_eq!(retried.inserted_objects, 0);
    assert_eq!(retried.existing_objects, closure.objects.len());

    drop(destination);
    fs::remove_dir_all(destination_path.join("store/extra")).unwrap();
    assert!(!destination_path.join("store/extra").exists());
    let rebuilt = block_on(MachineGitRepository::open(&destination_path, &settings())).unwrap();
    let commit_ids = closure
        .objects
        .iter()
        .filter(|object| object.key.kind == ObjectKind::Commit)
        .map(|object| CommitId::from_bytes(&object.key.id.0))
        .collect::<Vec<_>>();
    for commit_id in &commit_ids {
        let source_commit =
            block_on(source.repo().store().backend().read_commit(commit_id)).unwrap();
        let rebuilt_commit =
            block_on(rebuilt.repo().store().backend().read_commit(commit_id)).unwrap();
        assert_eq!(source_commit.change_id, rebuilt_commit.change_id);
        assert_eq!(source_commit.root_tree, rebuilt_commit.root_tree);
        assert_eq!(
            source_commit.conflict_labels,
            rebuilt_commit.conflict_labels
        );
        assert_eq!(source_commit, rebuilt_commit);
    }
    let foreign_id = CommitId::from_bytes(&foreign_commit.0);
    let foreign = block_on(rebuilt.repo().store().backend().read_commit(&foreign_id)).unwrap();
    assert_eq!(foreign.change_id.as_bytes().len(), 16);
}

#[test]
fn install_checks_references_and_refuses_a_consistently_rehashed_corrupt_pack() {
    let temp = tempfile::tempdir().unwrap();
    let source = block_on(MachineGitRepository::init(
        temp.path().join("source"),
        &settings(),
    ))
    .unwrap();
    let foreign = write_foreign_commit(&source, b"no clobber\n");
    let closure = source.object_closure([foreign]).unwrap();
    let blob = closure
        .objects
        .iter()
        .find(|object| object.key.kind == ObjectKind::Blob)
        .unwrap()
        .key;

    let missing_dependency_pack = build_packs(
        &source,
        &closure,
        &BTreeSet::from([blob]),
        PackOptions::default(),
    )
    .unwrap();
    let empty = block_on(MachineGitRepository::init(
        temp.path().join("empty"),
        &settings(),
    ))
    .unwrap();
    let missing = &missing_dependency_pack.packs[0];
    assert!(matches!(
        empty.install_pack(missing.id, &missing.manifest_bytes, &missing.chunks),
        Err(PackInstallError::MissingReference { target, .. }) if target == blob.id
    ));

    let built = build_packs(&source, &closure, &BTreeSet::new(), PackOptions::default()).unwrap();
    let pack = &built.packs[0];
    let destination = block_on(MachineGitRepository::init(
        temp.path().join("destination"),
        &settings(),
    ))
    .unwrap();
    destination
        .install_pack(pack.id, &pack.manifest_bytes, &pack.chunks)
        .unwrap();
    let first_key = pack.manifest.objects()[0].key;
    let before = read_raw(&destination, first_key);

    let mut corrupt_manifest = pack.manifest_bytes.clone();
    let mut corrupt_chunks = pack.chunks.clone();
    corrupt_chunks[0][0] ^= 1;
    corrupt_manifest[32..96].copy_from_slice(&hash64(
        &corrupt_chunks.iter().flatten().copied().collect::<Vec<_>>(),
    ));
    let chunk_table =
        96 + pack.manifest.head_commits().len() * Oid::LENGTH + pack.manifest.objects().len() * 44;
    corrupt_manifest[chunk_table + 16..chunk_table + 80]
        .copy_from_slice(&hash64(&corrupt_chunks[0]));
    let corrupt_id = hash64(&corrupt_manifest);

    assert!(matches!(
        destination.install_pack(corrupt_id, &corrupt_manifest, &corrupt_chunks),
        Err(PackInstallError::ExistingObjectMismatch(key)) if key == first_key
    ));
    assert_eq!(read_raw(&destination, first_key), before);
}

fn build_semantic_fixture(repository: &MachineGitRepository) -> (Oid, Oid) {
    let store = repository.repo().store();
    let left = block_on(tree_with_file(store, "left.txt", b"left\n"));
    let base = block_on(tree_with_file(store, "base.txt", b"base\n"));
    let right = block_on(tree_with_file(store, "right.txt", b"right\n"));
    let mut transaction = repository.repo().start_transaction();
    let parent = block_on(
        transaction
            .repo_mut()
            .new_commit(vec![store.root_commit_id().clone()], left.clone())
            .set_change_id(ChangeId::new(vec![0x11; 16]))
            .set_description("parent")
            .write(),
    )
    .unwrap();
    let conflict_tree = MergedTree::new(
        store.clone(),
        Merge::from_vec(vec![
            left.tree_ids().as_resolved().unwrap().clone(),
            base.tree_ids().as_resolved().unwrap().clone(),
            right.tree_ids().as_resolved().unwrap().clone(),
        ]),
        ConflictLabels::from_vec(vec!["left".into(), "base".into(), "right".into()]),
    );
    let conflict = block_on(
        transaction
            .repo_mut()
            .new_commit(vec![parent.id().clone()], conflict_tree)
            .set_change_id(ChangeId::new(vec![0x22; 16]))
            .set_description("conflicted root tree")
            .write(),
    )
    .unwrap();
    block_on(transaction.commit("semantic fixture")).unwrap();

    let foreign = write_foreign_commit(repository, b"foreign synthetic change id\n");
    (Oid(conflict.id().as_bytes().try_into().unwrap()), foreign)
}

async fn tree_with_file(
    store: &std::sync::Arc<jj_lib::store::Store>,
    name: &str,
    contents: &[u8],
) -> MergedTree {
    let path = RepoPathBuf::from_internal_string(name).unwrap();
    let mut reader = contents;
    let file_id = store.write_file(&path, &mut reader).await.unwrap();
    let mut builder = MergedTreeBuilder::new(store.empty_merged_tree());
    builder.set_or_remove(
        path,
        Merge::normal(TreeValue::File {
            id: file_id,
            executable: false,
            copy_id: CopyId::placeholder(),
        }),
    );
    builder.write_tree().await.unwrap()
}

fn write_foreign_commit(repository: &MachineGitRepository, contents: &[u8]) -> Oid {
    let blob = write_raw(repository, ObjectKind::Blob, contents);
    let mut tree = b"100644 foreign.txt\0".to_vec();
    tree.extend_from_slice(&blob.0);
    let tree = write_raw(repository, ObjectKind::Tree, &tree);
    let commit = format!(
        "tree {}\nauthor Foreign <foreign@example.invalid> 1700000000 +0000\ncommitter Foreign <foreign@example.invalid> 1700000000 +0000\n\nforeign commit\n",
        oid_hex(tree)
    );
    write_raw(repository, ObjectKind::Commit, commit.as_bytes())
}

fn write_raw(repository: &MachineGitRepository, kind: ObjectKind, bytes: &[u8]) -> Oid {
    let validated = validate(kind, bytes).unwrap();
    let git_repo = gix::open(repository.git_repo_path()).unwrap();
    let kind = match kind {
        ObjectKind::Blob => GitObjectKind::Blob,
        ObjectKind::Tree => GitObjectKind::Tree,
        ObjectKind::Commit => GitObjectKind::Commit,
    };
    let actual = git_repo.objects.write_buf(kind, bytes).unwrap();
    assert_eq!(actual.as_bytes(), validated.id.0);
    validated.id
}

fn read_raw(repository: &MachineGitRepository, key: ObjectKey) -> Vec<u8> {
    let git_repo = gix::open(repository.git_repo_path()).unwrap();
    git_repo
        .find_object(gix::ObjectId::from_bytes_or_panic(&key.id.0))
        .unwrap()
        .data
        .clone()
}

fn run_git(repository: &MachineGitRepository, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(repository.git_repo_path())
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn oid_hex(oid: Oid) -> String {
    oid.0.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hash64(bytes: &[u8]) -> [u8; 64] {
    Blake2b512::digest(bytes).into()
}
