use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use devspace_machine::{
    GitProjection, HttpTransport, MachineConfig, MachineId, MachineRepository, MachineStoreError,
    ProjectionSnapshot, RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName,
    SharedSecret,
};

mod support;

use support::fake_worker::{create_server, read_http_request, respond};
use support::{
    ds_command_with_home as ds_command, ds_with_home as ds, machine_store, seal_commit, settings,
    stderr, stdout, write_cli_config,
};

const FIRST_MACHINE_ID: &str = "12121212121212121212121212121212";
const SECOND_MACHINE_ID: &str = "34343434343434343434343434343434";
const DEVELOPMENT_SECRET: &str = "git-push-development-secret";
const PRIVATE_SENTINEL: &[u8] = b"DEVSPACE_PRIVATE_SENTINEL\0\xff";

#[tokio::test]
async fn devspace_checkout_owns_fetch_and_fences_unowned_git_commands() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("machine");
    fs::create_dir_all(&home).unwrap();
    configure_machine(
        &home,
        "http://127.0.0.1:1",
        FIRST_MACHINE_ID,
        DEVELOPMENT_SECRET,
    );
    let config = write_cli_config(&home);
    let checkout = local_checkout(&home, &config, "git-fence").await;

    let fetch = ds(&checkout, &home, &config, &["git", "fetch"]);
    assert_eq!(fetch.status.code(), Some(1));
    assert!(
        !stderr(&fetch).contains("not yet implemented"),
        "{}",
        stderr(&fetch)
    );

    let literal_fetch = ds(&checkout, &home, &config, &["git", "fetch", "-b", "a..b"]);
    assert_eq!(literal_fetch.status.code(), Some(1));
    assert!(
        stderr(&literal_fetch).contains("bookmark is not a valid Git branch name"),
        "{}",
        stderr(&literal_fetch)
    );

    let export = ds(&checkout, &home, &config, &["git", "export"]);
    assert_eq!(export.status.code(), Some(1));
    assert!(
        stderr(&export).contains("Devspace owns the Git boundary"),
        "{}",
        stderr(&export)
    );

    let broad_push = ds(&checkout, &home, &config, &["git", "push", "--all"]);
    assert_eq!(broad_push.status.code(), Some(1));
    assert!(
        stderr(&broad_push).contains("does not support `all`"),
        "{}",
        stderr(&broad_push)
    );

    let store = machine_store(&home);
    let entry = store
        .resolve(&RepositoryName::parse("git-fence").unwrap())
        .unwrap()
        .unwrap();
    store
        .unregister_repository(
            &RepositoryName::parse("git-fence").unwrap(),
            &entry.identity,
        )
        .unwrap();
    let unregistered = ds(&checkout, &home, &config, &["git", "fetch", "-b", "main"]);
    assert_eq!(unregistered.status.code(), Some(1));
    assert!(
        stderr(&unregistered).contains("repository-not-registered"),
        "{}",
        stderr(&unregistered)
    );
}

#[tokio::test]
async fn git_push_waits_for_the_repository_sync_lock_then_proceeds() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("machine");
    fs::create_dir_all(&home).unwrap();
    configure_machine(
        &home,
        "http://127.0.0.1:1",
        FIRST_MACHINE_ID,
        DEVELOPMENT_SECRET,
    );
    let config = write_cli_config(&home);
    let checkout = local_checkout(&home, &config, "locked-push").await;
    let store = machine_store(&home);
    let entry = store
        .resolve(&RepositoryName::parse("locked-push").unwrap())
        .unwrap()
        .unwrap();
    let guard = store.try_lock_repository_sync(&entry.identity).unwrap();
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        drop(guard);
    });

    let started = Instant::now();
    let output = ds(&checkout, &home, &config, &["git", "push", "-b", "main"]);
    let elapsed = started.elapsed();
    release.join().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        elapsed >= Duration::from_millis(200),
        "push did not wait for the held lock: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "push did not proceed after the lock was released: {elapsed:?}"
    );
    let diagnostic = stderr(&output);
    assert_eq!(
        diagnostic
            .matches("Waiting for an in-flight operation")
            .count(),
        1,
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains("already being synchronized"),
        "{diagnostic}"
    );
}

#[tokio::test]
async fn git_push_holds_the_repository_sync_lock_after_sync_completes() {
    let (base_url, push_reached, release_push, server) = cloud_paused_at_remote_list();
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("machine");
    fs::create_dir_all(&home).unwrap();
    configure_machine(&home, &base_url, FIRST_MACHINE_ID, DEVELOPMENT_SECRET);
    let config = write_cli_config(&home);
    let checkout = local_checkout(&home, &config, "lock-lifetime").await;
    let store = machine_store(&home);
    let entry = store
        .resolve(&RepositoryName::parse("lock-lifetime").unwrap())
        .unwrap()
        .unwrap();
    let projection_path = store.repository_projection_path(&entry.identity);
    fs::create_dir_all(projection_path.join("store")).unwrap();
    let child = ds_command(&checkout, &home, &config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["git", "push", "-b", "main"])
        .spawn()
        .unwrap();

    push_reached
        .recv_timeout(Duration::from_secs(10))
        .expect("push did not reach the post-sync projection request");
    assert!(matches!(
        store.try_lock_repository_sync(&entry.identity),
        Err(MachineStoreError::RepositorySyncAlreadyLocked { .. })
    ));
    release_push.send(()).unwrap();

    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no such Git remote `origin`"));
    assert!(stderr(&output).contains("remote-not-found"));
    assert!(stderr(&output).contains("Rebuilt the local Git projection sidecar"));
    GitProjection::open(&projection_path, &settings()).unwrap();
    drop(store.try_lock_repository_sync(&entry.identity).unwrap());
}

#[tokio::test]
async fn remote_add_prints_the_workers_kebab_case_error_code_without_the_url() {
    let (base_url, server) = create_server(|_, _, stream| {
        let body = r#"{"error":"remote URL must not contain userinfo credentials","code":"credentials-in-remote-url"}"#;
        respond(stream, "400 Bad Request", body);
        true
    });
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("machine");
    fs::create_dir_all(&home).unwrap();
    configure_machine(&home, &base_url, FIRST_MACHINE_ID, DEVELOPMENT_SECRET);
    let config = write_cli_config(&home);
    let checkout = local_checkout(&home, &config, "remote-error").await;
    let sentinel = "REMOTE_PASSWORD_SENTINEL";
    let url = format!("https://user:{sentinel}@example.invalid/repo.git");

    let output = ds(
        &checkout,
        &home,
        &config,
        &["git", "remote", "add", "origin", &url],
    );
    let diagnostic = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        !server.join().unwrap().is_empty(),
        "CLI never contacted the Worker: {diagnostic}"
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        diagnostic.contains("credentials-in-remote-url"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains(sentinel), "{diagnostic}");
    assert!(!diagnostic.contains(&url), "{diagnostic}");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn pushes_hidden_history_without_publishing_private_objects() {
    let fixture = LiveFixture::new("happy").await;
    fs::write(fixture.checkout_a.join(".dsprivate"), b"secret*\n").unwrap();
    fs::write(fixture.checkout_a.join("secret.bin"), PRIVATE_SENTINEL).unwrap();
    fs::write(fixture.checkout_a.join("visible.txt"), b"public one\n").unwrap();
    seal_commit(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "public main",
    );
    set_bookmark(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
        "@-",
    );
    fixture.add_origin(&fixture.checkout_a, &fixture.home_a, &fixture.config_a);
    let listed = ds(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        &["git", "remote", "list"],
    );
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert_eq!(
        stdout(&listed).trim(),
        format!("origin {}", fixture.remote.display())
    );

    let pushed = fixture.push(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
    );
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    assert!(stderr(&pushed).contains("created"), "{}", stderr(&pushed));
    assert!(remote_ref(&fixture.remote, "main").is_some());
    assert_public_object_store(&fixture.remote, PRIVATE_SENTINEL);

    let before = fixture.snapshot(&fixture.home_a).await;
    let repeated = fixture.push(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
    );
    assert!(repeated.status.success(), "{}", stderr(&repeated));
    assert!(
        stderr(&repeated).contains("up to date"),
        "{}",
        stderr(&repeated)
    );
    let after = fixture.snapshot(&fixture.home_a).await;
    assert_eq!(after, before);

    set_bookmark(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "release",
        "@-",
    );
    let second = fixture.push(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "release",
    );
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(remote_ref(&fixture.remote, "release").is_some());
    assert_public_object_store(&fixture.remote, PRIVATE_SENTINEL);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn fresh_machine_claims_and_replays_a_push_left_pending_after_git_moved() {
    let fixture = LiveFixture::new("recovery").await;
    fs::write(fixture.checkout_a.join(".dsprivate"), b"secret*\n").unwrap();
    fs::write(fixture.checkout_a.join("secret.bin"), PRIVATE_SENTINEL).unwrap();
    fs::write(fixture.checkout_a.join("visible.txt"), b"before crash\n").unwrap();
    seal_commit(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "pending main",
    );
    set_bookmark(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
        "@-",
    );
    fixture.add_origin(&fixture.checkout_a, &fixture.home_a, &fixture.config_a);

    let crashed = ds_command(&fixture.checkout_a, &fixture.home_a, &fixture.config_a)
        .env("DEVSPACE_FAILPOINT", "after_git_push_before_finalize")
        .args(["git", "push", "-b", "main"])
        .output()
        .unwrap();
    assert_eq!(crashed.status.code(), Some(86), "{}", stderr(&crashed));
    let first_remote = remote_ref(&fixture.remote, "main").expect("Git push moved main");
    let pending = fixture.snapshot(&fixture.home_a).await;
    assert_eq!(pending.pending.len(), 1);

    fs::create_dir_all(&fixture.home_b).unwrap();
    configure_machine(
        &fixture.home_b,
        &fixture.base_url,
        SECOND_MACHINE_ID,
        &fixture.shared_secret,
    );
    let config_b = write_cli_config(&fixture.home_b);
    let checkout_b = fixture.home_b.join("checkout");
    let added = ds(
        &fixture.home_b,
        &fixture.home_b,
        &config_b,
        &[
            "add",
            &fixture.repository_name,
            "-r",
            "main",
            checkout_b.to_str().unwrap(),
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    let machine_b_entry = machine_store(&fixture.home_b)
        .resolve(&RepositoryName::parse(&fixture.repository_name).unwrap())
        .unwrap()
        .unwrap();
    assert!(
        !machine_store(&fixture.home_b)
            .repository_projection_path(&machine_b_entry.identity)
            .exists(),
        "fresh machine unexpectedly has a Git projection sidecar"
    );

    let recovered = fixture.push(&checkout_b, &fixture.home_b, &config_b, "main");
    assert!(recovered.status.success(), "{}", stderr(&recovered));
    assert_eq!(remote_ref(&fixture.remote, "main"), Some(first_remote));
    let accepted = fixture.snapshot(&fixture.home_b).await;
    assert!(accepted.pending.is_empty());
    assert!(accepted.cursors.iter().any(|cursor| {
        cursor.remote == "origin" && cursor.bookmark == "main" && cursor.git_oid == first_remote
    }));

    fs::write(checkout_b.join("visible.txt"), b"after recovery\n").unwrap();
    seal_commit(&checkout_b, &fixture.home_b, &config_b, "advanced main");
    set_bookmark(&checkout_b, &fixture.home_b, &config_b, "main", "@-");
    let advanced = fixture.push(&checkout_b, &fixture.home_b, &config_b, "main");
    assert!(advanced.status.success(), "{}", stderr(&advanced));
    let advanced_remote = remote_ref(&fixture.remote, "main").expect("advanced remote main");
    assert_ne!(advanced_remote, first_remote);
    let advanced_snapshot = fixture.snapshot(&fixture.home_b).await;
    assert!(advanced_snapshot.pending.is_empty());
    assert!(advanced_snapshot.cursors.iter().any(|cursor| {
        cursor.remote == "origin" && cursor.bookmark == "main" && cursor.git_oid == advanced_remote
    }));
    assert_public_object_store(&fixture.remote, PRIVATE_SENTINEL);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn deleting_a_local_bookmark_deletes_the_journaled_remote_ref() {
    let fixture = LiveFixture::new("deletion").await;
    fs::write(fixture.checkout_a.join("visible.txt"), b"delete me\n").unwrap();
    seal_commit(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "deletion main",
    );
    set_bookmark(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
        "@-",
    );
    set_bookmark(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "feature",
        "@-",
    );
    fixture.add_origin(&fixture.checkout_a, &fixture.home_a, &fixture.config_a);
    for bookmark in ["main", "feature"] {
        let created = fixture.push(
            &fixture.checkout_a,
            &fixture.home_a,
            &fixture.config_a,
            bookmark,
        );
        assert!(created.status.success(), "{}", stderr(&created));
    }

    let deleted = ds(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        &["bookmark", "delete", "feature"],
    );
    assert!(deleted.status.success(), "{}", stderr(&deleted));
    let pushed = fixture.push(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "feature",
    );
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    assert!(stderr(&pushed).contains("deleted"), "{}", stderr(&pushed));
    assert!(remote_ref(&fixture.remote, "feature").is_none());
    assert!(remote_ref(&fixture.remote, "main").is_some());
    assert!(
        !fixture
            .snapshot(&fixture.home_a)
            .await
            .cursors
            .iter()
            .any(|cursor| { cursor.remote == "origin" && cursor.bookmark == "feature" })
    );

    // Deleting the remote's current branch is refused remote-side; the journal
    // must abort without losing the cursor, and the CLI must surface the
    // remote's stated reason.
    let deleted = ds(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        &["bookmark", "delete", "main"],
    );
    assert!(deleted.status.success(), "{}", stderr(&deleted));
    let refused = fixture.push(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
    );
    assert!(!refused.status.success());
    let refusal = stderr(&refused);
    assert!(refusal.contains("delete the current branch"), "{refusal}");
    assert!(remote_ref(&fixture.remote, "main").is_some());
    assert!(
        fixture
            .snapshot(&fixture.home_a)
            .await
            .cursors
            .iter()
            .any(|cursor| { cursor.remote == "origin" && cursor.bookmark == "main" })
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn failed_git_transport_redacts_the_registered_remote_url() {
    let fixture = LiveFixture::new("redaction").await;
    fs::write(fixture.checkout_a.join("visible.txt"), b"redaction\n").unwrap();
    seal_commit(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "redaction main",
    );
    set_bookmark(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
        "@-",
    );
    let sentinel = "DO_NOT_PRINT_REMOTE_SENTINEL";
    let missing = fixture
        .temp
        .path()
        .join(format!("missing-{sentinel}/origin.git"));
    let full_url = missing.to_str().unwrap().to_owned();
    let added = ds(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        &["git", "remote", "add", "origin", &full_url],
    );
    assert!(added.status.success(), "{}", stderr(&added));

    let pushed = fixture.push(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
    );
    assert_eq!(pushed.status.code(), Some(1));
    let diagnostics = format!("{}{}", stdout(&pushed), stderr(&pushed));
    assert!(!diagnostics.contains(sentinel), "{diagnostics}");
    assert!(!diagnostics.contains(&full_url), "{diagnostics}");
    let log = sync_log(&fixture.home_a, &fixture.repository_name);
    assert!(!log.contains(sentinel), "{log}");
    assert!(!log.contains(&full_url), "{log}");
}

struct LiveFixture {
    temp: tempfile::TempDir,
    base_url: String,
    shared_secret: String,
    repository_name: String,
    home_a: PathBuf,
    home_b: PathBuf,
    config_a: PathBuf,
    checkout_a: PathBuf,
    remote: PathBuf,
}

impl LiveFixture {
    async fn new(label: &str) -> Self {
        let base_url = std::env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
        let shared_secret =
            std::env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
        let temp = tempfile::tempdir().unwrap();
        let home_a = temp.path().join("machine-a");
        let home_b = temp.path().join("machine-b");
        fs::create_dir_all(&home_a).unwrap();
        configure_machine(&home_a, &base_url, FIRST_MACHINE_ID, &shared_secret);
        let config_a = write_cli_config(&home_a);
        let repository_name = unique_repository_name(temp.path(), label);
        let created = ds(
            &home_a,
            &home_a,
            &config_a,
            &["repo", "new", &repository_name],
        );
        assert!(created.status.success(), "{}", stderr(&created));
        let checkout_a = home_a.join("checkout");
        let added = ds(
            &home_a,
            &home_a,
            &config_a,
            &[
                "add",
                &repository_name,
                "-r",
                "root()",
                checkout_a.to_str().unwrap(),
            ],
        );
        assert!(added.status.success(), "{}", stderr(&added));
        let remote = temp.path().join("origin.git");
        git(&["init", "--bare", remote.to_str().unwrap()], None);
        Self {
            temp,
            base_url,
            shared_secret,
            repository_name,
            home_a,
            home_b,
            config_a,
            checkout_a,
            remote,
        }
    }

    fn add_origin(&self, checkout: &Path, home: &Path, config: &Path) {
        let added = ds(
            checkout,
            home,
            config,
            &[
                "git",
                "remote",
                "add",
                "origin",
                self.remote.to_str().unwrap(),
            ],
        );
        assert!(added.status.success(), "{}", stderr(&added));
    }

    fn push(&self, checkout: &Path, home: &Path, config: &Path, bookmark: &str) -> Output {
        ds(checkout, home, config, &["git", "push", "-b", bookmark])
    }

    async fn snapshot(&self, home: &Path) -> ProjectionSnapshot {
        let store = machine_store(home);
        let entry = store
            .resolve(&RepositoryName::parse(&self.repository_name).unwrap())
            .unwrap()
            .unwrap();
        let config = store.load_config().unwrap();
        let transport = HttpTransport::new(
            &config,
            entry.identity.repository_id.as_str(),
            parse_incarnation(entry.identity.incarnation.as_str()),
        )
        .unwrap();
        load_snapshot(&transport).await
    }
}

fn cloud_paused_at_remote_list() -> (String, Receiver<()>, SyncSender<()>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let (push_reached_tx, push_reached_rx) = sync_channel(0);
    let (release_push_tx, release_push_rx) = sync_channel(0);
    let server = thread::spawn(move || {
        loop {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let request_line = request.lines().next().unwrap();
            if request_line.starts_with("GET ") && request_line.contains("/remotes?") {
                push_reached_tx.send(()).unwrap();
                release_push_rx.recv().unwrap();
                respond(&mut stream, "200 OK", r#"{"remotes":[]}"#);
                return;
            }
            if request_line.starts_with("GET ") && request_line.contains("/packs?") {
                respond(
                    &mut stream,
                    "200 OK",
                    r#"{"packs":[],"nextAfter":0,"through":0,"hasMore":false}"#,
                );
            } else if request_line.starts_with("GET ") && request_line.contains("/heads?") {
                respond(&mut stream, "200 OK", r#"{"cursor":0,"heads":[]}"#);
            } else if request_line.starts_with("POST ")
                && request_line.contains("/objects/inventory ")
            {
                let body: serde_json::Value = serde_json::from_str(request_body(&request)).unwrap();
                respond(
                    &mut stream,
                    "200 OK",
                    &serde_json::json!({ "objects": body["objects"] }).to_string(),
                );
            } else if request_line.starts_with("POST ") && request_line.contains("/heads ") {
                let body: serde_json::Value = serde_json::from_str(request_body(&request)).unwrap();
                respond(
                    &mut stream,
                    "200 OK",
                    &serde_json::json!({ "cursor": 1, "heads": [body["newHead"]] }).to_string(),
                );
            } else {
                panic!("unexpected fake cloud request: {request_line}");
            }
        }
    });
    (base_url, push_reached_rx, release_push_tx, server)
}

fn request_body(request: &str) -> &str {
    request.split_once("\r\n\r\n").unwrap().1
}

async fn local_checkout(home: &Path, config: &Path, name: &str) -> PathBuf {
    let store = machine_store(home);
    let identity = RepositoryIdentity::new(
        RepositoryId::parse("ab".repeat(32)).unwrap(),
        RepositoryIncarnation::parse("cd".repeat(16)).unwrap(),
    );
    let entry = store
        .register_repository(RepositoryName::parse(name).unwrap(), identity)
        .unwrap();
    MachineRepository::init(&entry.native_repository_path, &settings())
        .await
        .unwrap();
    let checkout = home.join("checkout");
    let added = ds(
        home,
        home,
        config,
        &["add", name, "-r", "root()", checkout.to_str().unwrap()],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    checkout
}

fn configure_machine(root: &Path, base_url: &str, machine_id: &str, secret: &str) {
    machine_store(root)
        .write_config(
            &MachineConfig::new(
                base_url,
                MachineId::parse(machine_id).unwrap(),
                SharedSecret::new(secret).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
}

fn set_bookmark(cwd: &Path, home: &Path, config: &Path, name: &str, revision: &str) {
    let output = ds(
        cwd,
        home,
        config,
        &["bookmark", "set", name, "-r", revision],
    );
    assert!(output.status.success(), "{}", stderr(&output));
}

fn remote_ref(remote: &Path, bookmark: &str) -> Option<[u8; 20]> {
    let output = git_command(
        &[
            "show-ref",
            "--hash",
            "--verify",
            &format!("refs/heads/{bookmark}"),
        ],
        Some(remote),
    )
    .output()
    .unwrap();
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).unwrap();
    Some(parse_git_oid(value.trim()))
}

fn assert_public_object_store(remote: &Path, sentinel: &[u8]) {
    let objects = git_output(
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
        Some(remote),
    );
    for line in objects.lines() {
        let (id, _) = line.split_once(' ').unwrap();
        let output = git_command(&["cat-file", "-p", id], Some(remote))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(
            !contains_bytes(&output.stdout, sentinel),
            "private sentinel entered Git"
        );
        assert!(
            !contains_bytes(&output.stdout, b".dsprivate"),
            ".dsprivate entered Git"
        );
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn git(args: &[&str], git_dir: Option<&Path>) {
    let output = git_command(args, git_dir).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        stderr(&output)
    );
}

fn git_output(args: &[&str], git_dir: Option<&Path>) -> String {
    let output = git_command(args, git_dir).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    String::from_utf8(output.stdout).unwrap()
}

fn git_command(args: &[&str], git_dir: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(git_dir) = git_dir {
        command.arg("--git-dir").arg(git_dir);
    }
    command.args(args);
    command
}

fn unique_repository_name(temp: &Path, label: &str) -> String {
    let suffix = temp
        .file_name()
        .unwrap()
        .to_string_lossy()
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(|byte| byte.to_ascii_lowercase() as char)
        .collect::<String>();
    format!("git-push-{label}-{}-{suffix}", std::process::id())
}

fn sync_log(home: &Path, repository_name: &str) -> String {
    let store = machine_store(home);
    let entry = store
        .resolve(&RepositoryName::parse(repository_name).unwrap())
        .unwrap()
        .unwrap();
    fs::read_to_string(
        entry
            .native_repository_path
            .parent()
            .unwrap()
            .join("sync.log"),
    )
    .unwrap_or_default()
}

async fn load_snapshot(transport: &HttpTransport) -> ProjectionSnapshot {
    let mut snapshot = transport.get(0, None).await.unwrap();
    let through = snapshot.through;
    while snapshot.has_more {
        let page = transport
            .get(snapshot.next_after, Some(through))
            .await
            .unwrap();
        snapshot.mappings.extend(page.mappings);
        snapshot.next_after = page.next_after;
        snapshot.has_more = page.has_more;
    }
    snapshot
}

fn parse_incarnation(value: &str) -> [u8; 16] {
    std::array::from_fn(|index| u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).unwrap())
}

fn parse_git_oid(value: &str) -> [u8; 20] {
    std::array::from_fn(|index| u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).unwrap())
}
