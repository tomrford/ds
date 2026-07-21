mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use devspace_machine::{
    MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineRepository, MachineStore,
    MachineSyncStore, RepositoryName, SharedSecret,
};
#[cfg(unix)]
use support::daemon_socket_path;
use support::{
    commit_id_with_home as commit_id, ds_with_home as ds, machine_store, operation_heads,
    poll_until, repository_commit_ids, seal_commit, settings, stderr, stdout, write_cli_config,
};

const FIRST_MACHINE_ID: &str = "12121212121212121212121212121212";
const SECOND_MACHINE_ID: &str = "34343434343434343434343434343434";

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
async fn virgin_repository_syncs_and_clones_before_any_checkout_exists() {
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
    let synced = ds(
        &home_a,
        &home_a,
        &config_a,
        &["sync", "run", "--repository", &repository_name],
    );
    assert!(synced.status.success(), "{}", stderr(&synced));

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
    fs::write(checkout_b.join("from-b.txt"), "machine B\n").unwrap();
    seal_commit(&checkout_b, &home_b, &config_b, "first commit from B");

    let removed = ds(
        &home_a,
        &home_a,
        &config_a,
        &["repo", "remove", &repository_name, "--force"],
    );
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(
        stderr(&removed).contains(&format!("Deleted repository `{repository_name}`.")),
        "{}",
        stderr(&removed)
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

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn daemon_disabled_boundaries_converge_two_offline_divergent_machines() {
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

    let created = ds_degraded(
        &home_a,
        &home_a,
        &config_a,
        &["repo", "new", &repository_name],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let checkout_a = home_a.join("checkout");
    let added_a = ds_degraded(
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
    seal_commit_degraded(&checkout_a, &home_a, &config_a, "shared base");
    let commit_a = commit_id_degraded(&checkout_a, &home_a, &config_a, "@-");

    assert!(
        poll_until(Duration::from_secs(60), || {
            let output = ds_degraded(&checkout_a, &home_a, &config_a, &["status"]);
            output.status.success()
                && stderr(&output)
                    .contains("sync: in sync with cloud as of the last successful sync")
        }),
        "machine A did not upload {commit_a}: {}",
        sync_log(&machine_store(&home_a), &repository_name)
    );

    let checkout_b = home_b.join("checkout");
    let added_b = ds_degraded(
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
            let output = repository_log_degraded(&checkout_b, &home_b, &config_b);
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
    let based_b = ds_degraded(&checkout_b, &home_b, &config_b, &["new", &commit_a]);
    assert!(based_b.status.success(), "{}", stderr(&based_b));

    fs::write(checkout_a.join("offline-a.txt"), "offline machine A\n").unwrap();
    seal_commit_degraded(&checkout_a, &home_a, &config_a, "offline machine A");
    let offline_a = commit_id_degraded(&checkout_a, &home_a, &config_a, "@-");
    fs::write(checkout_b.join("offline-b.txt"), "offline machine B\n").unwrap();
    seal_commit_degraded(&checkout_b, &home_b, &config_b, "offline machine B");
    let offline_b = commit_id_degraded(&checkout_b, &home_b, &config_b, "@-");

    let pending = ds_degraded(&checkout_a, &home_a, &config_a, &["status"]);
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
        let log_a = repository_log_degraded(&checkout_a, &home_a, &config_a);
        assert!(log_a.status.success(), "{}", stderr(&log_a));
        let log_b = repository_log_degraded(&checkout_b, &home_b, &config_b);
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
            let output = ds_degraded(&checkout_a, &home_a, &config_a, &["status"]);
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

    let created = ds_boundary(
        &home_a,
        &home_a,
        &config_a,
        &["repo", "new", &repository_name],
    );
    assert!(created.status.success(), "{}", stderr(&created));
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
    seal_commit_boundary(&checkout_a, &home_a, &config_a, "daemon polling machine A");
    let commit_a = commit_id_boundary(&checkout_a, &home_a, &config_a, "@-");
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
    let socket_a = daemon_socket_path(store_a.root());
    assert!(
        poll_until(Duration::from_secs(3), || !socket_a.exists()),
        "machine A boundary daemon did not exit after its test idle timeout"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn daemon_restart_drains_one_offline_commit_exactly_once() {
    let base_url = std::env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
    let shared_secret =
        std::env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("machine");
    fs::create_dir_all(&home).unwrap();
    configure_machine(&home, &base_url, FIRST_MACHINE_ID, &shared_secret);
    let config = write_cli_config(&home);
    let repository_name = unique_repository_name(temp.path());

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
    let synchronized = ds(
        &home,
        &home,
        &config,
        &["sync", "run", "--repository", &repository_name],
    );
    assert!(synchronized.status.success(), "{}", stderr(&synchronized));

    let name = RepositoryName::parse(repository_name.clone()).unwrap();
    let store = machine_store(&home);
    let entry = store.resolve(&name).unwrap().unwrap();
    let sync_store = MachineSyncStore::open(store.repository_sync_path(&entry.identity)).unwrap();
    assert!(sync_store.load_outbox().unwrap().is_none());

    let daemon = start_daemon(&home, &config);
    assert!(
        poll_until(Duration::from_secs(30), || fs::read_to_string(
            store.root().join("daemon.log")
        )
        .is_ok_and(
            |log| log.contains(&format!("sync `{repository_name}` completed"))
        )),
        "initial daemon did not complete its startup pass: {}",
        fs::read_to_string(store.root().join("daemon.log")).unwrap_or_default()
    );
    drop(daemon);
    let baseline = sync_store.load_state().unwrap();

    fs::write(
        checkout.join("one-restart-drain.txt"),
        "one pending commit\n",
    )
    .unwrap();
    seal_commit_without_boundary(
        &checkout,
        &home,
        &config,
        "one operation across daemon restart",
    );
    let expected_heads = operation_head_ids(&entry.native_repository_path).await;
    assert_eq!(expected_heads.len(), 1);
    assert_eq!(sync_store.load_state().unwrap(), baseline);
    assert!(sync_store.load_outbox().unwrap().is_none());

    let restarted = start_daemon(&home, &config);
    assert!(
        poll_until(Duration::from_secs(60), || {
            let state = sync_store.load_state().unwrap();
            state.accepted_heads == expected_heads && sync_store.load_outbox().unwrap().is_none()
        }),
        "restarted daemon did not drain the commit: {}",
        fs::read_to_string(store.root().join("daemon.log")).unwrap_or_default()
    );
    let final_state = sync_store.load_state().unwrap();
    assert_eq!(final_state.cloud_cursor, baseline.cloud_cursor + 1);
    assert_eq!(final_state.accepted_heads, expected_heads);
    assert!(sync_store.load_outbox().unwrap().is_none());
    assert_eq!(
        operation_heads(&entry.native_repository_path).await.len(),
        1
    );
    drop(restarted);
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

fn ds_boundary(cwd: &Path, home: &Path, config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ds"))
        .current_dir(cwd)
        .env(MACHINE_STORE_OVERRIDE, home.join("machine-store"))
        .env("JJ_CONFIG", config)
        .env("DEVSPACE_BOUNDARY_SYNC", "1")
        .env("DEVSPACE_DAEMON_TEST_HOOKS", "1")
        .env("DEVSPACE_DAEMON_TEST_POLL_MS", "10000")
        .env("DEVSPACE_DAEMON_TEST_IDLE_MS", "250")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .args(args)
        .output()
        .unwrap()
}

fn ds_degraded(cwd: &Path, home: &Path, config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ds"))
        .current_dir(cwd)
        .env(MACHINE_STORE_OVERRIDE, home.join("machine-store"))
        .env("JJ_CONFIG", config)
        .env("DEVSPACE_BOUNDARY_SYNC", "1")
        .env("DEVSPACE_DAEMON", "0")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .args(args)
        .output()
        .unwrap()
}

#[cfg(unix)]
fn start_daemon(home: &Path, config: &Path) -> DaemonProcess {
    let socket = daemon_socket_path(machine_store(home).root());
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
    DaemonProcess { child, socket }
}

#[cfg(unix)]
struct DaemonProcess {
    child: Child,
    socket: PathBuf,
}

#[cfg(unix)]
impl Drop for DaemonProcess {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
        if self.socket.exists() {
            fs::remove_file(&self.socket).unwrap();
        }
    }
}

fn seal_commit_boundary(cwd: &Path, home: &Path, config: &Path, description: &str) {
    let described = ds_boundary(cwd, home, config, &["describe", "-m", description]);
    assert!(described.status.success(), "{}", stderr(&described));
    let sealed = ds_boundary(cwd, home, config, &["new"]);
    assert!(sealed.status.success(), "{}", stderr(&sealed));
}

fn seal_commit_degraded(cwd: &Path, home: &Path, config: &Path, description: &str) {
    let described = ds_degraded(cwd, home, config, &["describe", "-m", description]);
    assert!(described.status.success(), "{}", stderr(&described));
    let sealed = ds_degraded(cwd, home, config, &["new"]);
    assert!(sealed.status.success(), "{}", stderr(&sealed));
}

fn seal_commit_without_boundary(cwd: &Path, home: &Path, config: &Path, description: &str) {
    let described = ds(cwd, home, config, &["describe", "-m", description]);
    assert!(described.status.success(), "{}", stderr(&described));
    let sealed = ds(cwd, home, config, &["new"]);
    assert!(sealed.status.success(), "{}", stderr(&sealed));
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

fn commit_id_degraded(cwd: &Path, home: &Path, config: &Path, revision: &str) -> String {
    let output = ds_degraded(
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

fn repository_log_degraded(cwd: &Path, home: &Path, config: &Path) -> Output {
    ds_degraded(
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

async fn operation_head_ids(repository_path: &Path) -> BTreeSet<[u8; 64]> {
    let repository = MachineRepository::open(repository_path, &settings())
        .await
        .unwrap();
    repository
        .current_operation_heads()
        .await
        .unwrap()
        .into_iter()
        .collect()
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
