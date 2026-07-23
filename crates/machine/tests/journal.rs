use std::collections::BTreeMap;
use std::process::Command;

use devspace_kernel::{ObjectKind, Oid, parse_commit, validate};
use devspace_machine::{
    GitHttpTransport, GitProcessEnvironment, GitProcessMode, JournalFlowError, LeaseUpdate,
    MachineGitRepository, PushErrorKind, PushFailpoint, PushHead, QualifiedRef, RemoteUrl,
    fetch_with_journal, push, push_with_journal,
};
use gix::objs::{Kind as GitObjectKind, Write as _};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::settings::UserSettings;

fn settings() -> UserSettings {
    let mut config = StackedConfig::with_defaults();
    config.add_layer(
        ConfigLayer::parse(
            ConfigSource::User,
            r#"
                [user]
                name = "Journal Test"
                email = "journal@example.invalid"

                [git]
                write-change-id-header = true
            "#,
        )
        .unwrap(),
    );
    UserSettings::from_config(config).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn real_lease_push_preserves_signed_identity_bytes_and_observes_rejection() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MachineGitRepository::init(temp.path().join("machine"), &settings())
        .await
        .unwrap();
    let tree = write_tree(&repository, b"signed identity");
    let signed = write_commit(&repository, tree, &[], b"signed\n", true);
    let remote = temp.path().join("remote.git");
    let initialized = Command::new("git")
        .args(["init", "--bare", remote.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(initialized.status.success());
    let remote_url = RemoteUrl::new(remote.to_string_lossy());
    let reference = QualifiedRef::from_bookmark("main").unwrap();
    let environment = GitProcessEnvironment::new("git", GitProcessMode::Foreground);
    let report = push(
        repository.git_repo_path(),
        &remote_url,
        &BTreeMap::from([(
            reference.clone(),
            LeaseUpdate {
                expected_old_oid: None,
                new_oid: Some(signed),
            },
        )]),
        &environment,
    )
    .unwrap();
    assert_eq!(report.refs[&reference].observed_oid, Some(signed));
    let remote_bytes = Command::new("git")
        .arg(format!("--git-dir={}", remote.display()))
        .args(["cat-file", "commit", &oid_hex(signed)])
        .output()
        .unwrap();
    assert!(remote_bytes.status.success());
    assert_eq!(remote_bytes.stdout, raw_object(&repository, signed));

    let next = write_commit(&repository, tree, &[signed], b"next\n", false);
    let rejected = push(
        repository.git_repo_path(),
        &remote_url,
        &BTreeMap::from([(
            reference.clone(),
            LeaseUpdate {
                expected_old_oid: Some(Oid([0x22; 20])),
                new_oid: Some(next),
            },
        )]),
        &environment,
    )
    .unwrap_err();
    assert_eq!(rejected.kind, PushErrorKind::PushFailed);
    assert_eq!(rejected.report.refs[&reference].observed_oid, Some(signed));
    assert!(
        !rejected
            .report
            .diagnostic
            .command
            .contains(remote.to_str().unwrap())
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn live_v2_journal_push_recovery_and_fetch_proofs() {
    let total = std::time::Instant::now();
    let base_url = std::env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
    let shared_secret =
        std::env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
    if std::env::var_os("DEVSPACE_JOURNAL_CRASH_CHILD").is_some() {
        let repository_id = std::env::var("DEVSPACE_JOURNAL_REPOSITORY_ID").unwrap();
        let incarnation = std::env::var("DEVSPACE_JOURNAL_INCARNATION").unwrap();
        let repository_path = std::env::var_os("DEVSPACE_JOURNAL_MACHINE_PATH").unwrap();
        let crash_head = Oid::from_hex(
            std::env::var("DEVSPACE_JOURNAL_CRASH_OID")
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        let repository = MachineGitRepository::open(repository_path, &settings())
            .await
            .unwrap();
        let transport = GitHttpTransport::new(
            &base_url,
            &shared_secret,
            &"11".repeat(16),
            &repository_id,
            &incarnation,
        )
        .unwrap();
        let result = push_with_journal(
            &repository,
            &transport,
            "origin",
            &[PushHead {
                bookmark: "crash".to_owned(),
                canonical_oid: Some(crash_head),
            }],
            [0x33; 16],
            &GitProcessEnvironment::new("git", GitProcessMode::Foreground),
            PushFailpoint::AfterGitPush,
        )
        .await;
        if matches!(
            result,
            Err(JournalFlowError::AfterPushFailpoint { batch_id }) if batch_id == [0x33; 16]
        ) {
            std::process::exit(86);
        }
        panic!("crash child did not reach AFTER_PUSH: {result:?}");
    }
    let (repository_id, incarnation) = create_live_repository(&base_url, &shared_secret).await;
    let machine_a = "11".repeat(16);
    let machine_b = "22".repeat(16);
    let transport_a = GitHttpTransport::new(
        &base_url,
        &shared_secret,
        &machine_a,
        &repository_id,
        &incarnation,
    )
    .unwrap();
    let transport_b = GitHttpTransport::new(
        &base_url,
        &shared_secret,
        &machine_b,
        &repository_id,
        &incarnation,
    )
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let initialized = Command::new("git")
        .args(["init", "--bare", remote.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(initialized.status.success());
    transport_a
        .set_remote("origin", remote.to_str().unwrap())
        .await
        .unwrap();
    let a = MachineGitRepository::init(temp.path().join("machine-a"), &settings())
        .await
        .unwrap();
    let environment = GitProcessEnvironment::new("git", GitProcessMode::Foreground);

    // (a) Hidden-bearing canonical history becomes a public-only remote graph.
    let started = std::time::Instant::now();
    let (hidden_head, secret) = write_hidden_commit(&a, None, b"private-live-sentinel\0\xff");
    let pushed_hidden = push_with_journal(
        &a,
        &transport_a,
        "origin",
        &[PushHead {
            bookmark: "main".to_owned(),
            canonical_oid: Some(hidden_head),
        }],
        [0x31; 16],
        &environment,
        PushFailpoint::None,
    )
    .await
    .unwrap();
    let public_main = pushed_hidden.public_heads["main"].unwrap();
    assert_ne!(public_main, hidden_head);
    assert_eq!(remote_ref(&remote, "main"), public_main);
    assert_remote_blobs_exclude(&remote, &secret);
    let snapshot = transport_a.projection_snapshot_all().await.unwrap();
    let cursor = snapshot
        .cursors
        .iter()
        .find(|cursor| cursor.remote == "origin" && cursor.bookmark == "main")
        .unwrap();
    assert_eq!(cursor.canonical_oid, hidden_head);
    assert_eq!(cursor.public_oid, public_main);
    eprintln!("LIVE_PROOF a passed in {:?}", started.elapsed());

    // (b) An identity-projected signed commit crosses a real push byte-for-byte.
    let started = std::time::Instant::now();
    let signed_tree = write_tree(&a, b"signed live identity");
    let signed = write_commit(&a, signed_tree, &[], b"signed live\n", true);
    let signed_bytes = raw_object(&a, signed);
    let signed_result = push_with_journal(
        &a,
        &transport_a,
        "origin",
        &[PushHead {
            bookmark: "signed".to_owned(),
            canonical_oid: Some(signed),
        }],
        [0x32; 16],
        &environment,
        PushFailpoint::None,
    )
    .await
    .unwrap();
    assert_eq!(signed_result.public_heads["signed"], Some(signed));
    assert_eq!(remote_commit(&remote, signed), signed_bytes);
    eprintln!("LIVE_PROOF b passed in {:?}", started.elapsed());

    // (c) A stops after Git push; fresh B claims and recovers from packs only.
    let started = std::time::Instant::now();
    let (crash_head, _) = write_hidden_commit(&a, Some(hidden_head), b"crash-private\0\xfe");
    let crashed = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "live_v2_journal_push_recovery_and_fetch_proofs",
            "--ignored",
            "--nocapture",
        ])
        .env("DEVSPACE_JOURNAL_CRASH_CHILD", "1")
        .env("DEVSPACE_JOURNAL_REPOSITORY_ID", &repository_id)
        .env("DEVSPACE_JOURNAL_INCARNATION", &incarnation)
        .env("DEVSPACE_JOURNAL_MACHINE_PATH", a.path())
        .env("DEVSPACE_JOURNAL_CRASH_OID", oid_hex(crash_head))
        .output()
        .unwrap();
    assert_eq!(
        crashed.status.code(),
        Some(86),
        "crash child failed:\n{}{}",
        String::from_utf8_lossy(&crashed.stdout),
        String::from_utf8_lossy(&crashed.stderr)
    );
    let pending = transport_a.projection_snapshot_all().await.unwrap();
    let pending_public = pending
        .pending
        .iter()
        .find(|batch| batch.batch_id == [0x33; 16])
        .unwrap()
        .refs[0]
        .proposed_public_oid
        .unwrap();
    assert_eq!(remote_ref(&remote, "crash"), pending_public);
    let b = MachineGitRepository::init(temp.path().join("machine-b"), &settings())
        .await
        .unwrap();
    assert!(!b.git_repo_path().join("refs/devspace").exists());
    let recovered = push_with_journal(
        &b,
        &transport_b,
        "origin",
        &[PushHead {
            bookmark: "crash".to_owned(),
            canonical_oid: Some(crash_head),
        }],
        [0x34; 16],
        &environment,
        PushFailpoint::None,
    )
    .await
    .unwrap();
    assert_eq!(recovered.recovered_batches, vec![[0x33; 16]]);
    assert_eq!(recovered.outcome, "up-to-date");
    assert_eq!(raw_object(&b, crash_head), raw_object(&a, crash_head));
    assert_eq!(
        raw_object(&b, pending_public),
        raw_object(&a, pending_public)
    );
    let recovered_snapshot = transport_b.projection_snapshot_all().await.unwrap();
    assert!(recovered_snapshot.pending.is_empty());
    assert_eq!(
        recovered_snapshot
            .cursors
            .iter()
            .find(|cursor| cursor.bookmark == "crash")
            .unwrap()
            .public_oid,
        pending_public
    );
    eprintln!("LIVE_PROOF c passed in {:?}", started.elapsed());

    // (d) A foreign child of rewritten public P is fetched, hidden state is
    // replayed onto it, recordFetch stores L(F)->F, and the next push reuses it.
    let started = std::time::Instant::now();
    let public_tree = parse_commit(&raw_object(&a, public_main)).unwrap().tree;
    let foreign = write_commit(
        &a,
        public_tree,
        &[public_main],
        b"foreign after projected history\n",
        false,
    );
    let direct = push(
        a.git_repo_path(),
        &RemoteUrl::new(remote.to_string_lossy()),
        &BTreeMap::from([(
            QualifiedRef::from_bookmark("main").unwrap(),
            LeaseUpdate {
                expected_old_oid: Some(public_main),
                new_oid: Some(foreign),
            },
        )]),
        &environment,
    )
    .unwrap();
    assert_eq!(
        direct.refs[&QualifiedRef::from_bookmark("main").unwrap()].observed_oid,
        Some(foreign)
    );
    let fetched = fetch_with_journal(
        &b,
        &transport_b,
        "origin",
        &["main".to_owned()],
        [0x35; 16],
        &environment,
    )
    .await
    .unwrap();
    assert_eq!(fetched.public_heads["main"], foreign);
    assert_ne!(fetched.canonical_heads["main"], foreign);
    assert_eq!(fetched.mirrors.len(), 1);
    assert_eq!(fetched.mirrors[0].public_parents, vec![public_main]);
    assert_eq!(fetched.mirrors[0].canonical_parents, vec![hidden_head]);
    assert!(fetched.disclosure_warnings.is_empty());
    let canonical_foreign_bytes = raw_object(&b, fetched.canonical_heads["main"]);
    let canonical_foreign = parse_commit(&canonical_foreign_bytes).unwrap();
    assert_eq!(canonical_foreign.parents, vec![hidden_head]);
    assert!(tree_has_entry(&b, canonical_foreign.tree, b".dsprivate"));
    assert!(tree_has_entry(&b, canonical_foreign.tree, b"secret.bin"));
    let final_snapshot = transport_b.projection_snapshot_all().await.unwrap();
    let main_cursor = final_snapshot
        .cursors
        .iter()
        .find(|cursor| cursor.remote == "origin" && cursor.bookmark == "main")
        .unwrap();
    assert_eq!(main_cursor.public_oid, foreign);
    assert_eq!(main_cursor.canonical_oid, fetched.canonical_heads["main"]);
    assert!(final_snapshot.mappings.iter().any(|mapping| {
        mapping.canonical_oid == fetched.canonical_heads["main"] && mapping.public_oid == foreign
    }));

    let (local_after_fetch, _) = write_hidden_commit(
        &b,
        Some(fetched.canonical_heads["main"]),
        b"local-after-lift\0\xfd",
    );
    let pushed_after_fetch = push_with_journal(
        &b,
        &transport_b,
        "origin",
        &[PushHead {
            bookmark: "main".to_owned(),
            canonical_oid: Some(local_after_fetch),
        }],
        [0x36; 16],
        &environment,
        PushFailpoint::None,
    )
    .await
    .unwrap();
    let public_after_fetch = pushed_after_fetch.public_heads["main"].unwrap();
    assert_eq!(
        parse_commit(&raw_object(&b, public_after_fetch))
            .unwrap()
            .parents,
        vec![foreign]
    );
    assert_eq!(remote_ref(&remote, "main"), public_after_fetch);
    eprintln!("LIVE_PROOF d passed in {:?}", started.elapsed());
    eprintln!("LIVE_PROOF total {:?}", total.elapsed());
}

fn write_tree(repository: &MachineGitRepository, contents: &[u8]) -> Oid {
    let blob = write_raw(repository, ObjectKind::Blob, contents);
    let mut tree = b"100644 file\0".to_vec();
    tree.extend_from_slice(&blob.0);
    write_raw(repository, ObjectKind::Tree, &tree)
}

fn write_hidden_commit(
    repository: &MachineGitRepository,
    parent: Option<Oid>,
    secret: &[u8],
) -> (Oid, Vec<u8>) {
    let policy = write_raw(repository, ObjectKind::Blob, b"secret.bin\n");
    let public = write_raw(repository, ObjectKind::Blob, b"public live bytes\n");
    let secret_oid = write_raw(repository, ObjectKind::Blob, secret);
    let mut tree = Vec::new();
    for (name, oid) in [
        (b".dsprivate".as_slice(), policy),
        (b"public.txt".as_slice(), public),
        (b"secret.bin".as_slice(), secret_oid),
    ] {
        tree.extend_from_slice(b"100644 ");
        tree.extend_from_slice(name);
        tree.push(0);
        tree.extend_from_slice(&oid.0);
    }
    let tree = write_raw(repository, ObjectKind::Tree, &tree);
    (
        write_commit(
            repository,
            tree,
            &parent.into_iter().collect::<Vec<_>>(),
            b"hidden live\n",
            false,
        ),
        secret.to_vec(),
    )
}

fn write_commit(
    repository: &MachineGitRepository,
    tree: Oid,
    parents: &[Oid],
    message: &[u8],
    signed: bool,
) -> Oid {
    let mut bytes = format!("tree {}\n", oid_hex(tree)).into_bytes();
    for parent in parents {
        bytes.extend_from_slice(format!("parent {}\n", oid_hex(*parent)).as_bytes());
    }
    bytes.extend_from_slice(b"author Journal <journal@example.invalid> 1700000000 +0000\n");
    bytes.extend_from_slice(b"committer Journal <journal@example.invalid> 1700000000 +0000\n");
    if signed {
        bytes.extend_from_slice(
            b"gpgsig -----BEGIN PGP SIGNATURE-----\n fake\n -----END PGP SIGNATURE-----\n",
        );
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

fn tree_has_entry(repository: &MachineGitRepository, tree: Oid, name: &[u8]) -> bool {
    devspace_kernel::parse_tree(&raw_object(repository, tree))
        .unwrap()
        .entries
        .iter()
        .any(|entry| entry.name == name)
}

fn oid_hex(id: Oid) -> String {
    id.0.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn remote_ref(remote: &std::path::Path, bookmark: &str) -> Oid {
    let output = Command::new("git")
        .arg(format!("--git-dir={}", remote.display()))
        .args(["rev-parse", &format!("refs/heads/{bookmark}")])
        .output()
        .unwrap();
    assert!(output.status.success());
    Oid::from_hex(String::from_utf8(output.stdout).unwrap().trim().as_bytes()).unwrap()
}

fn remote_commit(remote: &std::path::Path, oid: Oid) -> Vec<u8> {
    let output = Command::new("git")
        .arg(format!("--git-dir={}", remote.display()))
        .args(["cat-file", "commit", &oid_hex(oid)])
        .output()
        .unwrap();
    assert!(output.status.success());
    output.stdout
}

fn assert_remote_blobs_exclude(remote: &std::path::Path, sentinel: &[u8]) {
    let listed = Command::new("git")
        .arg(format!("--git-dir={}", remote.display()))
        .args([
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ])
        .output()
        .unwrap();
    assert!(listed.status.success());
    for line in String::from_utf8(listed.stdout).unwrap().lines() {
        let (oid, kind) = line.split_once(' ').unwrap();
        if kind != "blob" {
            continue;
        }
        let blob = Command::new("git")
            .arg(format!("--git-dir={}", remote.display()))
            .args(["cat-file", "blob", oid])
            .output()
            .unwrap();
        assert!(blob.status.success());
        assert!(
            !blob
                .stdout
                .windows(sentinel.len())
                .any(|window| window == sentinel),
            "private sentinel reached remote blob {oid}"
        );
    }
}

async fn create_live_repository(base_url: &str, shared_secret: &str) -> (String, String) {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CreatedRepository {
        repository_id: String,
        incarnation: String,
    }
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let response = reqwest::Client::new()
        .post(format!("{}/repositories", base_url.trim_end_matches('/')))
        .header("authorization", format!("Bearer {shared_secret}"))
        .header("x-devspace-machine-id", "11".repeat(16))
        .json(&serde_json::json!({
            "name": format!("git-journal-spike-{suffix}"),
            "idempotencyKey": format!("{suffix:032x}"),
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    assert!(
        status.is_success(),
        "create failed {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    let created: CreatedRepository = serde_json::from_slice(&bytes).unwrap();
    (created.repository_id, created.incarnation)
}
