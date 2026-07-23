use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use devspace_machine::MachineGitRepository as MachineRepository;
use devspace_machine::{
    GitHttpTransport, MachineStoreError, ProjectionGitSnapshot, RepositoryId, RepositoryIdentity,
    RepositoryIncarnation, RepositoryName, encode_lower_hex,
};
use jj_lib::op_store::RemoteRef;
use jj_lib::ref_name::{RefName, RemoteName, RemoteRefSymbol};

mod support;

use support::fake_worker::{create_server, read_http_request, respond};
use support::{
    configure_machine_as as configure_machine, ds_command_with_home as ds_command,
    ds_with_home as ds, machine_store, seal_commit, settings, stderr, stdout, write_cli_config,
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
async fn git_push_resolves_bookmarks_after_waiting_for_the_sync_lock() {
    let fixture = FakePushFixture::new("post-sync-resolution").await;
    fixture.commit("main", "first\n");
    let created = fixture.push(&["-b", "main"]);
    assert!(created.status.success(), "{}", stderr(&created));
    fixture.commit("main", "second\n");

    let store = machine_store(&fixture.home);
    let guard = store
        .try_lock_repository_sync(&fixture.entry().identity)
        .unwrap();
    let child = ds_command(&fixture.checkout, &fixture.home, &fixture.config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["git", "push", "-b", "main"])
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(200));
    fixture.commit("main", "third\n");
    drop(guard);

    let pushed = child.wait_with_output().unwrap();
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    let view = fixture.bookmark_list();
    assert_eq!(
        bookmark_target(&view, "main"),
        bookmark_target(&view, "main@origin"),
        "{view}"
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

#[tokio::test]
async fn push_moves_the_remote_tracking_bookmark_to_the_local_target() {
    let fixture = FakePushFixture::new("view-move").await;
    fixture.commit("main", "first\n");
    let created = fixture.push(&["-b", "main"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let first_view = fixture.bookmark_list();
    assert!(
        first_view.contains("main@origin|true|true|"),
        "{first_view}"
    );

    fixture.commit("main", "second\n");
    let updated = fixture.push(&["-b", "main"]);
    assert!(updated.status.success(), "{}", stderr(&updated));

    let updated_view = fixture.bookmark_list();
    assert!(
        updated_view.contains("main@origin|true|true|"),
        "{updated_view}"
    );
    assert_ne!(updated_view, first_view);
}

#[tokio::test]
async fn push_records_a_jj_operation_with_the_stock_description() {
    let fixture = FakePushFixture::new("operation").await;
    fixture.commit("main", "operation\n");
    let pushed = fixture.push(&["-b", "main"]);
    assert!(pushed.status.success(), "{}", stderr(&pushed));

    assert_eq!(
        fixture.operation_description(),
        "push bookmark main to git remote origin"
    );
}

#[tokio::test]
async fn creation_push_tracks_the_new_remote_bookmark() {
    let fixture = FakePushFixture::new("auto-track").await;
    fixture.commit("main", "created\n");

    let pushed = fixture.push(&["-b", "main"]);
    assert!(pushed.status.success(), "{}", stderr(&pushed));

    let view = fixture.bookmark_list();
    assert!(view.contains("main@origin|true|true|"), "{view}");
}

#[tokio::test]
async fn deletion_push_removes_the_remote_tracking_bookmark() {
    let fixture = FakePushFixture::new("view-delete").await;
    fixture.commit("feature", "created\n");
    let created = fixture.push(&["-b", "feature"]);
    assert!(created.status.success(), "{}", stderr(&created));
    fixture.delete_bookmark("feature");

    let deleted = fixture.push(&["-b", "feature"]);
    assert!(deleted.status.success(), "{}", stderr(&deleted));

    let view = fixture.bookmark_list();
    assert!(!view.contains("feature@origin|"), "{view}");
    assert_eq!(
        fixture.operation_description(),
        "push bookmark feature to git remote origin"
    );
    assert!(remote_ref(&fixture.remote, "feature").is_none());
}

#[tokio::test]
async fn recovered_pending_deletion_is_reported_as_success() {
    let fixture = FakePushFixture::new("recovered-deletion").await;
    fixture.commit("feature", "created\n");
    let created = fixture.push(&["-b", "feature"]);
    assert!(created.status.success(), "{}", stderr(&created));
    fixture.delete_bookmark("feature");

    let crashed = ds_command(&fixture.checkout, &fixture.home, &fixture.config)
        .env("DEVSPACE_FAILPOINT", "after_git_push_before_finalize")
        .args(["git", "push", "-b", "feature"])
        .output()
        .unwrap();
    assert_eq!(crashed.status.code(), Some(86), "{}", stderr(&crashed));
    assert!(remote_ref(&fixture.remote, "feature").is_none());

    let recovered = fixture.push(&["-b", "feature"]);
    assert!(recovered.status.success(), "{}", stderr(&recovered));
    assert!(
        stderr(&recovered).contains("pushed feature to origin: deleted"),
        "{}",
        stderr(&recovered)
    );
    let view = fixture.bookmark_list();
    assert!(!view.contains("feature@origin|"), "{view}");
}

#[tokio::test]
async fn deleted_combines_with_explicit_bookmarks() {
    let fixture = FakePushFixture::new("deleted-selection").await;
    fixture.commit("feature", "created\n");
    let created = fixture.push(&["-b", "feature"]);
    assert!(created.status.success(), "{}", stderr(&created));
    fixture.delete_bookmark("feature");
    fixture.commit("main", "main\n");

    let deleted = fixture.push(&["-b", "main", "--deleted"]);
    assert!(deleted.status.success(), "{}", stderr(&deleted));
    assert!(stderr(&deleted).contains("pushed feature to origin: deleted"));
    assert!(stderr(&deleted).contains("pushed main to origin: created"));
    assert!(!fixture.remote_bookmark("feature").await.is_present());
    assert!(remote_ref(&fixture.remote, "feature").is_none());
    assert!(remote_ref(&fixture.remote, "main").is_some());
}

#[tokio::test]
async fn up_to_date_push_repairs_a_stale_remote_tracking_bookmark() {
    let fixture = FakePushFixture::new("self-heal").await;
    fixture.commit("main", "created\n");
    let created = fixture.push(&["-b", "main"]);
    assert!(created.status.success(), "{}", stderr(&created));
    fixture.remove_remote_bookmark("main").await;

    let repaired = fixture.push(&["-b", "main"]);
    assert!(repaired.status.success(), "{}", stderr(&repaired));
    assert!(stderr(&repaired).contains("up to date"));

    let view = fixture.bookmark_list();
    assert!(view.contains("main@origin|true|true|"), "{view}");
    assert_eq!(
        fixture.operation_description(),
        "push bookmark main to git remote origin"
    );
}

#[tokio::test]
async fn multiple_explicit_bookmarks_use_sorted_operation_description() {
    let fixture = FakePushFixture::new("operation-sorted").await;
    fixture.commit("zeta", "zeta\n");
    fixture.commit("alpha", "alpha\n");

    let pushed = fixture.push(&["-b", "zeta", "-b", "alpha"]);
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    assert_eq!(
        fixture.operation_description(),
        "push bookmarks alpha, zeta to git remote origin"
    );
}

#[tokio::test]
async fn explicit_push_refuses_a_present_untracked_remote_bookmark() {
    let fixture = FakePushFixture::new("untracked-explicit").await;
    fixture.commit("main", "first\n");
    let created = fixture.push(&["-b", "main"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let untracked = fixture.ds(&["bookmark", "untrack", "main@origin"]);
    assert!(untracked.status.success(), "{}", stderr(&untracked));
    fixture.commit("main", "second\n");

    let refused = fixture.push(&["-b", "main"]);
    assert_eq!(refused.status.code(), Some(1));
    let diagnostic = stderr(&refused);
    assert!(
        diagnostic.contains("Non-tracking remote bookmark main@origin exists"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("ds bookmark track main --remote=origin"),
        "{diagnostic}"
    );
}

#[tokio::test]
async fn deleted_selects_only_absent_local_tracked_bookmarks_on_the_remote() {
    let fixture = FakePushFixture::new("deleted-matrix").await;

    fixture.commit("deleted", "deleted\n");
    assert!(fixture.push(&["-b", "deleted"]).status.success());
    fixture.delete_bookmark("deleted");

    fixture.commit("untracked", "untracked\n");
    assert!(fixture.push(&["-b", "untracked"]).status.success());
    let forgotten = fixture.ds(&["bookmark", "forget", "untracked"]);
    assert!(forgotten.status.success(), "{}", stderr(&forgotten));

    fixture.commit("local", "local\n");
    assert!(fixture.push(&["-b", "local"]).status.success());

    fixture.add_remote("backup");
    fixture.commit("elsewhere", "elsewhere\n");
    assert!(
        fixture
            .push(&["-b", "elsewhere", "--remote", "backup"])
            .status
            .success()
    );
    fixture.delete_bookmark("elsewhere");

    let pushed = fixture.push(&["--deleted"]);
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    let diagnostic = stderr(&pushed);
    assert!(diagnostic.contains("pushed deleted to origin: deleted"));
    assert!(!diagnostic.contains("pushed untracked"), "{diagnostic}");
    assert!(!diagnostic.contains("pushed local"), "{diagnostic}");
    assert!(!diagnostic.contains("pushed elsewhere"), "{diagnostic}");
    assert!(remote_ref(&fixture.remote, "deleted").is_none());
    assert!(remote_ref(&fixture.remote, "untracked").is_some());
    assert!(remote_ref(&fixture.remote, "local").is_some());
    assert!(remote_ref(&fixture.remote, "elsewhere").is_some());
    assert_eq!(
        fixture.operation_description(),
        "push all deleted bookmarks to git remote origin"
    );
}

#[tokio::test]
async fn push_surfaces_missing_mapped_object_repair() {
    let fixture = FakePushFixture::new("missing-mapped-object").await;
    fs::write(fixture.checkout.join(".dsprivate"), b"/secret.txt\n").unwrap();
    fs::write(fixture.checkout.join("secret.txt"), b"private\n").unwrap();
    fixture.commit("main", "first\n");
    let created = fixture.push(&["-b", "main"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let oid = remote_ref(&fixture.remote, "main").unwrap();
    let hex = encode_lower_hex(&oid);
    let repository = fixture.repository().await;
    let object_path = repository
        .git_repo_path()
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    drop(repository);
    fs::remove_file(&object_path).unwrap();
    fixture.commit("main", "second\n");

    let pushed = fixture.push(&["-b", "main"]);
    assert_eq!(pushed.status.code(), Some(1));
    let diagnostic = stderr(&pushed);
    assert!(
        diagnostic.contains("failed to read Git object"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("ds git fetch --remote origin -b main"),
        "{diagnostic}"
    );
}

#[tokio::test]
async fn new_bookmark_reuses_the_git_oid_of_imported_history() {
    let fixture = FakePushFixture::new("imported-history").await;
    let imported_oid = signed_commit(&fixture.remote, "main");

    let fetched = fixture.fetch(&["-b", "main"]);
    assert!(fetched.status.success(), "{}", stderr(&fetched));
    let tracked = ds(
        &fixture.checkout,
        &fixture.home,
        &fixture.config,
        &["bookmark", "track", "main@origin"],
    );
    assert!(tracked.status.success(), "{}", stderr(&tracked));
    let checked_out = ds(
        &fixture.checkout,
        &fixture.home,
        &fixture.config,
        &["new", "main"],
    );
    assert!(checked_out.status.success(), "{}", stderr(&checked_out));

    fixture.commit("main", "descendant\n");
    let main = fixture.push(&["-b", "main"]);
    assert!(main.status.success(), "{}", stderr(&main));
    let main_oid = remote_ref(&fixture.remote, "main").unwrap();
    assert_eq!(
        parse_git_oid(git_output(&["rev-parse", "main^"], Some(&fixture.remote)).trim()),
        imported_oid
    );

    set_bookmark(
        &fixture.checkout,
        &fixture.home,
        &fixture.config,
        "release",
        "main",
    );
    let release = fixture.push(&["-b", "release"]);
    assert!(release.status.success(), "{}", stderr(&release));

    assert_eq!(remote_ref(&fixture.remote, "release"), Some(main_oid));
}

struct FakePushFixture {
    _temp: tempfile::TempDir,
    _server: JoinHandle<Vec<String>>,
    home: PathBuf,
    config: PathBuf,
    checkout: PathBuf,
    remote: PathBuf,
    repository_name: String,
}

impl FakePushFixture {
    async fn new(label: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("origin.git");
        git(&["init", "--bare", remote.to_str().unwrap()], None);
        let (base_url, server) = create_push_server(remote.to_str().unwrap().to_owned());
        let home = temp.path().join("machine");
        fs::create_dir_all(&home).unwrap();
        configure_machine(&home, &base_url, FIRST_MACHINE_ID, DEVELOPMENT_SECRET);
        let config = write_cli_config(&home);
        let repository_name = format!("git-push-{label}");
        let checkout = local_checkout(&home, &config, &repository_name).await;
        let added = ds(
            &checkout,
            &home,
            &config,
            &["git", "remote", "add", "origin", remote.to_str().unwrap()],
        );
        assert!(added.status.success(), "{}", stderr(&added));
        Self {
            _temp: temp,
            _server: server,
            home,
            config,
            checkout,
            remote,
            repository_name,
        }
    }

    fn commit(&self, bookmark: &str, contents: &str) {
        fs::write(self.checkout.join("visible.txt"), contents).unwrap();
        seal_commit(
            &self.checkout,
            &self.home,
            &self.config,
            &format!("{bookmark} commit"),
        );
        set_bookmark(&self.checkout, &self.home, &self.config, bookmark, "@-");
    }

    fn delete_bookmark(&self, bookmark: &str) {
        let deleted = ds(
            &self.checkout,
            &self.home,
            &self.config,
            &["bookmark", "delete", bookmark],
        );
        assert!(deleted.status.success(), "{}", stderr(&deleted));
    }

    fn push(&self, args: &[&str]) -> Output {
        let mut command = ds_command(&self.checkout, &self.home, &self.config);
        command.arg("git").arg("push").args(args).output().unwrap()
    }

    fn ds(&self, args: &[&str]) -> Output {
        ds(&self.checkout, &self.home, &self.config, args)
    }

    fn bookmark_list(&self) -> String {
        let output = self.ds(&[
            "bookmark",
            "list",
            "--all-remotes",
            "--ignore-working-copy",
            "-T",
            r#"name ++ if(remote, "@" ++ remote) ++ "|" ++ present ++ "|" ++ tracked ++ "|" ++ if(present, normal_target.commit_id().short(12)) ++ "\n""#,
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
    }

    fn operation_description(&self) -> String {
        let output = self.ds(&[
            "operation",
            "log",
            "--ignore-working-copy",
            "--no-graph",
            "--limit",
            "1",
            "-T",
            "description",
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
    }

    fn add_remote(&self, name: &str) {
        let added = self.ds(&["git", "remote", "add", name, self.remote.to_str().unwrap()]);
        assert!(added.status.success(), "{}", stderr(&added));
    }

    fn fetch(&self, args: &[&str]) -> Output {
        let mut command = ds_command(&self.checkout, &self.home, &self.config);
        command.arg("git").arg("fetch").args(args).output().unwrap()
    }

    fn entry(&self) -> devspace_machine::CatalogEntry {
        machine_store(&self.home)
            .resolve(&RepositoryName::parse(&self.repository_name).unwrap())
            .unwrap()
            .unwrap()
    }

    async fn repository(&self) -> MachineRepository {
        MachineRepository::open(&self.entry().native_repository_path, &settings())
            .await
            .unwrap()
    }

    async fn remote_bookmark(&self, name: &str) -> RemoteRef {
        self.repository()
            .await
            .repo()
            .view()
            .get_remote_bookmark(RemoteRefSymbol {
                name: RefName::new(name),
                remote: RemoteName::new("origin"),
            })
            .clone()
    }

    async fn remove_remote_bookmark(&self, name: &str) {
        let repository = self.repository().await;
        let mut transaction = repository.repo().start_transaction();
        transaction.repo_mut().set_remote_bookmark(
            RemoteRefSymbol {
                name: RefName::new(name),
                remote: RemoteName::new("origin"),
            },
            RemoteRef::absent(),
        );
        transaction
            .commit("remove fixture remote bookmark")
            .await
            .unwrap();
    }
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
            .repository_legacy_projection_path(&machine_b_entry.identity)
            .exists(),
        "fresh machine unexpectedly has a legacy projection directory"
    );

    let recovered = fixture.push(&checkout_b, &fixture.home_b, &config_b, "main");
    assert!(recovered.status.success(), "{}", stderr(&recovered));
    assert_eq!(remote_ref(&fixture.remote, "main"), Some(first_remote));
    let accepted = fixture.snapshot(&fixture.home_b).await;
    assert!(accepted.pending.is_empty());
    assert!(accepted.cursors.iter().any(|cursor| {
        cursor.remote == "origin"
            && cursor.bookmark == "main"
            && cursor.public_oid.0 == first_remote
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
        cursor.remote == "origin"
            && cursor.bookmark == "main"
            && cursor.public_oid.0 == advanced_remote
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

    async fn snapshot(&self, home: &Path) -> ProjectionGitSnapshot {
        let store = machine_store(home);
        let entry = store
            .resolve(&RepositoryName::parse(&self.repository_name).unwrap())
            .unwrap()
            .unwrap();
        let config = store.load_config().unwrap();
        let transport = GitHttpTransport::new(
            config.base_url(),
            config.shared_secret().as_str(),
            config.machine_id().as_str(),
            entry.identity.repository_id.as_str(),
            entry.identity.incarnation.as_str(),
        )
        .unwrap();
        load_snapshot(&transport).await
    }
}

fn create_push_server(git_url: String) -> (String, JoinHandle<Vec<String>>) {
    let mut head = None::<String>;
    let mut head_cursor = 0_u64;
    let mut op_objects = BTreeMap::<String, String>::new();
    let mut activation_cursor = 0_u64;
    let mut cursors = Vec::<serde_json::Value>::new();
    let mut mappings = Vec::<serde_json::Value>::new();
    let mut pending = None::<serde_json::Value>;
    let mut pending_fence = 1_u64;
    let mut remotes = BTreeMap::<String, String>::new();
    create_server(move |_, request, stream| {
        let request_line = request.lines().next().unwrap();
        if request_line.starts_with("PUT ") && request_line.contains("/remotes/") {
            assert_eq!(request_json(request)["url"], git_url);
            let name = remote_name_from_request(request_line);
            remotes.insert(name.clone(), git_url.clone());
            respond(
                stream,
                "200 OK",
                &serde_json::json!({"remote": {"name": name, "url": git_url}}).to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/packs?") {
            respond(
                stream,
                "200 OK",
                r#"{"packs":[],"nextAfter":0,"through":0,"hasMore":false}"#,
            );
        } else if request_line.starts_with("PUT ") && request_line.contains("/packs/") {
            respond(stream, "200 OK", r#"{"inserted":true,"installed":false}"#);
        } else if request_line.starts_with("POST ")
            && request_line.contains("/packs/")
            && request_line.contains("/install ")
        {
            respond(
                stream,
                "200 OK",
                r#"{"installed":true,"insertedObjects":1}"#,
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/git/ops/heads ") {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "cursor": head_cursor,
                    "heads": head.iter().collect::<Vec<_>>(),
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ") && request_line.contains("/git/ops/inventory ")
        {
            let requested = request_json(request)["keys"].as_array().unwrap().clone();
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "keys": requested.into_iter().filter(|key| {
                        op_objects.contains_key(key.as_str().unwrap())
                    }).collect::<Vec<_>>()
                })
                .to_string(),
            );
        } else if request_line.starts_with("PUT ")
            && (request_line.contains("/git/ops/views/")
                || request_line.contains("/git/ops/operations/"))
        {
            let path = request_line.split_whitespace().nth(1).unwrap();
            let (kind, id) = if let Some(id) = path.split("/git/ops/views/").nth(1) {
                ("v", id)
            } else {
                ("o", path.split("/git/ops/operations/").nth(1).unwrap())
            };
            op_objects.insert(format!("{kind}:{id}"), request_body(request).to_owned());
            respond(stream, "200 OK", r#"{}"#);
        } else if request_line.starts_with("GET ")
            && (request_line.contains("/git/ops/views/")
                || request_line.contains("/git/ops/operations/"))
        {
            let path = request_line.split_whitespace().nth(1).unwrap();
            let (kind, id) = if let Some(id) = path.split("/git/ops/views/").nth(1) {
                ("v", id)
            } else {
                ("o", path.split("/git/ops/operations/").nth(1).unwrap())
            };
            respond(
                stream,
                "200 OK",
                op_objects
                    .get(&format!("{kind}:{id}"))
                    .expect("requested operation object exists"),
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/ops/heads/transactions ")
        {
            let body = request_json(request);
            head = Some(body["newHead"].as_str().unwrap().to_owned());
            head_cursor += 1;
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "cursor": head_cursor,
                    "heads": head.iter().collect::<Vec<_>>(),
                })
                .to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/remotes?") {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "remotes": remotes
                        .iter()
                        .map(|(name, url)| serde_json::json!({"name": name, "url": url}))
                        .collect::<Vec<_>>()
                })
                .to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/git/projection?") {
            let pending_batches = pending
                .iter()
                .map(|batch| {
                    serde_json::json!({
                        "batchId": batch["batchId"],
                        "remote": batch["remote"],
                        "ownerMachine": batch["machineId"],
                        "fence": pending_fence,
                        "refs": batch["updates"].as_array().unwrap().iter().map(|update| {
                            let proposed = update["proposedState"]
                                .as_u64()
                                .map(|index| update["states"][index as usize]["publicOid"].clone())
                                .or_else(|| update["identityOid"].as_str().map(serde_json::Value::from))
                                .unwrap_or(serde_json::Value::Null);
                            serde_json::json!({
                                "bookmark": update["bookmark"],
                                "expectedOldOid": update["expectedOldOid"],
                                "proposedPublicOid": proposed,
                            })
                        }).collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>();
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "activationCursor": activation_cursor,
                    "cursors": cursors,
                    "mappings": mappings,
                    "nextAfter": activation_cursor,
                    "through": activation_cursor,
                    "hasMore": false,
                    "pending": pending_batches,
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/fetches ")
        {
            let body = request_json(request);
            let remote = body["remote"].as_str().unwrap();
            for fetch_ref in body["refs"].as_array().unwrap() {
                let bookmark = fetch_ref["bookmark"].as_str().unwrap();
                let current = cursors
                    .iter()
                    .find(|cursor| cursor["remote"] == remote && cursor["bookmark"] == bookmark)
                    .map(|cursor| cursor["publicOid"].clone())
                    .unwrap_or(serde_json::Value::Null);
                assert_eq!(fetch_ref["expectedCursorOid"], current);
                cursors
                    .retain(|cursor| cursor["remote"] != remote || cursor["bookmark"] != bookmark);
                for state in fetch_ref["states"].as_array().unwrap() {
                    activation_cursor += 1;
                    let mapping = serde_json::json!({
                        "remote": remote,
                        "bookmark": bookmark,
                        "canonicalOid": state["canonicalOid"],
                        "publicOid": state["publicOid"],
                        "hiddenSetId": state["hiddenSetId"],
                    });
                    if !mappings.iter().any(|existing| existing == &mapping) {
                        mappings.push(mapping);
                    }
                }
                let state = fetch_ref["proposedState"]
                    .as_u64()
                    .map(|index| fetch_ref["states"][index as usize].clone())
                    .or_else(|| {
                        fetch_ref["identityOid"].as_str().map(|oid| {
                            serde_json::json!({
                                "canonicalOid": oid,
                                "publicOid": oid,
                                "hiddenSetId": null,
                            })
                        })
                    })
                    .expect("fetch records a state or identity cursor");
                cursors.push(serde_json::json!({
                    "remote": remote,
                    "bookmark": bookmark,
                    "canonicalOid": state["canonicalOid"],
                    "publicOid": state["publicOid"],
                    "hiddenSetId": state["hiddenSetId"],
                    "activationSequence": activation_cursor,
                }));
            }
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "fetchId": body["fetchId"],
                    "activationCursor": activation_cursor,
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/pushes/")
            && request_line.contains("/claim ")
        {
            let previous_fence = pending_fence;
            pending_fence += 1;
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "fence": pending_fence,
                    "previousFence": previous_fence,
                })
                .to_string(),
            );
        } else if request_line.starts_with("GET ")
            && request_line.contains("/git/projection/pushes/")
            && request_line.contains("/replay?")
        {
            let batch = pending.as_ref().expect("replay follows a pending batch");
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "batchId": batch["batchId"],
                    "remote": batch["remote"],
                    "ownerMachine": batch["machineId"],
                    "fence": pending_fence,
                    "updates": batch["updates"],
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/pushes/")
            && request_line.contains("/recover ")
        {
            let body = request_json(request);
            let batch = pending.take().expect("recover follows begin");
            let remote = batch["remote"].as_str().unwrap();
            activation_cursor += 1;
            for update in batch["updates"].as_array().unwrap() {
                let bookmark = update["bookmark"].as_str().unwrap();
                let observation = body["observations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|observation| observation["bookmark"] == bookmark)
                    .unwrap();
                cursors
                    .retain(|cursor| cursor["remote"] != remote || cursor["bookmark"] != bookmark);
                if let Some(index) = update["proposedState"].as_u64() {
                    let state = &update["states"][index as usize];
                    assert_eq!(observation["liveOid"], state["publicOid"]);
                    cursors.push(serde_json::json!({
                        "remote": remote,
                        "bookmark": bookmark,
                        "canonicalOid": state["canonicalOid"],
                        "publicOid": state["publicOid"],
                        "hiddenSetId": state["hiddenSetId"],
                        "activationSequence": activation_cursor,
                    }));
                    for state in update["states"].as_array().unwrap() {
                        let mapping = serde_json::json!({
                            "remote": remote,
                            "bookmark": bookmark,
                            "canonicalOid": state["canonicalOid"],
                            "publicOid": state["publicOid"],
                            "hiddenSetId": state["hiddenSetId"],
                        });
                        if !mappings.iter().any(|existing| existing == &mapping) {
                            mappings.push(mapping);
                        }
                    }
                } else if let Some(identity_oid) = update["identityOid"].as_str() {
                    assert_eq!(observation["liveOid"], identity_oid);
                    cursors.push(serde_json::json!({
                        "remote": remote,
                        "bookmark": bookmark,
                        "canonicalOid": identity_oid,
                        "publicOid": identity_oid,
                        "hiddenSetId": null,
                        "activationSequence": activation_cursor,
                    }));
                } else {
                    assert!(observation["liveOid"].is_null());
                }
            }
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "pending": false,
                    "fence": pending_fence,
                    "outcome": "accepted",
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/pushes ")
        {
            let body = request_json(request);
            for update in body["updates"].as_array().unwrap() {
                let bookmark = &update["bookmark"];
                let current = cursors
                    .iter()
                    .find(|cursor| {
                        cursor["remote"] == body["remote"] && cursor["bookmark"] == *bookmark
                    })
                    .map(|cursor| cursor["publicOid"].clone())
                    .unwrap_or(serde_json::Value::Null);
                assert_eq!(update["expectedOldOid"], current);
            }
            pending = Some(body);
            pending_fence = 1;
            respond(
                stream,
                "200 OK",
                r#"{"pending":true,"fence":1,"outcome":null}"#,
            );
        } else {
            panic!("unexpected fake push request: {request_line}");
        }
        false
    })
}

fn remote_name_from_request(request_line: &str) -> String {
    request_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .split("/remotes/")
        .nth(1)
        .unwrap()
        .split('?')
        .next()
        .unwrap()
        .to_owned()
}

fn bookmark_target<'a>(view: &'a str, name: &str) -> &'a str {
    view.lines()
        .find_map(|line| {
            let (symbol, rest) = line.split_once('|')?;
            (symbol == name).then(|| rest.rsplit('|').next().unwrap())
        })
        .unwrap_or_else(|| panic!("bookmark `{name}` is missing from:\n{view}"))
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
            if request_line.starts_with("GET ") && request_line.contains("/git/projection?") {
                push_reached_tx.send(()).unwrap();
                release_push_rx.recv().unwrap();
                respond(
                    &mut stream,
                    "200 OK",
                    r#"{"activationCursor":0,"cursors":[],"mappings":[],"nextAfter":0,"through":0,"hasMore":false,"pending":[]}"#,
                );
                continue;
            }
            if request_line.starts_with("GET ") && request_line.contains("/remotes?") {
                respond(&mut stream, "200 OK", r#"{"remotes":[]}"#);
                return;
            }
            if request_line.starts_with("GET ") && request_line.contains("/packs?") {
                respond(
                    &mut stream,
                    "200 OK",
                    r#"{"packs":[],"nextAfter":0,"through":0,"hasMore":false}"#,
                );
            } else if request_line.starts_with("PUT ") && request_line.contains("/packs/") {
                respond(
                    &mut stream,
                    "200 OK",
                    r#"{"inserted":true,"installed":false}"#,
                );
            } else if request_line.starts_with("POST ")
                && request_line.contains("/packs/")
                && request_line.contains("/install ")
            {
                respond(
                    &mut stream,
                    "200 OK",
                    r#"{"installed":true,"insertedObjects":1}"#,
                );
            } else if request_line.starts_with("GET ") && request_line.contains("/git/ops/heads ") {
                respond(&mut stream, "200 OK", r#"{"cursor":0,"heads":[]}"#);
            } else if request_line.starts_with("POST ")
                && request_line.contains("/git/ops/inventory ")
            {
                let body: serde_json::Value = serde_json::from_str(request_body(&request)).unwrap();
                respond(
                    &mut stream,
                    "200 OK",
                    &serde_json::json!({ "keys": body["keys"] }).to_string(),
                );
            } else if request_line.starts_with("POST ")
                && request_line.contains("/git/ops/heads/transactions ")
            {
                let body: serde_json::Value = serde_json::from_str(request_body(&request)).unwrap();
                respond(
                    &mut stream,
                    "200 OK",
                    &serde_json::json!({ "cursor": 1, "heads": [body["newHead"]] }).to_string(),
                );
            } else if request_line.starts_with("PUT ")
                && (request_line.contains("/git/ops/views/")
                    || request_line.contains("/git/ops/operations/"))
            {
                respond(&mut stream, "200 OK", r#"{}"#);
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

fn request_json(request: &str) -> serde_json::Value {
    serde_json::from_str(request_body(request)).unwrap()
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

fn signed_commit(remote: &Path, bookmark: &str) -> [u8; 20] {
    let tree = git_output(&["mktree"], Some(remote));
    let raw = format!(
        "tree {}\nauthor Imported <imported@example.invalid> 0 +0000\ncommitter Imported <imported@example.invalid> 0 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n dummy\n -----END PGP SIGNATURE-----\n\nimported history\n",
        tree.trim()
    );
    let mut hash = git_command(
        &["hash-object", "-t", "commit", "-w", "--stdin"],
        Some(remote),
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
    hash.stdin
        .as_mut()
        .unwrap()
        .write_all(raw.as_bytes())
        .unwrap();
    let output = hash.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let oid = String::from_utf8(output.stdout).unwrap();
    git(
        &["update-ref", &format!("refs/heads/{bookmark}"), oid.trim()],
        Some(remote),
    );
    parse_git_oid(oid.trim())
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

async fn load_snapshot(transport: &GitHttpTransport) -> ProjectionGitSnapshot {
    transport.projection_snapshot_all().await.unwrap()
}

fn parse_git_oid(value: &str) -> [u8; 20] {
    std::array::from_fn(|index| u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).unwrap())
}
