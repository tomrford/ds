use std::collections::BTreeSet;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;

use devspace_machine::{
    MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineRepository, MachineStore, PackOptions,
    RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName, SharedSecret,
    build_packs,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::settings::UserSettings;
use jj_lib::workspace_store::{SimpleWorkspaceStore, WorkspaceStore as _};

const DEVELOPMENT_SECRET: &str = "cli-development-secret";
const MACHINE_ID: &str = "12121212121212121212121212121212";

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
        "#,
    )
    .unwrap();
    path
}

fn ds(cwd: &Path, config: &Path, args: &[&str]) -> Output {
    ds_command(cwd, config).args(args).output().unwrap()
}

fn ds_command(cwd: &Path, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ds"));
    command
        .current_dir(cwd)
        .env(
            MACHINE_STORE_OVERRIDE,
            config.parent().unwrap().join("machine-store"),
        )
        .env("JJ_CONFIG", config)
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

fn machine_store(root: &Path) -> MachineStore {
    MachineStore::new(root.join("machine-store"))
}

fn configure_machine(root: &Path, base_url: &str) {
    machine_store(root)
        .write_config(
            &MachineConfig::new(
                base_url,
                MachineId::parse(MACHINE_ID).unwrap(),
                SharedSecret::new(DEVELOPMENT_SECRET).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
}

async fn local_repository(root: &Path, repository_name: &str) -> PathBuf {
    let store = machine_store(root);
    configure_machine(root, "http://127.0.0.1:1");
    let entry = store
        .register_repository(
            RepositoryName::parse(repository_name).unwrap(),
            RepositoryIdentity::new(
                RepositoryId::parse("ab".repeat(32)).unwrap(),
                RepositoryIncarnation::parse("cd".repeat(16)).unwrap(),
            ),
        )
        .unwrap();
    MachineRepository::init(&entry.native_repository_path, &settings())
        .await
        .unwrap();
    entry.native_repository_path
}

fn create_server<F>(mut handle: F) -> (String, thread::JoinHandle<Vec<String>>)
where
    F: FnMut(&str, &mut TcpStream) -> bool + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        let mut deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return requests;
                    }
                    thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("failed to accept test HTTP connection: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            let request = read_http_request(&mut stream);
            let done = handle(&request, &mut stream);
            requests.push(request);
            if done {
                return requests;
            }
            deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        }
    });
    (address, server)
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 16 * 1024];
    let expected_length = loop {
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "HTTP request ended before its headers");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break header_end + 4 + content_length;
        }
    };
    while bytes.len() < expected_length {
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "HTTP request ended before its body");
        bytes.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8_lossy(&bytes[..expected_length]).into_owned()
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    respond_bytes(stream, status, "application/json", body.as_bytes());
}

fn respond_bytes(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
}

fn repository_response(name: &str) -> String {
    format!(
        r#"{{"name":"{name}","repositoryId":"{}","incarnation":"{}"}}"#,
        "ab".repeat(32),
        "cd".repeat(16)
    )
}

fn request_body(request: &str) -> serde_json::Value {
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    serde_json::from_str(body).unwrap()
}

struct CloudFixture {
    pack_id: String,
    manifest: Vec<u8>,
    chunks: Vec<Vec<u8>>,
    operation_head: String,
}

async fn cloud_fixture(root: &Path) -> CloudFixture {
    let fixture_root = root.join("cloud-fixture");
    fs::create_dir(&fixture_root).unwrap();
    let repository_path = local_repository(&fixture_root, "fixture").await;
    let config = write_cli_config(&fixture_root);
    let destination = fixture_root.join("checkout");
    let output = add(&fixture_root, &config, "fixture", "root()", &destination);
    assert!(output.status.success(), "{}", stderr(&output));
    let repository = MachineRepository::open(&repository_path, &settings())
        .await
        .unwrap();
    let closure = repository.object_closure(&BTreeSet::new()).await.unwrap();
    let operation_head = hex_bytes(*closure.operation_heads.first().unwrap());
    let built = build_packs(
        &closure,
        &BTreeSet::new(),
        fixture_root.join("packs"),
        PackOptions::default(),
    )
    .unwrap();
    let pack = built.packs.into_iter().next().unwrap();
    let chunks = (0..pack.manifest.chunks().len())
        .map(|position| fs::read(pack.directory.join(format!("{position:08}.chunk"))).unwrap())
        .collect();
    CloudFixture {
        pack_id: hex_bytes(pack.id),
        manifest: pack.manifest.encode(),
        chunks,
        operation_head,
    }
}

fn hex_bytes<const N: usize>(bytes: [u8; N]) -> String {
    bytes
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn create_cloud_sync_server(fixture: CloudFixture) -> (String, thread::JoinHandle<Vec<String>>) {
    create_server(move |request, stream| {
        let request_line = request.lines().next().unwrap();
        if request_line.starts_with("GET ") && request_line.contains("/packs?") {
            respond(
                stream,
                "200 OK",
                &format!(
                    r#"{{"packs":[{{"sequence":1,"id":"{}"}}],"nextAfter":1,"through":1,"hasMore":false}}"#,
                    fixture.pack_id
                ),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/manifest?") {
            respond_bytes(
                stream,
                "200 OK",
                "application/octet-stream",
                &fixture.manifest,
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/chunks/") {
            let position = request_line
                .split("/chunks/")
                .nth(1)
                .unwrap()
                .split(['?', ' '])
                .next()
                .unwrap()
                .parse::<usize>()
                .unwrap();
            respond_bytes(
                stream,
                "200 OK",
                "application/octet-stream",
                &fixture.chunks[position],
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/heads?") {
            respond(
                stream,
                "200 OK",
                &format!(r#"{{"cursor":1,"heads":["{}"]}}"#, fixture.operation_head),
            );
        } else if request_line.starts_with("POST ") && request_line.contains("/objects/inventory ")
        {
            let objects = request_body(request)["objects"].clone();
            respond(
                stream,
                "200 OK",
                &serde_json::json!({ "objects": objects }).to_string(),
            );
        } else {
            respond(stream, "200 OK", "{}");
        }
        false
    })
}

#[test]
fn add_catalog_miss_reports_unreachable_control_plane_without_registering() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(temp.path(), "http://127.0.0.1:1");
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");

    let output = add(temp.path(), &config, "offline-miss", "root()", &destination);

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(
            "Repository `offline-miss` is unknown locally and the control plane is unreachable"
        ),
        "{}",
        stderr(&output)
    );
    assert!(
        machine_store(temp.path())
            .resolve(&RepositoryName::parse("offline-miss").unwrap())
            .unwrap()
            .is_none()
    );
    assert!(!destination.exists());
}

#[test]
fn add_catalog_miss_classifies_unknown_name_by_remote_code() {
    let temp = tempfile::tempdir().unwrap();
    let (base_url, server) = create_server(|_, stream| {
        respond(
            stream,
            "409 Conflict",
            r#"{"error":"opaque directory failure","code":"repository-not-found"}"#,
        );
        true
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");

    let output = add(temp.path(), &config, "unknown-name", "root()", &destination);
    let requests = server.join().unwrap();

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("Repository `unknown-name` was not found in the control plane."),
        "{}",
        stderr(&output)
    );
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /repositories/unknown-name "));
    assert!(
        machine_store(temp.path())
            .resolve(&RepositoryName::parse("unknown-name").unwrap())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn add_catalog_hit_never_contacts_the_control_plane() {
    let temp = tempfile::tempdir().unwrap();
    local_repository(temp.path(), "offline-hit").await;
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");

    let output = add(temp.path(), &config, "offline-hit", "root()", &destination);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(destination.join(".jj/repo").is_file());
}

#[tokio::test]
async fn add_resumes_after_kill_between_catalog_registration_and_native_publication() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = cloud_fixture(temp.path()).await;
    let response = repository_response("kill-clone");
    let (base_url, resolver) = create_server(move |_, stream| {
        respond(stream, "200 OK", &response);
        true
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");
    let ready = temp.path().join("ready");
    let mut child = spawn_add_at_failpoint(
        temp.path(),
        &config,
        "kill-clone",
        &destination,
        "after_clone_registration",
        &ready,
        None,
    );
    wait_for_failpoint(&ready);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(resolver.join().unwrap().len(), 1);

    let store = machine_store(temp.path());
    let name = RepositoryName::parse("kill-clone").unwrap();
    let entry = store.resolve(&name).unwrap().unwrap();
    assert!(!entry.native_repository_path.exists());
    assert!(!destination.exists());

    configure_machine(temp.path(), "http://127.0.0.1:1");
    let offline = add(temp.path(), &config, "kill-clone", "root()", &destination);
    assert_eq!(offline.status.code(), Some(1), "{}", stderr(&offline));
    assert!(
        stderr(&offline).contains("Repository `kill-clone` is incomplete locally"),
        "{}",
        stderr(&offline)
    );
    assert!(!entry.native_repository_path.exists());
    assert!(!destination.exists());

    let repository_directory = entry.native_repository_path.parent().unwrap();
    for path in [
        repository_directory.join(".clone-staging"),
        store.repository_sync_path(&entry.identity),
        store.repository_packs_path(&entry.identity),
    ] {
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("stale"), "must disappear").unwrap();
    }

    let (base_url, sync_server) = create_cloud_sync_server(fixture);
    configure_machine(temp.path(), &base_url);
    let retry = add(temp.path(), &config, "kill-clone", "root()", &destination);
    let requests = sync_server.join().unwrap();

    assert!(retry.status.success(), "{}", stderr(&retry));
    assert!(entry.native_repository_path.is_dir());
    assert!(destination.join(".jj/repo").is_file());
    assert!(
        requests
            .iter()
            .all(|request| !request.starts_with("GET /repositories/kill-clone "))
    );
    assert!(!repository_directory.join(".clone-staging").exists());
    assert!(
        !store
            .repository_sync_path(&entry.identity)
            .join("stale")
            .exists()
    );
    assert!(
        !store
            .repository_packs_path(&entry.identity)
            .join("stale")
            .exists()
    );
}

fn add(cwd: &Path, config: &Path, repository_name: &str, revision: &str, path: &Path) -> Output {
    ds_command(cwd, config)
        .args(["add", repository_name, "-r", revision])
        .arg(path)
        .output()
        .unwrap()
}

fn add_json(
    cwd: &Path,
    config: &Path,
    repository_name: &str,
    revision: &str,
    path: &Path,
) -> serde_json::Value {
    let output = ds_command(cwd, config)
        .args(["add", repository_name, "-r", revision])
        .arg(path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn commit_id(cwd: &Path, config: &Path, revision: &str) -> String {
    let output = ds(
        cwd,
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

fn spawn_add_at_failpoint(
    cwd: &Path,
    config: &Path,
    repository_name: &str,
    destination: &Path,
    failpoint: &str,
    ready: &Path,
    continue_path: Option<&Path>,
) -> Child {
    let mut command = ds_command(cwd, config);
    command
        .env("DEVSPACE_TEST_CHECKOUT_FAILPOINT", failpoint)
        .env("DEVSPACE_TEST_CHECKOUT_FAILPOINT_READY", ready)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["add", repository_name, "-r", "root()"])
        .arg(destination)
        .arg("--json");
    if let Some(path) = continue_path {
        command.env("DEVSPACE_TEST_CHECKOUT_FAILPOINT_CONTINUE", path);
    }
    command.spawn().unwrap()
}

fn wait_for_failpoint(ready: &Path) {
    for _ in 0..1_000 {
        if ready.exists() {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("checkout failpoint was not reached");
}

async fn only_workspace(repository_path: &Path) -> (WorkspaceNameBuf, jj_lib::backend::CommitId) {
    let repository = MachineRepository::open(repository_path, &settings())
        .await
        .unwrap();
    let (name, commit) = repository
        .repo()
        .view()
        .wc_commit_ids()
        .first_key_value()
        .unwrap();
    (name.clone(), commit.clone())
}

fn stored_workspace_path(
    repository_path: &Path,
    workspace_name: &WorkspaceName,
) -> Option<PathBuf> {
    SimpleWorkspaceStore::load(repository_path)
        .unwrap()
        .get_workspace_path(workspace_name)
        .unwrap()
        .map(|path| {
            let path = if path.is_absolute() {
                path
            } else {
                dunce::canonicalize(repository_path).unwrap().join(path)
            };
            dunce::canonicalize(&path)
                .unwrap_or_else(|error| panic!("failed to resolve {}: {error}", path.display()))
        })
}

#[tokio::test]
async fn add_fresh_creates_deterministic_owned_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "fresh";
    let repository_path = local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("parent/../checkout");

    let added = add_json(
        temp.path(),
        &config,
        repository_name,
        "root()",
        &destination,
    );
    let canonical_destination = dunce::canonicalize(temp.path()).unwrap().join("checkout");
    let workspace_id = added["workspace_id"].as_str().unwrap();
    assert!(workspace_id.starts_with(&format!("{MACHINE_ID}-")));
    assert_eq!(workspace_id.len(), MACHINE_ID.len() + 1 + 24);
    assert_eq!(added["root"], canonical_destination.to_str().unwrap());
    assert!(canonical_destination.join(".jj/repo").is_file());
    let owner: serde_json::Value = serde_json::from_slice(
        &fs::read(
            canonical_destination
                .join(".jj")
                .join("devspace-checkout-owner"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(owner["repository_id"], "ab".repeat(32));
    assert_eq!(owner["incarnation"], "cd".repeat(16));
    assert_eq!(owner["workspace_name"], workspace_id);
    let (registered, _) = only_workspace(&repository_path).await;
    assert_eq!(registered.as_str(), workspace_id);
    assert!(
        !temp
            .path()
            .join("machine-store/checkout-creations.json")
            .exists()
    );
}

#[tokio::test]
async fn add_existing_owned_checkout_is_idempotent_and_repairs_workspace_store() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "existing";
    let repository_path = local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");
    let added = add_json(
        temp.path(),
        &config,
        repository_name,
        "root()",
        &destination,
    );
    let workspace_name = WorkspaceNameBuf::from(added["workspace_id"].as_str().unwrap().to_owned());
    SimpleWorkspaceStore::load(&repository_path)
        .unwrap()
        .forget(&[&workspace_name])
        .unwrap();
    assert!(stored_workspace_path(&repository_path, &workspace_name).is_none());

    let retry = add(
        temp.path(),
        &config,
        repository_name,
        "root()",
        &destination,
    );
    assert!(retry.status.success(), "{}", stderr(&retry));
    assert!(stderr(&retry).contains("already exists"));
    assert_eq!(
        stored_workspace_path(&repository_path, &workspace_name).unwrap(),
        dunce::canonicalize(&destination).unwrap()
    );
}

#[tokio::test]
async fn add_refuses_foreign_destination_then_succeeds_after_removal() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "foreign";
    local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("keep"), "untouched").unwrap();

    let refused = add(
        temp.path(),
        &config,
        repository_name,
        "root()",
        &destination,
    );
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("without the matching Devspace ownership marker"));
    assert_eq!(
        fs::read_to_string(destination.join("keep")).unwrap(),
        "untouched"
    );
    fs::remove_dir_all(&destination).unwrap();

    let added = add(
        temp.path(),
        &config,
        repository_name,
        "root()",
        &destination,
    );
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(destination.join(".jj/repo").is_file());
}

#[tokio::test]
async fn add_adopts_registered_workspace_when_destination_is_absent() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "adopt";
    local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");
    let ready = temp.path().join("ready");
    let mut child = spawn_add_at_failpoint(
        temp.path(),
        &config,
        repository_name,
        &destination,
        "after_workspace_registration",
        &ready,
        None,
    );
    wait_for_failpoint(&ready);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!destination.exists());

    let retry = add(
        temp.path(),
        &config,
        repository_name,
        "root()",
        &destination,
    );
    assert!(retry.status.success(), "{}", stderr(&retry));
    assert!(stderr(&retry).contains("Rebuilt workspace"));
    assert!(destination.join(".jj/working_copy/type").is_file());
}

#[tokio::test]
async fn add_rejects_registered_workspace_at_a_different_revision() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "different-revision";
    local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");
    add_json(
        temp.path(),
        &config,
        repository_name,
        "root()",
        &destination,
    );
    let describe = ds(&destination, &config, &["describe", "-m", "base"]);
    assert!(describe.status.success(), "{}", stderr(&describe));
    let new = ds(&destination, &config, &["new", "-m", "working copy"]);
    assert!(new.status.success(), "{}", stderr(&new));
    let matching_parent = commit_id(&destination, &config, "@-");
    fs::remove_dir_all(&destination).unwrap();

    let mismatch = add(
        temp.path(),
        &config,
        repository_name,
        "root()",
        &destination,
    );
    assert_eq!(mismatch.status.code(), Some(1), "{}", stderr(&mismatch));
    assert!(stderr(&mismatch).contains("is registered at working-copy commit"));
    assert!(stderr(&mismatch).contains("pass the matching parent commit"));
    assert!(!destination.exists());

    let matching = add(
        temp.path(),
        &config,
        repository_name,
        &matching_parent,
        &destination,
    );
    assert!(matching.status.success(), "{}", stderr(&matching));
    assert_eq!(commit_id(&destination, &config, "@-"), matching_parent);
}

#[tokio::test]
async fn add_rebuilds_after_completed_checkout_is_removed() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "removed";
    local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");
    let first = add_json(
        temp.path(),
        &config,
        repository_name,
        "root()",
        &destination,
    );
    fs::write(destination.join("untracked"), "discarded").unwrap();
    fs::remove_dir_all(&destination).unwrap();

    let second = add_json(
        temp.path(),
        &config,
        repository_name,
        "root()",
        &destination,
    );
    assert_eq!(first["workspace_id"], second["workspace_id"]);
    assert!(destination.join(".jj/repo").is_file());
    assert!(!destination.join("untracked").exists());
}

#[tokio::test]
async fn add_recovers_after_kill_at_each_live_boundary() {
    for failpoint in [
        "after_workspace_registration",
        "after_checkout_staging",
        "after_final_publication",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let repository_name = "kill-recovery";
        local_repository(temp.path(), repository_name).await;
        let config = write_cli_config(temp.path());
        let destination = temp.path().join("checkout");
        let ready = temp.path().join("ready");
        let mut child = spawn_add_at_failpoint(
            temp.path(),
            &config,
            repository_name,
            &destination,
            failpoint,
            &ready,
            None,
        );
        wait_for_failpoint(&ready);
        child.kill().unwrap();
        child.wait().unwrap();

        if failpoint == "after_checkout_staging" {
            let staging = fs::read_dir(temp.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".devspace-staging-")
                })
                .unwrap();
            fs::write(staging.join("stale"), "must disappear").unwrap();
        }

        let retry = add(
            temp.path(),
            &config,
            repository_name,
            "root()",
            &destination,
        );
        assert!(retry.status.success(), "{failpoint}: {}", stderr(&retry));
        assert!(destination.join(".jj/repo").is_file());
        assert!(!destination.join("stale").exists());
    }
}

#[tokio::test]
async fn concurrent_adds_report_the_live_creator_without_clobbering() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "concurrent";
    local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");
    let ready = temp.path().join("ready");
    let continue_path = temp.path().join("continue");
    let child = spawn_add_at_failpoint(
        temp.path(),
        &config,
        repository_name,
        &destination,
        "after_checkout_staging",
        &ready,
        Some(&continue_path),
    );
    wait_for_failpoint(&ready);

    let concurrent = add(
        temp.path(),
        &config,
        repository_name,
        "root()",
        &destination,
    );
    assert_eq!(concurrent.status.code(), Some(1), "{}", stderr(&concurrent));
    assert!(stderr(&concurrent).contains("already in progress"));
    fs::write(&continue_path, "continue").unwrap();
    let winner = child.wait_with_output().unwrap();
    assert!(winner.status.success(), "{}", stderr(&winner));
    assert!(destination.join(".jj/repo").is_file());
}

#[tokio::test]
async fn add_preserves_revision_resolution_rules() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "revisions";
    local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let source = temp.path().join("source");
    add_json(temp.path(), &config, repository_name, "root()", &source);
    let describe = ds(&source, &config, &["describe", "-m", "base"]);
    assert!(describe.status.success(), "{}", stderr(&describe));
    let bookmark = ds(&source, &config, &["bookmark", "set", "base", "-r", "@"]);
    assert!(bookmark.status.success(), "{}", stderr(&bookmark));
    let base_id = commit_id(&source, &config, "@");
    let new = ds(&source, &config, &["new", "-m", "descendant"]);
    assert!(new.status.success(), "{}", stderr(&new));
    let descendant_id = commit_id(&source, &config, "@");
    let edit = ds(&source, &config, &["edit", "@-"]);
    assert!(edit.status.success(), "{}", stderr(&edit));

    for (label, cwd, revision, expected) in [
        ("plus", source.as_path(), "@+", descendant_id.as_str()),
        ("bookmark", temp.path(), "base", base_id.as_str()),
        ("commit", temp.path(), base_id.as_str(), base_id.as_str()),
        ("at", source.as_path(), "@", base_id.as_str()),
    ] {
        let destination = temp.path().join(label);
        add_json(cwd, &config, repository_name, revision, &destination);
        assert_eq!(commit_id(&destination, &config, "@-"), expected, "{label}");
    }
}
