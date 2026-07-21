use std::collections::BTreeSet;
use std::fs;

use devspace_machine::{
    MAX_CHUNK_BYTES, MAX_PACK_BYTES, MAX_PACK_OBJECTS, MIN_CHUNK_BYTES, MIN_PACK_BYTES,
    MachineObject, MachineRepository, ObjectClosure, ObjectKey, ObjectKind, PackBuildError,
    PackOptions, build_packs,
};
use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::merge::Merge;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;

mod common;

use common::settings;

async fn repository_with_file() -> (tempfile::TempDir, MachineRepository, ObjectClosure) {
    let temp = tempfile::tempdir().unwrap();
    let machine_repo = MachineRepository::init(temp.path(), &settings())
        .await
        .unwrap();
    let file_path = RepoPathBuf::from_internal_string("README.md").unwrap();
    let mut contents = b"deterministic pack contents\n".as_slice();
    let file_id = machine_repo
        .repo()
        .store()
        .write_file(&file_path, &mut contents)
        .await
        .unwrap();
    let mut tree_builder = MergedTreeBuilder::new(machine_repo.repo().store().empty_merged_tree());
    tree_builder.set_or_remove(
        file_path,
        Merge::normal(TreeValue::File {
            id: file_id,
            executable: false,
            copy_id: CopyId::placeholder(),
        }),
    );
    let tree = tree_builder.write_tree().await.unwrap();
    let mut transaction = machine_repo.repo().start_transaction();
    transaction
        .repo_mut()
        .new_commit(
            vec![machine_repo.repo().store().root_commit_id().clone()],
            tree,
        )
        .write()
        .await
        .unwrap();
    transaction.commit("create pack fixture").await.unwrap();

    let closure = machine_repo.object_closure(&BTreeSet::new()).await.unwrap();
    (temp, machine_repo, closure)
}

fn chunk_bytes(pack: &devspace_machine::BuiltPack) -> Vec<u8> {
    let mut bytes = Vec::new();
    for index in 0..pack.manifest.chunks().len() {
        bytes.extend(fs::read(pack.directory.join(format!("{index:08}.chunk"))).unwrap());
    }
    bytes
}

#[tokio::test]
async fn builds_deterministic_bounded_packs_and_skips_cloud_objects() {
    let (_temp, _machine_repo, closure) = repository_with_file().await;
    let known = closure
        .objects
        .iter()
        .find(|object| object.key.kind == ObjectKind::File)
        .unwrap()
        .key;
    let cloud_objects = BTreeSet::from([known]);
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();

    let options = PackOptions::default();
    let first = build_packs(&closure, &cloud_objects, first_root.path(), options).unwrap();
    let second = build_packs(&closure, &cloud_objects, second_root.path(), options).unwrap();
    let reused = build_packs(&closure, &cloud_objects, first_root.path(), options).unwrap();

    assert_eq!(first.packs.len(), 1);
    assert_eq!(first.packs[0].id, second.packs[0].id);
    assert_eq!(first.packs[0].id, reused.packs[0].id);
    assert_eq!(first.packs[0].manifest, second.packs[0].manifest);
    assert_eq!(first.packs[0].manifest, reused.packs[0].manifest);
    let first_pack = &first.packs[0];
    let second_pack = &second.packs[0];
    assert_eq!(first_pack.manifest.encode(), second_pack.manifest.encode());
    assert_eq!(chunk_bytes(first_pack), chunk_bytes(second_pack));
    assert_eq!(first.metrics.discovered_objects, closure.objects.len());
    assert_eq!(first.metrics.skipped_known_objects, 1);
    assert_eq!(first.metrics.packed_objects, closure.objects.len() - 1);
    assert_eq!(
        first.metrics.packed_bytes,
        first_pack.manifest.pack_length()
    );
    assert!(
        !first_pack
            .manifest
            .objects()
            .iter()
            .any(|object| object.key == known)
    );
    assert!(
        first_pack
            .manifest
            .objects()
            .windows(2)
            .all(|pair| pair[0].key < pair[1].key)
    );

    let packed = chunk_bytes(first_pack);
    let expected = first_pack
        .manifest
        .objects()
        .iter()
        .flat_map(|entry| {
            let object = closure
                .objects
                .iter()
                .find(|object| object.key == entry.key)
                .unwrap();
            let bytes = fs::read(&object.path).unwrap();
            assert_eq!(entry.length, bytes.len() as u64);
            bytes
        })
        .collect::<Vec<_>>();
    assert_eq!(packed, expected);

    let mut offset = 0_u64;
    for (index, chunk) in first_pack.manifest.chunks().iter().enumerate() {
        assert_eq!(chunk.offset, offset);
        assert!(chunk.length <= options.chunk_bytes);
        if index + 1 < first_pack.manifest.chunks().len() {
            assert_eq!(chunk.length, options.chunk_bytes);
        }
        offset += u64::from(chunk.length);
    }
    assert_eq!(offset, first_pack.manifest.pack_length());
}

#[test]
fn splits_large_closures_at_deterministic_pack_boundaries() {
    let sources = tempfile::tempdir().unwrap();
    let objects = [0x35, 0xa7]
        .into_iter()
        .enumerate()
        .map(|(index, byte)| {
            let bytes = vec![byte; 600 * 1024];
            let path = sources.path().join(format!("object-{index}"));
            fs::write(&path, &bytes).unwrap();
            MachineObject {
                key: ObjectKey {
                    kind: ObjectKind::File,
                    id: devspace_kernel::validate(ObjectKind::File, &bytes)
                        .unwrap()
                        .id,
                },
                path,
                length: bytes.len() as u64,
            }
        })
        .collect();
    let closure = ObjectClosure {
        operation_heads: Vec::new(),
        objects,
    };
    let options = PackOptions {
        chunk_bytes: MIN_CHUNK_BYTES,
        pack_bytes: MIN_PACK_BYTES,
        pack_objects: MAX_PACK_OBJECTS,
    };
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    let first = build_packs(&closure, &BTreeSet::new(), first_root.path(), options).unwrap();
    let second = build_packs(&closure, &BTreeSet::new(), second_root.path(), options).unwrap();

    assert_eq!(first.packs.len(), 2);
    assert_eq!(first.metrics.packed_bytes, 2 * 600 * 1024);
    for (first_pack, second_pack) in first.packs.iter().zip(&second.packs) {
        assert_eq!(first_pack.id, second_pack.id);
        assert_eq!(first_pack.manifest, second_pack.manifest);
        assert_eq!(first_pack.manifest.objects().len(), 1);
        assert!(first_pack.manifest.pack_length() <= options.pack_bytes);
        assert!(
            first_pack
                .manifest
                .chunks()
                .iter()
                .all(|chunk| chunk.length <= options.chunk_bytes)
        );
    }
}

#[tokio::test]
async fn revalidates_sources_and_existing_pack_chunks() {
    let (_temp, _machine_repo, closure) = repository_with_file().await;
    let packs = tempfile::tempdir().unwrap();
    let options = PackOptions::default();
    let built = build_packs(&closure, &BTreeSet::new(), packs.path(), options).unwrap();
    let first_chunk = built.packs[0].directory.join("00000000.chunk");
    let mut corrupt_chunk = fs::read(&first_chunk).unwrap();
    corrupt_chunk[0] ^= 0xff;
    fs::write(&first_chunk, corrupt_chunk).unwrap();
    assert!(matches!(
        build_packs(&closure, &BTreeSet::new(), packs.path(), options),
        Err(PackBuildError::ExistingPackInvalid { .. })
    ));

    let file = closure
        .objects
        .iter()
        .find(|object| object.key.kind == ObjectKind::File)
        .unwrap();
    fs::write(&file.path, b"changed after discovery\n").unwrap();
    assert!(matches!(
        build_packs(
            &closure,
            &BTreeSet::new(),
            tempfile::tempdir().unwrap().path(),
            options,
        ),
        Err(PackBuildError::ObjectIdMismatch { key, .. }) if key == file.key
    ));
}

#[tokio::test]
async fn rejects_invalid_chunk_sizes() {
    let (_temp, _machine_repo, closure) = repository_with_file().await;
    let packs = tempfile::tempdir().unwrap();
    for chunk_bytes in [MIN_CHUNK_BYTES - 1, MAX_CHUNK_BYTES + 1] {
        assert!(matches!(
            build_packs(
                &closure,
                &BTreeSet::new(),
                packs.path(),
                PackOptions {
                    chunk_bytes,
                    ..PackOptions::default()
                },
            ),
            Err(PackBuildError::InvalidChunkSize { .. })
        ));
    }
    for options in [
        PackOptions {
            pack_bytes: MIN_PACK_BYTES - 1,
            ..PackOptions::default()
        },
        PackOptions {
            pack_bytes: MAX_PACK_BYTES + 1,
            ..PackOptions::default()
        },
        PackOptions {
            pack_objects: 0,
            ..PackOptions::default()
        },
        PackOptions {
            pack_objects: MAX_PACK_OBJECTS + 1,
            ..PackOptions::default()
        },
    ] {
        assert!(build_packs(&closure, &BTreeSet::new(), packs.path(), options).is_err());
    }
}
