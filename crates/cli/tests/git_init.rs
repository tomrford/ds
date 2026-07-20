use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use devspace_machine::{
    MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineRepository, MachineStore,
    RepositoryName, SharedSecret,
};
use jj_lib::ref_name::{RefName, RemoteName, RemoteRefSymbol};

const PRIVATE_SENTINEL: &[u8] = b"INIT_PRIVATE_SENTINEL\0\xff";

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn init_materializes_history_tracks_head_round_trips_and_converges_a_second_machine() {
    let fixture = LiveFixture::new("history");
    let remote = fixture.history_remote();
    let checkout = fixture.root.join("checkout");
    let initialized = fixture.init(&remote, &checkout, None);
    assert!(initialized.status.success(), "{}", stderr(&initialized));
    let output = stderr(&initialized);
    assert!(output.contains(&format!("Repository: {}", fixture.name)));
    assert!(output.contains("Remote: origin ->"));
    assert!(output.contains("Fetched bookmarks: feature, main"));
    let canonical_checkout = checkout.canonicalize().unwrap();
    assert!(
        output.contains(&format!("Checkout: {}", canonical_checkout.display())),
        "{output}"
    );

    assert_eq!(fs::read(checkout.join("visible.txt")).unwrap(), b"main\n");
    assert_eq!(
        fs::read(checkout.join("feature.txt")).unwrap(),
        b"feature\n"
    );
    assert!(!checkout.join("ignored.txt").exists());
    assert!(checkout.join(".git").is_dir());
    assert_eq!(
        git_ls_files(&checkout),
        [".gitignore", "feature.txt", "visible.txt"]
    );
    let log = fixture.ds(
        &checkout,
        &["log", "-r", "main::", "-T", "description.first_line()"],
    );
    assert!(log.status.success(), "{}", stderr(&log));
    assert!(stdout(&log).contains("merge feature"));
    let status = fixture.ds(&checkout, &["status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(
        stdout(&status).contains("The working copy has no changes."),
        "{}{}",
        stdout(&status),
        stderr(&status)
    );
    assert!(
        stderr(&status).contains("sync: in sync with cloud as of the last successful sync"),
        "{}",
        stderr(&status)
    );

    let repository = fixture.repository(&fixture.home).await;
    let local = repository
        .repo()
        .view()
        .get_local_bookmark(RefName::new("main"));
    let remote_ref = repository
        .repo()
        .view()
        .get_remote_bookmark(RemoteRefSymbol {
            name: RefName::new("main"),
            remote: RemoteName::new("origin"),
        });
    assert!(remote_ref.is_tracked());
    assert_eq!(local.as_normal(), remote_ref.target.as_normal());

    let collaborator = fixture.root.join("collaborator");
    run_git(["clone", path(&remote), path(&collaborator)]);
    fs::write(checkout.join(".dsprivate"), b"/secret.bin\n").unwrap();
    fs::write(checkout.join("secret.bin"), PRIVATE_SENTINEL).unwrap();
    let described = fixture.ds(&checkout, &["describe", "-m", "add private sentinel"]);
    assert!(described.status.success(), "{}", stderr(&described));
    let sealed = fixture.ds(&checkout, &["new"]);
    assert!(sealed.status.success(), "{}", stderr(&sealed));
    let bookmark = fixture.ds(&checkout, &["bookmark", "set", "main", "-r", "@-"]);
    assert!(bookmark.status.success(), "{}", stderr(&bookmark));
    let pushed = fixture.ds(&checkout, &["git", "push", "-b", "main"]);
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    assert_no_private_objects(&remote, PRIVATE_SENTINEL);
    git_worktree(&collaborator, &["pull", "--ff-only"]);

    let second_home = fixture.root.join("second-machine");
    fs::create_dir(&second_home).unwrap();
    configure_machine(&second_home, "78".repeat(16));
    let second_config = write_cli_config(&second_home);
    let second_checkout = fixture.root.join("second-checkout");
    let added = ds(
        &fixture.root,
        &second_home,
        &second_config,
        &[
            "add",
            &fixture.name,
            "-r",
            "main",
            second_checkout.to_str().unwrap(),
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    let second_main = ds(
        &second_checkout,
        &second_home,
        &second_config,
        &["log", "-r", "main", "-T", "commit_id"],
    );
    let first_main = fixture.ds(&checkout, &["log", "-r", "main", "-T", "commit_id"]);
    assert_eq!(stdout(&second_main), stdout(&first_main));

    let collision_path = fixture.root.join("collision");
    let collision = fixture.init(&remote, &collision_path, None);
    assert_eq!(collision.status.code(), Some(1));
    assert!(stderr(&collision).contains("already registered in this machine's local catalog"));
    assert!(!collision_path.exists());
}

#[test]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
fn init_rejects_empty_remotes_and_nonempty_destinations_before_creation() {
    let fixture = LiveFixture::new("preflight");
    let empty = fixture.root.join("empty.git");
    run_git(["init", "--bare", path(&empty)]);
    let empty_checkout = fixture.root.join("empty-checkout");
    let output = fixture.init(
        &empty,
        &empty_checkout,
        Some(&format!("{}-empty", fixture.name)),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("use `ds repo new`"));
    assert!(!empty_checkout.exists());

    let remote = fixture.history_remote();
    let destination = fixture.root.join("nonempty");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("keep"), b"keep").unwrap();
    let name = format!("{}-nonempty", fixture.name);
    let output = fixture.init(&remote, &destination, Some(&name));
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("is not empty; no cloud repository was created"));
    assert!(
        MachineStore::new(fixture.home.join("machine-store"))
            .resolve(&RepositoryName::parse(name).unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(fs::read(destination.join("keep")).unwrap(), b"keep");
}

#[test]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
fn init_replays_the_creation_intent_after_a_lost_cloud_response() {
    let fixture = LiveFixture::new("lost-response");
    let remote = fixture.history_remote();
    let checkout = fixture.root.join("checkout");
    let mut failed = fixture.init_command(&remote, &checkout, None);
    let failed = failed
        .env("DEVSPACE_FAILPOINT", "after_init_cloud_registration")
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(86), "{}", stderr(&failed));
    assert!(!checkout.exists());
    assert!(
        MachineStore::new(fixture.home.join("machine-store"))
            .resolve(&RepositoryName::parse(&fixture.name).unwrap())
            .unwrap()
            .is_none()
    );

    let retried = fixture.init(&remote, &checkout, None);
    assert!(retried.status.success(), "{}", stderr(&retried));
    assert!(checkout.join("visible.txt").exists());
}

#[test]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
fn init_imports_deep_history_through_receipt_pages_and_retries_after_page_crash() {
    const COMMITS: usize = 60;
    let fixture = LiveFixture::new("paged-history");
    let remote = fixture.linear_remote(COMMITS);
    let checkout = fixture.root.join("deep-checkout");
    let mut command = fixture.init_command(&remote, &checkout, None);
    let output = command
        .env("DEVSPACE_HTTP_TEST_HOOKS", "1")
        .env("DEVSPACE_TEST_RECEIPT_PAGE_SIZE", "16")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_history_count(&fixture, &checkout, COMMITS);
    assert_eq!(
        fs::read(checkout.join("visible.txt")).unwrap(),
        b"history\n"
    );
    let status = fixture.ds(&checkout, &["status"]);
    assert!(status.status.success(), "{}", stderr(&status));

    let retry_fixture = LiveFixture::new("paged-retry");
    let retry_remote = retry_fixture.linear_remote(COMMITS);
    let retry_checkout = retry_fixture.root.join("retry-checkout");
    let mut failed = retry_fixture.init_command(&retry_remote, &retry_checkout, None);
    let failed = failed
        .env("DEVSPACE_HTTP_TEST_HOOKS", "1")
        .env("DEVSPACE_TEST_RECEIPT_PAGE_SIZE", "16")
        .env("DEVSPACE_FAILPOINT", "after_receipt_page")
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(86), "{}", stderr(&failed));

    let mut retried = ds_command(&retry_checkout, &retry_fixture.home, &retry_fixture.config);
    let retried = retried
        .env("DEVSPACE_HTTP_TEST_HOOKS", "1")
        .env("DEVSPACE_TEST_RECEIPT_PAGE_SIZE", "16")
        .args(["git", "fetch"])
        .output()
        .unwrap();
    assert!(retried.status.success(), "{}", stderr(&retried));
    assert_history_count(&retry_fixture, &retry_checkout, COMMITS);
    let positioned = retry_fixture.ds(&retry_checkout, &["new", "main@origin"]);
    assert!(positioned.status.success(), "{}", stderr(&positioned));
    assert_eq!(
        fs::read(retry_checkout.join("visible.txt")).unwrap(),
        b"history\n"
    );
}

struct LiveFixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    config: PathBuf,
    name: String,
}

impl LiveFixture {
    fn new(label: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_owned();
        let home = root.join("machine");
        fs::create_dir(&home).unwrap();
        configure_machine(&home, "56".repeat(16));
        let config = write_cli_config(&home);
        let suffix = root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .bytes()
            .filter(|byte| byte.is_ascii_alphanumeric())
            .map(|byte| byte.to_ascii_lowercase() as char)
            .collect::<String>();
        let name = format!("git-init-{label}-{}-{suffix}", std::process::id());
        Self {
            _temp: temp,
            root,
            home,
            config,
            name,
        }
    }

    fn history_remote(&self) -> PathBuf {
        let source = self.root.join(format!("{}-source", self.name));
        run_git(["init", "-b", "main", path(&source)]);
        configure_git_identity(&source);
        fs::write(source.join(".gitignore"), b"ignored.txt\n").unwrap();
        fs::write(source.join("ignored.txt"), b"never tracked\n").unwrap();
        fs::write(source.join("visible.txt"), b"base\n").unwrap();
        git_worktree(&source, &["add", ".gitignore", "visible.txt"]);
        git_worktree(&source, &["commit", "-m", "base"]);
        git_worktree(&source, &["switch", "-c", "feature"]);
        fs::write(source.join("feature.txt"), b"feature\n").unwrap();
        git_worktree(&source, &["add", "feature.txt"]);
        git_worktree(&source, &["commit", "-m", "feature"]);
        git_worktree(&source, &["switch", "main"]);
        fs::write(source.join("visible.txt"), b"main\n").unwrap();
        git_worktree(&source, &["commit", "-am", "main change"]);
        git_worktree(
            &source,
            &["merge", "--no-ff", "feature", "-m", "merge feature"],
        );
        let remote = self.root.join(format!("{}.git", self.name));
        run_git(["clone", "--bare", path(&source), path(&remote)]);
        remote
    }

    fn linear_remote(&self, commits: usize) -> PathBuf {
        assert!(commits > 0);
        let source = self.root.join(format!("{}-source", self.name));
        run_git(["init", "-b", "main", path(&source)]);
        configure_git_identity(&source);
        fs::write(source.join("visible.txt"), b"history\n").unwrap();
        git_worktree(&source, &["add", "visible.txt"]);
        git_worktree(&source, &["commit", "-m", "commit 0"]);
        for index in 1..commits {
            git_worktree(
                &source,
                &["commit", "--allow-empty", "-m", &format!("commit {index}")],
            );
        }
        let remote = self.root.join(format!("{}.git", self.name));
        run_git(["clone", "--bare", path(&source), path(&remote)]);
        remote
    }

    fn init(&self, remote: &Path, checkout: &Path, name: Option<&str>) -> Output {
        self.init_command(remote, checkout, name).output().unwrap()
    }

    fn init_command(&self, remote: &Path, checkout: &Path, name: Option<&str>) -> Command {
        let mut command = ds_command(&self.root, &self.home, &self.config);
        command.arg("init").arg(remote).arg(checkout);
        if let Some(name) = name {
            command.args(["--name", name]);
        } else {
            command.args(["--name", &self.name]);
        }
        command
    }

    fn ds(&self, cwd: &Path, args: &[&str]) -> Output {
        ds(cwd, &self.home, &self.config, args)
    }

    async fn repository(&self, home: &Path) -> MachineRepository {
        let store = MachineStore::new(home.join("machine-store"));
        let entry = store
            .resolve(&RepositoryName::parse(&self.name).unwrap())
            .unwrap()
            .unwrap();
        MachineRepository::open(&entry.native_repository_path, &settings())
            .await
            .unwrap()
    }
}

fn assert_history_count(fixture: &LiveFixture, checkout: &Path, expected: usize) {
    let log = fixture.ds(
        checkout,
        &[
            "log",
            "--no-graph",
            "-r",
            "root()..main@origin",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert!(log.status.success(), "{}", stderr(&log));
    assert_eq!(stdout(&log).lines().count(), expected, "{}", stdout(&log));
}

fn git_ls_files(checkout: &Path) -> Vec<String> {
    let output = Command::new("git")
        .current_dir(checkout)
        .args(["ls-files"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn configure_machine(root: &Path, machine_id: String) {
    let base_url = std::env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
    let shared_secret =
        std::env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
    MachineStore::new(root.join("machine-store"))
        .write_config(
            &MachineConfig::new(
                base_url,
                MachineId::parse(machine_id).unwrap(),
                SharedSecret::new(shared_secret).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
}

fn write_cli_config(root: &Path) -> PathBuf {
    let path = root.join("jj-config.toml");
    fs::write(
        &path,
        r#"
            [user]
            name = "Devspace Test"
            email = "devspace@example.invalid"

            [ui]
            color = "never"

            [snapshot]
            auto-update-stale = true
        "#,
    )
    .unwrap();
    path
}

fn settings() -> jj_lib::settings::UserSettings {
    let mut config = jj_lib::config::StackedConfig::with_defaults();
    config.add_layer(
        jj_lib::config::ConfigLayer::parse(
            jj_lib::config::ConfigSource::User,
            r#"
                [user]
                name = "Devspace Test"
                email = "devspace@example.invalid"
            "#,
        )
        .unwrap(),
    );
    jj_lib::settings::UserSettings::from_config(config).unwrap()
}

fn ds(cwd: &Path, home: &Path, config: &Path, args: &[&str]) -> Output {
    ds_command(cwd, home, config).args(args).output().unwrap()
}

fn ds_command(cwd: &Path, home: &Path, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ds"));
    command
        .current_dir(cwd)
        .env(MACHINE_STORE_OVERRIDE, home.join("machine-store"))
        .env("JJ_CONFIG", config)
        .env("DEVSPACE_BOUNDARY_SYNC", "0")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat");
    command
}

fn configure_git_identity(worktree: &Path) {
    git_worktree(worktree, &["config", "user.name", "Plain Git"]);
    git_worktree(worktree, &["config", "user.email", "plain@example.invalid"]);
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

fn run_git<const N: usize>(args: [&str; N]) {
    let output = Command::new("git").args(args).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

fn assert_no_private_objects(remote: &Path, sentinel: &[u8]) {
    let objects = Command::new("git")
        .args([
            "--git-dir",
            remote.to_str().unwrap(),
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ])
        .output()
        .unwrap();
    assert!(objects.status.success(), "{}", stderr(&objects));
    for line in stdout(&objects).lines() {
        let (id, _) = line.split_once(' ').unwrap();
        let output = Command::new("git")
            .args(["--git-dir", remote.to_str().unwrap(), "cat-file", "-p", id])
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

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
