use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

use devspace_machine::{
    MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineRepository, MachineStore,
    RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName, SharedSecret,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::settings::UserSettings;
use jj_lib::workspace_store::{SimpleWorkspaceStore, WorkspaceStore as _};

mod support_fs;

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

            [snapshot]
            auto-update-stale = true
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

async fn local_repository(root: &Path, name: &str) -> PathBuf {
    configure_machine(root, "http://127.0.0.1:1");
    let entry = machine_store(root)
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
    entry.native_repository_path
}

fn add_checkout(
    cwd: &Path,
    config: &Path,
    repo: &str,
    revision: &str,
    path: &Path,
) -> serde_json::Value {
    let output = ds_command(cwd, config)
        .args(["add", repo, "-r", revision])
        .arg(path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn remove_checkout(cwd: &Path, config: &Path, path: &Path) -> Output {
    ds_command(cwd, config)
        .arg("remove")
        .arg(path)
        .output()
        .unwrap()
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

fn visible_head_ids(cwd: &Path, config: &Path, repo: &str) -> Vec<String> {
    let output = ds(
        cwd,
        config,
        &[
            "-R",
            repo,
            "log",
            "-r",
            "visible_heads()",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output).lines().map(str::to_owned).collect()
}

async fn workspace_names(repository_path: &Path) -> Vec<WorkspaceNameBuf> {
    MachineRepository::open(repository_path, &settings())
        .await
        .unwrap()
        .repo()
        .view()
        .wc_commit_ids()
        .keys()
        .cloned()
        .collect()
}

async fn commit_before_forget(
    repository_path: &Path,
    workspace_name: &WorkspaceName,
) -> jj_lib::backend::CommitId {
    let repository = MachineRepository::open(repository_path, &settings())
        .await
        .unwrap();
    let operation = repository.repo().operation();
    assert!(
        operation
            .store_operation()
            .metadata
            .description
            .starts_with("forget Devspace checkout workspace")
    );
    let parent = operation
        .parents()
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let parent_repo = repository.repo().loader().load_at(&parent).await.unwrap();
    parent_repo
        .view()
        .get_wc_commit_id(workspace_name)
        .unwrap()
        .clone()
}

fn stored_workspace_path(
    repository_path: &Path,
    workspace_name: &WorkspaceName,
) -> Option<PathBuf> {
    SimpleWorkspaceStore::load(repository_path)
        .unwrap()
        .get_workspace_path(workspace_name)
        .unwrap()
}

fn checkout_repository_path(checkout: &Path) -> PathBuf {
    let pointer =
        PathBuf::from(String::from_utf8(fs::read(checkout.join(".jj/repo")).unwrap()).unwrap());
    let pointer = if pointer.is_absolute() {
        pointer
    } else {
        checkout.join(".jj").join(pointer)
    };
    dunce::canonicalize(pointer).unwrap()
}

fn create_server<F>(mut handle: F) -> (String, thread::JoinHandle<String>)
where
    F: FnMut(&str, &mut TcpStream) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        handle(&request, &mut stream);
        request
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

fn respond(stream: &mut TcpStream, body: &str) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

#[tokio::test]
async fn checkpoint_three_checkout_lifecycle_preserves_one_native_repository() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "lifecycle";
    let response = format!(
        r#"{{"name":"{repository_name}","repositoryId":"{}","incarnation":"{}"}}"#,
        "ab".repeat(32),
        "cd".repeat(16)
    );
    let (base_url, server) = create_server(move |request, stream| {
        assert!(request.starts_with("POST /repositories HTTP/1.1"));
        respond(stream, &response);
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());

    let created = ds(temp.path(), &config, &["repo", "new", repository_name]);
    assert!(created.status.success(), "{}", stderr(&created));
    server.join().unwrap();
    let entry = machine_store(temp.path())
        .resolve(&RepositoryName::parse(repository_name).unwrap())
        .unwrap()
        .unwrap();

    let checkout_a = temp.path().join("checkout-a");
    let checkout_b = temp.path().join("checkout-b");
    let added_a = add_checkout(temp.path(), &config, repository_name, "root()", &checkout_a);
    let workspace_a = WorkspaceNameBuf::from(added_a["workspace_id"].as_str().unwrap().to_owned());
    fs::write(checkout_a.join("shared.txt"), "from checkout A\n").unwrap();
    let status_a = ds(&checkout_a, &config, &["status"]);
    assert!(status_a.status.success(), "{}", stderr(&status_a));
    let base_commit = commit_id(&checkout_a, &config, "@");

    let added_b = add_checkout(
        temp.path(),
        &config,
        repository_name,
        &base_commit,
        &checkout_b,
    );
    let workspace_b = WorkspaceNameBuf::from(added_b["workspace_id"].as_str().unwrap().to_owned());
    assert_eq!(machine_store(temp.path()).entries().unwrap().len(), 1);
    assert!(entry.native_repository_path.is_dir());
    let names = workspace_names(&entry.native_repository_path).await;
    assert!(names.contains(&workspace_a));
    assert!(names.contains(&workspace_b));
    for checkout in [&checkout_a, &checkout_b] {
        assert!(checkout.join(".jj/repo").is_file());
        assert_eq!(
            checkout_repository_path(checkout),
            dunce::canonicalize(&entry.native_repository_path).unwrap()
        );
        assert!(!checkout.join(".jj/store").exists());
        assert!(!checkout.join(".jj/op_store").exists());
    }
    for cwd in [&checkout_a, &checkout_b] {
        let log = ds(cwd, &config, &["log", "-r", "all()"]);
        assert!(log.status.success(), "{}", stderr(&log));
    }
    let bare_log = ds(
        temp.path(),
        &config,
        &["-R", repository_name, "log", "-r", "all()"],
    );
    assert!(bare_log.status.success(), "{}", stderr(&bare_log));

    fs::write(checkout_a.join("final.txt"), "preserved at removal\n").unwrap();
    let removed_a = remove_checkout(temp.path(), &config, &checkout_a);
    assert!(removed_a.status.success(), "{}", stderr(&removed_a));
    assert!(!checkout_a.exists());
    let preserved_commit = commit_before_forget(&entry.native_repository_path, &workspace_a).await;
    let preserved_id = preserved_commit.hex();
    let preserved_file = ds(
        &checkout_b,
        &config,
        &["file", "show", "-r", &preserved_id, "final.txt"],
    );
    assert!(
        preserved_file.status.success(),
        "{}",
        stderr(&preserved_file)
    );
    assert_eq!(stdout(&preserved_file), "preserved at removal\n");
    for command in [["status"].as_slice(), ["log", "-r", "all()"].as_slice()] {
        let output = ds(&checkout_b, &config, command);
        assert!(output.status.success(), "{}", stderr(&output));
    }

    let removed_b = remove_checkout(temp.path(), &config, &checkout_b);
    assert!(removed_b.status.success(), "{}", stderr(&removed_b));
    assert!(!checkout_b.exists());
    assert!(entry.native_repository_path.is_dir());
    assert_eq!(machine_store(temp.path()).entries().unwrap().len(), 1);
    let final_log = ds(
        temp.path(),
        &config,
        &["-R", repository_name, "log", "-r", "all()"],
    );
    assert!(final_log.status.success(), "{}", stderr(&final_log));

    let recreated = add_checkout(
        temp.path(),
        &config,
        repository_name,
        &preserved_id,
        &checkout_a,
    );
    assert_eq!(recreated["workspace_id"], added_a["workspace_id"]);
    assert_eq!(
        fs::read_to_string(checkout_a.join("final.txt")).unwrap(),
        "preserved at removal\n"
    );
}

#[tokio::test]
async fn remove_refuses_an_unmarked_directory() {
    let temp = tempfile::tempdir().unwrap();
    local_repository(temp.path(), "unmarked").await;
    let config = write_cli_config(temp.path());
    let path = temp.path().join("checkout");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("keep"), "untouched").unwrap();

    let output = remove_checkout(temp.path(), &config, &path);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("not a Devspace checkout"));
    assert_eq!(fs::read_to_string(path.join("keep")).unwrap(), "untouched");
    assert!(!temp.path().join("machine-store/locks/checkouts").exists());
}

#[tokio::test]
async fn remove_refuses_a_missing_unregistered_path() {
    let temp = tempfile::tempdir().unwrap();
    local_repository(temp.path(), "missing").await;
    let config = write_cli_config(temp.path());
    let path = temp.path().join("missing-checkout");

    let output = remove_checkout(temp.path(), &config, &path);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("not a Devspace checkout"));
    assert!(!temp.path().join("machine-store/locks/checkouts").exists());
}

#[tokio::test]
async fn remove_refuses_a_marker_without_its_catalog_entry() {
    let temp = tempfile::tempdir().unwrap();
    local_repository(temp.path(), "missing-catalog").await;
    let config = write_cli_config(temp.path());
    let path = temp.path().join("checkout");
    add_checkout(temp.path(), &config, "missing-catalog", "root()", &path);

    let empty = tempfile::tempdir().unwrap();
    configure_machine(empty.path(), "http://127.0.0.1:1");
    let empty_config = write_cli_config(empty.path());
    let output = ds_command(temp.path(), &empty_config)
        .arg("remove")
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("not in this machine store"));
    assert!(path.join(".jj/devspace-checkout-owner").is_file());
}

#[tokio::test]
async fn remove_points_an_incomplete_clone_back_to_add() {
    let temp = tempfile::tempdir().unwrap();
    let repository_path = local_repository(temp.path(), "incomplete-remove").await;
    let config = write_cli_config(temp.path());
    let path = temp.path().join("checkout");
    add_checkout(temp.path(), &config, "incomplete-remove", "root()", &path);
    fs::remove_dir_all(repository_path).unwrap();

    let output = remove_checkout(temp.path(), &config, &path);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("has an incomplete clone; run `ds add` again to finish it"),
        "{}",
        stderr(&output)
    );
    assert!(path.join(".jj/devspace-checkout-owner").is_file());
}

#[tokio::test]
async fn remove_snapshots_uncommitted_edits_before_forgetting() {
    let temp = tempfile::tempdir().unwrap();
    let repository_path = local_repository(temp.path(), "snapshot").await;
    let config = write_cli_config(temp.path());
    let path = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "snapshot", "root()", &path);
    let workspace = WorkspaceNameBuf::from(added["workspace_id"].as_str().unwrap().to_owned());
    fs::write(path.join("unsnapshotted.txt"), "survives\n").unwrap();

    let output = remove_checkout(temp.path(), &config, &path);
    assert!(output.status.success(), "{}", stderr(&output));
    let commit = commit_before_forget(&repository_path, &workspace).await;
    assert!(
        visible_head_ids(temp.path(), &config, "snapshot").contains(&commit.hex()),
        "edited working-copy commit should remain visible after removal"
    );
    let rebuilt = temp.path().join("rebuilt");
    add_checkout(temp.path(), &config, "snapshot", &commit.hex(), &rebuilt);
    assert_eq!(
        fs::read_to_string(rebuilt.join("unsnapshotted.txt")).unwrap(),
        "survives\n"
    );
}

#[tokio::test]
async fn remove_discards_an_unedited_working_copy_head() {
    let temp = tempfile::tempdir().unwrap();
    local_repository(temp.path(), "unedited").await;
    let config = write_cli_config(temp.path());
    let heads_before_add = visible_head_ids(temp.path(), &config, "unedited");
    let path = temp.path().join("checkout");
    add_checkout(temp.path(), &config, "unedited", "root()", &path);

    let output = remove_checkout(temp.path(), &config, &path);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        visible_head_ids(temp.path(), &config, "unedited"),
        heads_before_add
    );
}

#[tokio::test]
async fn remove_json_reports_the_removed_checkout_identity() {
    let temp = tempfile::tempdir().unwrap();
    local_repository(temp.path(), "json").await;
    let config = write_cli_config(temp.path());
    let path = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "json", "root()", &path);

    let output = ds_command(temp.path(), &config)
        .arg("remove")
        .arg(&path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let removed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(removed["root"], added["root"]);
    assert_eq!(removed["repo"], "json");
    assert_eq!(removed["workspace_id"], added["workspace_id"]);
    assert!(!path.exists());
}

#[tokio::test]
async fn remove_finishes_when_the_workspace_was_already_forgotten() {
    let temp = tempfile::tempdir().unwrap();
    let repository_path = local_repository(temp.path(), "forgotten").await;
    let config = write_cli_config(temp.path());
    let path = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "forgotten", "root()", &path);
    let workspace = WorkspaceNameBuf::from(added["workspace_id"].as_str().unwrap().to_owned());
    let repository = MachineRepository::open(&repository_path, &settings())
        .await
        .unwrap();
    let mut transaction = repository.repo().start_transaction();
    transaction
        .repo_mut()
        .remove_wc_commit(&workspace)
        .await
        .unwrap();
    transaction.repo_mut().rebase_descendants().await.unwrap();
    transaction
        .commit("simulated interrupted remove")
        .await
        .unwrap();
    assert!(stored_workspace_path(&repository_path, &workspace).is_some());

    let output = remove_checkout(temp.path(), &config, &path);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!path.exists());
    assert!(stored_workspace_path(&repository_path, &workspace).is_none());
}

#[tokio::test]
async fn remove_forgets_a_workspace_when_its_directory_was_already_gone() {
    let temp = tempfile::tempdir().unwrap();
    let repository_path = local_repository(temp.path(), "missing-directory").await;
    let config = write_cli_config(temp.path());
    let path = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "missing-directory", "root()", &path);
    let workspace = WorkspaceNameBuf::from(added["workspace_id"].as_str().unwrap().to_owned());
    support_fs::remove_dir_all(&path);

    let output = remove_checkout(temp.path(), &config, &path);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("was already gone"));
    assert!(!workspace_names(&repository_path).await.contains(&workspace));
    assert!(stored_workspace_path(&repository_path, &workspace).is_none());
}

#[tokio::test]
async fn remove_rejects_unsupported_global_options() {
    let temp = tempfile::tempdir().unwrap();
    local_repository(temp.path(), "options").await;
    let config = write_cli_config(temp.path());
    let path = temp.path().join("checkout");
    add_checkout(temp.path(), &config, "options", "root()", &path);

    for option in [
        "--no-integrate-operation",
        "--ignore-working-copy",
        "--at-operation=@",
    ] {
        let output = ds_command(temp.path(), &config)
            .arg(option)
            .arg("remove")
            .arg(&path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{option}");
        assert!(
            stderr(&output).contains("does not support"),
            "{option}: {}",
            stderr(&output)
        );
        assert!(path.exists(), "{option}");
    }
}
