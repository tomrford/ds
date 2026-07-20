use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use devspace_machine::{
    CatalogEntry, MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineRepository,
    MachineStore, MachineSyncStore, PendingHeadBatch, PendingHeadTransaction, RepositoryId,
    RepositoryIdentity, RepositoryIncarnation, RepositoryName, SharedSecret, SyncState,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::object_id::ObjectId as _;
use jj_lib::settings::UserSettings;

const DEVELOPMENT_SECRET: &str = "cli-development-secret";
const FIRST_MACHINE_ID: &str = "12121212121212121212121212121212";
const SECOND_MACHINE_ID: &str = "34343434343434343434343434343434";

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
    ds_command(cwd, home, config).args(args).output().unwrap()
}

fn ds_boundary(cwd: &Path, home: &Path, config: &Path, args: &[&str]) -> Output {
    ds_command(cwd, home, config)
        .env("DEVSPACE_BOUNDARY_SYNC", "1")
        .args(args)
        .output()
        .unwrap()
}

fn ds_degraded_boundary(cwd: &Path, home: &Path, config: &Path, args: &[&str]) -> Output {
    ds_command(cwd, home, config)
        .env("DEVSPACE_BOUNDARY_SYNC", "1")
        .env("DEVSPACE_DAEMON", "0")
        .args(args)
        .output()
        .unwrap()
}

#[cfg(unix)]
fn ds_auto_start_boundary(cwd: &Path, home: &Path, config: &Path, args: &[&str]) -> Output {
    ds_command(cwd, home, config)
        .env("DEVSPACE_BOUNDARY_SYNC", "1")
        .env("DEVSPACE_DAEMON_TEST_HOOKS", "1")
        .env("DEVSPACE_DAEMON_TEST_POLL_MS", "10000")
        .env("DEVSPACE_DAEMON_TEST_IDLE_MS", "250")
        .args(args)
        .output()
        .unwrap()
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

async fn local_repository(root: &Path, name: &str, base_url: &str) -> CatalogEntry {
    configure_machine(root, base_url, FIRST_MACHINE_ID, DEVELOPMENT_SECRET);
    let store = machine_store(root);
    let entry = store
        .register_repository(
            RepositoryName::parse(name).unwrap(),
            RepositoryIdentity::new(
                RepositoryId::parse("ab".repeat(32)).unwrap(),
                RepositoryIncarnation::parse("cd".repeat(16)).unwrap(),
            ),
        )
        .unwrap();
    MachineRepository::init(&entry.native_repository_path, &settings())
        .await
        .unwrap();
    entry
}

async fn catalog_repository(
    root: &Path,
    name: &str,
    identity_byte: u8,
    complete: bool,
) -> CatalogEntry {
    let entry = machine_store(root)
        .register_repository(
            RepositoryName::parse(name).unwrap(),
            RepositoryIdentity::new(
                RepositoryId::parse(format!("{identity_byte:02x}").repeat(32)).unwrap(),
                RepositoryIncarnation::parse(format!("{:02x}", identity_byte + 1).repeat(16))
                    .unwrap(),
            ),
        )
        .unwrap();
    if complete {
        MachineRepository::init(&entry.native_repository_path, &settings())
            .await
            .unwrap();
    }
    entry
}

#[tokio::test]
async fn sync_run_skips_when_the_repository_lock_is_held() {
    let temp = tempfile::tempdir().unwrap();
    let entry = local_repository(temp.path(), "locked", "http://127.0.0.1:1").await;
    let config = write_cli_config(temp.path());
    let _guard = machine_store(temp.path())
        .try_lock_repository_sync(&entry.identity)
        .unwrap();

    let output = ds(
        temp.path(),
        temp.path(),
        &config,
        &["sync", "run", "--repository", "locked"],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).is_empty());
    assert_eq!(
        stderr(&output),
        "Repository `locked` is already being synchronized; skipping.\n"
    );
}

#[test]
fn sync_run_rejects_an_unknown_repository_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(
        temp.path(),
        "http://127.0.0.1:1",
        FIRST_MACHINE_ID,
        DEVELOPMENT_SECRET,
    );
    let config = write_cli_config(temp.path());

    let output = ds(
        temp.path(),
        temp.path(),
        &config,
        &["sync", "run", "--repository", "missing-repository"],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output)
            .contains("Repository `missing-repository` is not present in this machine store."),
        "{}",
        stderr(&output)
    );
}

#[test]
fn sync_run_points_an_incomplete_clone_back_to_add() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(
        temp.path(),
        "http://127.0.0.1:1",
        FIRST_MACHINE_ID,
        DEVELOPMENT_SECRET,
    );
    let store = machine_store(temp.path());
    store
        .register_repository(
            RepositoryName::parse("incomplete").unwrap(),
            RepositoryIdentity::new(
                RepositoryId::parse("ab".repeat(32)).unwrap(),
                RepositoryIncarnation::parse("cd".repeat(16)).unwrap(),
            ),
        )
        .unwrap();
    let config = write_cli_config(temp.path());

    let output = ds(
        temp.path(),
        temp.path(),
        &config,
        &["sync", "run", "--repository", "incomplete"],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains(
            "Repository `incomplete` has an incomplete clone; run `ds add` again to finish it."
        ),
        "{}",
        stderr(&output)
    );
}

#[tokio::test]
async fn sync_run_reports_offline_transport_failure_without_mutating_durable_state() {
    let temp = tempfile::tempdir().unwrap();
    let entry = local_repository(temp.path(), "offline", "http://127.0.0.1:1").await;
    let store = machine_store(temp.path());
    let config = write_cli_config(temp.path());

    drop(store.try_lock_repository_sync(&entry.identity).unwrap());
    let sync_store = MachineSyncStore::open(store.repository_sync_path(&entry.identity)).unwrap();
    drop(sync_store.lock().unwrap());
    let before = snapshot_files(store.root());
    let operation_before = MachineRepository::open(&entry.native_repository_path, &settings())
        .await
        .unwrap()
        .repo()
        .op_id()
        .clone();

    let output = ds(
        temp.path(),
        temp.path(),
        &config,
        &["sync", "run", "--repository", "offline"],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("error sending request"),
        "{}",
        stderr(&output)
    );
    assert_eq!(snapshot_files(store.root()), before);
    let repository = MachineRepository::open(&entry.native_repository_path, &settings())
        .await
        .unwrap();
    assert_eq!(repository.repo().op_id(), &operation_before);
    assert!(sync_store.load_outbox().unwrap().is_none());
}

#[tokio::test]
async fn status_reports_local_sync_state_even_when_boundary_sync_is_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let entry = local_repository(temp.path(), "status-indicator", "http://127.0.0.1:1").await;
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("checkout");
    let added = ds(
        temp.path(),
        temp.path(),
        &config,
        &[
            "add",
            "status-indicator",
            "-r",
            "root()",
            checkout.to_str().unwrap(),
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));

    let never = ds(&checkout, temp.path(), &config, &["status"]);
    assert!(never.status.success(), "{}", stderr(&never));
    assert!(
        stdout(&never).contains("Working copy"),
        "{}",
        stdout(&never)
    );
    assert!(!stdout(&never).contains("sync:"), "{}", stdout(&never));
    assert!(
        stderr(&never).contains("sync: never synchronized"),
        "{}",
        stderr(&never)
    );
    assert_eq!(stderr(&never).matches("sync:").count(), 1);
    let log = ds(
        &checkout,
        temp.path(),
        &config,
        &["log", "-r", "root()", "--no-graph"],
    );
    assert!(log.status.success(), "{}", stderr(&log));
    assert!(!stdout(&log).contains("sync:"), "{}", stdout(&log));
    assert!(!stderr(&log).contains("sync:"), "{}", stderr(&log));

    let store = machine_store(temp.path());
    let sync_store = MachineSyncStore::open(store.repository_sync_path(&entry.identity)).unwrap();
    let pending = ds(&checkout, temp.path(), &config, &["status"]);
    assert!(pending.status.success(), "{}", stderr(&pending));
    assert!(
        stderr(&pending).contains("sync: 1 operation pending upload"),
        "{}",
        stderr(&pending)
    );

    let accepted_heads = operation_head_ids(&entry.native_repository_path).await;
    sync_store
        .save_state(&SyncState {
            accepted_heads: accepted_heads.clone(),
            ..SyncState::default()
        })
        .unwrap();
    let synchronized = ds(&checkout, temp.path(), &config, &["status"]);
    assert!(synchronized.status.success(), "{}", stderr(&synchronized));
    assert!(
        stderr(&synchronized).contains("sync: in sync with cloud as of the last successful sync"),
        "{}",
        stderr(&synchronized)
    );

    let accepted_head = *accepted_heads.first().unwrap();
    sync_store
        .save_outbox(
            &PendingHeadBatch::from_transactions(vec![
                PendingHeadTransaction {
                    idempotency_key: [1; 16],
                    new_head: accepted_head,
                    observed_heads: BTreeSet::new(),
                },
                PendingHeadTransaction {
                    idempotency_key: [2; 16],
                    new_head: [7; 64],
                    observed_heads: BTreeSet::new(),
                },
            ])
            .unwrap(),
        )
        .unwrap();
    let outbox = ds(&checkout, temp.path(), &config, &["status"]);
    assert!(outbox.status.success(), "{}", stderr(&outbox));
    assert!(
        stderr(&outbox).contains("sync: 2 operations pending upload"),
        "{}",
        stderr(&outbox)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sync_status_snapshots_catalog_rows_and_daemon_liveness_from_local_state() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(
        temp.path(),
        "http://127.0.0.1:1",
        FIRST_MACHINE_ID,
        DEVELOPMENT_SECRET,
    );
    let config = write_cli_config(temp.path());
    let in_sync = catalog_repository(temp.path(), "in-sync", 0x10, true).await;
    let _incomplete = catalog_repository(temp.path(), "incomplete", 0x20, false).await;
    let never = catalog_repository(temp.path(), "never", 0x30, true).await;
    let pending = catalog_repository(temp.path(), "pending", 0x40, true).await;
    let store = machine_store(temp.path());

    let repository = MachineRepository::open(&pending.native_repository_path, &settings())
        .await
        .unwrap();
    repository
        .repo()
        .start_transaction()
        .commit("pending status fixture")
        .await
        .unwrap();

    let accepted_heads = operation_head_ids(&in_sync.native_repository_path).await;
    MachineSyncStore::open(store.repository_sync_path(&in_sync.identity))
        .unwrap()
        .save_state(&SyncState {
            accepted_heads,
            ..SyncState::default()
        })
        .unwrap();
    let pending_head = *operation_head_ids(&pending.native_repository_path)
        .await
        .first()
        .unwrap();
    MachineSyncStore::open(store.repository_sync_path(&pending.identity))
        .unwrap()
        .save_outbox(
            &PendingHeadBatch::from_transactions(vec![
                PendingHeadTransaction {
                    idempotency_key: [1; 16],
                    new_head: pending_head,
                    observed_heads: BTreeSet::new(),
                },
                PendingHeadTransaction {
                    idempotency_key: [2; 16],
                    new_head: [7; 64],
                    observed_heads: BTreeSet::new(),
                },
            ])
            .unwrap(),
        )
        .unwrap();

    let stopped = ds(temp.path(), temp.path(), &config, &["sync", "status"]);
    assert!(stopped.status.success(), "{}", stderr(&stopped));
    assert!(stdout(&stopped).is_empty());
    assert_eq!(
        stderr(&stopped),
        concat!(
            "daemon: not running\n",
            "in-sync: pending: 0; in sync with cloud as of the last successful sync\n",
            "incomplete: incomplete clone; pending: 0; never synchronized\n",
            "never: pending: 1; never synchronized\n",
            "pending: pending: 2\n",
        )
    );

    let _locks = [in_sync, never, pending]
        .map(|entry| store.try_lock_repository_sync(&entry.identity).unwrap());
    let mut daemon = spawn_test_daemon(temp.path(), &config, 10_000, 60_000);
    wait_for_path(&store.root().join("daemon.sock"));
    let running = ds(temp.path(), temp.path(), &config, &["sync", "status"]);
    assert!(running.status.success(), "{}", stderr(&running));
    assert_eq!(
        stderr(&running),
        stderr(&stopped).replacen("daemon: not running", "daemon: running", 1)
    );
    stop_process(&mut daemon);
}

#[tokio::test]
async fn daemon_opt_out_spawns_a_silent_detached_sync() {
    let temp = tempfile::tempdir().unwrap();
    let entry = local_repository(temp.path(), "boundary-offline", "http://127.0.0.1:1").await;
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("checkout");
    let added = ds(
        temp.path(),
        temp.path(),
        &config,
        &[
            "add",
            "boundary-offline",
            "-r",
            "root()",
            checkout.to_str().unwrap(),
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));

    let started = Instant::now();
    let output = ds_degraded_boundary(
        &checkout,
        temp.path(),
        &config,
        &["log", "-r", "root()", "--no-graph"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "repository command waited for boundary sync: {:?}",
        started.elapsed()
    );
    for visible_output in [stdout(&output), stderr(&output)] {
        assert!(!visible_output.contains("error sending request"));
        assert!(!visible_output.contains("synchroniz"));
    }

    let sync_log = entry
        .native_repository_path
        .parent()
        .unwrap()
        .join("sync.log");
    assert!(
        poll_until(Duration::from_secs(3), || fs::read_to_string(&sync_log)
            .is_ok_and(|log| log.contains("error sending request"))),
        "detached sync did not report its transport failure in {}",
        sync_log.display()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ordinary_read_auto_starts_the_daemon_without_waiting_for_sync() {
    let temp = tempfile::tempdir().unwrap();
    let _entry = local_repository(temp.path(), "auto-start", "http://127.0.0.1:1").await;
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("checkout");
    let added = ds(
        temp.path(),
        temp.path(),
        &config,
        &[
            "add",
            "auto-start",
            "-r",
            "root()",
            checkout.to_str().unwrap(),
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));

    let started = Instant::now();
    let output = ds_auto_start_boundary(
        &checkout,
        temp.path(),
        &config,
        &["log", "-r", "root()", "--no-graph"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "repository read waited for daemon startup or sync: {:?}",
        started.elapsed()
    );
    for visible_output in [stdout(&output), stderr(&output)] {
        assert!(!visible_output.contains("error sending request"));
        assert!(!visible_output.contains("synchroniz"));
    }

    let store = machine_store(temp.path());
    let daemon_log = store.root().join("daemon.log");
    assert!(
        poll_until(Duration::from_secs(3), || fs::read_to_string(&daemon_log)
            .is_ok_and(|log| log.contains("sync `auto-start` started"))),
        "auto-started daemon did not receive the boundary: {}",
        fs::read_to_string(&daemon_log).unwrap_or_default()
    );
    assert!(
        poll_until(Duration::from_secs(3), || fs::read_to_string(&daemon_log)
            .is_ok_and(|log| log.contains("daemon exiting after idle timeout"))),
        "auto-started daemon did not exit after its test idle timeout: {}",
        fs::read_to_string(&daemon_log).unwrap_or_default()
    );
    assert!(!store.root().join("daemon.sock").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn failed_daemon_start_falls_back_to_the_detached_one_shot() {
    let temp = tempfile::tempdir().unwrap();
    let entry = local_repository(temp.path(), "fallback", "http://127.0.0.1:1").await;
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("checkout");
    let added = ds(
        temp.path(),
        temp.path(),
        &config,
        &[
            "add",
            "fallback",
            "-r",
            "root()",
            checkout.to_str().unwrap(),
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    fs::create_dir(machine_store(temp.path()).root().join("daemon.sock")).unwrap();

    let output = ds_auto_start_boundary(
        &checkout,
        temp.path(),
        &config,
        &["log", "-r", "root()", "--no-graph"],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let sync_log = entry
        .native_repository_path
        .parent()
        .unwrap()
        .join("sync.log");
    assert!(
        poll_until(Duration::from_secs(3), || fs::read_to_string(&sync_log)
            .is_ok_and(|log| log.contains("error sending request"))),
        "lost boundary did not fall back to a one-shot: {}",
        fs::read_to_string(&sync_log).unwrap_or_default()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sigkill_and_restart_preserve_pending_local_work_and_recover_the_socket() {
    let temp = tempfile::tempdir().unwrap();
    let entry = local_repository(temp.path(), "kill-survival", "http://127.0.0.1:1").await;
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("checkout");
    let added = ds(
        temp.path(),
        temp.path(),
        &config,
        &[
            "add",
            "kill-survival",
            "-r",
            "root()",
            checkout.to_str().unwrap(),
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    let store = machine_store(temp.path());
    let socket = store.root().join("daemon.sock");
    let daemon_log = store.root().join("daemon.log");
    let mut daemon = spawn_test_daemon(temp.path(), &config, 10_000, 60_000);
    wait_for_path(&socket);
    let startup_attempts = fs::read_to_string(&daemon_log)
        .unwrap_or_default()
        .matches("sync `kill-survival` started")
        .count();

    fs::write(checkout.join("survives-kill.txt"), "durable local work\n").unwrap();
    seal_commit_boundary(
        &checkout,
        temp.path(),
        &config,
        "pending across daemon kill",
    );
    assert!(
        poll_until(Duration::from_secs(3), || fs::read_to_string(&daemon_log)
            .unwrap_or_default()
            .matches("sync `kill-survival` started")
            .count()
            > startup_attempts),
        "daemon did not attempt the pending operation: {}",
        fs::read_to_string(&daemon_log).unwrap_or_default()
    );
    let heads_before = operation_heads(&entry.native_repository_path).await;
    let pending_before = ds(&checkout, temp.path(), &config, &["status"]);
    assert!(
        stderr(&pending_before).contains("pending upload"),
        "{}",
        stderr(&pending_before)
    );

    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(
        socket.exists(),
        "SIGKILL unexpectedly cleaned up the socket"
    );

    let mut restarted = spawn_test_daemon(temp.path(), &config, 10_000, 60_000);
    assert!(
        poll_until(Duration::from_secs(3), || {
            let status = ds(temp.path(), temp.path(), &config, &["sync", "status"]);
            status.status.success() && stderr(&status).starts_with("daemon: running\n")
        }),
        "restarted daemon did not replace the stale socket"
    );
    assert!(restarted.try_wait().unwrap().is_none());
    assert_eq!(
        operation_heads(&entry.native_repository_path).await,
        heads_before
    );
    let pending_after = ds(&checkout, temp.path(), &config, &["status"]);
    assert!(
        stderr(&pending_after).contains("pending upload"),
        "{}",
        stderr(&pending_after)
    );
    stop_process(&mut restarted);
}

#[tokio::test]
async fn removal_from_inside_the_checkout_still_spawns_boundary_sync() {
    let temp = tempfile::tempdir().unwrap();
    let entry = local_repository(temp.path(), "boundary-removed", "http://127.0.0.1:1").await;
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("checkout");
    let added = ds(
        temp.path(),
        temp.path(),
        &config,
        &[
            "add",
            "boundary-removed",
            "-r",
            "root()",
            checkout.to_str().unwrap(),
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));

    // Removing the checkout deletes the command's own working directory; the
    // detached sync must still run from a surviving one.
    let output = ds_degraded_boundary(&checkout, temp.path(), &config, &["remove", "."]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!checkout.exists());

    let sync_log = entry
        .native_repository_path
        .parent()
        .unwrap()
        .join("sync.log");
    assert!(
        poll_until(Duration::from_secs(3), || fs::read_to_string(&sync_log)
            .is_ok_and(|log| log.contains("error sending request"))),
        "detached sync after removal did not run from a surviving directory: {}",
        sync_log.display()
    );
}

#[tokio::test]
async fn failed_repository_command_does_not_spawn_boundary_sync() {
    let temp = tempfile::tempdir().unwrap();
    let entry = local_repository(temp.path(), "boundary-failed", "http://127.0.0.1:1").await;
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("checkout");
    let added = ds(
        temp.path(),
        temp.path(),
        &config,
        &[
            "add",
            "boundary-failed",
            "-r",
            "root()",
            checkout.to_str().unwrap(),
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    let store = machine_store(temp.path());
    let _sync_guard = store.try_lock_repository_sync(&entry.identity).unwrap();
    let sync_log = entry
        .native_repository_path
        .parent()
        .unwrap()
        .join("sync.log");

    let output = ds_degraded_boundary(
        &checkout,
        temp.path(),
        &config,
        &["log", "-r", "does-not-exist", "--no-graph"],
    );
    assert!(!output.status.success());
    assert!(!poll_until(Duration::from_secs(1), || sync_log.exists()));
}

#[tokio::test]
async fn sync_run_does_not_respawn_itself() {
    let temp = tempfile::tempdir().unwrap();
    let entry = local_repository(temp.path(), "boundary-recursion", "http://127.0.0.1:1").await;
    let config = write_cli_config(temp.path());
    let store = machine_store(temp.path());
    let _sync_guard = store.try_lock_repository_sync(&entry.identity).unwrap();
    let sync_log = entry
        .native_repository_path
        .parent()
        .unwrap()
        .join("sync.log");
    fs::write(&sync_log, "sentinel\n").unwrap();

    let output = ds_boundary(
        temp.path(),
        temp.path(),
        &config,
        &["sync", "run", "--repository", "boundary-recursion"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("already being synchronized; skipping"));
    assert!(!poll_until(Duration::from_secs(1), || {
        fs::read_to_string(&sync_log).unwrap() != "sentinel\n"
    }));
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn two_machine_cli_sync_converges_through_a_live_worker() {
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
    let name = RepositoryName::parse(repository_name.clone()).unwrap();
    let store_a = machine_store(&home_a);
    let entry_a = store_a.resolve(&name).unwrap().unwrap();

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
    fs::write(checkout_a.join("from-a.txt"), "machine A\n").unwrap();
    seal_commit(&checkout_a, &home_a, &config_a, "machine A");
    let commit_a = commit_id(&checkout_a, &home_a, &config_a, "@-");

    let uploaded_a = ds(
        &home_a,
        &home_a,
        &config_a,
        &["sync", "run", "--repository", &repository_name],
    );
    assert!(uploaded_a.status.success(), "{}", stderr(&uploaded_a));

    let store_b = machine_store(&home_b);
    let checkout_b = home_b.join("checkout");
    let added_b = ds(
        &home_b,
        &home_b,
        &config_b,
        &[
            "add",
            &repository_name,
            "-r",
            &commit_a,
            checkout_b.to_str().unwrap(),
        ],
    );
    assert!(added_b.status.success(), "{}", stderr(&added_b));
    assert_eq!(
        fs::read_to_string(checkout_b.join("from-a.txt")).unwrap(),
        "machine A\n"
    );
    fs::write(checkout_b.join("from-b.txt"), "machine B\n").unwrap();
    seal_commit(&checkout_b, &home_b, &config_b, "machine B");
    let commit_b = commit_id(&checkout_b, &home_b, &config_b, "@-");

    for (home, config) in [
        (&home_a, &config_a),
        (&home_b, &config_b),
        (&home_a, &config_a),
    ] {
        let output = ds(
            home,
            home,
            config,
            &["sync", "run", "--repository", &repository_name],
        );
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(stdout(&output).is_empty());
    }

    let heads_a = operation_heads(&entry_a.native_repository_path).await;
    let entry_b = store_b.resolve(&name).unwrap().unwrap();
    let heads_b = operation_heads(&entry_b.native_repository_path).await;
    assert_eq!(heads_a.len(), 1);
    assert_eq!(heads_a, heads_b);

    for (home, config) in [(&home_a, &config_a), (&home_b, &config_b)] {
        let commits = repository_commit_ids(home, config, &repository_name);
        assert!(
            commits.contains(&commit_a),
            "missing {commit_a} in {commits:?}"
        );
        assert!(
            commits.contains(&commit_b),
            "missing {commit_b} in {commits:?}"
        );
    }
    assert!(
        MachineSyncStore::open(store_a.repository_sync_path(&entry_a.identity))
            .unwrap()
            .load_outbox()
            .unwrap()
            .is_none()
    );
    assert!(
        MachineSyncStore::open(store_b.repository_sync_path(&entry_b.identity))
            .unwrap()
            .load_outbox()
            .unwrap()
            .is_none()
    );

    fs::remove_dir_all(store_b.root()).unwrap();
    configure_machine(&home_b, &base_url, SECOND_MACHINE_ID, &shared_secret);
    let rebuilt_b = ds(
        &home_b,
        &home_b,
        &config_b,
        &[
            "add",
            &repository_name,
            "-r",
            &commit_b,
            checkout_b.to_str().unwrap(),
        ],
    );
    assert!(rebuilt_b.status.success(), "{}", stderr(&rebuilt_b));
    let rebuilt_entry_b = store_b.resolve(&name).unwrap().unwrap();
    assert_eq!(
        operation_heads(&rebuilt_entry_b.native_repository_path).await,
        heads_a
    );
    assert_eq!(
        fs::read_to_string(checkout_b.join("from-a.txt")).unwrap(),
        "machine A\n"
    );
    assert_eq!(
        fs::read_to_string(checkout_b.join("from-b.txt")).unwrap(),
        "machine B\n"
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn boundary_sync_uploads_machine_a_without_explicit_sync() {
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

    let created = ds_boundary(
        &home_a,
        &home_a,
        &config_a,
        &["repo", "new", &repository_name],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let name = RepositoryName::parse(repository_name.clone()).unwrap();
    let store_a = machine_store(&home_a);
    let entry_a = store_a.resolve(&name).unwrap().unwrap();

    let checkout_a = home_a.join("checkout");
    let added_a = ds_boundary(
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
    fs::write(checkout_a.join("from-boundary-a.txt"), "machine A\n").unwrap();
    seal_commit_boundary(&checkout_a, &home_a, &config_a, "boundary machine A");
    let commit_a = commit_id_boundary(&checkout_a, &home_a, &config_a, "@-");

    let store_b = machine_store(&home_b);
    let entry_b = store_b
        .register_repository(name.clone(), entry_a.identity.clone())
        .unwrap();
    MachineRepository::init(&entry_b.native_repository_path, &settings())
        .await
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let boundary = ds_boundary(
            &checkout_a,
            &home_a,
            &config_a,
            &["log", "-r", &commit_a, "--no-graph"],
        );
        assert!(boundary.status.success(), "{}", stderr(&boundary));

        let pulled_b = ds(
            &home_b,
            &home_b,
            &config_b,
            &["sync", "run", "--repository", &repository_name],
        );
        assert!(pulled_b.status.success(), "{}", stderr(&pulled_b));
        if repository_commit_ids(&home_b, &config_b, &repository_name).contains(&commit_a) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "machine A boundary sync did not upload {commit_a}; daemon log: {}",
            fs::read_to_string(store_a.root().join("daemon.log"))
                .unwrap_or_else(|error| format!("<unavailable: {error}>"))
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn seal_commit(cwd: &Path, home: &Path, config: &Path, description: &str) {
    let described = ds(cwd, home, config, &["describe", "-m", description]);
    assert!(described.status.success(), "{}", stderr(&described));
    let sealed = ds(cwd, home, config, &["new"]);
    assert!(sealed.status.success(), "{}", stderr(&sealed));
}

fn seal_commit_boundary(cwd: &Path, home: &Path, config: &Path, description: &str) {
    let described = ds_boundary(cwd, home, config, &["describe", "-m", description]);
    assert!(described.status.success(), "{}", stderr(&described));
    let sealed = ds_boundary(cwd, home, config, &["new"]);
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

fn commit_id_boundary(cwd: &Path, home: &Path, config: &Path, revision: &str) -> String {
    let output = ds_boundary(
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

fn repository_commit_ids(home: &Path, config: &Path, name: &str) -> Vec<String> {
    let output = ds(
        home,
        home,
        config,
        &[
            "-R",
            name,
            "log",
            "-r",
            "all()",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output).lines().map(str::to_owned).collect()
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

async fn operation_head_ids(repository_path: &Path) -> BTreeSet<[u8; 64]> {
    let repository = MachineRepository::open(repository_path, &settings())
        .await
        .unwrap();
    repository
        .repo()
        .op_heads_store()
        .get_op_heads()
        .await
        .unwrap()
        .into_iter()
        .map(|head| head.as_bytes().try_into().unwrap())
        .collect()
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
    format!("sync-live-{}-{suffix}", std::process::id())
}

fn poll_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    condition()
}

#[cfg(unix)]
fn spawn_test_daemon(root: &Path, config: &Path, poll_ms: u64, idle_ms: u64) -> Child {
    ds_command(root, root, config)
        .env("DEVSPACE_DAEMON_TEST_HOOKS", "1")
        .env("DEVSPACE_DAEMON_TEST_POLL_MS", poll_ms.to_string())
        .env("DEVSPACE_DAEMON_TEST_IDLE_MS", idle_ms.to_string())
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[cfg(unix)]
fn wait_for_path(path: &Path) {
    assert!(
        poll_until(Duration::from_secs(3), || path.exists()),
        "path did not appear: {}",
        path.display()
    );
}

#[cfg(unix)]
fn stop_process(child: &mut Child) {
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
    }
    child.wait().unwrap();
}

fn snapshot_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }

    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}
