use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};

use devspace_machine::{MAX_OBJECT_BYTES, MachineRepository, ObjectClosureError, ObjectKind};
use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::merge::Merge;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteRefSymbol};
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;

mod common;

use common::settings;

fn set_remote_bookmark(
    transaction: &mut jj_lib::transaction::Transaction,
    name: &str,
    root_id: jj_lib::backend::CommitId,
) {
    let symbol = RemoteRefSymbol {
        name: RefName::new(name),
        remote: "origin".as_ref(),
    };
    transaction.repo_mut().set_remote_bookmark(
        symbol,
        RemoteRef {
            target: RefTarget::normal(root_id),
            state: RemoteRefState::New,
        },
    );
}

#[tokio::test]
async fn discovers_divergent_heads_and_stops_at_the_accepted_frontier() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let machine_repo = MachineRepository::init(temp.path(), &settings)
        .await
        .unwrap();
    let root_id = machine_repo.repo().store().root_commit_id().clone();

    let mut first = machine_repo.repo().start_transaction();
    let mut second = machine_repo.repo().start_transaction();
    set_remote_bookmark(&mut first, "first", root_id.clone());
    set_remote_bookmark(&mut second, "second", root_id);
    let first_repo = first.commit("first offline operation").await.unwrap();
    let second_repo = second.commit("second offline operation").await.unwrap();
    let first_head: [u8; 64] = first_repo.op_id().as_bytes().try_into().unwrap();
    let second_head: [u8; 64] = second_repo.op_id().as_bytes().try_into().unwrap();

    let closure = machine_repo.object_closure(&BTreeSet::new()).await.unwrap();
    let mut expected_heads = vec![first_head, second_head];
    expected_heads.sort_unstable();
    assert_eq!(closure.operation_heads, expected_heads);
    assert_eq!(closure.objects.len(), 4);
    assert_eq!(
        closure
            .objects
            .iter()
            .map(|object| object.key.kind)
            .collect::<Vec<_>>(),
        vec![
            ObjectKind::View,
            ObjectKind::View,
            ObjectKind::Operation,
            ObjectKind::Operation,
        ]
    );
    assert!(
        closure
            .objects
            .iter()
            .all(|object| object.length > 0 && object.path.is_file())
    );

    let accepted = BTreeSet::from([first_head]);
    let pending = machine_repo.object_closure(&accepted).await.unwrap();
    assert_eq!(pending.operation_heads, expected_heads);
    assert_eq!(pending.objects.len(), 2);
    assert!(
        pending
            .objects
            .iter()
            .any(|object| object.key.id == second_head)
    );
    assert!(
        pending
            .objects
            .iter()
            .all(|object| object.key.id != first_head)
    );
}

#[tokio::test]
async fn walks_every_simple_backend_object_path_and_reports_a_missing_leaf() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let machine_repo = MachineRepository::init(temp.path(), &settings)
        .await
        .unwrap();
    let file_path = RepoPathBuf::from_internal_string("README.md").unwrap();
    let symlink_path = RepoPathBuf::from_internal_string("current").unwrap();
    let mut contents = b"hello\n".as_slice();
    let file_id = machine_repo
        .repo()
        .store()
        .write_file(&file_path, &mut contents)
        .await
        .unwrap();
    let symlink_id = machine_repo
        .repo()
        .store()
        .write_symlink(&symlink_path, "README.md")
        .await
        .unwrap();
    let mut tree_builder = MergedTreeBuilder::new(machine_repo.repo().store().empty_merged_tree());
    tree_builder.set_or_remove(
        file_path,
        Merge::normal(TreeValue::File {
            id: file_id.clone(),
            executable: false,
            copy_id: CopyId::placeholder(),
        }),
    );
    tree_builder.set_or_remove(
        symlink_path,
        Merge::normal(TreeValue::Symlink(symlink_id.clone())),
    );
    let tree = tree_builder.write_tree().await.unwrap();
    let tree_id = tree.tree_ids().as_resolved().unwrap().clone();
    let mut transaction = machine_repo.repo().start_transaction();
    let commit = transaction
        .repo_mut()
        .new_commit(
            vec![machine_repo.repo().store().root_commit_id().clone()],
            tree,
        )
        .write()
        .await
        .unwrap();
    let commit_id = commit.id().clone();
    transaction
        .commit("commit every object kind")
        .await
        .unwrap();

    let closure = machine_repo.object_closure(&BTreeSet::new()).await.unwrap();
    assert_eq!(
        closure
            .objects
            .iter()
            .map(|object| object.key.kind)
            .collect::<Vec<_>>(),
        vec![
            ObjectKind::File,
            ObjectKind::Symlink,
            ObjectKind::Tree,
            ObjectKind::Commit,
            ObjectKind::View,
            ObjectKind::Operation,
        ]
    );
    assert_eq!(
        closure,
        machine_repo.object_closure(&BTreeSet::new()).await.unwrap()
    );

    let expected = [
        (ObjectKind::File, file_id.hex(), "store/files", 6),
        (ObjectKind::Symlink, symlink_id.hex(), "store/symlinks", 9),
        (ObjectKind::Tree, tree_id.hex(), "store/trees", 0),
        (ObjectKind::Commit, commit_id.hex(), "store/commits", 0),
    ];
    for (kind, id, directory, leaf_length) in expected {
        let object = closure
            .objects
            .iter()
            .find(|object| object.key.kind == kind)
            .unwrap();
        assert_eq!(object.path, temp.path().join(directory).join(id));
        if leaf_length > 0 {
            assert_eq!(object.length, leaf_length);
        }
    }

    let file_object = closure
        .objects
        .iter()
        .find(|object| object.key.kind == ObjectKind::File)
        .unwrap();
    fs::rename(&file_object.path, temp.path().join("missing-file-object")).unwrap();
    let error = machine_repo
        .object_closure(&BTreeSet::new())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ObjectClosureError::ReadObject { key, .. } if key.kind == ObjectKind::File
    ));
}

#[tokio::test]
async fn fails_closed_when_a_structured_object_is_corrupt() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let machine_repo = MachineRepository::init(temp.path(), &settings)
        .await
        .unwrap();
    let mut transaction = machine_repo.repo().start_transaction();
    set_remote_bookmark(
        &mut transaction,
        "main",
        machine_repo.repo().store().root_commit_id().clone(),
    );
    transaction.commit("operation").await.unwrap();

    let closure = machine_repo.object_closure(&BTreeSet::new()).await.unwrap();
    let operation = closure
        .objects
        .iter()
        .find(|object| object.key.kind == ObjectKind::Operation)
        .unwrap();
    fs::write(&operation.path, b"not protobuf").unwrap();

    let error = machine_repo
        .object_closure(&BTreeSet::new())
        .await
        .unwrap_err();
    assert!(matches!(error, ObjectClosureError::ValidateObject { .. }));
}

#[tokio::test]
async fn rejects_oversized_structured_objects_before_buffering_them() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let machine_repo = MachineRepository::init(temp.path(), &settings)
        .await
        .unwrap();
    let mut transaction = machine_repo.repo().start_transaction();
    set_remote_bookmark(
        &mut transaction,
        "main",
        machine_repo.repo().store().root_commit_id().clone(),
    );
    transaction.commit("operation").await.unwrap();

    let closure = machine_repo.object_closure(&BTreeSet::new()).await.unwrap();
    let operation = closure
        .objects
        .iter()
        .find(|object| object.key.kind == ObjectKind::Operation)
        .unwrap();
    OpenOptions::new()
        .write(true)
        .open(&operation.path)
        .unwrap()
        .set_len(MAX_OBJECT_BYTES + 1)
        .unwrap();

    let error = machine_repo
        .object_closure(&BTreeSet::new())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ObjectClosureError::StructuredObjectTooLarge {
            length,
            limit: MAX_OBJECT_BYTES,
            ..
        } if length == MAX_OBJECT_BYTES + 1
    ));
}
