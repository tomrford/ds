use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use devspace_machine::{
    MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineRepository, MachineStore,
    RepositoryName, SharedSecret,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::object_id::ObjectId as _;
use jj_lib::settings::UserSettings;

const FIRST_MACHINE_ID: &str = "12121212121212121212121212121212";
const SECOND_MACHINE_ID: &str = "34343434343434343434343434343434";

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn ordinary_commands_converge_two_offline_divergent_machines() {
    let base_url = std::env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
    let shared_secret =
        std::env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
    let temp = tempfile::tempdir().unwrap();
    let home_a = temp.path().join("machine-a");
    let home_b = temp.path().join("machine-b");
    fs::create_dir_all(&home_a).unwrap();
    fs::create_dir_all(&home_b).unwrap();
    configure_machine(&home_a, &base_url, FIRST_MACHINE_ID, &shared_secret);
    configure_machine(&home_b, &base_url, SECOND_MACHINE_ID, &shared_secret);
    let config_a = write_cli_config(&home_a);
    let config_b = write_cli_config(&home_b);
    let repository_name = unique_repository_name(temp.path());

    let created = ds(
        &home_a,
        &home_a,
        &config_a,
        &["repo", "new", &repository_name],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let checkout_a = home_a.join("checkout");
    let added_a = ds(
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
    assert!(added_a.status.success(), "{}", stderr(&added_a));
    fs::write(checkout_a.join("shared.txt"), "created on machine A\n").unwrap();
    seal_commit(&checkout_a, &home_a, &config_a, "shared base");
    let commit_a = commit_id(&checkout_a, &home_a, &config_a, "@-");

    assert!(
        poll_until(Duration::from_secs(60), || {
            let output = ds(&checkout_a, &home_a, &config_a, &["status"]);
            output.status.success()
                && stderr(&output)
                    .contains("sync: in sync with cloud as of the last successful sync")
        }),
        "machine A did not upload {commit_a}: {}",
        sync_log(&machine_store(&home_a), &repository_name)
    );

    let checkout_b = home_b.join("checkout");
    let added_b = ds(
        &home_b,
        &home_b,
        &config_b,
        &[
            "add",
            &repository_name,
            "-r",
            "root()",
            checkout_b.to_str().unwrap(),
        ],
    );
    assert!(added_b.status.success(), "{}", stderr(&added_b));
    assert!(
        poll_until(Duration::from_secs(60), || {
            let output = repository_log(&checkout_b, &home_b, &config_b);
            output.status.success() && stdout(&output).contains(&commit_a)
        }),
        "machine B did not pull {commit_a}: {}",
        sync_log(&machine_store(&home_b), &repository_name)
    );

    configure_machine(
        &home_a,
        "http://127.0.0.1:1",
        FIRST_MACHINE_ID,
        &shared_secret,
    );
    configure_machine(
        &home_b,
        "http://127.0.0.1:1",
        SECOND_MACHINE_ID,
        &shared_secret,
    );
    let based_b = ds(&checkout_b, &home_b, &config_b, &["new", &commit_a]);
    assert!(based_b.status.success(), "{}", stderr(&based_b));

    fs::write(checkout_a.join("offline-a.txt"), "offline machine A\n").unwrap();
    seal_commit(&checkout_a, &home_a, &config_a, "offline machine A");
    let offline_a = commit_id(&checkout_a, &home_a, &config_a, "@-");
    fs::write(checkout_b.join("offline-b.txt"), "offline machine B\n").unwrap();
    seal_commit(&checkout_b, &home_b, &config_b, "offline machine B");
    let offline_b = commit_id(&checkout_b, &home_b, &config_b, "@-");

    let pending = ds(&checkout_a, &home_a, &config_a, &["status"]);
    assert!(pending.status.success(), "{}", stderr(&pending));
    assert!(
        stderr(&pending).contains("pending upload"),
        "{}",
        stderr(&pending)
    );

    configure_machine(&home_a, &base_url, FIRST_MACHINE_ID, &shared_secret);
    configure_machine(&home_b, &base_url, SECOND_MACHINE_ID, &shared_secret);
    let name = RepositoryName::parse(repository_name.clone()).unwrap();
    let store_a = machine_store(&home_a);
    let store_b = machine_store(&home_b);
    let entry_a = store_a.resolve(&name).unwrap().unwrap();
    let entry_b = store_b.resolve(&name).unwrap().unwrap();
    let expected_commits = [offline_a.as_str(), offline_b.as_str()];
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let log_a = repository_log(&checkout_a, &home_a, &config_a);
        assert!(log_a.status.success(), "{}", stderr(&log_a));
        let log_b = repository_log(&checkout_b, &home_b, &config_b);
        assert!(log_b.status.success(), "{}", stderr(&log_b));
        let heads_a = operation_heads(&entry_a.native_repository_path).await;
        let heads_b = operation_heads(&entry_b.native_repository_path).await;
        if expected_commits
            .iter()
            .all(|commit| stdout(&log_a).contains(*commit) && stdout(&log_b).contains(*commit))
            && heads_a == heads_b
            && heads_a.len() == 1
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ordinary commands did not converge {offline_a} and {offline_b}; A heads: {heads_a:?}; B heads: {heads_b:?}; A sync: {}; B sync: {}",
            sync_log(&store_a, &repository_name),
            sync_log(&store_b, &repository_name)
        );
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        poll_until(Duration::from_secs(60), || {
            let output = ds(&checkout_a, &home_a, &config_a, &["status"]);
            output.status.success()
                && stderr(&output)
                    .contains("sync: in sync with cloud as of the last successful sync")
        }),
        "machine A did not reach in-sync status: {}",
        sync_log(&store_a, &repository_name)
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn daemon_polling_converges_without_a_machine_b_command() {
    let base_url = std::env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
    let shared_secret =
        std::env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
    let temp = tempfile::tempdir().unwrap();
    let home_a = temp.path().join("machine-a");
    let home_b = temp.path().join("machine-b");
    fs::create_dir_all(&home_a).unwrap();
    fs::create_dir_all(&home_b).unwrap();
    configure_machine(&home_a, &base_url, FIRST_MACHINE_ID, &shared_secret);
    configure_machine(&home_b, &base_url, SECOND_MACHINE_ID, &shared_secret);
    let config_a = write_cli_config(&home_a);
    let config_b = write_cli_config(&home_b);
    let repository_name = unique_repository_name(temp.path());

    let created = ds(
        &home_a,
        &home_a,
        &config_a,
        &["repo", "new", &repository_name],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let checkout_a = home_a.join("checkout");
    let added_a = ds(
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
    assert!(added_a.status.success(), "{}", stderr(&added_a));

    let name = RepositoryName::parse(repository_name.clone()).unwrap();
    let store_a = machine_store(&home_a);
    let entry_a = store_a.resolve(&name).unwrap().unwrap();
    let store_b = machine_store(&home_b);
    let entry_b = store_b
        .register_repository(name, entry_a.identity.clone())
        .unwrap();
    MachineRepository::init(&entry_b.native_repository_path, &settings())
        .await
        .unwrap();

    let _daemon = start_daemon(&home_b, &config_b);
    let baseline_a = operation_heads(&entry_a.native_repository_path).await;
    let baseline_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let heads_b = operation_heads(&entry_b.native_repository_path).await;
        if heads_b == baseline_a {
            break;
        }
        assert!(
            Instant::now() < baseline_deadline,
            "machine B daemon did not drain startup work; A heads: {baseline_a:?}; B heads: {heads_b:?}; daemon log: {}",
            fs::read_to_string(store_b.root().join("daemon.log"))
                .unwrap_or_else(|error| format!("<unavailable: {error}>"))
        );
        thread::sleep(Duration::from_millis(25));
    }

    fs::write(checkout_a.join("from-daemon-a.txt"), "machine A\n").unwrap();
    seal_commit(&checkout_a, &home_a, &config_a, "daemon polling machine A");
    let commit_a = commit_id(&checkout_a, &home_a, &config_a, "@-");
    let operation_a = operation_heads(&entry_a.native_repository_path)
        .await
        .into_iter()
        .next()
        .expect("machine A has an operation head");
    assert!(!baseline_a.contains(&operation_a));

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let heads_b = operation_heads(&entry_b.native_repository_path).await;
        if heads_b.contains(&operation_a) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "machine B did not receive operation {operation_a} for commit {commit_a} without a B command; B heads: {heads_b:?}; daemon log: {}",
            fs::read_to_string(store_b.root().join("daemon.log"))
                .unwrap_or_else(|error| format!("<unavailable: {error}>"))
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn settings() -> UserSettings {
    let mut config = StackedConfig::with_defaults();
    config.add_layer(
        ConfigLayer::parse(
            ConfigSource::User,
            r#"
                [user]
                name = "Devspace Test"
                email = "devspace@example.invalid"
            "#,
        )
        .unwrap(),
    );
    UserSettings::from_config(config).unwrap()
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

fn machine_store(root: &Path) -> MachineStore {
    MachineStore::new(root.join("machine-store"))
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

fn ds(cwd: &Path, home: &Path, config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ds"))
        .current_dir(cwd)
        .env(MACHINE_STORE_OVERRIDE, home.join("machine-store"))
        .env("JJ_CONFIG", config)
        .env("DEVSPACE_BOUNDARY_SYNC", "1")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .args(args)
        .output()
        .unwrap()
}

#[cfg(unix)]
fn start_daemon(home: &Path, config: &Path) -> DaemonProcess {
    let socket = machine_store(home).root().join("daemon.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_ds"))
        .current_dir(home)
        .env(MACHINE_STORE_OVERRIDE, home.join("machine-store"))
        .env("JJ_CONFIG", config)
        .env("DEVSPACE_BOUNDARY_SYNC", "0")
        .env("DEVSPACE_DAEMON_TEST_HOOKS", "1")
        .env("DEVSPACE_DAEMON_TEST_POLL_MS", "100")
        .env("DEVSPACE_DAEMON_TEST_IDLE_MS", "60000")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        poll_until(Duration::from_secs(10), || socket.exists()),
        "daemon socket did not appear at {}",
        socket.display()
    );
    DaemonProcess(child)
}

#[cfg(unix)]
struct DaemonProcess(Child);

#[cfg(unix)]
impl Drop for DaemonProcess {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn seal_commit(cwd: &Path, home: &Path, config: &Path, description: &str) {
    let described = ds(cwd, home, config, &["describe", "-m", description]);
    assert!(described.status.success(), "{}", stderr(&described));
    let sealed = ds(cwd, home, config, &["new"]);
    assert!(sealed.status.success(), "{}", stderr(&sealed));
}

fn commit_id(cwd: &Path, home: &Path, config: &Path, revision: &str) -> String {
    let output = ds(
        cwd,
        home,
        config,
        &[
            "log",
            "-r",
            revision,
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output).trim().to_owned()
}

fn repository_log(cwd: &Path, home: &Path, config: &Path) -> Output {
    ds(
        cwd,
        home,
        config,
        &[
            "log",
            "-r",
            "all()",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    )
}

async fn operation_heads(repository_path: &Path) -> Vec<String> {
    let repository = MachineRepository::open(repository_path, &settings())
        .await
        .unwrap();
    let mut heads = repository
        .repo()
        .op_heads_store()
        .get_op_heads()
        .await
        .unwrap()
        .into_iter()
        .map(|head| head.hex())
        .collect::<Vec<_>>();
    heads.sort();
    heads
}

fn sync_log(store: &MachineStore, repository_name: &str) -> String {
    let name = RepositoryName::parse(repository_name).unwrap();
    let entry = store.resolve(&name).unwrap().unwrap();
    fs::read_to_string(
        entry
            .native_repository_path
            .parent()
            .unwrap()
            .join("sync.log"),
    )
    .unwrap_or_else(|error| format!("<unavailable: {error}>"))
}

fn unique_repository_name(temp: &Path) -> String {
    let suffix = temp
        .file_name()
        .unwrap()
        .to_string_lossy()
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(|byte| byte.to_ascii_lowercase() as char)
        .collect::<String>();
    format!("boundary-live-{}-{suffix}", std::process::id())
}

fn poll_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    condition()
}
