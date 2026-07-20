use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::Arc;

use devspace_machine::{
    CommitMapping, FetchedGitRef, GitLiftError, GitProjection, GitSeed, ImportMappings,
    MAX_IMPORT_HEADS, MAX_IMPORT_TOTAL_COMMITS, MAX_IMPORT_TREE_DEPTH, MAX_IMPORT_TREE_ENTRIES,
    MachineRepository, ProjectionCursor, ProjectionError, ProjectionMapping, ProjectionSnapshot,
    SeedSelection, TOMBSTONE_A, TOMBSTONE_B, lift_imported, select_seeds,
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
            "[user]\nname = 'Devspace Test'\nemail = 'devspace@example.invalid'\n",
        )
        .unwrap(),
    );
    UserSettings::from_config(config).unwrap()
}

fn path(value: &str) -> RepoPathBuf {
    RepoPathBuf::from_internal_string(value).unwrap()
}

fn component(value: &str) -> RepoPathComponentBuf {
    RepoPathComponentBuf::new(value.to_owned()).unwrap()
}

fn signature(seed: i64) -> Signature {
    Signature {
        name: "Devspace Test".to_owned(),
        email: "devspace@example.invalid".to_owned(),
        timestamp: Timestamp {
            timestamp: MillisSinceEpoch(seed),
            tz_offset: 0,
        },
    }
}

async fn file(store: &Arc<Store>, file_path: &str, bytes: &[u8]) -> TreeValue {
    let mut reader = bytes;
    let id = store
        .write_file(&path(file_path), &mut reader)
        .await
        .unwrap();
    TreeValue::File {
        id,
        executable: false,
        copy_id: CopyId::placeholder(),
    }
}

async fn tree(store: &Arc<Store>, dir: &str, entries: Vec<(&str, TreeValue)>) -> TreeId {
    let mut entries = entries
        .into_iter()
        .map(|(name, value)| (component(name), value))
        .collect::<Vec<_>>();
    entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
    store
        .write_tree(
            &RepoPathBuf::from_internal_string(dir).unwrap(),
            BackendTree::from_sorted_entries(entries),
        )
        .await
        .unwrap()
        .id()
        .clone()
}

async fn commit(
    store: &Arc<Store>,
    parents: Vec<CommitId>,
    root_tree: TreeId,
    seed: u8,
) -> CommitId {
    store
        .write_commit(
            BackendCommit {
                parents,
                predecessors: Vec::new(),
                root_tree: Merge::resolved(root_tree),
                conflict_labels: Merge::resolved(String::new()),
                change_id: ChangeId::new(vec![seed; store.change_id_length()]),
                description: format!("fixture {seed}"),
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

async fn flat_tree(store: &Arc<Store>, entries: Vec<(&str, &[u8])>) -> TreeId {
    let mut values = Vec::new();
    for (name, bytes) in entries {
        values.push((name, file(store, name, bytes).await));
    }
    tree(store, "", values).await
}

async fn value(store: &Arc<Store>, commit_id: &CommitId, at: &str) -> Merge<Option<TreeValue>> {
    store
        .get_commit_async(commit_id)
        .await
        .unwrap()
        .tree()
        .path_value(&path(at))
        .await
        .unwrap()
}

async fn bytes(store: &Arc<Store>, at: &str, value: &TreeValue) -> Vec<u8> {
    let TreeValue::File { id, .. } = value else {
        panic!("expected file at {at}");
    };
    let mut output = Vec::new();
    let reader = store.read_file(&path(at), id).await.unwrap();
    jj_lib::file_util::copy_async_to_sync(reader, &mut output)
        .await
        .unwrap();
    output
}

async fn private_seed(
    store: &Arc<Store>,
    policy: &[u8],
    hidden: Option<(&str, &[u8])>,
) -> CommitId {
    let policy_value = file(store, ".dsprivate", policy).await;
    let mut entries = vec![
        (".dsprivate", policy_value),
        ("visible", file(store, "visible", b"base").await),
    ];
    if let Some((name, contents)) = hidden {
        entries.push((name, file(store, name, contents).await));
    }
    let root = tree(store, "", entries).await;
    commit(store, vec![store.root_commit_id().clone()], root, 1).await
}

fn mapping(git: u8, public: CommitId) -> CommitMapping {
    CommitMapping {
        canonical_id: public,
        git_id: CommitId::new(vec![git; 20]),
    }
}

fn selection(seed: GitSeed) -> SeedSelection {
    SeedSelection::from_seeds([seed]).unwrap()
}

struct LiftFixture {
    _temp: tempfile::TempDir,
    repository: MachineRepository,
    public_seed: CommitId,
    private_seed: CommitId,
}

async fn lift_fixture(policy: &[u8], hidden: Option<(&str, &[u8])>) -> LiftFixture {
    let temp = tempfile::tempdir().unwrap();
    let repository = MachineRepository::init(temp.path().join("native"), &settings())
        .await
        .unwrap();
    let store = repository.repo().store();
    let private_seed = private_seed(store, policy, hidden).await;
    let public_root = flat_tree(store, vec![("visible", b"base")]).await;
    let public_seed = commit(store, vec![store.root_commit_id().clone()], public_root, 2).await;
    LiftFixture {
        _temp: temp,
        repository,
        public_seed,
        private_seed,
    }
}

fn seed_for(fixture: &LiftFixture) -> GitSeed {
    GitSeed {
        git_oid: [1; 20],
        public_commit_id: fixture.public_seed.clone(),
        canonical_commit_id: fixture.private_seed.clone(),
        hidden_set_id: devspace_machine::HiddenSetIdentity::from_projection_id(None),
    }
}

#[tokio::test]
async fn plain_edit_lifts_and_preserves_hidden_structure() {
    let fixture = lift_fixture(b"/secret\n/nested/private\n", Some(("secret", b"private"))).await;
    let store = fixture.repository.repo().store();
    let nested_policy = file(store, "nested/.dsprivate", b"private\n").await;
    let nested_private = file(store, "nested/private", b"nested-private").await;
    let nested = tree(
        store,
        "nested",
        vec![(".dsprivate", nested_policy), ("private", nested_private)],
    )
    .await;
    let private_root = tree(
        store,
        "",
        vec![
            (
                ".dsprivate",
                file(store, ".dsprivate", b"/secret\n/nested/private\n").await,
            ),
            ("secret", file(store, "secret", b"private").await),
            ("nested", TreeValue::Tree(nested)),
            ("visible", file(store, "visible", b"base").await),
        ],
    )
    .await;
    let private = commit(store, vec![store.root_commit_id().clone()], private_root, 3).await;
    let child_tree = flat_tree(store, vec![("visible", b"edited")]).await;
    let public = commit(store, vec![fixture.public_seed.clone()], child_tree, 4).await;
    let selected = selection(GitSeed {
        canonical_commit_id: private,
        ..seed_for(&fixture)
    });
    let result = lift_imported(&fixture.repository, &[mapping(2, public)], &selected)
        .await
        .unwrap();
    let lifted = &result.states[0].canonical_commit_id;
    for (at, expected) in [
        ("visible", b"edited".as_slice()),
        ("secret", b"private".as_slice()),
        ("nested/.dsprivate", b"private\n".as_slice()),
        ("nested/private", b"nested-private".as_slice()),
    ] {
        let clean = value(store, lifted, at)
            .await
            .into_resolved()
            .unwrap()
            .unwrap();
        assert_eq!(bytes(store, at, &clean).await, expected);
    }
}

#[tokio::test]
async fn pollution_uses_tombstones_natural_conflicts_and_deletion_cleanup() {
    let fixture = lift_fixture(b"/hidden\n/secret\n", Some(("secret", b"private"))).await;
    let store = fixture.repository.repo().store();
    let added_tree = flat_tree(
        store,
        vec![
            ("visible", b"base"),
            ("hidden", b"public-add"),
            ("secret", b"public-edit"),
        ],
    )
    .await;
    let added = commit(store, vec![fixture.public_seed.clone()], added_tree, 5).await;
    let deleted_tree = flat_tree(store, vec![("visible", b"base")]).await;
    let deleted = commit(store, vec![added.clone()], deleted_tree, 6).await;
    let selected = selection(seed_for(&fixture));
    let result = lift_imported(
        &fixture.repository,
        &[mapping(2, added), mapping(3, deleted)],
        &selected,
    )
    .await
    .unwrap();
    assert_eq!(
        result
            .polluted_paths
            .iter()
            .map(|path| path.as_internal_file_string())
            .collect::<Vec<_>>(),
        ["hidden", "secret"]
    );
    let added_lift = &result.states[0].canonical_commit_id;
    let hidden = value(store, added_lift, "hidden").await;
    assert!(!hidden.is_resolved());
    let removed = hidden.removes().next().unwrap().as_ref().unwrap();
    assert_eq!(bytes(store, "hidden", removed).await, TOMBSTONE_A);
    let secret = value(store, added_lift, "secret").await;
    assert!(!secret.is_resolved());
    assert!(secret.removes().all(Option::is_none));
    let deleted_lift = &result.states[1].canonical_commit_id;
    let clean = value(store, deleted_lift, "secret")
        .await
        .into_resolved()
        .unwrap()
        .unwrap();
    assert_eq!(bytes(store, "secret", &clean).await, b"private");
}

#[tokio::test]
async fn pollution_diff_keeps_a_large_untouched_sibling_subtree() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MachineRepository::init(temp.path().join("native"), &settings())
        .await
        .unwrap();
    let store = repository.repo().store();

    let names = (0..128)
        .map(|index| format!("entry-{index:03}"))
        .collect::<Vec<_>>();
    let mut deep_entries = Vec::with_capacity(names.len());
    for name in &names {
        deep_entries.push((
            name.as_str(),
            file(store, &format!("untouched/deep/{name}"), b"same").await,
        ));
    }
    let deep = tree(store, "untouched/deep", deep_entries).await;
    let untouched = tree(store, "untouched", vec![("deep", TreeValue::Tree(deep))]).await;
    let public_base_tree = tree(
        store,
        "",
        vec![
            ("untouched", TreeValue::Tree(untouched.clone())),
            ("visible", file(store, "visible", b"base").await),
        ],
    )
    .await;
    let private_base_tree = tree(
        store,
        "",
        vec![
            (".dsprivate", file(store, ".dsprivate", b"/hidden\n").await),
            ("untouched", TreeValue::Tree(untouched.clone())),
            ("visible", file(store, "visible", b"base").await),
        ],
    )
    .await;
    let public_seed = commit(
        store,
        vec![store.root_commit_id().clone()],
        public_base_tree,
        42,
    )
    .await;
    let private_seed = commit(
        store,
        vec![store.root_commit_id().clone()],
        private_base_tree,
        43,
    )
    .await;
    let arriving_tree = tree(
        store,
        "",
        vec![
            ("hidden", file(store, "hidden", b"public").await),
            ("untouched", TreeValue::Tree(untouched)),
            ("visible", file(store, "visible", b"base").await),
        ],
    )
    .await;
    let arriving = commit(store, vec![public_seed.clone()], arriving_tree, 44).await;
    let lifted = lift_imported(
        &repository,
        &[mapping(44, arriving)],
        &selection(GitSeed {
            git_oid: [42; 20],
            public_commit_id: public_seed,
            canonical_commit_id: private_seed,
            hidden_set_id: devspace_machine::HiddenSetIdentity::from_projection_id(None),
        }),
    )
    .await
    .unwrap();

    let lifted_id = &lifted.states[0].canonical_commit_id;
    let conflict = value(store, lifted_id, "hidden").await;
    let removed = conflict.removes().next().unwrap().as_ref().unwrap();
    assert_eq!(bytes(store, "hidden", removed).await, TOMBSTONE_A);
    let untouched_value = value(store, lifted_id, "untouched/deep/entry-127")
        .await
        .into_resolved()
        .unwrap()
        .unwrap();
    assert_eq!(
        bytes(store, "untouched/deep/entry-127", &untouched_value).await,
        b"same"
    );
}

#[tokio::test]
async fn public_dsprivate_and_hidden_directory_are_tombstoned_per_file() {
    let fixture = lift_fixture(b"/private-dir/\n", None).await;
    let store = fixture.repository.repo().store();
    let dir = tree(
        store,
        "private-dir",
        vec![
            ("a", file(store, "private-dir/a", b"a").await),
            (
                "link",
                TreeValue::Symlink(
                    store
                        .write_symlink(&path("private-dir/link"), "target")
                        .await
                        .unwrap(),
                ),
            ),
        ],
    )
    .await;
    let root = tree(
        store,
        "",
        vec![
            ("visible", file(store, "visible", b"base").await),
            (
                ".dsprivate",
                file(store, ".dsprivate", b"public policy").await,
            ),
            ("private-dir", TreeValue::Tree(dir)),
        ],
    )
    .await;
    let public = commit(store, vec![fixture.public_seed.clone()], root, 7).await;
    let result = lift_imported(
        &fixture.repository,
        &[mapping(4, public)],
        &selection(seed_for(&fixture)),
    )
    .await
    .unwrap();
    for at in [".dsprivate", "private-dir/a", "private-dir/link"] {
        assert!(
            !value(store, &result.states[0].canonical_commit_id, at)
                .await
                .is_resolved(),
            "{at} should be conflicted"
        );
    }

    let clean_temp = tempfile::tempdir().unwrap();
    let clean_repo = MachineRepository::init(clean_temp.path().join("native"), &settings())
        .await
        .unwrap();
    let clean_store = clean_repo.repo().store();
    let clean_root = flat_tree(clean_store, vec![("visible", b"base")]).await;
    let clean_private = commit(
        clean_store,
        vec![clean_store.root_commit_id().clone()],
        clean_root.clone(),
        70,
    )
    .await;
    let clean_public = commit(
        clean_store,
        vec![clean_store.root_commit_id().clone()],
        clean_root,
        71,
    )
    .await;
    let arriving_root = flat_tree(
        clean_store,
        vec![("visible", b"base"), (".dsprivate", b"arriving")],
    )
    .await;
    let arriving = commit(clean_store, vec![clean_public.clone()], arriving_root, 72).await;
    let arriving_seed = GitSeed {
        git_oid: [70; 20],
        public_commit_id: clean_public,
        canonical_commit_id: clean_private,
        hidden_set_id: devspace_machine::HiddenSetIdentity::from_projection_id(None),
    };
    let arriving_lift = lift_imported(
        &clean_repo,
        &[mapping(71, arriving)],
        &selection(arriving_seed),
    )
    .await
    .unwrap();
    let conflict = value(
        clean_store,
        &arriving_lift.states[0].canonical_commit_id,
        ".dsprivate",
    )
    .await;
    let removed = conflict.removes().next().unwrap().as_ref().unwrap();
    assert_eq!(bytes(clean_store, ".dsprivate", removed).await, TOMBSTONE_A);
}

#[tokio::test]
async fn two_and_three_parent_merges_preserve_order_and_replay_delta() {
    let fixture = lift_fixture(b"/secret\n", Some(("secret", b"private"))).await;
    let store = fixture.repository.repo().store();
    let mut branches = Vec::new();
    for (seed, name) in [(10, "a"), (11, "b"), (12, "c")] {
        let root = flat_tree(store, vec![("visible", b"base"), (name, name.as_bytes())]).await;
        branches.push(commit(store, vec![fixture.public_seed.clone()], root, seed).await);
    }
    let merge2_tree = flat_tree(store, vec![("visible", b"base"), ("a", b"a"), ("b", b"b")]).await;
    let merge2 = commit(
        store,
        vec![branches[1].clone(), branches[0].clone()],
        merge2_tree,
        13,
    )
    .await;
    let merge3_tree = flat_tree(
        store,
        vec![
            ("visible", b"merged"),
            ("a", b"a"),
            ("b", b"b"),
            ("c", b"c"),
        ],
    )
    .await;
    let merge3 = commit(
        store,
        vec![branches[2].clone(), merge2.clone(), branches[0].clone()],
        merge3_tree,
        14,
    )
    .await;
    let imported = vec![
        mapping(10, branches[0].clone()),
        mapping(11, branches[1].clone()),
        mapping(12, branches[2].clone()),
        mapping(13, merge2),
        mapping(14, merge3),
    ];
    let result = lift_imported(
        &fixture.repository,
        &imported,
        &selection(seed_for(&fixture)),
    )
    .await
    .unwrap();
    let two = store
        .backend()
        .read_commit(&result.states[3].canonical_commit_id)
        .await
        .unwrap();
    assert_eq!(
        two.parents,
        vec![
            result.states[1].canonical_commit_id.clone(),
            result.states[0].canonical_commit_id.clone()
        ]
    );
    let three = store
        .backend()
        .read_commit(&result.states[4].canonical_commit_id)
        .await
        .unwrap();
    assert_eq!(
        three.parents,
        vec![
            result.states[2].canonical_commit_id.clone(),
            result.states[3].canonical_commit_id.clone(),
            result.states[0].canonical_commit_id.clone()
        ]
    );
    for at in ["a", "b", "c", "secret"] {
        assert!(
            value(store, &result.states[4].canonical_commit_id, at)
                .await
                .is_resolved()
        );
    }
}

fn snapshot(
    mappings: Vec<ProjectionMapping>,
    cursors: Vec<ProjectionCursor>,
) -> ProjectionSnapshot {
    ProjectionSnapshot {
        activation_cursor: mappings.len() as u64,
        cursors,
        mappings,
        next_after: 0,
        through: 0,
        has_more: false,
        pending: Vec::new(),
    }
}

fn state_mapping(
    bookmark: &str,
    git: &CommitId,
    public: &CommitId,
    private: &CommitId,
    hidden: u8,
) -> ProjectionMapping {
    ProjectionMapping {
        remote: "origin".to_owned(),
        bookmark: bookmark.to_owned(),
        git_oid: git.as_bytes().try_into().unwrap(),
        canonical_commit_id: private.as_bytes().try_into().unwrap(),
        public_commit_id: public.as_bytes().try_into().unwrap(),
        hidden_set_id: Some([hidden; 64]),
    }
}

#[tokio::test]
async fn seed_selection_rejects_ambiguity_and_rewritten_refs_and_allows_scratch() {
    let temp = tempfile::tempdir().unwrap();
    let projection = GitProjection::init(temp.path().join("git"), &settings()).unwrap();
    let store = projection.store();
    let root = flat_tree(store, vec![("visible", b"x")]).await;
    let ancestor = commit(
        store,
        vec![store.root_commit_id().clone()],
        root.clone(),
        20,
    )
    .await;
    let head = commit(store, vec![ancestor.clone()], root, 21).await;
    let public = CommitId::new(vec![30; 64]);
    let private_a = CommitId::new(vec![31; 64]);
    let private_b = CommitId::new(vec![32; 64]);
    let receipts = ImportMappings::from_rows([CommitMapping {
        git_id: ancestor.clone(),
        canonical_id: public.clone(),
    }])
    .unwrap();
    let fetched = [FetchedGitRef {
        remote: "origin".to_owned(),
        bookmark: "main".to_owned(),
        head: head.as_bytes().try_into().unwrap(),
    }];
    let ambiguous = snapshot(
        vec![
            state_mapping("main", &ancestor, &public, &private_a, 1),
            state_mapping("other", &ancestor, &public, &private_b, 2),
        ],
        Vec::new(),
    );
    assert!(matches!(
        select_seeds(&projection, &fetched, &ambiguous, &receipts).await,
        Err(GitLiftError::AmbiguousSeed { .. })
    ));

    let agreeing_newest = snapshot(
        vec![
            state_mapping("main", &ancestor, &public, &private_b, 2),
            state_mapping("main", &ancestor, &public, &private_a, 1),
            state_mapping("other", &ancestor, &public, &private_a, 1),
        ],
        Vec::new(),
    );
    let selected = select_seeds(&projection, &fetched, &agreeing_newest, &receipts)
        .await
        .unwrap();
    assert_eq!(
        selected.seeds().next().unwrap().canonical_commit_id,
        private_a
    );

    let rewritten = snapshot(
        vec![state_mapping("main", &ancestor, &public, &private_a, 1)],
        vec![ProjectionCursor {
            remote: "origin".to_owned(),
            bookmark: "main".to_owned(),
            git_oid: [99; 20],
            canonical_commit_id: [31; 64],
            public_commit_id: [30; 64],
            hidden_set_id: Some([1; 64]),
            activation_sequence: 1,
        }],
    );
    assert!(matches!(
        select_seeds(&projection, &fetched, &rewritten, &receipts).await,
        Err(GitLiftError::RefRewritten { .. })
    ));

    // A receipt recorded through ANOTHER remote is not lineage for this one:
    // it must not enter the stop set, or lifting would strand at a commit
    // with no private parent mapping. Import proceeds from scratch instead.
    let mut foreign_state = state_mapping("main", &ancestor, &public, &private_a, 1);
    foreign_state.remote = "upstream".to_owned();
    let foreign = select_seeds(
        &projection,
        &fetched,
        &snapshot(vec![foreign_state], Vec::new()),
        &receipts,
    )
    .await
    .unwrap();
    assert_eq!(foreign.seeds().count(), 0);
    assert_eq!(foreign.stop_set().rows().count(), 0);

    let scratch = select_seeds(
        &projection,
        &fetched,
        &snapshot(Vec::new(), Vec::new()),
        &ImportMappings::default(),
    )
    .await
    .unwrap();
    assert_eq!(scratch.seeds().count(), 0);
    let repository = MachineRepository::init(temp.path().join("scratch-native"), &settings())
        .await
        .unwrap();
    let imported = projection
        .import_reachable_with_stops(
            repository.repo().store(),
            &[head],
            scratch.stop_set(),
            &mut ImportMappings::default(),
        )
        .await
        .unwrap();
    assert_eq!(imported.new_mappings.len(), 2);
}

async fn deterministic_fixture() -> CommitId {
    let fixture = lift_fixture(b"/hidden\n", None).await;
    let store = fixture.repository.repo().store();
    let public_tree = flat_tree(store, vec![("visible", b"changed"), ("hidden", b"public")]).await;
    let public = commit(store, vec![fixture.public_seed.clone()], public_tree, 40).await;
    lift_imported(
        &fixture.repository,
        &[mapping(40, public)],
        &selection(seed_for(&fixture)),
    )
    .await
    .unwrap()
    .states[0]
        .canonical_commit_id
        .clone()
}

#[tokio::test]
async fn lifting_is_deterministic_across_fresh_stores() {
    assert_eq!(deterministic_fixture().await, deterministic_fixture().await);
}

#[tokio::test]
async fn tombstone_bytes_ids_and_a_collision_fallback_are_frozen() {
    assert_eq!(TOMBSTONE_A.last(), Some(&b'\n'));
    assert_eq!(TOMBSTONE_B.last(), Some(&b'\n'));
    assert_eq!(TOMBSTONE_A.len(), 350);
    assert_eq!(TOMBSTONE_B.len(), 350);
    let fixture = lift_fixture(b"/hidden\n", None).await;
    let store = fixture.repository.repo().store();
    let a = file(store, "a", TOMBSTONE_A).await;
    let b = file(store, "b", TOMBSTONE_B).await;
    let TreeValue::File { id: a, .. } = a else {
        unreachable!()
    };
    let TreeValue::File { id: b, .. } = b else {
        unreachable!()
    };
    assert_eq!(
        a.hex(),
        "c42fcd7b43c502ae07f09900079f9e6037d125791bf53adf32856d27332de570b833f3c2e5169f2c2703877aefa1e95199f70f73085d5ab74e961d28c1fc2b16"
    );
    assert_eq!(
        b.hex(),
        "ca45fda6a1c31b119bcdbc7465f98245a8085f85a4845bc89e1a92a2a9d52609ca635ffa0cacb2a07fa3c875ea55a5464508bd7a68e8375cf471e376a055ebfd"
    );

    let public_tree = flat_tree(store, vec![("visible", b"base"), ("hidden", TOMBSTONE_A)]).await;
    let public = commit(store, vec![fixture.public_seed.clone()], public_tree, 41).await;
    let lifted = lift_imported(
        &fixture.repository,
        &[mapping(41, public)],
        &selection(seed_for(&fixture)),
    )
    .await
    .unwrap();
    let conflict = value(store, &lifted.states[0].canonical_commit_id, "hidden").await;
    let removed = conflict.removes().next().unwrap().as_ref().unwrap();
    assert_eq!(bytes(store, "hidden", removed).await, TOMBSTONE_B);
}

#[tokio::test]
async fn import_strips_secure_signature_and_stops_at_receipts() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let projection = GitProjection::init(temp.path().join("git"), &settings).unwrap();
    let git = projection.store();
    let root = flat_tree(git, vec![("visible", b"base")]).await;
    let parent = commit(git, vec![git.root_commit_id().clone()], root.clone(), 50).await;
    let child = commit(git, vec![parent.clone()], root, 51).await;
    let raw = Command::new("git")
        .args([
            "--git-dir",
            projection.git_repo_path().to_str().unwrap(),
            "cat-file",
            "commit",
            &child.hex(),
        ])
        .output()
        .unwrap();
    assert!(raw.status.success());
    let raw = String::from_utf8(raw.stdout).unwrap();
    let marker = raw.find("\n\n").unwrap();
    let signed_raw = format!(
        "{}\ngpgsig -----BEGIN PGP SIGNATURE-----\n dummy\n -----END PGP SIGNATURE-----{}",
        &raw[..marker],
        &raw[marker..]
    );
    let mut hash = Command::new("git")
        .args([
            "--git-dir",
            projection.git_repo_path().to_str().unwrap(),
            "hash-object",
            "-t",
            "commit",
            "-w",
            "--stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    hash.stdin
        .as_mut()
        .unwrap()
        .write_all(signed_raw.as_bytes())
        .unwrap();
    let output = hash.wait_with_output().unwrap();
    assert!(output.status.success());
    let signed_hex = String::from_utf8(output.stdout).unwrap();
    let signed_oid = devspace_machine::GitOid::from_hex(signed_hex.trim()).unwrap();
    let signed_id = CommitId::new(signed_oid.0.to_vec());
    assert!(
        git.backend()
            .read_commit(&signed_id)
            .await
            .unwrap()
            .secure_sig
            .is_some()
    );

    let public_parent_tree = flat_tree(repository.repo().store(), vec![("visible", b"base")]).await;
    let public_parent = commit(
        repository.repo().store(),
        vec![repository.repo().store().root_commit_id().clone()],
        public_parent_tree,
        50,
    )
    .await;
    let stops = ImportMappings::from_rows([CommitMapping {
        git_id: parent,
        canonical_id: public_parent.clone(),
    }])
    .unwrap();
    let imported = projection
        .import_reachable_with_stops(
            repository.repo().store(),
            &[signed_id],
            &stops,
            &mut ImportMappings::default(),
        )
        .await
        .unwrap();
    assert_eq!(imported.reached_mappings.len(), 1);
    assert_eq!(imported.reached_mappings[0].canonical_id, public_parent);
    let imported_commit = repository
        .repo()
        .store()
        .backend()
        .read_commit(&imported.canonical_heads[0])
        .await
        .unwrap();
    assert!(imported_commit.secure_sig.is_none());
}

#[tokio::test]
async fn conflicted_applicable_policy_fails_closed() {
    let fixture = lift_fixture(b"", None).await;
    let store = fixture.repository.repo().store();
    let left = flat_tree(store, vec![("visible", b"base"), (".dsprivate", b"left\n")]).await;
    let right = flat_tree(
        store,
        vec![("visible", b"base"), (".dsprivate", b"right\n")],
    )
    .await;
    let conflicted = store
        .write_commit(
            BackendCommit {
                parents: vec![store.root_commit_id().clone()],
                predecessors: Vec::new(),
                root_tree: Merge::from_vec(vec![left, store.empty_tree_id().clone(), right]),
                conflict_labels: Merge::resolved(String::new()),
                change_id: ChangeId::new(vec![80; store.change_id_length()]),
                description: "conflicted policy".to_owned(),
                author: signature(80),
                committer: signature(80),
                secure_sig: None,
            },
            None,
        )
        .await
        .unwrap()
        .id()
        .clone();
    let child_tree = flat_tree(store, vec![("visible", b"changed")]).await;
    let child = commit(store, vec![fixture.public_seed.clone()], child_tree, 81).await;
    let result = lift_imported(
        &fixture.repository,
        &[mapping(81, child)],
        &selection(GitSeed {
            canonical_commit_id: conflicted,
            ..seed_for(&fixture)
        }),
    )
    .await;
    assert!(matches!(
        result,
        Err(GitLiftError::Projection(
            ProjectionError::ConflictedDsprivate { .. }
        ))
    ));
}

#[tokio::test]
async fn import_bounds_fail_with_typed_errors() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let repository = MachineRepository::init(temp.path().join("native"), &settings)
        .await
        .unwrap();
    let projection = GitProjection::init(temp.path().join("git"), &settings).unwrap();
    let git = projection.store();

    let heads = vec![git.root_commit_id().clone(); MAX_IMPORT_HEADS + 1];
    assert!(matches!(
        projection
            .import_reachable(
                repository.repo().store(),
                &heads,
                &mut ImportMappings::default()
            )
            .await,
        Err(ProjectionError::ImportHeadLimit { .. })
    ));

    let blob = file(git, "entry", b"x").await;
    let wide_entries = (0..=MAX_IMPORT_TREE_ENTRIES)
        .map(|index| (component(&format!("entry-{index:05}")), blob.clone()))
        .collect::<Vec<_>>();
    let wide_tree = git
        .write_tree(
            RepoPath::root(),
            BackendTree::from_sorted_entries(wide_entries),
        )
        .await
        .unwrap()
        .id()
        .clone();
    let wide = commit(git, vec![git.root_commit_id().clone()], wide_tree, 90).await;
    assert!(matches!(
        projection
            .import_reachable(
                repository.repo().store(),
                &[wide],
                &mut ImportMappings::default()
            )
            .await,
        Err(ProjectionError::ImportTreeWidthLimit { .. })
    ));

    let mut nested = flat_tree(git, vec![("leaf", b"x")]).await;
    for _ in 0..=MAX_IMPORT_TREE_DEPTH {
        nested = tree(git, "", vec![("d", TreeValue::Tree(nested))]).await;
    }
    let deep_tree = commit(git, vec![git.root_commit_id().clone()], nested, 91).await;
    assert!(matches!(
        projection
            .import_reachable(
                repository.repo().store(),
                &[deep_tree],
                &mut ImportMappings::default()
            )
            .await,
        Err(ProjectionError::ImportTreeDepthLimit { .. })
    ));

    assert_eq!(
        ProjectionError::ImportCommitLimit {
            actual: MAX_IMPORT_TOTAL_COMMITS + 1,
            limit: MAX_IMPORT_TOTAL_COMMITS,
        }
        .to_string(),
        format!(
            "Git import has {} commits, exceeding the safety limit of {MAX_IMPORT_TOTAL_COMMITS}",
            MAX_IMPORT_TOTAL_COMMITS + 1
        )
    );
}
