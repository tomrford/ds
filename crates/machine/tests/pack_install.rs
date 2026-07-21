use std::collections::BTreeSet;
use std::fs;

use devspace_machine::{
    MachineObject, MachineRepository, ObjectClosure, ObjectKey, ObjectKind, PackInstallError,
    PackOptions, build_packs,
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

async fn repository_with_file(path: &std::path::Path) -> (MachineRepository, [u8; 64]) {
    let repository = MachineRepository::init(path, &settings()).await.unwrap();
    let file_path = RepoPathBuf::from_internal_string("downloaded.txt").unwrap();
    let mut contents = b"cloud round trip".as_slice();
    let file_id = repository
        .repo()
        .store()
        .write_file(&file_path, &mut contents)
        .await
        .unwrap();
    let mut tree_builder = MergedTreeBuilder::new(repository.repo().store().empty_merged_tree());
    tree_builder.set_or_remove(
        file_path,
        Merge::normal(TreeValue::File {
            id: file_id,
            executable: false,
            copy_id: CopyId::placeholder(),
        }),
    );
    let tree = tree_builder.write_tree().await.unwrap();
    let mut transaction = repository.repo().start_transaction();
    let commit = transaction
        .repo_mut()
        .new_commit(
            vec![repository.repo().store().root_commit_id().clone()],
            tree,
        )
        .write()
        .await
        .unwrap();
    transaction.repo_mut().set_remote_bookmark(
        RemoteRefSymbol {
            name: RefName::new("cloud"),
            remote: "origin".as_ref(),
        },
        RemoteRef {
            target: RefTarget::normal(commit.id().clone()),
            state: RemoteRefState::New,
        },
    );
    let head = transaction
        .commit("cloud round trip")
        .await
        .unwrap()
        .op_id()
        .as_bytes()
        .try_into()
        .unwrap();
    (repository, head)
}

fn pack_bytes(pack: &devspace_machine::BuiltPack) -> (Vec<u8>, Vec<Vec<u8>>) {
    let manifest = fs::read(pack.directory.join("manifest.bin")).unwrap();
    let chunks = (0..pack.manifest.chunks().len())
        .map(|index| fs::read(pack.directory.join(format!("{index:08}.chunk"))).unwrap())
        .collect();
    (manifest, chunks)
}

#[tokio::test]
async fn installs_downloaded_packs_idempotently_and_reconciles_their_head() {
    let temp = tempfile::tempdir().unwrap();
    let (source, source_head) = repository_with_file(&temp.path().join("source")).await;
    let closure = source.object_closure(&BTreeSet::new()).await.unwrap();
    let built = build_packs(
        &closure,
        &BTreeSet::new(),
        temp.path().join("packs"),
        PackOptions::default(),
    )
    .unwrap();
    assert_eq!(built.packs.len(), 1);
    let pack = &built.packs[0];
    let (manifest, chunks) = pack_bytes(pack);

    let mut destination = MachineRepository::init(temp.path().join("destination"), &settings())
        .await
        .unwrap();
    let installed = destination
        .install_pack(pack.id, &manifest, &chunks)
        .unwrap();
    assert_eq!(installed.operation_heads, vec![source_head]);
    assert_eq!(installed.inserted_objects, closure.objects.len());
    assert_eq!(installed.existing_objects, 0);

    let retried = destination
        .install_pack(pack.id, &manifest, &chunks)
        .unwrap();
    assert_eq!(retried.inserted_objects, 0);
    assert_eq!(retried.existing_objects, closure.objects.len());
    let reconciled = destination
        .reconcile_operation_heads(&BTreeSet::from([source_head]))
        .await
        .unwrap();
    assert_eq!(reconciled.as_bytes(), source_head);
    let remote = destination
        .repo()
        .view()
        .get_remote_bookmark(RemoteRefSymbol {
            name: RefName::new("cloud"),
            remote: "origin".as_ref(),
        });
    assert!(remote.target.is_present());
}

#[tokio::test]
async fn installs_a_zero_length_object_from_a_downloaded_pack() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("objects");
    fs::create_dir(&sources).unwrap();
    let fixtures = [
        (ObjectKind::File, b"before".as_slice()),
        (ObjectKind::Tree, b"".as_slice()),
        (
            ObjectKind::View,
            [0x4a, 0x04, 0x1a, 0x02, 0x12, 0x00, 0x60, 0x01].as_slice(),
        ),
    ];
    let objects = fixtures
        .into_iter()
        .enumerate()
        .map(|(index, (kind, bytes))| {
            let path = sources.join(format!("object-{index}"));
            fs::write(&path, bytes).unwrap();
            MachineObject {
                key: ObjectKey {
                    kind,
                    id: devspace_kernel::validate(kind, bytes).unwrap().id,
                },
                path,
                length: bytes.len() as u64,
            }
        })
        .collect::<Vec<_>>();
    let empty_tree = objects[1].key;
    let built = build_packs(
        &ObjectClosure {
            operation_heads: Vec::new(),
            objects,
        },
        &BTreeSet::new(),
        temp.path().join("packs"),
        PackOptions::default(),
    )
    .unwrap();
    let pack = &built.packs[0];
    assert_eq!(pack.manifest.objects()[1].key, empty_tree);
    assert_eq!(pack.manifest.objects()[1].length, 0);
    let (manifest, chunks) = pack_bytes(pack);

    let destination_path = temp.path().join("destination");
    let destination = MachineRepository::init(&destination_path, &settings())
        .await
        .unwrap();
    let empty_tree_path = destination_path.join("store/trees").join(
        empty_tree
            .id
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    );
    assert_eq!(fs::metadata(&empty_tree_path).unwrap().len(), 0);
    fs::remove_file(&empty_tree_path).unwrap();

    let installed = destination
        .install_pack(pack.id, &manifest, &chunks)
        .unwrap();
    assert_eq!(installed.inserted_objects, 3);
    assert_eq!(fs::metadata(empty_tree_path).unwrap().len(), 0);
}

#[tokio::test]
async fn rejects_a_corrupt_download_before_publishing_its_head() {
    let temp = tempfile::tempdir().unwrap();
    let (source, _) = repository_with_file(&temp.path().join("source")).await;
    let closure = source.object_closure(&BTreeSet::new()).await.unwrap();
    let built = build_packs(
        &closure,
        &BTreeSet::new(),
        temp.path().join("packs"),
        PackOptions::default(),
    )
    .unwrap();
    let pack = &built.packs[0];
    let (manifest, mut chunks) = pack_bytes(pack);
    chunks[0][0] ^= 1;

    let destination = MachineRepository::init(temp.path().join("destination"), &settings())
        .await
        .unwrap();
    let local_head = destination.repo().op_id().clone();
    assert!(matches!(
        destination.install_pack(pack.id, &manifest, &chunks),
        Err(PackInstallError::ChunkHash { index: 0 })
    ));
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
