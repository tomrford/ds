use std::collections::BTreeSet;
use std::process::Command;

use devspace_kernel::{ObjectKind, Oid, TreeEntryKind, parse_commit, parse_tree, validate};
use devspace_machine::{
    CommitMapping, MachineGitRepository, ObjectKey, PackOptions, ProjectionError,
    ProjectionMappings, build_packs,
};
use futures::executor::block_on;
use gix::objs::{Kind as GitObjectKind, Write as _};
use jj_lib::backend::{ChangeId, CopyId, TreeValue};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;

const ROOT_SECRET: &[u8] = b"ROOT-PRIVATE-SENTINEL\0\xff";
const DIRECTORY_SECRET: &[u8] = b"DIRECTORY-PRIVATE-SENTINEL\0\xfe";
const NESTED_SECRET: &[u8] = b"NESTED-PRIVATE-SENTINEL\0\xfd";

fn settings() -> UserSettings {
    let mut config = StackedConfig::with_defaults();
    config.add_layer(
        ConfigLayer::parse(
            ConfigSource::User,
            r#"
                [user]
                name = "Projection Test"
                email = "projection@example.invalid"

                [git]
                write-change-id-header = true
            "#,
        )
        .unwrap(),
    );
    UserSettings::from_config(config).unwrap()
}

#[test]
fn hidden_free_history_is_an_object_free_identity_projection() {
    let temp = tempfile::tempdir().unwrap();
    let repository = block_on(MachineGitRepository::init(temp.path(), &settings())).unwrap();
    let blob = write_raw(&repository, ObjectKind::Blob, b"public bytes\n");
    let tree = write_tree(&repository, &[(b"100644", b"public.txt", blob)]);
    let base = write_commit(&repository, tree, &[], &[], b"base\n");
    let signed = write_commit(
        &repository,
        tree,
        &[base],
        &[
            (b"encoding", b"ISO-8859-1"),
            (b"change-id", b"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"),
            (b"x-foreign", b"opaque bytes"),
            (
                b"gpgsig",
                b"-----BEGIN PGP SIGNATURE-----\n fake-signature\n -----END PGP SIGNATURE-----",
            ),
            (
                b"mergetag",
                b"object 0000000000000000000000000000000000000000\n type commit\n tag identity",
            ),
        ],
        b"signed identity\n",
    );
    let before = all_objects(&repository);
    let canonical_bytes = read_raw(
        &repository,
        ObjectKey {
            kind: ObjectKind::Commit,
            id: signed,
        },
    );

    let mut mappings = ProjectionMappings::default();
    let result = block_on(repository.project_hidden_paths(&[base, signed], &mut mappings)).unwrap();

    assert_eq!(result.public_heads, vec![base, signed]);
    assert!(result.new_mappings.is_empty());
    assert!(mappings.rows().next().is_none());
    assert_eq!(all_objects(&repository), before);
    assert_eq!(
        read_raw(
            &repository,
            ObjectKey {
                kind: ObjectKind::Commit,
                id: result.public_heads[1],
            },
        ),
        canonical_bytes
    );
}

#[test]
fn hidden_projection_is_minimal_deterministic_and_cloud_durable() {
    let temp = tempfile::tempdir().unwrap();
    let source = block_on(MachineGitRepository::init(
        temp.path().join("source"),
        &settings(),
    ))
    .unwrap();
    let fixture = build_hidden_fixture(&source);

    let mut mappings = ProjectionMappings::default();
    let projected = block_on(source.project_hidden_paths(&[fixture.merge], &mut mappings)).unwrap();
    let public_head = projected.public_heads[0];
    assert_eq!(projected.new_mappings.len(), 2);
    assert_eq!(
        mappings
            .rows()
            .map(|row| row.canonical_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([fixture.hidden, fixture.merge])
    );
    assert!(!mappings.rows().any(|row| row.canonical_id == fixture.clean));
    assert!(!mappings.rows().any(|row| row.canonical_id == fixture.base));

    let public_merge = parse_commit_bytes(&source, public_head);
    let public_hidden = mappings
        .rows()
        .find(|row| row.canonical_id == fixture.hidden)
        .unwrap()
        .public_id;
    assert_eq!(public_merge.parents, vec![public_hidden, fixture.clean]);

    let rewritten_hidden = read_commit_bytes(&source, public_hidden);
    assert!(
        !rewritten_hidden
            .windows(b"gpgsig".len())
            .any(|w| w == b"gpgsig")
    );
    assert!(
        !rewritten_hidden
            .windows(b"mergetag".len())
            .any(|w| w == b"mergetag")
    );
    assert!(
        rewritten_hidden
            .windows(b"change-id zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".len())
            .any(|w| w == b"change-id zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")
    );
    assert!(
        rewritten_hidden
            .windows(b"x-foreign retained".len())
            .any(|w| w == b"x-foreign retained")
    );

    let hidden_commit = parse_commit_bytes(&source, public_hidden);
    assert_eq!(
        tree_entry(&source, hidden_commit.tree, b"shared")
            .unwrap()
            .2,
        fixture.shared_tree
    );
    assert!(tree_entry(&source, hidden_commit.tree, b".dsprivate").is_none());
    assert!(tree_entry(&source, hidden_commit.tree, b"secret.bin").is_none());
    assert!(tree_entry(&source, hidden_commit.tree, b"private-dir").is_none());
    let nested = tree_entry(&source, hidden_commit.tree, b"nested")
        .unwrap()
        .2;
    assert!(tree_entry(&source, nested, b".dsprivate").is_none());
    assert!(tree_entry(&source, nested, b"hidden.key").is_none());
    assert!(tree_entry(&source, nested, b"keep.key").is_some());

    let hidden_set = block_on(source.hidden_set_for_commit(fixture.merge)).unwrap();
    match source.scan_hidden_paths(fixture.merge, &hidden_set) {
        Err(ProjectionError::HiddenPathLeak { leaked, .. }) => {
            let leaked = leaked
                .iter()
                .map(|path| path.as_internal_file_string())
                .collect::<BTreeSet<_>>();
            assert!(leaked.contains(".dsprivate"));
            assert!(leaked.contains("private-dir"));
            assert!(leaked.contains("secret.bin"));
        }
        result => panic!("expected a typed hidden-path leak, got {result:?}"),
    }
    source.scan_hidden_paths(public_head, &hidden_set).unwrap();
    assert_no_private_sentinel(&source, public_head);

    let public_bytes = read_commit_bytes(&source, public_head);
    let mut second_mappings = ProjectionMappings::default();
    let second =
        block_on(source.project_hidden_paths(&[fixture.merge], &mut second_mappings)).unwrap();
    assert_eq!(second.public_heads, vec![public_head]);
    assert_eq!(
        read_commit_bytes(&source, second.public_heads[0]),
        public_bytes
    );
    assert_eq!(second.new_mappings, projected.new_mappings);

    let before_seeded = all_objects(&source);
    let mut seeded = mappings.clone();
    let seeded_result =
        block_on(source.project_hidden_paths(&[fixture.merge], &mut seeded)).unwrap();
    assert_eq!(seeded_result.public_heads, vec![public_head]);
    assert!(seeded_result.new_mappings.is_empty());
    assert_eq!(seeded_result.reached_mappings.len(), 1);
    assert_eq!(all_objects(&source), before_seeded);

    let closure = source.object_closure([public_head]).unwrap();
    assert!(
        !closure
            .objects
            .iter()
            .any(|object| object.key.id == fixture.hidden)
    );
    let built = build_packs(&source, &closure, &BTreeSet::new(), PackOptions::default()).unwrap();
    let destination = block_on(MachineGitRepository::init(
        temp.path().join("destination"),
        &settings(),
    ))
    .unwrap();
    for pack in &built.packs {
        destination
            .install_pack(pack.id, &pack.manifest_bytes, &pack.chunks)
            .unwrap();
    }
    assert_eq!(read_commit_bytes(&destination, public_head), public_bytes);
    let installed_commit = block_on(
        destination
            .repo()
            .store()
            .backend()
            .read_commit(&jj_lib::backend::CommitId::from_bytes(&public_head.0)),
    )
    .unwrap();
    assert_eq!(
        installed_commit.parents,
        public_merge
            .parents
            .iter()
            .map(|id| jj_lib::backend::CommitId::from_bytes(&id.0))
            .collect::<Vec<_>>()
    );
    assert_eq!(destination.object_closure([public_head]).unwrap(), closure);
    assert_no_private_sentinel(&destination, public_head);
}

#[test]
fn rewritten_parent_forces_a_clean_descendant_rewrite() {
    let temp = tempfile::tempdir().unwrap();
    let repository = block_on(MachineGitRepository::init(temp.path(), &settings())).unwrap();
    let fixture = build_hidden_fixture(&repository);
    let clean_tree = write_tree(
        &repository,
        &[(b"100644", b"published.txt", fixture.public_blob)],
    );
    let descendant = write_commit(
        &repository,
        clean_tree,
        &[fixture.hidden],
        &[],
        b"policy removed\n",
    );

    let mut mappings = ProjectionMappings::default();
    let result = block_on(repository.project_hidden_paths(&[descendant], &mut mappings)).unwrap();
    assert_ne!(result.public_heads[0], descendant);
    assert_eq!(result.new_mappings.len(), 2);
    let public_descendant = parse_commit_bytes(&repository, result.public_heads[0]);
    assert_eq!(public_descendant.tree, clean_tree);
    assert_eq!(
        public_descendant.parents,
        vec![
            mappings
                .rows()
                .find(|row| row.canonical_id == fixture.hidden)
                .unwrap()
                .public_id
        ]
    );
}

#[test]
fn rejects_conflicting_policy_non_file_policy_gitlinks_and_seed_collisions() {
    let temp = tempfile::tempdir().unwrap();
    let repository = block_on(MachineGitRepository::init(temp.path(), &settings())).unwrap();

    let conflict = block_on(write_conflicted_dsprivate(&repository));
    assert!(matches!(
        block_on(repository.project_hidden_paths(
            &[conflict],
            &mut ProjectionMappings::default()
        )),
        Err(ProjectionError::ConflictedDsprivate { path, .. })
            if path.as_internal_file_string() == ".dsprivate"
    ));

    let empty = write_tree(&repository, &[]);
    let non_file_tree = write_tree(&repository, &[(b"40000", b".dsprivate", empty)]);
    let non_file = write_commit(&repository, non_file_tree, &[], &[], b"invalid policy\n");
    assert!(matches!(
        block_on(repository.project_hidden_paths(&[non_file], &mut ProjectionMappings::default())),
        Err(ProjectionError::InvalidDsprivateEntry { .. })
    ));

    let target = write_commit(&repository, empty, &[], &[], b"gitlink target\n");
    let gitlink_tree = write_tree(&repository, &[(b"160000", b"submodule", target)]);
    let gitlink = write_commit(&repository, gitlink_tree, &[], &[], b"gitlink\n");
    assert!(matches!(
        block_on(repository.project_hidden_paths(
            &[gitlink],
            &mut ProjectionMappings::default()
        )),
        Err(ProjectionError::Gitlink { path })
            if path.as_internal_file_string() == "submodule"
    ));

    assert!(matches!(
        ProjectionMappings::from_rows([
            CommitMapping {
                canonical_id: target,
                public_id: non_file,
            },
            CommitMapping {
                canonical_id: target,
                public_id: gitlink,
            },
        ]),
        Err(ProjectionError::ConflictingMapping { .. })
    ));
}

struct HiddenFixture {
    base: Oid,
    clean: Oid,
    hidden: Oid,
    merge: Oid,
    shared_tree: Oid,
    public_blob: Oid,
}

fn build_hidden_fixture(repository: &MachineGitRepository) -> HiddenFixture {
    let public_blob = write_raw(repository, ObjectKind::Blob, b"public bytes\n");
    let shared_blob = write_raw(repository, ObjectKind::Blob, b"shared public bytes\n");
    let shared_tree = write_tree(repository, &[(b"100644", b"shared.txt", shared_blob)]);
    let base_tree = write_tree(
        repository,
        &[
            (b"100644", b"public.txt", public_blob),
            (b"40000", b"shared", shared_tree),
        ],
    );
    let base = write_commit(repository, base_tree, &[], &[], b"clean base\n");
    let clean_blob = write_raw(repository, ObjectKind::Blob, b"clean branch\n");
    let clean_tree = write_tree(
        repository,
        &[
            (b"100644", b"clean.txt", clean_blob),
            (b"100644", b"public.txt", public_blob),
            (b"40000", b"shared", shared_tree),
        ],
    );
    let clean = write_commit(repository, clean_tree, &[base], &[], b"clean side\n");

    let root_policy = write_raw(
        repository,
        ObjectKind::Blob,
        b"secret.bin\nprivate-dir/\n!private-dir/private.bin\nnested/root-only.txt\n",
    );
    let nested_policy = write_raw(repository, ObjectKind::Blob, b"*.key\n!keep.key\n");
    let root_secret = write_raw(repository, ObjectKind::Blob, ROOT_SECRET);
    let directory_secret = write_raw(repository, ObjectKind::Blob, DIRECTORY_SECRET);
    let nested_secret = write_raw(repository, ObjectKind::Blob, NESTED_SECRET);
    let keep = write_raw(repository, ObjectKind::Blob, b"kept by nested negation\n");
    let root_only = write_raw(repository, ObjectKind::Blob, b"hidden by root policy\n");
    let private_dir = write_tree(repository, &[(b"100644", b"private.bin", directory_secret)]);
    let nested = write_tree(
        repository,
        &[
            (b"100644", b".dsprivate", nested_policy),
            (b"100644", b"hidden.key", nested_secret),
            (b"100644", b"keep.key", keep),
            (b"100644", b"root-only.txt", root_only),
        ],
    );
    let hidden_tree = write_tree(
        repository,
        &[
            (b"100644", b".dsprivate", root_policy),
            (b"40000", b"nested", nested),
            (b"40000", b"private-dir", private_dir),
            (b"100644", b"public.txt", public_blob),
            (b"100644", b"secret.bin", root_secret),
            (b"40000", b"shared", shared_tree),
        ],
    );
    let hidden = write_commit(
        repository,
        hidden_tree,
        &[base],
        &[
            (b"encoding", b"ISO-8859-1"),
            (b"change-id", b"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"),
            (b"x-before", b"first opaque header"),
            (
                b"x-continuation",
                b"first opaque line\n second opaque line\n third opaque line",
            ),
            (
                b"gpgsig",
                b"-----BEGIN PGP SIGNATURE-----\n hidden-signature\n -----END PGP SIGNATURE-----",
            ),
            (b"x-foreign", b"retained"),
            (
                b"gpgsig-sha256",
                b"-----BEGIN SSH SIGNATURE-----\n stale-sha256-signature\n -----END SSH SIGNATURE-----",
            ),
            (
                b"mergetag",
                b"object 0000000000000000000000000000000000000000\n type commit\n tag signed",
            ),
            (b"x-after", b"last opaque header"),
        ],
        b"hidden side\n",
    );
    let merge = write_commit(
        repository,
        hidden_tree,
        &[hidden, clean],
        &[],
        b"mixed merge\n",
    );
    HiddenFixture {
        base,
        clean,
        hidden,
        merge,
        shared_tree,
        public_blob,
    }
}

async fn write_conflicted_dsprivate(repository: &MachineGitRepository) -> Oid {
    let store = repository.repo().store();
    let path = RepoPathBuf::from_internal_string(".dsprivate").unwrap();
    let left = tree_with_file(store, &path, b"left-secret\n").await;
    let base = tree_with_file(store, &path, b"base-secret\n").await;
    let right = tree_with_file(store, &path, b"right-secret\n").await;
    let tree = MergedTree::new(
        store.clone(),
        Merge::from_vec(vec![
            left.tree_ids().as_resolved().unwrap().clone(),
            base.tree_ids().as_resolved().unwrap().clone(),
            right.tree_ids().as_resolved().unwrap().clone(),
        ]),
        ConflictLabels::from_vec(vec!["left".into(), "base".into(), "right".into()]),
    );
    let mut transaction = repository.repo().start_transaction();
    let commit = transaction
        .repo_mut()
        .new_commit(vec![store.root_commit_id().clone()], tree)
        .set_change_id(ChangeId::new(vec![0x55; 16]))
        .set_description("conflicted policy")
        .write()
        .await
        .unwrap();
    transaction
        .commit("conflicted policy fixture")
        .await
        .unwrap();
    Oid(commit.id().as_bytes().try_into().unwrap())
}

async fn tree_with_file(
    store: &std::sync::Arc<jj_lib::store::Store>,
    path: &RepoPathBuf,
    contents: &[u8],
) -> MergedTree {
    let mut reader = contents;
    let file_id = store.write_file(path, &mut reader).await.unwrap();
    let mut builder = MergedTreeBuilder::new(store.empty_merged_tree());
    builder.set_or_remove(
        path.clone(),
        Merge::normal(TreeValue::File {
            id: file_id,
            executable: false,
            copy_id: CopyId::placeholder(),
        }),
    );
    builder.write_tree().await.unwrap()
}

fn assert_no_private_sentinel(repository: &MachineGitRepository, head: Oid) {
    let closure = repository.object_closure([head]).unwrap();
    for object in closure
        .objects
        .iter()
        .filter(|object| object.key.kind == ObjectKind::Blob)
    {
        let bytes = read_raw(repository, object.key);
        for sentinel in [ROOT_SECRET, DIRECTORY_SECRET, NESTED_SECRET] {
            assert!(
                !bytes
                    .windows(sentinel.len())
                    .any(|window| window == sentinel),
                "private sentinel is reachable in {:?}",
                object.key
            );
        }
    }
}

fn write_commit(
    repository: &MachineGitRepository,
    tree: Oid,
    parents: &[Oid],
    extras: &[(&[u8], &[u8])],
    message: &[u8],
) -> Oid {
    let mut bytes = format!(
        "tree {}\n",
        tree.0
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
    .into_bytes();
    for parent in parents {
        bytes.extend_from_slice(
            format!(
                "parent {}\n",
                parent
                    .0
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            )
            .as_bytes(),
        );
    }
    bytes.extend_from_slice(b"author Projection <projection@example.invalid> 1700000000 +0000\n");
    bytes
        .extend_from_slice(b"committer Projection <projection@example.invalid> 1700000000 +0000\n");
    for (name, value) in extras {
        bytes.extend_from_slice(name);
        bytes.push(b' ');
        bytes.extend_from_slice(value);
        bytes.push(b'\n');
    }
    bytes.push(b'\n');
    bytes.extend_from_slice(message);
    write_raw(repository, ObjectKind::Commit, &bytes)
}

fn write_tree(repository: &MachineGitRepository, entries: &[(&[u8], &[u8], Oid)]) -> Oid {
    let mut bytes = Vec::new();
    for (mode, name, id) in entries {
        bytes.extend_from_slice(mode);
        bytes.push(b' ');
        bytes.extend_from_slice(name);
        bytes.push(0);
        bytes.extend_from_slice(&id.0);
    }
    write_raw(repository, ObjectKind::Tree, &bytes)
}

fn write_raw(repository: &MachineGitRepository, kind: ObjectKind, bytes: &[u8]) -> Oid {
    let validated = validate(kind, bytes).unwrap();
    let git_repo = gix::open(repository.git_repo_path()).unwrap();
    let git_kind = match kind {
        ObjectKind::Blob => GitObjectKind::Blob,
        ObjectKind::Tree => GitObjectKind::Tree,
        ObjectKind::Commit => GitObjectKind::Commit,
    };
    let actual = git_repo.objects.write_buf(git_kind, bytes).unwrap();
    assert_eq!(actual.as_bytes(), validated.id.0);
    validated.id
}

fn parse_commit_bytes(repository: &MachineGitRepository, id: Oid) -> devspace_kernel::Commit<'_> {
    let bytes = Box::leak(read_commit_bytes(repository, id).into_boxed_slice());
    parse_commit(bytes).unwrap()
}

fn read_commit_bytes(repository: &MachineGitRepository, id: Oid) -> Vec<u8> {
    read_raw(
        repository,
        ObjectKey {
            kind: ObjectKind::Commit,
            id,
        },
    )
}

fn tree_entry(
    repository: &MachineGitRepository,
    tree: Oid,
    name: &[u8],
) -> Option<(TreeEntryKind, Vec<u8>, Oid)> {
    let bytes = read_raw(
        repository,
        ObjectKey {
            kind: ObjectKind::Tree,
            id: tree,
        },
    );
    parse_tree(&bytes)
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.name == name)
        .map(|entry| (entry.kind, entry.name.to_vec(), entry.oid))
}

fn read_raw(repository: &MachineGitRepository, key: ObjectKey) -> Vec<u8> {
    gix::open(repository.git_repo_path())
        .unwrap()
        .find_object(gix::ObjectId::from_bytes_or_panic(&key.id.0))
        .unwrap()
        .data
        .clone()
}

fn all_objects(repository: &MachineGitRepository) -> BTreeSet<String> {
    String::from_utf8(run_git(
        repository,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    ))
    .unwrap()
    .lines()
    .map(str::to_owned)
    .collect()
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
