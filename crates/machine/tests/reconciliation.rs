use std::collections::BTreeSet;
use std::fs;

use devspace_machine::{
    MachineRepository, ObjectClosure, ObjectClosureError, ObjectKind, ReconcileOperationHeadsError,
};
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

async fn record_remote_bookmark(repository: &MachineRepository, name: &str) -> [u8; 64] {
    let symbol = RemoteRefSymbol {
        name: RefName::new(name),
        remote: "origin".as_ref(),
    };
    let mut transaction = repository.repo().start_transaction();
    transaction.repo_mut().set_remote_bookmark(
        symbol,
        RemoteRef {
            target: RefTarget::normal(repository.repo().store().root_commit_id().clone()),
            state: RemoteRefState::New,
        },
    );
    transaction
        .commit(format!("record {name}"))
        .await
        .unwrap()
        .op_id()
        .as_bytes()
        .try_into()
        .unwrap()
}

async fn record_commit_with_leaves(repository: &MachineRepository) -> [u8; 64] {
    let file_path = RepoPathBuf::from_internal_string("file").unwrap();
    let symlink_path = RepoPathBuf::from_internal_string("link").unwrap();
    let mut contents = b"contents".as_slice();
    let file_id = repository
        .repo()
        .store()
        .write_file(&file_path, &mut contents)
        .await
        .unwrap();
    let symlink_id = repository
        .repo()
        .store()
        .write_symlink(&symlink_path, "file")
        .await
        .unwrap();
    let mut builder = MergedTreeBuilder::new(repository.repo().store().empty_merged_tree());
    builder.set_or_remove(
        file_path,
        Merge::normal(TreeValue::File {
            id: file_id,
            executable: false,
            copy_id: CopyId::placeholder(),
        }),
    );
    builder.set_or_remove(symlink_path, Merge::normal(TreeValue::Symlink(symlink_id)));
    let tree = builder.write_tree().await.unwrap();
    let mut transaction = repository.repo().start_transaction();
    transaction
        .repo_mut()
        .new_commit(
            vec![repository.repo().store().root_commit_id().clone()],
            tree,
        )
        .write()
        .await
        .unwrap();
    transaction
        .commit("record leaves")
        .await
        .unwrap()
        .op_id()
        .as_bytes()
        .try_into()
        .unwrap()
}

fn copy_closure(
    source: &MachineRepository,
    destination: &MachineRepository,
    closure: &ObjectClosure,
) {
    for object in &closure.objects {
        let relative = object.path.strip_prefix(source.path()).unwrap();
        let destination_path = destination.path().join(relative);
        fs::create_dir_all(destination_path.parent().unwrap()).unwrap();
        fs::copy(&object.path, destination_path).unwrap();
    }
}

fn remote_bookmark(repository: &MachineRepository, name: &str) -> RemoteRef {
    let symbol = RemoteRefSymbol {
        name: RefName::new(name),
        remote: "origin".as_ref(),
    };
    repository.repo().view().get_remote_bookmark(symbol).clone()
}

#[tokio::test]
async fn stock_jj_reconciles_offline_heads_and_both_machines_converge() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let mut left = MachineRepository::init(temp.path().join("left"), &settings)
        .await
        .unwrap();
    let mut right = MachineRepository::init(temp.path().join("right"), &settings)
        .await
        .unwrap();
    let left_head = record_remote_bookmark(&left, "left").await;
    let right_head = record_remote_bookmark(&right, "right").await;
    assert_ne!(left_head, right_head);

    let left_closure = left.object_closure(&BTreeSet::new()).await.unwrap();
    copy_closure(&left, &right, &left_closure);
    let merged_id = right
        .reconcile_operation_heads(&BTreeSet::from([left_head]))
        .await
        .unwrap();
    assert_ne!(merged_id.as_bytes(), left_head);
    assert_ne!(merged_id.as_bytes(), right_head);
    assert_eq!(
        right
            .repo()
            .operation()
            .parent_ids()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            jj_lib::op_store::OperationId::new(left_head.to_vec()),
            jj_lib::op_store::OperationId::new(right_head.to_vec()),
        ])
    );
    assert!(remote_bookmark(&right, "left").target.is_present());
    assert!(remote_bookmark(&right, "right").target.is_present());

    let retried_id = right
        .reconcile_operation_heads(&BTreeSet::from([left_head]))
        .await
        .unwrap();
    assert_eq!(retried_id, merged_id);

    let right_closure = right.object_closure(&BTreeSet::new()).await.unwrap();
    copy_closure(&right, &left, &right_closure);
    let converged_id = left
        .reconcile_operation_heads(&BTreeSet::from([merged_id.as_bytes().try_into().unwrap()]))
        .await
        .unwrap();
    assert_eq!(converged_id, merged_id);
    assert_eq!(
        remote_bookmark(&left, "left"),
        remote_bookmark(&right, "left")
    );
    assert_eq!(
        remote_bookmark(&left, "right"),
        remote_bookmark(&right, "right")
    );
}

#[tokio::test]
async fn incomplete_cloud_closure_is_rejected_before_publishing_its_head() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let source = MachineRepository::init(temp.path().join("source"), &settings)
        .await
        .unwrap();
    let mut destination = MachineRepository::init(temp.path().join("destination"), &settings)
        .await
        .unwrap();
    let cloud_head = record_remote_bookmark(&source, "cloud").await;
    let local_head = destination.repo().op_id().clone();
    let closure = source.object_closure(&BTreeSet::new()).await.unwrap();
    let operation = closure
        .objects
        .iter()
        .find(|object| object.key.kind == ObjectKind::Operation)
        .unwrap();
    let relative = operation.path.strip_prefix(source.path()).unwrap();
    let destination_path = destination.path().join(relative);
    fs::create_dir_all(destination_path.parent().unwrap()).unwrap();
    fs::copy(&operation.path, destination_path).unwrap();

    let error = destination
        .reconcile_operation_heads(&BTreeSet::from([cloud_head]))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ReconcileOperationHeadsError::ValidateClosure(ObjectClosureError::ReadObject {
            key,
            ..
        }) if key.kind == ObjectKind::View
    ));
    assert_eq!(destination.repo().op_id(), &local_head);
    assert_eq!(
        destination
            .repo()
            .op_heads_store()
            .get_op_heads()
            .await
            .unwrap(),
        vec![local_head]
    );
}

#[tokio::test]
async fn corrupt_cloud_leaves_are_rejected_before_publishing_their_head() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let source = MachineRepository::init(temp.path().join("leaf-source"), &settings)
        .await
        .unwrap();
    let cloud_head = record_commit_with_leaves(&source).await;
    let closure = source.object_closure(&BTreeSet::new()).await.unwrap();

    for kind in [ObjectKind::File, ObjectKind::Symlink] {
        let mut destination = MachineRepository::init(
            temp.path().join(format!("leaf-destination-{kind:?}")),
            &settings,
        )
        .await
        .unwrap();
        let local_head = destination.repo().op_id().clone();
        copy_closure(&source, &destination, &closure);
        let leaf = closure
            .objects
            .iter()
            .find(|object| object.key.kind == kind)
            .unwrap();
        let destination_path = destination
            .path()
            .join(leaf.path.strip_prefix(source.path()).unwrap());
        fs::write(destination_path, [0xff]).unwrap();

        let error = destination
            .reconcile_operation_heads(&BTreeSet::from([cloud_head]))
            .await
            .unwrap_err();
        match kind {
            ObjectKind::File => assert!(matches!(
                error,
                ReconcileOperationHeadsError::ValidateClosure(
                    ObjectClosureError::ObjectIdMismatch { key, .. }
                ) if key.kind == ObjectKind::File
            )),
            ObjectKind::Symlink => assert!(matches!(
                error,
                ReconcileOperationHeadsError::ValidateClosure(
                    ObjectClosureError::ValidateObject { key, .. }
                ) if key.kind == ObjectKind::Symlink
            )),
            _ => unreachable!(),
        }
        assert_eq!(destination.repo().op_id(), &local_head);
        assert_eq!(
            destination
                .repo()
                .op_heads_store()
                .get_op_heads()
                .await
                .unwrap(),
            vec![local_head]
        );
    }
}
