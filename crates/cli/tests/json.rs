use std::path::{Path, PathBuf};

use devspace_machine::{
    MachineRepository, RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName,
};

mod support;

use support::fake_worker::{create_server, respond};
use support::{configure_machine, ds, machine_store, settings, stderr, stdout, write_cli_config};

fn identity(id_byte: u8) -> RepositoryIdentity {
    RepositoryIdentity::new(
        RepositoryId::parse(format!("{id_byte:02x}").repeat(32)).unwrap(),
        RepositoryIncarnation::parse(format!("{:02x}", id_byte + 1).repeat(16)).unwrap(),
    )
}

async fn local_repository(root: &Path, name: &str, id_byte: u8) -> PathBuf {
    let entry = machine_store(root)
        .register_repository(RepositoryName::parse(name).unwrap(), identity(id_byte))
        .unwrap();
    MachineRepository::init(&entry.native_repository_path, &settings())
        .await
        .unwrap();
    entry.native_repository_path
}

fn add_checkout(root: &Path, config: &Path, name: &str, checkout: &Path) {
    let output = support::ds_command(root, config)
        .args(["add", name, "-r", "root()"])
        .arg(checkout)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

fn assert_json_document(output: &std::process::Output) -> serde_json::Value {
    assert!(output.status.success(), "{}", stderr(output));
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    let document = &output.stdout[..output.stdout.len() - 1];
    assert!(!document.ends_with(b" "));
    serde_json::from_slice(document).unwrap()
}

#[tokio::test]
async fn list_json_has_full_semantic_ids_and_pure_stdout() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(temp.path(), "http://127.0.0.1:1");
    local_repository(temp.path(), "workspaces", 0x20).await;
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("checkout");
    add_checkout(temp.path(), &config, "workspaces", &checkout);
    let described = ds(&checkout, &config, &["describe", "-m", "JSON workspace"]);
    assert!(described.status.success(), "{}", stderr(&described));

    let ids = ds(
        &checkout,
        &config,
        &[
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "change_id ++ \"\\n\" ++ commit_id ++ \"\\n\"",
        ],
    );
    assert!(ids.status.success(), "{}", stderr(&ids));
    let ids = stdout(&ids);
    let mut ids = ids.lines();
    let expected_change_id = ids.next().unwrap();
    let expected_commit_id = ids.next().unwrap();
    assert_eq!(ids.next(), None);

    let output = ds(&checkout, &config, &["list", "--json"]);
    let value = assert_json_document(&output);
    assert_eq!(stderr(&output), "");
    let entries = value.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["current"], true);
    assert_eq!(entries[0]["description"], "JSON workspace");
    assert_eq!(entries[0]["change_id"], expected_change_id);
    assert_eq!(entries[0]["commit_id"], expected_commit_id);
    assert!(entries[0]["workspace_id"].as_str().is_some());
    assert!(expected_change_id.len() > 12);
    assert!(expected_commit_id.len() > 12);
}

#[test]
fn repo_list_json_emits_an_empty_array_without_diagnostics() {
    let temp = tempfile::tempdir().unwrap();
    let (base_url, server) = create_server(|_, request, stream| {
        assert!(request.starts_with("GET /repositories HTTP/1.1"));
        respond(stream, "200 OK", r#"{"repositories":[]}"#);
        true
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["repo", "list", "--json"]);
    server.join().unwrap();

    assert_eq!(assert_json_document(&output), serde_json::json!([]));
    assert_eq!(stderr(&output), "");
}

#[tokio::test]
async fn repo_list_json_uses_stable_availability_values_and_utf8_paths() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(temp.path(), "http://127.0.0.1:1");
    local_repository(temp.path(), "available", 0x30).await;
    local_repository(temp.path(), "missing", 0x40).await;
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("missing-checkout");
    add_checkout(temp.path(), &config, "missing", &checkout);

    let available = serde_json::json!({
        "name": "available",
        "repositoryId": "30".repeat(32),
        "incarnation": "31".repeat(16),
    });
    let cloud_only = serde_json::json!({
        "name": "cloud-only",
        "repositoryId": "50".repeat(32),
        "incarnation": "51".repeat(16),
    });
    let list = serde_json::json!({"repositories": [available, cloud_only]}).to_string();
    let (base_url, server) = create_server(move |index, request, stream| {
        if index == 0 {
            assert!(request.starts_with("GET /repositories HTTP/1.1"));
            respond(stream, "200 OK", &list);
            false
        } else {
            assert!(request.starts_with("GET /repositories/missing HTTP/1.1"));
            respond(
                stream,
                "404 Not Found",
                r#"{"error":"missing","code":"repository-not-found"}"#,
            );
            true
        }
    });
    configure_machine(temp.path(), &base_url);

    let output = ds(temp.path(), &config, &["repo", "list", "--json"]);
    server.join().unwrap();
    let value = assert_json_document(&output);
    assert_eq!(stderr(&output), "");
    assert_eq!(
        value,
        serde_json::json!([
            {"name":"available","availability":"available-locally","checkouts":[]},
            {"name":"cloud-only","availability":"cloud-only","checkouts":[]},
            {
                "name":"missing",
                "availability":"missing-from-cloud",
                "checkouts":[dunce::canonicalize(checkout).unwrap().to_str().unwrap()]
            }
        ])
    );
}

#[tokio::test]
async fn sync_status_json_reports_semantic_state_on_stdout_only() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(temp.path(), "http://127.0.0.1:1");
    local_repository(temp.path(), "complete", 0x60).await;
    machine_store(temp.path())
        .register_repository(RepositoryName::parse("incomplete").unwrap(), identity(0x70))
        .unwrap();
    let config = write_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["sync", "status", "--json"]);
    let value = assert_json_document(&output);
    assert_eq!(stderr(&output), "");
    assert_eq!(value["daemon_running"], false);
    assert_eq!(
        value["repositories"],
        serde_json::json!([
            {"repo":"complete","complete":true,"has_sync_state":false,"pending":1},
            {"repo":"incomplete","complete":false,"has_sync_state":false,"pending":0}
        ])
    );
}

#[tokio::test]
async fn git_remote_list_json_is_a_typed_document_with_a_newline() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(temp.path(), "http://127.0.0.1:1");
    local_repository(temp.path(), "remotes", 0x80).await;
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("checkout");
    add_checkout(temp.path(), &config, "remotes", &checkout);

    let (base_url, server) = create_server(|index, request, stream| {
        assert!(request.starts_with("GET "));
        assert!(request.contains("/remotes?"));
        if index == 0 {
            respond(
                stream,
                "200 OK",
                r#"{"remotes":[{"name":"origin","url":"ssh://example.invalid/repo.git"}]}"#,
            );
            false
        } else {
            respond(stream, "200 OK", r#"{"remotes":[]}"#);
            true
        }
    });
    configure_machine(temp.path(), &base_url);

    let output = ds(&checkout, &config, &["git", "remote", "list", "--json"]);
    assert_eq!(
        assert_json_document(&output),
        serde_json::json!([{"name":"origin","url":"ssh://example.invalid/repo.git"}])
    );
    assert_eq!(stderr(&output), "");

    let empty = ds(&checkout, &config, &["git", "remote", "list", "--json"]);
    server.join().unwrap();
    assert_eq!(assert_json_document(&empty), serde_json::json!([]));
    assert_eq!(stderr(&empty), "");
}

#[test]
fn git_remote_list_json_flag_is_local_and_stock_dispatch_remains_available() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());

    let json = ds(temp.path(), &config, &["git", "remote", "list", "--json"]);
    assert_eq!(json.status.code(), Some(1));
    assert!(
        stderr(&json).contains("available only inside a Devspace checkout"),
        "{}",
        stderr(&json)
    );

    let stock = ds(temp.path(), &config, &["git", "remote", "list"]);
    assert_eq!(stock.status.code(), Some(1));
    assert!(stderr(&stock).contains("There is no jj repo"));
    assert!(!stderr(&stock).contains("Devspace checkout"));

    let other = ds(
        temp.path(),
        &config,
        &["git", "remote", "add", "origin", "url", "--json"],
    );
    assert_eq!(other.status.code(), Some(2));
    assert!(stderr(&other).contains("unexpected argument '--json'"));
}
