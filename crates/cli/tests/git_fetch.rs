use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use devspace_machine::MachineGitRepository as MachineRepository;
use devspace_machine::{GitHttpTransport, ProjectionGitSnapshot, RepositoryName};

mod support;

use jj_lib::ref_name::RefName;
use jj_lib::repo::Repo as _;
use support::{
    configure_machine_as, ds_command_with_home as ds_command, ds_with_home as ds, machine_store,
    settings, stderr, stdout, write_cli_config,
};

const MACHINE_ID: &str = "56565656565656565656565656565656";
const PRIVATE_SENTINEL: &[u8] = b"FETCH_PRIVATE_SENTINEL\0\xff";

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn plain_git_fetch_lifts_hidden_history_and_round_trips_through_push() {
    let fixture = LiveFixture::new("roundtrip").await;
    let collaborator = fixture.prepare_tracked_main();
    fs::write(collaborator.join("visible.txt"), b"from plain git\n").unwrap();
    collaborator_commit(&collaborator, "plain git change");

    let fetched = fixture.fetch(&["-b", "main"]);
    assert!(fetched.status.success(), "{}", stderr(&fetched));
    assert!(stderr(&fetched).contains("fetched main from origin"));
    assert_eq!(fixture.file_at("main", ".dsprivate"), b"/secret.bin\n");
    assert_eq!(fixture.file_at("main", "secret.bin"), PRIVATE_SENTINEL);
    let log = fixture.ds(&["log", "-r", "main", "-T", "description.first_line()"]);
    assert!(log.status.success(), "{}", stderr(&log));
    assert!(stdout(&log).contains("plain git change"));
    assert_eq!(
        fixture.local_bookmark("main").await.as_normal(),
        Some(&fixture.remote_cursor("main").await)
    );

    let pushed = fixture.ds(&["git", "push", "-b", "main"]);
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    assert!(stderr(&pushed).contains("up to date"));
    assert_no_private_objects(&fixture.remote, PRIVATE_SENTINEL);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn hidden_path_pollution_warns_materializes_a_tombstone_and_pushes_a_deletion() {
    let fixture = LiveFixture::new("pollution").await;
    let collaborator = fixture.prepare_tracked_main();
    // A hidden path with a private value yields the natural add/add conflict;
    // a hidden path with NO private value yields the tombstone conflict.
    fs::write(
        fixture.checkout.join(".dsprivate"),
        b"/secret.bin\n/fresh.bin\n",
    )
    .unwrap();
    fixture.seal("hide fresh.bin");
    fixture.set_bookmark("main", "@-");
    let pushed = fixture.ds(&["git", "push", "-b", "main"]);
    assert!(pushed.status.success(), "{}", stderr(&pushed));

    git_worktree(&collaborator, &["pull", "--rebase", "origin", "main"]);
    fs::write(collaborator.join("secret.bin"), b"PUBLIC_POLLUTION\n").unwrap();
    fs::write(collaborator.join("fresh.bin"), b"PUBLIC_FRESH\n").unwrap();
    collaborator_commit(&collaborator, "publish hidden paths");

    let fetched = fixture.fetch(&["-b", "main"]);
    assert!(fetched.status.success(), "{}", stderr(&fetched));
    assert!(
        stderr(&fetched).contains("WARNING:"),
        "{}",
        stderr(&fetched)
    );
    assert!(
        stderr(&fetched).contains("secret.bin") && stderr(&fetched).contains("fresh.bin"),
        "{}",
        stderr(&fetched)
    );
    let checked_out = fixture.ds(&["new", "main"]);
    assert!(checked_out.status.success(), "{}", stderr(&checked_out));
    let natural = fs::read(fixture.checkout.join("secret.bin")).unwrap();
    assert!(
        contains_bytes(&natural, PRIVATE_SENTINEL) && contains_bytes(&natural, b"PUBLIC_POLLUTION"),
        "{}",
        String::from_utf8_lossy(&natural)
    );
    let tombstoned = fs::read(fixture.checkout.join("fresh.bin")).unwrap();
    assert!(
        contains_bytes(
            &tombstoned,
            b"This conflict placeholder was inserted by Devspace."
        ) && contains_bytes(&tombstoned, b"PUBLIC_FRESH"),
        "{}",
        String::from_utf8_lossy(&tombstoned)
    );

    fs::write(fixture.checkout.join("secret.bin"), PRIVATE_SENTINEL).unwrap();
    fs::remove_file(fixture.checkout.join("fresh.bin")).unwrap();
    fixture.seal("private pollution resolution");
    fixture.set_bookmark("main", "@-");
    let pushed = fixture.ds(&["git", "push", "-b", "main"]);
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    for path in ["main:secret.bin", "main:fresh.bin"] {
        let show = git_command(&fixture.remote, &["show", path])
            .output()
            .unwrap();
        assert!(
            !show.status.success(),
            "hidden path {path} remains at remote tip"
        );
    }
    assert_no_private_objects(&fixture.remote, PRIVATE_SENTINEL);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn concurrent_local_and_remote_moves_create_a_jj_bookmark_conflict() {
    let fixture = LiveFixture::new("divergence").await;
    let collaborator = fixture.prepare_tracked_main();
    fs::write(fixture.checkout.join("local.txt"), b"local\n").unwrap();
    fixture.seal("local move");
    fixture.set_bookmark("main", "@-");
    fs::write(collaborator.join("remote.txt"), b"remote\n").unwrap();
    collaborator_commit(&collaborator, "remote move");

    let fetched = fixture.fetch(&["-b", "main"]);
    assert!(fetched.status.success(), "{}", stderr(&fetched));
    let target = fixture.local_bookmark("main").await;
    assert!(target.has_conflict());
    let repository = fixture.repository().await;
    let descriptions = target
        .added_ids()
        .map(|id| {
            repository
                .repo()
                .store()
                .get_commit(id)
                .unwrap()
                .description()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert!(
        descriptions
            .iter()
            .any(|value| value.contains("local move"))
    );
    assert!(
        descriptions
            .iter()
            .any(|value| value.contains("remote move"))
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn force_pushed_remote_history_fails_closed_without_journal_mutation() {
    let fixture = LiveFixture::new("rewrite").await;
    let collaborator = fixture.prepare_tracked_main();
    let original = git_output(&fixture.remote, &["rev-parse", "main"]);
    fs::write(collaborator.join("visible.txt"), b"accepted remote\n").unwrap();
    collaborator_commit(&collaborator, "accepted remote move");
    let accepted = fixture.fetch(&["-b", "main"]);
    assert!(accepted.status.success(), "{}", stderr(&accepted));
    let before = fixture.snapshot().await;

    git(
        &fixture.remote,
        &["update-ref", "refs/heads/main", original.trim()],
    );
    let rejected = fixture.fetch(&["-b", "main"]);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(
        stderr(&rejected)
            .contains("remote ref origin/main does not descend from its projection cursor"),
        "{}",
        stderr(&rejected)
    );
    assert_eq!(fixture.snapshot().await, before);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn up_to_date_retry_is_a_no_op_and_post_record_failure_repairs_the_view() {
    let fixture = LiveFixture::new("retry").await;
    let collaborator = fixture.prepare_tracked_main();
    fs::write(collaborator.join("visible.txt"), b"first fetch\n").unwrap();
    collaborator_commit(&collaborator, "first fetch");
    let fetched = fixture.fetch(&["-b", "main"]);
    assert!(fetched.status.success(), "{}", stderr(&fetched));
    let before_snapshot = fixture.snapshot().await;
    let before_operation = fixture.repository().await.repo().op_id().clone();
    let repeated = fixture.fetch(&["-b", "main"]);
    assert!(repeated.status.success(), "{}", stderr(&repeated));
    assert!(stderr(&repeated).contains("up to date"));
    assert_eq!(fixture.snapshot().await, before_snapshot);
    assert_eq!(fixture.repository().await.repo().op_id(), &before_operation);

    fs::write(collaborator.join("visible.txt"), b"repair fetch\n").unwrap();
    collaborator_commit(&collaborator, "repair fetch");
    let crashed = fixture.fetch_with_failpoint("after_fetch_record_before_view");
    assert_eq!(crashed.status.code(), Some(86), "{}", stderr(&crashed));
    let cursor = fixture.remote_cursor("main").await;
    assert_ne!(
        fixture.remote_bookmark("main").await.target.as_normal(),
        Some(&cursor)
    );

    let repaired = fixture.fetch(&["-b", "main"]);
    assert!(repaired.status.success(), "{}", stderr(&repaired));
    assert_eq!(
        fixture.remote_bookmark("main").await.target.as_normal(),
        Some(&cursor)
    );
    assert_eq!(
        fixture.local_bookmark("main").await.as_normal(),
        Some(&cursor)
    );

    fs::write(collaborator.join("visible.txt"), b"lost response\n").unwrap();
    collaborator_commit(&collaborator, "lost response fetch");
    let replayed = fixture.fetch_with_failpoint("lost_fetch_record_response_once");
    assert!(replayed.status.success(), "{}", stderr(&replayed));
    let replayed_snapshot = fixture.snapshot().await;
    let repeated = fixture.fetch(&["-b", "main"]);
    assert!(repeated.status.success(), "{}", stderr(&repeated));
    assert_eq!(fixture.snapshot().await, replayed_snapshot);
}

struct LiveFixture {
    _temp: tempfile::TempDir,
    repository_name: String,
    home: PathBuf,
    config: PathBuf,
    checkout: PathBuf,
    remote: PathBuf,
}

impl LiveFixture {
    async fn new(label: &str) -> Self {
        let base_url = std::env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
        let shared_secret =
            std::env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("machine");
        fs::create_dir_all(&home).unwrap();
        configure_machine_as(&home, &base_url, MACHINE_ID, &shared_secret);
        let config = write_cli_config(&home);
        let repository_name = unique_repository_name(temp.path(), label);
        let created = ds(&home, &home, &config, &["repo", "new", &repository_name]);
        assert!(created.status.success(), "{}", stderr(&created));
        let checkout = home.join("checkout");
        let added = ds(
            &home,
            &home,
            &config,
            &[
                "add",
                &repository_name,
                "-r",
                "root()",
                checkout.to_str().unwrap(),
            ],
        );
        assert!(added.status.success(), "{}", stderr(&added));
        let remote = temp.path().join("origin.git");
        let init = Command::new("git")
            .args(["init", "--bare"])
            .arg(&remote)
            .output()
            .unwrap();
        assert!(init.status.success(), "{}", stderr(&init));
        Self {
            _temp: temp,
            repository_name,
            home,
            config,
            checkout,
            remote,
        }
    }

    fn prepare_tracked_main(&self) -> PathBuf {
        fs::write(self.checkout.join(".dsprivate"), b"/secret.bin\n").unwrap();
        fs::write(self.checkout.join("secret.bin"), PRIVATE_SENTINEL).unwrap();
        fs::write(self.checkout.join("visible.txt"), b"initial public\n").unwrap();
        self.seal("initial main");
        self.set_bookmark("main", "@-");
        let added = self.ds(&[
            "git",
            "remote",
            "add",
            "origin",
            self.remote.to_str().unwrap(),
        ]);
        assert!(added.status.success(), "{}", stderr(&added));
        let pushed = self.ds(&["git", "push", "-b", "main"]);
        assert!(pushed.status.success(), "{}", stderr(&pushed));
        let observed = self.fetch(&["-b", "main"]);
        assert!(observed.status.success(), "{}", stderr(&observed));
        let tracked = self.ds(&["bookmark", "track", "main@origin"]);
        assert!(tracked.status.success(), "{}", stderr(&tracked));

        let collaborator = self.remote.parent().unwrap().join("collaborator");
        let cloned = Command::new("git")
            .args(["clone"])
            .arg(&self.remote)
            .arg(&collaborator)
            .output()
            .unwrap();
        assert!(cloned.status.success(), "{}", stderr(&cloned));
        git_worktree(&collaborator, &["config", "user.name", "Plain Git"]);
        git_worktree(
            &collaborator,
            &["config", "user.email", "plain@example.invalid"],
        );
        git_worktree(&collaborator, &["checkout", "-B", "main", "origin/main"]);
        collaborator
    }

    fn ds(&self, args: &[&str]) -> Output {
        ds(&self.checkout, &self.home, &self.config, args)
    }

    fn fetch(&self, args: &[&str]) -> Output {
        let mut command = ds_command(&self.checkout, &self.home, &self.config);
        command.arg("git").arg("fetch").args(args).output().unwrap()
    }

    fn fetch_with_failpoint(&self, failpoint: &str) -> Output {
        let mut command = ds_command(&self.checkout, &self.home, &self.config);
        command
            .env("DEVSPACE_FAILPOINT", failpoint)
            .args(["git", "fetch", "-b", "main"])
            .output()
            .unwrap()
    }

    fn seal(&self, description: &str) {
        let described = self.ds(&["describe", "-m", description]);
        assert!(described.status.success(), "{}", stderr(&described));
        let sealed = self.ds(&["new"]);
        assert!(sealed.status.success(), "{}", stderr(&sealed));
    }

    fn set_bookmark(&self, name: &str, revision: &str) {
        let output = self.ds(&["bookmark", "set", name, "-r", revision]);
        assert!(output.status.success(), "{}", stderr(&output));
    }

    fn file_at(&self, revision: &str, path: &str) -> Vec<u8> {
        let output = self.ds(&["file", "show", "-r", revision, path]);
        assert!(output.status.success(), "{}", stderr(&output));
        output.stdout
    }

    async fn repository(&self) -> MachineRepository {
        let entry = self.entry();
        MachineRepository::open(&entry.native_repository_path, &settings())
            .await
            .unwrap()
    }

    fn entry(&self) -> devspace_machine::CatalogEntry {
        machine_store(&self.home)
            .resolve(&RepositoryName::parse(&self.repository_name).unwrap())
            .unwrap()
            .unwrap()
    }

    async fn local_bookmark(&self, name: &str) -> jj_lib::op_store::RefTarget {
        self.repository()
            .await
            .repo()
            .view()
            .get_local_bookmark(RefName::new(name))
            .clone()
    }

    async fn remote_bookmark(&self, name: &str) -> jj_lib::op_store::RemoteRef {
        self.repository()
            .await
            .repo()
            .view()
            .get_remote_bookmark(jj_lib::ref_name::RemoteRefSymbol {
                name: RefName::new(name),
                remote: jj_lib::ref_name::RemoteName::new("origin"),
            })
            .clone()
    }

    async fn remote_cursor(&self, name: &str) -> jj_lib::backend::CommitId {
        let snapshot = self.snapshot().await;
        let cursor = snapshot
            .cursors
            .iter()
            .find(|cursor| cursor.remote == "origin" && cursor.bookmark == name)
            .unwrap();
        jj_lib::backend::CommitId::new(cursor.canonical_oid.0.to_vec())
    }

    async fn snapshot(&self) -> ProjectionGitSnapshot {
        let store = machine_store(&self.home);
        let entry = self.entry();
        let config = store.load_config().unwrap();
        let transport = GitHttpTransport::new(
            config.base_url(),
            config.shared_secret().as_str(),
            config.machine_id().as_str(),
            entry.identity.repository_id.as_str(),
            entry.identity.incarnation.as_str(),
        )
        .unwrap();
        transport.projection_snapshot_all().await.unwrap()
    }
}

fn collaborator_commit(worktree: &Path, description: &str) {
    git_worktree(worktree, &["add", "."]);
    git_worktree(worktree, &["commit", "-m", description]);
    git_worktree(worktree, &["push", "origin", "main"]);
}

fn git(remote: &Path, args: &[&str]) {
    let output = git_command(remote, args).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

fn git_output(remote: &Path, args: &[&str]) -> String {
    let output = git_command(remote, args).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    String::from_utf8(output.stdout).unwrap()
}

fn git_command(remote: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.arg("--git-dir").arg(remote).args(args);
    command
}

fn git_worktree(worktree: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

fn assert_no_private_objects(remote: &Path, sentinel: &[u8]) {
    let objects = git_output(
        remote,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
    );
    for line in objects.lines() {
        let (id, _) = line.split_once(' ').unwrap();
        let output = git_command(remote, &["cat-file", "-p", id])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(!contains_bytes(&output.stdout, sentinel));
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
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
    format!("git-fetch-{label}-{}-{suffix}", std::process::id())
}
