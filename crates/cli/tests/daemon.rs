#![cfg(unix)]

#[path = "support/stalling_server.rs"]
mod stalling_server;
mod support;

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::Shutdown;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use devspace_machine::MachineGitRepository as MachineRepository;
use devspace_machine::{
    CatalogEntry, MACHINE_STORE_OVERRIDE, RepositoryId, RepositoryIdentity, RepositoryIncarnation,
    RepositoryName,
};
use stalling_server::StallingServer;
use support::{
    configure_machine, daemon_socket_path, machine_store, poll_until, settings, stderr,
    write_cli_config,
};

#[tokio::test]
async fn daemon_rebinds_stale_socket_is_singleton_and_serves_protocol_privately() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(temp.path(), "http://127.0.0.1:1");
    let config = write_cli_config(temp.path());
    let store = machine_store(temp.path());
    let socket = daemon_socket_path(store.root());
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    fs::write(&socket, "stale socket sentinel\n").unwrap();
    fs::write(store.root().join("daemon.log"), "stale log sentinel\n").unwrap();

    let mut daemon = spawn_daemon(temp.path(), &config, 10_000, 10_000);
    wait_for_socket(&socket);
    assert_eq!(ping(&socket), "pong\n");

    let second = daemon_command(temp.path(), &config, 10_000, 10_000)
        .output()
        .unwrap();
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(
        stderr(&second),
        "The devspace daemon is already running; exiting.\n"
    );

    notify(&socket, "sync missing-repository\n");
    let log_path = store.root().join("daemon.log");
    assert!(
        poll_until(Duration::from_secs(3), || read(&log_path).contains(
            "unknown repository `missing-repository` in notification"
        )),
        "{}",
        read(&log_path)
    );
    assert!(!read(&log_path).contains("stale log sentinel"));
    assert!(daemon.try_wait().unwrap().is_none());

    assert_eq!(
        fs::metadata(store.root()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let socket_metadata = fs::symlink_metadata(&socket).unwrap();
    assert!(socket_metadata.file_type().is_socket());
    assert_eq!(socket_metadata.permissions().mode() & 0o777, 0o600);

    stop(&mut daemon, &socket);
}

#[tokio::test]
async fn deep_machine_store_daemon_serves_ping_and_sync_notification() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("deep-machine-store-parent-".repeat(5));
    fs::create_dir_all(&root).unwrap();
    let entry = local_repository(&root, "notified").await;
    let config = write_cli_config(&root);
    let store = machine_store(&root);
    let old_socket = store.root().join("daemon.sock");
    assert!(old_socket.as_os_str().as_bytes().len() > 110);
    let socket = daemon_socket_path(store.root());
    let log_path = store.root().join("daemon.log");
    let mut daemon = spawn_daemon(&root, &config, 10_000, 10_000);
    wait_for_socket(&socket);
    assert_eq!(ping(&socket), "pong\n");

    assert!(
        poll_until(Duration::from_secs(3), || sync_attempts(
            &log_path, "notified"
        ) >= 1),
        "startup drain did not attempt synchronization: {}",
        read(&log_path)
    );
    let startup_attempts = sync_attempts(&log_path, "notified");
    notify(&socket, "sync notified\n");
    assert!(
        poll_until(Duration::from_secs(3), || sync_attempts(
            &log_path, "notified"
        ) > startup_attempts),
        "notification did not trigger synchronization: {}",
        read(&log_path)
    );
    assert!(
        read(&log_path).contains("error sending request"),
        "{}",
        read(&log_path)
    );
    assert!(entry.native_repository_path.is_dir());

    stop(&mut daemon, &socket);
}

#[test]
fn daemon_startup_failure_is_written_to_the_daemon_log() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(temp.path(), "http://127.0.0.1:1");
    let config = write_cli_config(temp.path());
    let store = machine_store(temp.path());
    let socket = daemon_socket_path(store.root());
    fs::create_dir_all(&socket).unwrap();

    let output = daemon_command(temp.path(), &config, 10_000, 10_000)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let expected = format!(
        "cannot replace stale daemon socket at {} because it is not a file",
        socket.display()
    );
    assert!(stderr(&output).contains(&expected), "{}", stderr(&output));
    let log = read(&store.root().join("daemon.log"));
    assert!(
        log.contains(&format!("daemon startup failed: {expected}")),
        "{log}"
    );

    fs::remove_dir(&socket).unwrap();
}

#[test]
fn daemon_exits_when_idle_despite_polling_and_removes_its_socket() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(temp.path(), "http://127.0.0.1:1");
    let config = write_cli_config(temp.path());
    let store = machine_store(temp.path());
    let socket = daemon_socket_path(store.root());

    let output = daemon_command(temp.path(), &config, 20, 150)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!socket.exists());
    assert!(read(&store.root().join("daemon.log")).contains("daemon exiting after idle timeout"));
}

#[tokio::test]
async fn daemon_drains_queued_repositories_before_idle_exit() {
    let server = StallingServer::start();
    let temp = tempfile::tempdir().unwrap();
    local_repository_with_identity(temp.path(), "first-slow", 0xab, server.base_url()).await;
    local_repository_with_identity(temp.path(), "second-queued", 0xbc, server.base_url()).await;
    let config = write_cli_config(temp.path());

    let output = daemon_command(temp.path(), &config, 10_000, 50)
        .env("DEVSPACE_HTTP_TEST_HOOKS", "1")
        .env("DEVSPACE_HTTP_TEST_REQUEST_TIMEOUT_MS", "150")
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    let log = read(&machine_store(temp.path()).root().join("daemon.log"));
    let first = log.find("sync `first-slow` started").unwrap();
    let second = log.find("sync `second-queued` started").unwrap();
    let exit = log.find("daemon exiting after idle timeout").unwrap();
    assert!(first < second && second < exit, "{log}");
}

async fn local_repository(root: &Path, name: &str) -> CatalogEntry {
    local_repository_with_identity(root, name, 0xab, "http://127.0.0.1:1").await
}

async fn local_repository_with_identity(
    root: &Path,
    name: &str,
    identity_byte: u8,
    base_url: &str,
) -> CatalogEntry {
    configure_machine(root, base_url);
    let store = machine_store(root);
    let entry = store
        .register_repository(
            RepositoryName::parse(name).unwrap(),
            RepositoryIdentity::new(
                RepositoryId::parse(format!("{identity_byte:02x}").repeat(32)).unwrap(),
                RepositoryIncarnation::parse(format!("{:02x}", identity_byte + 1).repeat(16))
                    .unwrap(),
            ),
        )
        .unwrap();
    MachineRepository::init(&entry.native_repository_path, &settings())
        .await
        .unwrap();
    entry
}

fn daemon_command(root: &Path, config: &Path, poll_ms: u64, idle_ms: u64) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ds"));
    command
        .current_dir(root)
        .env(MACHINE_STORE_OVERRIDE, root.join("machine-store"))
        .env("JJ_CONFIG", config)
        .env("DEVSPACE_BOUNDARY_SYNC", "0")
        .env("DEVSPACE_DAEMON_TEST_HOOKS", "1")
        .env("DEVSPACE_DAEMON_TEST_POLL_MS", poll_ms.to_string())
        .env("DEVSPACE_DAEMON_TEST_IDLE_MS", idle_ms.to_string())
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .args(["daemon", "run"]);
    command
}

fn spawn_daemon(root: &Path, config: &Path, poll_ms: u64, idle_ms: u64) -> Child {
    daemon_command(root, config, poll_ms, idle_ms)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_for_socket(path: &Path) {
    assert!(
        poll_until(Duration::from_secs(10), || fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.file_type().is_socket())),
        "daemon socket did not appear at {}",
        path.display()
    );
}

fn ping(path: &Path) -> String {
    let mut stream = UnixStream::connect(path).unwrap();
    stream.write_all(b"ping\n").unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    response
}

fn notify(path: &Path, line: &str) {
    let mut stream = UnixStream::connect(path).unwrap();
    stream.write_all(line.as_bytes()).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
}

fn sync_attempts(path: &Path, name: &str) -> usize {
    read(path)
        .matches(&format!("sync `{name}` started"))
        .count()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| format!("<unavailable: {error}>"))
}

fn stop(child: &mut Child, socket: &Path) {
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
    }
    child.wait().unwrap();
    if socket.exists() {
        fs::remove_file(socket).unwrap();
    }
}
