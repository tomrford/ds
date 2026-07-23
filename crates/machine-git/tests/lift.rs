use std::collections::BTreeSet;

use devspace_kernel_git::{ObjectKind, Oid, parse_commit, validate};
use devspace_machine_git::{CommitMapping, MachineGitRepository, ProjectionMappings, overlay_lift};
use gix::objs::{Kind as GitObjectKind, Write as _};
use jj_lib::backend::{CommitId, TreeValue};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::conflicts::{MaterializedTreeValue, materialize_tree_value};
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
                name = "Lift Test"
                email = "lift@example.invalid"

                [git]
                write-change-id-header = true
            "#,
        )
        .unwrap(),
    );
    UserSettings::from_config(config).unwrap()
}

struct HiddenSeed {
    canonical: Oid,
    public: Oid,
    mappings: Vec<CommitMapping>,
}

async fn hidden_seed(repository: &MachineGitRepository) -> HiddenSeed {
    let canonical_tree = write_flat_tree(
        repository,
        &[
            (".dsprivate", b".env\n".as_slice()),
            (".env", b"local secret\n".as_slice()),
            ("delete-me", b"public deletion target\n".as_slice()),
            ("public", b"base\n".as_slice()),
        ],
    );
    let canonical = write_commit(repository, canonical_tree, &[], b"hidden base\n", &[]);
    let mut projection = ProjectionMappings::default();
    let projected = repository
        .project_hidden_paths(&[canonical], &mut projection)
        .await
        .unwrap();
    HiddenSeed {
        canonical,
        public: projected.public_heads[0],
        mappings: projected.new_mappings,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn owner_chain_carries_policy_and_hidden_file_through_deletion_and_materialization() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MachineGitRepository::init(temp.path(), &settings())
        .await
        .unwrap();
    let seed = hidden_seed(&repository).await;
    let first_tree = write_flat_tree(
        &repository,
        &[
            ("delete-me", b"public deletion target\n".as_slice()),
            ("feature-one", b"one\n".as_slice()),
            ("public", b"edited\n".as_slice()),
        ],
    );
    let first = write_commit(
        &repository,
        first_tree,
        &[seed.public],
        b"feature one\n",
        &[],
    );
    let tip_tree = write_flat_tree(
        &repository,
        &[
            ("feature-one", b"one\n".as_slice()),
            ("feature-two", b"two\n".as_slice()),
            ("public", b"edited\n".as_slice()),
        ],
    );
    let tip = write_commit(&repository, tip_tree, &[first], b"feature two\n", &[]);

    let lifted = overlay_lift(&repository, &[tip], seed.mappings.clone())
        .await
        .unwrap();

    assert_eq!(lifted.new_mappings.len(), 2);
    assert_eq!(lifted.mirrors.len(), 2);
    assert!(lifted.disclosures.is_empty());
    assert_eq!(lifted.mirrors[0].canonical_parents, vec![seed.canonical]);
    assert_eq!(
        lifted.mirrors[1].canonical_parents,
        vec![lifted.mirrors[0].canonical_commit]
    );
    for mirror in &lifted.mirrors {
        assert_file(
            &repository,
            mirror.canonical_commit,
            ".env",
            b"local secret\n",
        )
        .await;
        assert_file(
            &repository,
            mirror.canonical_commit,
            ".dsprivate",
            b".env\n",
        )
        .await;
    }
    let canonical_tip = lifted.canonical_heads[0];
    assert_absent(&repository, canonical_tip, "delete-me").await;
    assert_file(&repository, canonical_tip, "feature-two", b"two\n").await;

    let store = repository.repo().store();
    let commit = store
        .get_commit_async(&CommitId::from_bytes(&canonical_tip.0))
        .await
        .unwrap();
    let path = path(".env");
    let value = commit.tree().path_value(&path).await.unwrap();
    let mut materialized =
        match materialize_tree_value(store, &path, value, commit.tree().labels()).await {
            Ok(MaterializedTreeValue::File(file)) => file,
            _ => panic!("working-copy materialization did not produce .env"),
        };
    assert_eq!(
        materialized.read_all(&path).await.unwrap(),
        b"local secret\n"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn accidental_hidden_commit_becomes_jj_tree_conflict_and_loud_disclosure() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MachineGitRepository::init(temp.path(), &settings())
        .await
        .unwrap();
    let seed = hidden_seed(&repository).await;
    let foreign_tree = write_flat_tree(
        &repository,
        &[
            (".env", b"published secret\n".as_slice()),
            ("delete-me", b"public deletion target\n".as_slice()),
            ("public", b"base\n".as_slice()),
        ],
    );
    let foreign = write_commit(
        &repository,
        foreign_tree,
        &[seed.public],
        b"accidental disclosure\n",
        &[(b"x-foreign", b"preserved")],
    );

    let lifted = overlay_lift(&repository, &[foreign], seed.mappings)
        .await
        .unwrap();

    assert_eq!(lifted.mirrors.len(), 1);
    assert_eq!(lifted.disclosures.len(), 1);
    assert_eq!(lifted.disclosures[0].path.as_internal_file_string(), ".env");
    let warning = lifted.disclosures[0].warning();
    assert!(warning.contains("WARNING: DATA DISCLOSURE"));
    assert!(warning.contains("`.env`"));
    assert!(warning.contains("publicly visible on the remote"));
    let mirror = lifted.canonical_heads[0];
    let backend = repository
        .repo()
        .store()
        .backend()
        .read_commit(&CommitId::from_bytes(&mirror.0))
        .await
        .unwrap();
    assert!(!backend.root_tree.is_resolved());
    assert!(
        !repository
            .repo()
            .store()
            .get_commit_async(&CommitId::from_bytes(&mirror.0))
            .await
            .unwrap()
            .tree()
            .path_value(&path(".env"))
            .await
            .unwrap()
            .is_resolved()
    );
    let raw = raw_object(&repository, mirror);
    assert!(
        raw.windows(b"jj:trees".len())
            .any(|part| part == b"jj:trees")
    );
    assert!(
        raw.windows(b"x-foreign preserved".len())
            .any(|part| part == b"x-foreign preserved")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn foreign_merge_replays_both_lineages_and_preserves_hidden_parent_conflict() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MachineGitRepository::init(temp.path(), &settings())
        .await
        .unwrap();
    let seed = hidden_seed(&repository).await;
    let left_tree = write_flat_tree(
        &repository,
        &[
            (".dsprivate", b".env\n".as_slice()),
            (".env", b"left secret\n".as_slice()),
            ("delete-me", b"public deletion target\n".as_slice()),
            ("left-local", b"left\n".as_slice()),
            ("public", b"base\n".as_slice()),
        ],
    );
    let right_tree = write_flat_tree(
        &repository,
        &[
            (".dsprivate", b".env\n".as_slice()),
            (".env", b"right secret\n".as_slice()),
            ("delete-me", b"public deletion target\n".as_slice()),
            ("public", b"base\n".as_slice()),
            ("right-local", b"right\n".as_slice()),
        ],
    );
    let left = write_commit(
        &repository,
        left_tree,
        &[seed.canonical],
        b"left local\n",
        &[],
    );
    let right = write_commit(
        &repository,
        right_tree,
        &[seed.canonical],
        b"right local\n",
        &[],
    );
    let mut projection = ProjectionMappings::from_rows(seed.mappings).unwrap();
    let projected = repository
        .project_hidden_paths(&[left, right], &mut projection)
        .await
        .unwrap();
    let public_left = projected.public_heads[0];
    let public_right = projected.public_heads[1];
    let left_foreign_tree = write_flat_tree(
        &repository,
        &[
            ("delete-me", b"public deletion target\n".as_slice()),
            ("left-foreign", b"left foreign\n".as_slice()),
            ("left-local", b"left\n".as_slice()),
            ("public", b"base\n".as_slice()),
        ],
    );
    let right_foreign_tree = write_flat_tree(
        &repository,
        &[
            ("delete-me", b"public deletion target\n".as_slice()),
            ("public", b"base\n".as_slice()),
            ("right-foreign", b"right foreign\n".as_slice()),
            ("right-local", b"right\n".as_slice()),
        ],
    );
    let left_foreign = write_commit(
        &repository,
        left_foreign_tree,
        &[public_left],
        b"left foreign\n",
        &[],
    );
    let right_foreign = write_commit(
        &repository,
        right_foreign_tree,
        &[public_right],
        b"right foreign\n",
        &[],
    );
    let merge_tree = write_flat_tree(
        &repository,
        &[
            ("delete-me", b"public deletion target\n".as_slice()),
            ("left-foreign", b"left foreign\n".as_slice()),
            ("left-local", b"left\n".as_slice()),
            ("public", b"base\n".as_slice()),
            ("right-foreign", b"right foreign\n".as_slice()),
            ("right-local", b"right\n".as_slice()),
        ],
    );
    let merge = write_commit(
        &repository,
        merge_tree,
        &[left_foreign, right_foreign],
        b"foreign merge\n",
        &[],
    );

    let lifted = overlay_lift(&repository, &[merge], projection.rows())
        .await
        .unwrap();

    assert_eq!(lifted.mirrors.len(), 3);
    let canonical_merge = lifted.canonical_heads[0];
    let canonical_merge_bytes = raw_object(&repository, canonical_merge);
    let parsed = parse_commit(&canonical_merge_bytes).unwrap();
    assert_eq!(
        parsed.parents,
        vec![
            lifted.mirrors[0].canonical_commit,
            lifted.mirrors[1].canonical_commit
        ]
    );
    assert_conflict(&repository, canonical_merge, ".env").await;
    assert_file(
        &repository,
        canonical_merge,
        "left-foreign",
        b"left foreign\n",
    )
    .await;
    assert_file(
        &repository,
        canonical_merge,
        "right-foreign",
        b"right foreign\n",
    )
    .await;
    assert_file(&repository, canonical_merge, ".dsprivate", b".env\n").await;
}

#[tokio::test(flavor = "current_thread")]
async fn hidden_free_history_is_identity_with_zero_mirrors_and_zero_rows() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MachineGitRepository::init(temp.path(), &settings())
        .await
        .unwrap();
    let base_tree = write_flat_tree(&repository, &[("public", b"base\n")]);
    let base = write_commit(&repository, base_tree, &[], b"base\n", &[]);
    let tip_tree = write_flat_tree(
        &repository,
        &[
            ("feature", b"feature\n".as_slice()),
            ("public", b"base\n".as_slice()),
        ],
    );
    let tip = write_commit(&repository, tip_tree, &[base], b"tip\n", &[]);
    let before = all_objects(&repository);

    let lifted = overlay_lift(&repository, &[tip], []).await.unwrap();

    assert_eq!(lifted.canonical_heads, vec![tip]);
    assert!(lifted.new_mappings.is_empty());
    assert!(lifted.mirrors.is_empty());
    assert!(lifted.disclosures.is_empty());
    assert_eq!(all_objects(&repository), before);
}

#[tokio::test(flavor = "current_thread")]
async fn lifted_pairs_seed_push_projection_and_short_circuit_mirror_replay() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MachineGitRepository::init(temp.path(), &settings())
        .await
        .unwrap();
    let seed = hidden_seed(&repository).await;
    let foreign_tree = write_flat_tree(
        &repository,
        &[
            ("delete-me", b"public deletion target\n".as_slice()),
            ("feature", b"foreign\n".as_slice()),
            ("public", b"base\n".as_slice()),
        ],
    );
    let foreign = write_commit(&repository, foreign_tree, &[seed.public], b"foreign\n", &[]);
    let lifted = overlay_lift(&repository, &[foreign], seed.mappings)
        .await
        .unwrap();
    let lifted_tip = lifted.canonical_heads[0];
    let local_tree = write_flat_tree(
        &repository,
        &[
            (".dsprivate", b".env\n".as_slice()),
            (".env", b"local secret\n".as_slice()),
            ("delete-me", b"public deletion target\n".as_slice()),
            ("feature", b"local edit\n".as_slice()),
            ("public", b"base\n".as_slice()),
        ],
    );
    let local = write_commit(
        &repository,
        local_tree,
        &[lifted_tip],
        b"local after fetch\n",
        &[],
    );

    let mut push_mappings = ProjectionMappings::from_rows(lifted.new_mappings.clone()).unwrap();
    let projected = repository
        .project_hidden_paths(&[local], &mut push_mappings)
        .await
        .unwrap();

    assert_eq!(projected.reached_mappings, lifted.new_mappings);
    assert_eq!(projected.new_mappings.len(), 1);
    let public_local_bytes = raw_object(&repository, projected.public_heads[0]);
    let public_local = parse_commit(&public_local_bytes).unwrap();
    assert_eq!(public_local.parents, vec![foreign]);
    let mut replay = ProjectionMappings::from_rows(lifted.new_mappings).unwrap();
    let replayed = repository
        .project_hidden_paths(&[local], &mut replay)
        .await
        .unwrap();
    assert_eq!(replayed.public_heads, projected.public_heads);
    assert_eq!(replayed.reached_mappings.len(), 1);
}

fn path(value: &str) -> RepoPathBuf {
    RepoPathBuf::from_internal_string(value).unwrap()
}

async fn assert_file(repository: &MachineGitRepository, commit: Oid, at: &str, expected: &[u8]) {
    let store = repository.repo().store();
    let value = store
        .get_commit_async(&CommitId::from_bytes(&commit.0))
        .await
        .unwrap()
        .tree()
        .path_value(&path(at))
        .await
        .unwrap()
        .into_resolved()
        .unwrap()
        .unwrap();
    let TreeValue::File { id, .. } = value else {
        panic!("{at} is not a file");
    };
    let mut bytes = Vec::new();
    let reader = store.read_file(&path(at), &id).await.unwrap();
    jj_lib::file_util::copy_async_to_sync(reader, &mut bytes)
        .await
        .unwrap();
    assert_eq!(bytes, expected, "{at}");
}

async fn assert_absent(repository: &MachineGitRepository, commit: Oid, at: &str) {
    let value = repository
        .repo()
        .store()
        .get_commit_async(&CommitId::from_bytes(&commit.0))
        .await
        .unwrap()
        .tree()
        .path_value(&path(at))
        .await
        .unwrap();
    assert_eq!(value.into_resolved(), Ok(None), "{at}");
}

async fn assert_conflict(repository: &MachineGitRepository, commit: Oid, at: &str) {
    let value = repository
        .repo()
        .store()
        .get_commit_async(&CommitId::from_bytes(&commit.0))
        .await
        .unwrap()
        .tree()
        .path_value(&path(at))
        .await
        .unwrap();
    assert!(!value.is_resolved(), "{at}");
}

fn write_flat_tree(repository: &MachineGitRepository, entries: &[(&str, &[u8])]) -> Oid {
    let mut entries = entries
        .iter()
        .map(|(name, bytes)| (*name, write_raw(repository, ObjectKind::Blob, bytes)))
        .collect::<Vec<_>>();
    entries.sort_unstable_by_key(|(name, _)| *name);
    let mut tree = Vec::new();
    for (name, id) in entries {
        tree.extend_from_slice(b"100644 ");
        tree.extend_from_slice(name.as_bytes());
        tree.push(0);
        tree.extend_from_slice(&id.0);
    }
    write_raw(repository, ObjectKind::Tree, &tree)
}

fn write_commit(
    repository: &MachineGitRepository,
    tree: Oid,
    parents: &[Oid],
    message: &[u8],
    extras: &[(&[u8], &[u8])],
) -> Oid {
    let mut bytes = format!("tree {}\n", oid_hex(tree)).into_bytes();
    for parent in parents {
        bytes.extend_from_slice(format!("parent {}\n", oid_hex(*parent)).as_bytes());
    }
    bytes.extend_from_slice(b"author Lift <lift@example.invalid> 1700000000 +0000\n");
    bytes.extend_from_slice(b"committer Lift <lift@example.invalid> 1700000000 +0000\n");
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

fn write_raw(repository: &MachineGitRepository, kind: ObjectKind, bytes: &[u8]) -> Oid {
    let expected = validate(kind, bytes).unwrap().id;
    let git = gix::open(repository.git_repo_path()).unwrap();
    let git_kind = match kind {
        ObjectKind::Blob => GitObjectKind::Blob,
        ObjectKind::Tree => GitObjectKind::Tree,
        ObjectKind::Commit => GitObjectKind::Commit,
    };
    let actual = git.objects.write_buf(git_kind, bytes).unwrap();
    assert_eq!(actual.as_bytes(), expected.0);
    expected
}

fn raw_object(repository: &MachineGitRepository, id: Oid) -> Vec<u8> {
    gix::open(repository.git_repo_path())
        .unwrap()
        .find_object(gix::ObjectId::from_bytes_or_panic(&id.0))
        .unwrap()
        .data
        .clone()
}

fn all_objects(repository: &MachineGitRepository) -> BTreeSet<String> {
    let output = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repository.git_repo_path())
        .args([
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn oid_hex(id: Oid) -> String {
    id.0.iter().map(|byte| format!("{byte:02x}")).collect()
}
