use std::fs;
use std::path::{Path, PathBuf};

use devspace_machine::{
    CatalogEntry, MachineStore, RepositoryId, RepositoryIdentity, RepositoryIncarnation,
    RepositoryName,
};

mod support;

use support::fake_worker::{create_server, respond};
use support::{configure_machine, ds, machine_store, settings, stderr, stdout, write_cli_config};

const REPOSITORY_NAME: &str = "removable";

fn identity() -> RepositoryIdentity {
    RepositoryIdentity::new(
        RepositoryId::parse("ab".repeat(32)).unwrap(),
        RepositoryIncarnation::parse("cd".repeat(16)).unwrap(),
    )
}

async fn local_repository(root: &Path, base_url: &str) -> CatalogEntry {
    configure_machine(root, base_url);
    let store = machine_store(root);
    let name = RepositoryName::parse(REPOSITORY_NAME).unwrap();
    let entry = store.register_repository(name.clone(), identity()).unwrap();
    store
        .materialize_repository(&name, &entry.identity, &settings())
        .await
        .unwrap();
    for path in repository_data_paths(&store, &entry) {
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("fixture"), b"local repository data").unwrap();
    }
    entry
}

fn repository_data_paths(store: &MachineStore, entry: &CatalogEntry) -> [PathBuf; 4] {
    [
        entry.native_repository_path.clone(),
        store.repository_sync_path(&entry.identity),
        store.repository_packs_path(&entry.identity),
        store.repository_projection_path(&entry.identity),
    ]
}

fn add_checkout(root: &Path, config: &Path, path: &Path) {
    let output = ds(
        root,
        config,
        &[
            "add",
            REPOSITORY_NAME,
            "--rev",
            "root()",
            path.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
}

#[tokio::test]
async fn repo_remove_refuses_registered_local_checkouts() {
    let temp = tempfile::tempdir().unwrap();
    let entry = local_repository(temp.path(), "http://127.0.0.1:9").await;
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("checkout");
    add_checkout(temp.path(), &config, &checkout);

    let output = ds(
        temp.path(),
        &config,
        &["repo", "remove", REPOSITORY_NAME, "--force"],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("has registered checkouts on this machine"));
    assert!(stderr(&output).contains(&checkout.display().to_string()));
    assert!(stderr(&output).contains("ds remove <path>"));
    assert_eq!(
        machine_store(temp.path())
            .resolve(&entry.name)
            .unwrap()
            .unwrap()
            .identity,
        entry.identity
    );
}

#[tokio::test]
async fn repo_remove_requires_force_when_not_interactive() {
    let temp = tempfile::tempdir().unwrap();
    local_repository(temp.path(), "http://127.0.0.1:9").await;
    let config = write_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["repo", "remove", REPOSITORY_NAME]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("requires `--force` when not interactive"));
}

#[test]
fn repo_remove_reports_cloud_not_found_without_a_local_entry() {
    let temp = tempfile::tempdir().unwrap();
    let (base_url, server) = create_server(|_, request, stream| {
        assert!(request.starts_with("GET /repositories/removable HTTP/1.1"));
        respond(
            stream,
            "404 Not Found",
            r#"{"error":"missing","code":"repository-not-found"}"#,
        );
        true
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());

    let output = ds(
        temp.path(),
        &config,
        &["repo", "remove", REPOSITORY_NAME, "--force"],
    );
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("repository-not-found"));
}

#[tokio::test]
async fn repo_remove_cleans_catalog_and_all_local_directories_after_success() {
    let temp = tempfile::tempdir().unwrap();
    let (base_url, server) = create_server(|_, request, stream| {
        assert!(request.starts_with("DELETE /repositories/removable HTTP/1.1"));
        assert!(request.contains(&format!(r#""repositoryId":"{}""#, "ab".repeat(32))));
        assert!(request.contains(&format!(r#""incarnation":"{}""#, "cd".repeat(16))));
        respond(stream, "200 OK", r#"{"deleted":true}"#);
        true
    });
    let entry = local_repository(temp.path(), &base_url).await;
    let store = machine_store(temp.path());
    let paths = repository_data_paths(&store, &entry);
    let config = write_cli_config(temp.path());

    let output = ds(
        temp.path(),
        &config,
        &["repo", "remove", REPOSITORY_NAME, "--force"],
    );
    server.join().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("Deleted repository `removable`."));
    assert!(stderr(&output).contains("Removed this machine's local repository data"));
    assert!(store.resolve(&entry.name).unwrap().is_none());
    assert!(paths.iter().all(|path| !path.exists()));
    let removal_root = store.repository_removal_root(&entry.identity);
    assert!(!removal_root.exists());
    assert!(!removal_root.parent().unwrap().exists());
}

#[tokio::test]
async fn repo_remove_cleans_local_residue_when_cloud_is_already_deleted() {
    let temp = tempfile::tempdir().unwrap();
    let (base_url, server) = create_server(|_, request, stream| {
        assert!(request.starts_with("DELETE /repositories/removable HTTP/1.1"));
        respond(
            stream,
            "404 Not Found",
            r#"{"error":"missing","code":"repository-not-found"}"#,
        );
        true
    });
    let entry = local_repository(temp.path(), &base_url).await;
    let config = write_cli_config(temp.path());

    let output = ds(
        temp.path(),
        &config,
        &["repo", "remove", REPOSITORY_NAME, "--force"],
    );
    server.join().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("The cloud repository was already deleted."));
    assert!(
        machine_store(temp.path())
            .resolve(&entry.name)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn repo_list_removes_stale_catalog_and_local_data_without_checkouts() {
    let temp = tempfile::tempdir().unwrap();
    let (base_url, server) = create_server(|index, request, stream| {
        if index == 0 {
            assert!(request.starts_with("GET /repositories HTTP/1.1"));
            respond(stream, "200 OK", r#"{"repositories":[]}"#);
            false
        } else {
            assert!(request.starts_with("GET /repositories/removable HTTP/1.1"));
            respond(
                stream,
                "404 Not Found",
                r#"{"error":"missing","code":"repository-not-found"}"#,
            );
            true
        }
    });
    let entry = local_repository(temp.path(), &base_url).await;
    let store = machine_store(temp.path());
    let paths = repository_data_paths(&store, &entry);
    let config = write_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["repo", "list"]);
    server.join().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("because it was deleted in the cloud"));
    assert!(store.resolve(&entry.name).unwrap().is_none());
    assert!(paths.iter().all(|path| !path.exists()));
}

#[tokio::test]
async fn repo_list_preserves_deleted_cloud_repository_with_local_checkouts() {
    let temp = tempfile::tempdir().unwrap();
    let (base_url, server) = create_server(|index, request, stream| {
        if index == 0 {
            assert!(request.starts_with("GET /repositories HTTP/1.1"));
            respond(stream, "200 OK", r#"{"repositories":[]}"#);
            false
        } else {
            assert!(request.starts_with("GET /repositories/removable HTTP/1.1"));
            respond(
                stream,
                "404 Not Found",
                r#"{"error":"missing","code":"repository-not-found"}"#,
            );
            true
        }
    });
    let entry = local_repository(temp.path(), &base_url).await;
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("checkout");
    add_checkout(temp.path(), &config, &checkout);

    let output = ds(temp.path(), &config, &["repo", "list"]);
    server.join().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("deleted in cloud; local checkouts remain"));
    assert!(stdout(&output).contains(&checkout.display().to_string()));
    assert_eq!(
        machine_store(temp.path())
            .resolve(&entry.name)
            .unwrap()
            .unwrap()
            .identity,
        entry.identity
    );
    assert!(entry.native_repository_path.exists());
}

#[tokio::test]
async fn repo_list_preserves_local_data_when_an_omitted_repository_still_resolves() {
    let temp = tempfile::tempdir().unwrap();
    let identity = identity();
    let (base_url, server) = create_server(move |index, request, stream| {
        if index == 0 {
            assert!(request.starts_with("GET /repositories HTTP/1.1"));
            respond(stream, "200 OK", r#"{"repositories":[]}"#);
            false
        } else {
            assert!(request.starts_with("GET /repositories/removable HTTP/1.1"));
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "name": REPOSITORY_NAME,
                    "repositoryId": identity.repository_id.as_str(),
                    "incarnation": identity.incarnation.as_str(),
                })
                .to_string(),
            );
            true
        }
    });
    let entry = local_repository(temp.path(), &base_url).await;
    let config = write_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["repo", "list"]);
    server.join().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("absent from the cloud listing"));
    assert!(stderr(&output).contains("local data was not changed"));
    assert!(
        machine_store(temp.path())
            .resolve(&entry.name)
            .unwrap()
            .is_some()
    );
    assert!(entry.native_repository_path.exists());
}

#[tokio::test]
async fn repo_list_preserves_local_data_when_deletion_confirmation_fails() {
    let temp = tempfile::tempdir().unwrap();
    let (base_url, server) = create_server(|index, request, stream| {
        if index == 0 {
            assert!(request.starts_with("GET /repositories HTTP/1.1"));
            respond(stream, "200 OK", r#"{"repositories":[]}"#);
            false
        } else {
            assert!(request.starts_with("GET /repositories/removable HTTP/1.1"));
            respond(stream, "503 Service Unavailable", r#"{"error":"offline"}"#);
            true
        }
    });
    let entry = local_repository(temp.path(), &base_url).await;
    let config = write_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["repo", "list"]);
    server.join().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("deletion could not be confirmed"));
    assert!(stderr(&output).contains("local data was not changed"));
    assert!(
        machine_store(temp.path())
            .resolve(&entry.name)
            .unwrap()
            .is_some()
    );
    assert!(entry.native_repository_path.exists());
}
