use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;

use devspace_machine::{
    CheckoutCreationPhase, MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineRepository,
    MachineStore, RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName,
    SharedSecret,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::default_index::DefaultIndexStore;
use jj_lib::default_submodule_store::DefaultSubmoduleStore;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::Repo as _;
use jj_lib::settings::UserSettings;
use jj_lib::simple_backend::SimpleBackend;
use jj_lib::simple_op_heads_store::SimpleOpHeadsStore;
use jj_lib::simple_op_store::SimpleOpStore;
use jj_lib::workspace_store::{SimpleWorkspaceStore, WorkspaceStore as _};

const DEVELOPMENT_SECRET: &str = "cli-development-secret";

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

fn write_unknown_signing_config(root: &Path) -> PathBuf {
    let path = root.join("jj-config.toml");
    fs::write(
        &path,
        r#"
            [user]
            name = "Devspace Test"
            email = "devspace@example.invalid"

            [signing]
            backend = "missing"

            [ui]
            color = "never"
        "#,
    )
    .unwrap();
    path
}

fn ds(cwd: &Path, config: &Path, args: &[&str]) -> Output {
    let mut command = ds_command(cwd, config);
    command.args(args).output().unwrap()
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
                MachineId::parse("12".repeat(16)).unwrap(),
                SharedSecret::new(DEVELOPMENT_SECRET).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
}

async fn local_repository(root: &Path, repository_name: &str) -> PathBuf {
    configure_machine(root, "http://127.0.0.1:1");
    let entry = machine_store(root)
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

fn wait_for_failpoint(ready: &Path) {
    for _ in 0..1_000 {
        if ready.exists() {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("checkout failpoint was not reached");
}

fn create_server<F>(
    request_count: usize,
    mut handle: F,
) -> (String, thread::JoinHandle<Vec<String>>)
where
    F: FnMut(usize, &str, &mut TcpStream) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..request_count {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            handle(index, &request, &mut stream);
            requests.push(request);
        }
        requests
    });
    (address, server)
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
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
    String::from_utf8(bytes[..expected_length].to_vec()).unwrap()
}

fn request_json(request: &str) -> serde_json::Value {
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    serde_json::from_str(body).unwrap()
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

fn repository_response(name: &str) -> String {
    format!(
        r#"{{"name":"{name}","repositoryId":"{}","incarnation":"{}"}}"#,
        "ab".repeat(32),
        "cd".repeat(16)
    )
}

fn checkout_repository_path(checkout: &Path) -> PathBuf {
    let pointer = fs::read(checkout.join(".jj/repo")).unwrap();
    let pointer = PathBuf::from(String::from_utf8(pointer).unwrap());
    let pointer = if pointer.is_absolute() {
        pointer
    } else {
        checkout.join(".jj").join(pointer)
    };
    dunce::canonicalize(pointer).unwrap()
}

fn stored_workspace_path(store: &SimpleWorkspaceStore, repo: &Path, name: &str) -> PathBuf {
    let path = store
        .get_workspace_path(WorkspaceName::new(name))
        .unwrap()
        .unwrap();
    let path = if path.is_absolute() {
        path
    } else {
        repo.join(path)
    };
    dunce::canonicalize(path).unwrap()
}

fn add_checkout(cwd: &Path, config: &Path, repo: &str, path: &Path) -> serde_json::Value {
    add_checkout_at_revision(cwd, config, repo, "root()", path)
}

fn add_checkout_at_revision(
    cwd: &Path,
    config: &Path,
    repo: &str,
    revision: &str,
    path: &Path,
) -> serde_json::Value {
    let output = ds(
        cwd,
        config,
        &[
            "add",
            repo,
            "-r",
            revision,
            path.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn repo_new_replays_a_lost_response_with_the_durable_request_key() {
    let temp = tempfile::tempdir().unwrap();
    let response = repository_response("retry-safe");
    let store_root = temp.path().join("machine-store");
    let (base_url, server) = create_server(2, move |index, request, stream| {
        let persisted = MachineStore::new(&store_root)
            .repository_creation_intent(&RepositoryName::parse("retry-safe").unwrap())
            .unwrap()
            .expect("intent must reach durable storage before the HTTP request");
        assert_eq!(
            request_json(request)["idempotencyKey"],
            persisted
                .key()
                .bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        if index == 1 {
            respond(stream, "200 OK", &response);
        }
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());

    let first = ds(temp.path(), &config, &["repo", "new", "retry-safe"]);
    assert_eq!(first.status.code(), Some(1), "{}", stderr(&first));
    assert!(
        stderr(&first).contains("cloud directory request failed"),
        "{}",
        stderr(&first)
    );
    assert!(!stderr(&first).contains(DEVELOPMENT_SECRET));
    assert!(!stderr(&first).contains(&base_url));
    let pending = machine_store(temp.path())
        .repository_creation_intent(&RepositoryName::parse("retry-safe").unwrap())
        .unwrap()
        .unwrap();
    assert!(pending.identity().is_none());
    assert!(
        machine_store(temp.path())
            .resolve(&RepositoryName::parse("retry-safe").unwrap())
            .unwrap()
            .is_none()
    );

    let second = ds(temp.path(), &config, &["repo", "new", "retry-safe"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(stdout(&second), "");
    assert!(stderr(&second).contains("Created repository `retry-safe`."));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.starts_with("POST /repositories HTTP/1.1"))
    );
    assert_eq!(
        request_json(&requests[0])["idempotencyKey"],
        request_json(&requests[1])["idempotencyKey"]
    );

    let store = machine_store(temp.path());
    let name = RepositoryName::parse("retry-safe").unwrap();
    let completed = store.repository_creation_intent(&name).unwrap().unwrap();
    assert!(completed.is_complete());
    let entry = store.resolve(&name).unwrap().unwrap();
    let repository = MachineRepository::open(&entry.native_repository_path, &settings())
        .await
        .unwrap();
    assert_eq!(repository.repo().view().heads().len(), 1);
    assert!(
        repository
            .repo()
            .view()
            .heads()
            .contains(repository.repo().store().root_commit_id())
    );
    assert!(!entry.native_repository_path.join(".jj").exists());
    for (directory, expected) in [
        ("store", SimpleBackend::name()),
        ("op_store", SimpleOpStore::name()),
        ("op_heads", SimpleOpHeadsStore::name()),
        ("index", DefaultIndexStore::name()),
        ("submodule_store", DefaultSubmoduleStore::name()),
    ] {
        assert_eq!(
            fs::read_to_string(entry.native_repository_path.join(directory).join("type")).unwrap(),
            expected
        );
    }

    let after_completion = ds(temp.path(), &config, &["repo", "new", "retry-safe"]);
    assert!(
        after_completion.status.success(),
        "{}",
        stderr(&after_completion)
    );
}

#[tokio::test]
async fn repo_new_attaches_two_independent_checkouts_to_one_machine_repository() {
    let temp = tempfile::tempdir().unwrap();
    let response = repository_response("shared-checkouts");
    let (base_url, server) = create_server(1, move |_, _, stream| {
        respond(stream, "200 OK", &response);
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());

    let create = ds(temp.path(), &config, &["repo", "new", "shared-checkouts"]);
    server.join().unwrap();
    assert!(create.status.success(), "{}", stderr(&create));

    let first_path = temp.path().join("first");
    let second_path = temp.path().join("second");
    let first = add_checkout(temp.path(), &config, "shared-checkouts", &first_path);
    let second =
        add_checkout_at_revision(&first_path, &config, "shared-checkouts", "@-", &second_path);
    let first_workspace = first["workspace_id"].as_str().unwrap();
    let second_workspace = second["workspace_id"].as_str().unwrap();
    assert_ne!(first_workspace, second_workspace);
    assert!(first_workspace.starts_with(&"12".repeat(16)));
    assert!(second_workspace.starts_with(&"12".repeat(16)));

    let store = machine_store(temp.path());
    let entry = store
        .resolve(&RepositoryName::parse("shared-checkouts").unwrap())
        .unwrap()
        .unwrap();
    let native_path = dunce::canonicalize(&entry.native_repository_path).unwrap();
    assert_eq!(checkout_repository_path(&first_path), native_path);
    assert_eq!(checkout_repository_path(&second_path), native_path);
    assert!(first_path.join(".jj/repo").is_file());
    assert!(second_path.join(".jj/repo").is_file());
    assert!(!first_path.join(".jj/repo/store").exists());
    assert!(!second_path.join(".jj/repo/store").exists());

    let workspace_store = SimpleWorkspaceStore::load(&entry.native_repository_path).unwrap();
    assert_eq!(
        stored_workspace_path(&workspace_store, &native_path, first_workspace),
        dunce::canonicalize(&first_path).unwrap()
    );
    assert_eq!(
        stored_workspace_path(&workspace_store, &native_path, second_workspace),
        dunce::canonicalize(&second_path).unwrap()
    );

    let first_at = ds(
        &first_path,
        &config,
        &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
    );
    let second_at = ds(
        &second_path,
        &config,
        &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
    );
    assert!(first_at.status.success(), "{}", stderr(&first_at));
    assert!(second_at.status.success(), "{}", stderr(&second_at));
    assert_ne!(stdout(&first_at), stdout(&second_at));

    fs::write(first_path.join("shared.txt"), "from the first checkout\n").unwrap();
    let status = ds(&first_path, &config, &["status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(
        stdout(&status).contains("A shared.txt"),
        "{}",
        stdout(&status)
    );
    assert!(!stdout(&status).contains("devspace-checkout-owner"));
    assert!(!first_path.join(".devspace-checkout-owner").exists());

    let first_revision = format!("{first_workspace}@");
    let observed = ds(
        &second_path,
        &config,
        &["file", "show", "-r", &first_revision, "shared.txt"],
    );
    assert!(observed.status.success(), "{}", stderr(&observed));
    assert_eq!(stdout(&observed), "from the first checkout\n");
    let bare_log = ds(
        temp.path(),
        &config,
        &[
            "-R",
            "shared-checkouts",
            "log",
            "-r",
            &first_revision,
            "--no-graph",
            "-T",
            "commit_id",
        ],
    );
    assert!(bare_log.status.success(), "{}", stderr(&bare_log));

    let operation_before_collisions =
        MachineRepository::open(&entry.native_repository_path, &settings())
            .await
            .unwrap()
            .repo()
            .op_id()
            .clone();
    let occupied_file = temp.path().join("occupied-file");
    fs::write(&occupied_file, "keep me").unwrap();
    let file_collision = ds(
        temp.path(),
        &config,
        &[
            "add",
            "shared-checkouts",
            "-r",
            "root()",
            occupied_file.to_str().unwrap(),
        ],
    );
    assert_eq!(
        file_collision.status.code(),
        Some(1),
        "{}",
        stderr(&file_collision)
    );
    assert_eq!(fs::read_to_string(&occupied_file).unwrap(), "keep me");

    let occupied_directory = temp.path().join("occupied-directory");
    fs::create_dir(&occupied_directory).unwrap();
    let directory_collision = ds(
        temp.path(),
        &config,
        &[
            "add",
            "shared-checkouts",
            "-r",
            "root()",
            occupied_directory.to_str().unwrap(),
        ],
    );
    assert_eq!(directory_collision.status.code(), Some(1));
    assert_eq!(fs::read_dir(&occupied_directory).unwrap().count(), 0);

    let invalid_path = temp.path().join("invalid-revision");
    let invalid_revision = ds(
        temp.path(),
        &config,
        &[
            "add",
            "shared-checkouts",
            "-r",
            "no-such-revision",
            invalid_path.to_str().unwrap(),
        ],
    );
    assert_eq!(invalid_revision.status.code(), Some(1));
    assert!(!invalid_path.exists());

    let unavailable_at_path = temp.path().join("unavailable-at");
    let unavailable_at = ds(
        temp.path(),
        &config,
        &[
            "add",
            "shared-checkouts",
            "-r",
            "@",
            unavailable_at_path.to_str().unwrap(),
        ],
    );
    assert_eq!(unavailable_at.status.code(), Some(1));
    assert!(!unavailable_at_path.exists());
    let operation_after_collisions =
        MachineRepository::open(&entry.native_repository_path, &settings())
            .await
            .unwrap()
            .repo()
            .op_id()
            .clone();
    assert_eq!(operation_after_collisions, operation_before_collisions);

    fs::remove_dir_all(&first_path).unwrap();
    assert!(entry.native_repository_path.exists());
    let surviving = ds(
        &second_path,
        &config,
        &[
            "log",
            "-r",
            &first_revision,
            "--no-graph",
            "-T",
            "commit_id",
        ],
    );
    assert!(surviving.status.success(), "{}", stderr(&surviving));
}

#[tokio::test]
async fn add_uses_non_root_bookmark_commit_id_and_exact_at_as_the_durable_base() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "non-root-revisions";
    local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let source = temp.path().join("source");
    add_checkout(temp.path(), &config, repository_name, &source);
    let describe = ds(&source, &config, &["describe", "-m", "non-root base"]);
    assert!(describe.status.success(), "{}", stderr(&describe));
    let bookmark = ds(&source, &config, &["bookmark", "set", "base", "-r", "@"]);
    assert!(bookmark.status.success(), "{}", stderr(&bookmark));
    let base_id = commit_id(&source, &config, "@");
    let new = ds(&source, &config, &["new", "-m", "descendant"]);
    assert!(new.status.success(), "{}", stderr(&new));
    let descendant_id = commit_id(&source, &config, "@");
    let edit = ds(&source, &config, &["edit", "@-"]);
    assert!(edit.status.success(), "{}", stderr(&edit));
    assert_eq!(commit_id(&source, &config, "@"), base_id);
    let plus = temp.path().join("plus");
    add_checkout_at_revision(&source, &config, repository_name, "@+", &plus);
    assert_eq!(commit_id(&plus, &config, "@-"), descendant_id);

    let root_id = commit_id(&source, &config, "root()");
    for (label, revision) in [("root", "root()"), ("parent", "@-")] {
        let destination = temp.path().join(label);
        add_checkout_at_revision(&source, &config, repository_name, revision, &destination);
        assert_eq!(commit_id(&destination, &config, "@-"), root_id, "{label}");
    }

    for (label, cwd, revision) in [
        ("bookmark", temp.path(), "base"),
        ("commit", temp.path(), base_id.as_str()),
        ("at", source.as_path(), "@"),
    ] {
        let destination = temp.path().join(label);
        add_checkout_at_revision(cwd, &config, repository_name, revision, &destination);
        assert_eq!(commit_id(&destination, &config, "@-"), base_id, "{label}");
    }
}

#[tokio::test]
async fn repo_new_recovers_locally_after_cloud_success_precedes_catalog_write() {
    let temp = tempfile::tempdir().unwrap();
    let catalog_path = machine_store(temp.path()).catalog_path();
    let response = repository_response("catalog-retry");
    let (base_url, server) = create_server(1, move |_, _, stream| {
        fs::create_dir(&catalog_path).unwrap();
        respond(stream, "200 OK", &response);
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());

    let first = ds(temp.path(), &config, &["repo", "new", "catalog-retry"]);
    server.join().unwrap();
    assert_eq!(first.status.code(), Some(1), "{}", stderr(&first));
    let store = machine_store(temp.path());
    let name = RepositoryName::parse("catalog-retry").unwrap();
    let intent = store.repository_creation_intent(&name).unwrap().unwrap();
    assert!(intent.identity().is_some());
    assert!(!intent.is_complete());
    fs::remove_dir(store.catalog_path()).unwrap();

    let second = ds(temp.path(), &config, &["repo", "new", "catalog-retry"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(store.resolve(&name).unwrap().is_some());
    assert!(
        store
            .repository_creation_intent(&name)
            .unwrap()
            .unwrap()
            .is_complete()
    );
}

#[test]
fn repo_new_does_not_restore_a_removed_completed_catalog_binding() {
    let temp = tempfile::tempdir().unwrap();
    let response = repository_response("removed-binding");
    let (base_url, server) = create_server(1, move |_, _, stream| {
        respond(stream, "200 OK", &response);
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());

    let first = ds(temp.path(), &config, &["repo", "new", "removed-binding"]);
    server.join().unwrap();
    assert!(first.status.success(), "{}", stderr(&first));

    let store = machine_store(temp.path());
    let name = RepositoryName::parse("removed-binding").unwrap();
    let entry = store.resolve(&name).unwrap().unwrap();
    store
        .unregister_repository(&name, &entry.identity)
        .unwrap()
        .unwrap();

    let retry = ds(temp.path(), &config, &["repo", "new", "removed-binding"]);
    assert_eq!(retry.status.code(), Some(1), "{}", stderr(&retry));
    assert!(
        stderr(&retry).contains("recorded local catalog binding is missing or changed"),
        "{}",
        stderr(&retry)
    );
    assert!(store.resolve(&name).unwrap().is_none());
}

#[tokio::test]
async fn repo_new_recovers_after_catalog_registration_precedes_materialization() {
    let temp = tempfile::tempdir().unwrap();
    let response = repository_response("materialization-retry");
    let (base_url, server) = create_server(1, move |_, _, stream| {
        respond(stream, "200 OK", &response);
    });
    configure_machine(temp.path(), &base_url);
    let bad_config = write_unknown_signing_config(temp.path());

    let first = ds(
        temp.path(),
        &bad_config,
        &["repo", "new", "materialization-retry"],
    );
    server.join().unwrap();
    assert_eq!(first.status.code(), Some(1), "{}", stderr(&first));
    let store = machine_store(temp.path());
    let name = RepositoryName::parse("materialization-retry").unwrap();
    let entry = store.resolve(&name).unwrap().unwrap();
    assert!(!entry.native_repository_path.exists());
    assert!(
        store
            .repository_creation_intent(&name)
            .unwrap()
            .unwrap()
            .identity()
            .is_some()
    );

    let good_config = write_cli_config(temp.path());
    let second = ds(
        temp.path(),
        &good_config,
        &["repo", "new", "materialization-retry"],
    );
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(entry.native_repository_path.exists());
    assert!(
        store
            .repository_creation_intent(&name)
            .unwrap()
            .unwrap()
            .is_complete()
    );
}

#[test]
fn repo_new_does_not_adopt_a_same_name_repository_from_another_request() {
    let temp = tempfile::tempdir().unwrap();
    let (base_url, server) = create_server(1, |_, _, stream| {
        respond(
            stream,
            "409 Conflict",
            r#"{"error":"repository name is already in use"}"#,
        );
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["repo", "new", "occupied"]);
    let requests = server.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("repository name is already in use"));
    assert!(!stderr(&output).contains(DEVELOPMENT_SECRET));
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /repositories HTTP/1.1"));
    let store = machine_store(temp.path());
    let name = RepositoryName::parse("occupied").unwrap();
    assert!(store.resolve(&name).unwrap().is_none());
    let intent = store.repository_creation_intent(&name).unwrap().unwrap();
    assert!(intent.identity().is_none());
}

#[tokio::test]
async fn add_rejects_unsupported_global_modifiers_before_checkout_effects() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "modifier-repository";
    let repository_path = local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let operation_before = MachineRepository::open(&repository_path, &settings())
        .await
        .unwrap()
        .repo()
        .op_id()
        .clone();

    for (index, option) in [
        "--no-integrate-operation",
        "--ignore-working-copy",
        "--at-operation=@",
    ]
    .into_iter()
    .enumerate()
    {
        let destination = temp.path().join(format!("unsupported-{index}"));
        let output = ds(
            temp.path(),
            &config,
            &[
                option,
                "add",
                repository_name,
                "-r",
                "root()",
                destination.to_str().unwrap(),
            ],
        );
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(stderr(&output).contains(option.trim_end_matches("=@")));
        assert!(!destination.exists());
        assert!(
            !temp
                .path()
                .join("machine-store/checkout-creations.json")
                .exists()
        );
    }

    let operation_after = MachineRepository::open(&repository_path, &settings())
        .await
        .unwrap()
        .repo()
        .op_id()
        .clone();
    assert_eq!(operation_after, operation_before);
}

#[cfg(unix)]
#[tokio::test]
async fn add_json_rejects_non_utf8_path_before_checkout_effects() {
    use std::os::unix::ffi::OsStringExt as _;

    let temp = tempfile::tempdir().unwrap();
    let repository_name = "json-path-repository";
    let repository_path = local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let operation_before = MachineRepository::open(&repository_path, &settings())
        .await
        .unwrap()
        .repo()
        .op_id()
        .clone();
    let destination = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"checkout-\xff".to_vec()));

    let output = ds_command(temp.path(), &config)
        .args(["add", repository_name, "-r", "root()"])
        .arg(&destination)
        .arg("--json")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("Non-UTF-8 argument"));
    assert!(!destination.exists());
    assert!(
        !temp
            .path()
            .join("machine-store/checkout-creations.json")
            .exists()
    );
    let operation_after = MachineRepository::open(&repository_path, &settings())
        .await
        .unwrap()
        .repo()
        .op_id()
        .clone();
    assert_eq!(operation_after, operation_before);
}

#[tokio::test]
async fn add_resumes_after_process_kill_at_each_durable_boundary() {
    for failpoint in [
        "after_staging_reservation_directory_created",
        "before_workspace_registration",
        "after_working_copy_state_directory_created",
        "after_workspace_registration",
        "after_checkout_staging",
        "after_final_publication",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let repository_name = "kill-recovery";
        local_repository(temp.path(), repository_name).await;
        let config = write_cli_config(temp.path());
        let destination = temp.path().join("checkout");
        let ready = temp.path().join("failpoint-ready");
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

        let pending = machine_store(temp.path())
            .checkout_creation_intent(&destination)
            .unwrap()
            .expect("the intent precedes every checkout side effect");
        assert!(pending.phase() < CheckoutCreationPhase::Complete);
        let staging = destination.parent().unwrap().join(pending.staging_name());
        if failpoint == "after_staging_reservation_directory_created" {
            assert!(!staging.exists());
        }
        if failpoint == "after_working_copy_state_directory_created" {
            assert!(!staging.join(".jj/working_copy").exists());
        }

        let retry = ds(
            temp.path(),
            &config,
            &[
                "add",
                repository_name,
                "-r",
                "root()",
                destination.to_str().unwrap(),
                "--json",
            ],
        );
        assert!(retry.status.success(), "{failpoint}: {}", stderr(&retry));
        assert!(destination.join(".jj/repo").is_file());
        assert_eq!(
            machine_store(temp.path())
                .checkout_creation_intent(&destination)
                .unwrap()
                .unwrap()
                .phase(),
            CheckoutCreationPhase::Complete
        );
    }
}

#[tokio::test]
async fn add_resumes_after_errors_inside_compound_directory_initialization() {
    for errorpoint in [
        "after_staging_reservation_directory_created",
        "after_working_copy_state_directory_created",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let repository_name = "error-recovery";
        local_repository(temp.path(), repository_name).await;
        let config = write_cli_config(temp.path());
        let destination = temp.path().join("checkout");
        let first = ds_command(temp.path(), &config)
            .env("DEVSPACE_TEST_CHECKOUT_ERRORPOINT", errorpoint)
            .args(["add", repository_name, "-r", "root()"])
            .arg(&destination)
            .arg("--json")
            .output()
            .unwrap();
        assert_eq!(first.status.code(), Some(1), "{}", stderr(&first));
        assert!(stderr(&first).contains("injected checkout failure"));

        let retry = ds(
            temp.path(),
            &config,
            &[
                "add",
                repository_name,
                "-r",
                "root()",
                destination.to_str().unwrap(),
                "--json",
            ],
        );
        assert!(retry.status.success(), "{errorpoint}: {}", stderr(&retry));
        assert!(destination.join(".jj/working_copy/type").is_file());
    }
}

#[tokio::test]
async fn add_retry_rejects_a_changed_revision_expression_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "revision-binding";
    let repository_path = local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");
    let ready = temp.path().join("ready");
    let mut child = spawn_add_at_failpoint(
        temp.path(),
        &config,
        repository_name,
        &destination,
        "after_checkout_staging",
        &ready,
        None,
    );
    wait_for_failpoint(&ready);
    child.kill().unwrap();
    child.wait().unwrap();
    let before_operation = MachineRepository::open(&repository_path, &settings())
        .await
        .unwrap()
        .repo()
        .op_id()
        .clone();
    let before_phase = machine_store(temp.path())
        .checkout_creation_intent(&destination)
        .unwrap()
        .unwrap()
        .phase();
    let before_workspace_store = fs::read(repository_path.join("workspace_store/index")).unwrap();

    let retry = ds(
        temp.path(),
        &config,
        &[
            "add",
            repository_name,
            "-r",
            "root() | root()",
            destination.to_str().unwrap(),
        ],
    );
    assert_eq!(retry.status.code(), Some(1), "{}", stderr(&retry));
    assert!(stderr(&retry).contains("different repository or revision intent"));
    assert!(!destination.exists());
    assert_eq!(
        machine_store(temp.path())
            .checkout_creation_intent(&destination)
            .unwrap()
            .unwrap()
            .phase(),
        before_phase
    );
    assert_eq!(
        MachineRepository::open(&repository_path, &settings())
            .await
            .unwrap()
            .repo()
            .op_id(),
        &before_operation
    );
    assert_eq!(
        fs::read(repository_path.join("workspace_store/index")).unwrap(),
        before_workspace_store
    );
}

#[tokio::test]
async fn add_retry_rejects_a_workspace_moved_to_a_same_base_sibling() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "moved-workspace";
    local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");
    let ready = temp.path().join("ready");
    let mut child = spawn_add_at_failpoint(
        temp.path(),
        &config,
        repository_name,
        &destination,
        "after_checkout_staging",
        &ready,
        None,
    );
    wait_for_failpoint(&ready);
    child.kill().unwrap();
    child.wait().unwrap();
    let pending = machine_store(temp.path())
        .checkout_creation_intent(&destination)
        .unwrap()
        .unwrap();
    let staging = destination.parent().unwrap().join(pending.staging_name());
    let move_workspace = ds(
        &staging,
        &config,
        &["new", "root()", "-m", "foreign sibling"],
    );
    assert!(
        move_workspace.status.success(),
        "{}",
        stderr(&move_workspace)
    );

    let retry = ds(
        temp.path(),
        &config,
        &[
            "add",
            repository_name,
            "-r",
            "root()",
            destination.to_str().unwrap(),
        ],
    );
    assert_eq!(retry.status.code(), Some(1), "{}", stderr(&retry));
    assert!(stderr(&retry).contains("moved while checkout creation was pending"));
    assert!(!destination.exists());
    assert_eq!(
        machine_store(temp.path())
            .checkout_creation_intent(&destination)
            .unwrap()
            .unwrap()
            .phase(),
        CheckoutCreationPhase::Staged
    );
}

#[cfg(unix)]
#[tokio::test]
async fn add_retry_never_adopts_internal_ownership_symlinks() {
    for symlink_component in ["jj", "marker"] {
        let temp = tempfile::tempdir().unwrap();
        let repository_name = "internal-symlink";
        let repository_path = local_repository(temp.path(), repository_name).await;
        let config = write_cli_config(temp.path());
        let destination = temp.path().join("checkout");
        let ready = temp.path().join("ready");
        let mut child = spawn_add_at_failpoint(
            temp.path(),
            &config,
            repository_name,
            &destination,
            "after_checkout_staging",
            &ready,
            None,
        );
        wait_for_failpoint(&ready);
        child.kill().unwrap();
        child.wait().unwrap();
        let pending = machine_store(temp.path())
            .checkout_creation_intent(&destination)
            .unwrap()
            .unwrap();
        let staging = destination.parent().unwrap().join(pending.staging_name());
        let workspace_store = SimpleWorkspaceStore::load(&repository_path).unwrap();
        let recorded_before =
            stored_workspace_path(&workspace_store, &repository_path, pending.workspace_id());
        fs::create_dir(&destination).unwrap();
        if symlink_component == "jj" {
            std::os::unix::fs::symlink(staging.join(".jj"), destination.join(".jj")).unwrap();
        } else {
            fs::create_dir(destination.join(".jj")).unwrap();
            std::os::unix::fs::symlink(
                staging.join(".jj/devspace-checkout-owner"),
                destination.join(".jj/devspace-checkout-owner"),
            )
            .unwrap();
        }

        let retry = ds(
            temp.path(),
            &config,
            &[
                "add",
                repository_name,
                "-r",
                "root()",
                destination.to_str().unwrap(),
            ],
        );
        assert_eq!(retry.status.code(), Some(1), "{}", stderr(&retry));
        assert!(stderr(&retry).contains("was not adopted or modified"));
        let pending_after = machine_store(temp.path())
            .checkout_creation_intent(&destination)
            .unwrap()
            .unwrap();
        assert_eq!(pending_after.phase(), CheckoutCreationPhase::Staged);
        assert_eq!(
            stored_workspace_path(&workspace_store, &repository_path, pending.workspace_id(),),
            recorded_before
        );
    }
}

#[tokio::test]
async fn add_noclobber_publication_preserves_inserted_destination_entries() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "destination-race";
    local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");
    let ready = temp.path().join("rename-ready");
    let continue_path = temp.path().join("rename-continue");
    let child = spawn_add_at_failpoint(
        temp.path(),
        &config,
        repository_name,
        &destination,
        "before_directory_rename",
        &ready,
        Some(&continue_path),
    );
    wait_for_failpoint(&ready);
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("inserted"), "keep me").unwrap();
    fs::write(&continue_path, "continue").unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(destination.join("inserted")).unwrap(),
        "keep me"
    );
    assert!(!destination.join(".jj").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn add_noclobber_publication_preserves_replacement_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "symlink-race";
    local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let destination = temp.path().join("checkout");
    let target = temp.path().join("replacement-target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("sentinel"), "keep me").unwrap();
    let ready = temp.path().join("rename-ready");
    let continue_path = temp.path().join("rename-continue");
    let child = spawn_add_at_failpoint(
        temp.path(),
        &config,
        repository_name,
        &destination,
        "before_directory_rename",
        &ready,
        Some(&continue_path),
    );
    wait_for_failpoint(&ready);
    std::os::unix::fs::symlink(&target, &destination).unwrap();
    fs::write(&continue_path, "continue").unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(target.join("sentinel")).unwrap(),
        "keep me"
    );
    assert!(!target.join(".jj").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn add_parent_replacement_cannot_redirect_publication_into_replacement_data() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "parent-race";
    local_repository(temp.path(), repository_name).await;
    let config = write_cli_config(temp.path());
    let parent = temp.path().join("checkout-parent");
    let moved_parent = temp.path().join("moved-checkout-parent");
    fs::create_dir(&parent).unwrap();
    let destination = parent.join("checkout");
    let ready = temp.path().join("rename-ready");
    let continue_path = temp.path().join("rename-continue");
    let child = spawn_add_at_failpoint(
        temp.path(),
        &config,
        repository_name,
        &destination,
        "before_directory_rename",
        &ready,
        Some(&continue_path),
    );
    wait_for_failpoint(&ready);
    fs::rename(&parent, &moved_parent).unwrap();
    fs::create_dir(&parent).unwrap();
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("replacement"), "keep me").unwrap();
    fs::write(&continue_path, "continue").unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(destination.join("replacement")).unwrap(),
        "keep me"
    );
    assert!(!destination.join(".jj").exists());
    assert!(moved_parent.join("checkout/.jj/repo").is_file());
    assert_eq!(
        machine_store(temp.path())
            .checkout_creation_intent(&destination)
            .unwrap()
            .unwrap()
            .phase(),
        CheckoutCreationPhase::Staged
    );
}
