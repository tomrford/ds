#[cfg(unix)]
#[path = "support/stalling_server.rs"]
mod stalling_server;
#[cfg(unix)]
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use devspace_machine::{
    CatalogEntry, MachineSyncStore, PendingHeadBatch, PendingHeadTransaction, RepositoryId,
    RepositoryIdentity, RepositoryIncarnation, RepositoryName, SyncState,
};
use devspace_machine_git::MachineGitRepository as MachineRepository;
use jj_lib::object_id::ObjectId as _;
#[cfg(unix)]
use stalling_server::StallingServer;
#[cfg(unix)]
use support::daemon_socket_path;
use support::{
    configure_machine_as as configure_machine, ds_command_with_home as ds_command,
    ds_with_home as ds, machine_store, operation_heads, poll_until, settings, stderr, stdout,
    write_cli_config,
};

const DEVELOPMENT_SECRET: &str = "cli-development-secret";
const FIRST_MACHINE_ID: &str = "12121212121212121212121212121212";

fn request_body(request: &str) -> serde_json::Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[test]
fn sync_help_lists_status_but_hides_run() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());

    let output = ds(temp.path(), temp.path(), &config, &["sync", "--help"]);

    assert!(output.status.success(), "{}", stderr(&output));
    let help = stdout(&output);
    assert!(
        help.lines()
            .any(|line| line.trim_start().starts_with("status")),
        "{help}"
    );
    assert!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with("run")),
        "{help}"
    );
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

#[tokio::test]
async fn sync_run_silences_colliding_alias_warning() {
    let (base_url, server) = support::fake_worker::create_server(|_, request, stream| {
        let request_line = request.lines().next().unwrap();
        if request_line.starts_with("GET ") && request_line.contains("/packs?") {
            support::fake_worker::respond(
                stream,
                "200 OK",
                r#"{"packs":[],"nextAfter":0,"through":0,"hasMore":false}"#,
            );
            false
        } else if request_line.starts_with("PUT ") && request_line.contains("/packs/") {
            support::fake_worker::respond(
                stream,
                "200 OK",
                r#"{"inserted":true,"installed":false}"#,
            );
            false
        } else if request_line.starts_with("POST ")
            && request_line.contains("/packs/")
            && request_line.contains("/install ")
        {
            support::fake_worker::respond(
                stream,
                "200 OK",
                r#"{"installed":true,"insertedObjects":1}"#,
            );
            false
        } else if request_line.starts_with("GET ") && request_line.contains("/git/ops/heads ") {
            support::fake_worker::respond(stream, "200 OK", r#"{"cursor":0,"heads":[]}"#);
            false
        } else if request_line.starts_with("POST ") && request_line.contains("/git/ops/inventory ")
        {
            support::fake_worker::respond(
                stream,
                "200 OK",
                &serde_json::json!({"keys": request_body(request)["keys"]}).to_string(),
            );
            false
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/ops/heads/transactions ")
        {
            support::fake_worker::respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "cursor": 1,
                    "heads": [request_body(request)["newHead"]],
                })
                .to_string(),
            );
            true
        } else {
            panic!("unexpected fake cloud request: {request_line}");
        }
    });
    let temp = tempfile::tempdir().unwrap();
    local_repository(temp.path(), "alias-warning", &base_url).await;
    let config = write_cli_config(temp.path());
    let mut contents = fs::read_to_string(&config).unwrap();
    contents.push_str("\n[aliases]\nsync = [\"status\"]\n");
    fs::write(&config, contents).unwrap();

    let output = ds(
        temp.path(),
        temp.path(),
        &config,
        &["sync", "run", "--repository-name", "alias-warning"],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
    assert!(
        server
            .join()
            .unwrap()
            .iter()
            .any(|request| request.contains("/git/ops/heads "))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sync_run_times_out_when_the_worker_accepts_but_never_responds() {
    let server = StallingServer::start();
    let temp = tempfile::tempdir().unwrap();
    local_repository(temp.path(), "stalled", server.base_url()).await;
    let config = write_cli_config(temp.path());

    let started = Instant::now();
    let output = ds_command(temp.path(), temp.path(), &config)
        .env("DEVSPACE_HTTP_TEST_HOOKS", "1")
        .env("DEVSPACE_HTTP_TEST_REQUEST_TIMEOUT_MS", "100")
        .args(["sync", "run", "--repository-name", "stalled"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "sync took {:?}",
        started.elapsed()
    );
    assert!(
        stderr(&output).contains("operation timed out"),
        "{}",
        stderr(&output)
    );
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

    let started = Instant::now();
    let output = ds(
        temp.path(),
        temp.path(),
        &config,
        &["sync", "run", "--repository-name", "locked"],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "sync run waited for the held lock: {:?}",
        started.elapsed()
    );
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
        &["sync", "run", "--repository-name", "missing-repository"],
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
        &["sync", "run", "--repository-name", "incomplete"],
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
        &["sync", "run", "--repository-name", "offline"],
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
    let socket = daemon_socket_path(store.root());
    wait_for_path(&socket);
    let running = ds(temp.path(), temp.path(), &config, &["sync", "status"]);
    assert!(running.status.success(), "{}", stderr(&running));
    assert_eq!(
        stderr(&running),
        stderr(&stopped).replacen("daemon: not running", "daemon: running", 1)
    );
    stop_process(&mut daemon);
    fs::remove_file(socket).unwrap();
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
    assert!(!daemon_socket_path(store.root()).exists());
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
    let socket = daemon_socket_path(machine_store(temp.path()).root());
    fs::create_dir_all(&socket).unwrap();

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
    fs::remove_dir(socket).unwrap();
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
    let socket = daemon_socket_path(store.root());
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
    fs::remove_file(socket).unwrap();
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
        &["sync", "run", "--repository-name", "boundary-recursion"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("already being synchronized; skipping"));
    assert!(!poll_until(Duration::from_secs(1), || {
        fs::read_to_string(&sync_log).unwrap() != "sentinel\n"
    }));
}

fn seal_commit_boundary(cwd: &Path, home: &Path, config: &Path, description: &str) {
    let described = ds_boundary(cwd, home, config, &["describe", "-m", description]);
    assert!(described.status.success(), "{}", stderr(&described));
    let sealed = ds_boundary(cwd, home, config, &["new"]);
    assert!(sealed.status.success(), "{}", stderr(&sealed));
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
