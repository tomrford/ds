use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use devspace_machine::RepositoryName;

mod support;

use support::fake_worker::{create_server, repository_response, respond};
use support::{
    configure_machine, daemon_socket_path, ds, ds_command, machine_store, poll_until, stderr,
    stdout, write_cli_config,
};

fn create_git_remote(root: &Path, name: &str) -> std::path::PathBuf {
    let remote = root.join(name);
    let status = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(&remote)
        .status()
        .unwrap();
    assert!(status.success());
    fs::write(remote.join("README.md"), "imported\n").unwrap();
    for args in [
        vec!["config", "user.name", "Devspace Test"],
        vec!["config", "user.email", "devspace@example.invalid"],
        vec!["add", "README.md"],
        vec!["commit", "-q", "-m", "initial"],
    ] {
        let status = Command::new("git")
            .current_dir(&remote)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }
    remote
}

fn request_body(request: &str) -> serde_json::Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

fn create_import_server(
    expected_name: String,
    git_url: String,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let response = repository_response(&expected_name);
    let mut head = None::<String>;
    let mut head_cursor = 0_u64;
    let mut projection = serde_json::json!({
        "activationCursor": 0,
        "cursors": [],
        "mappings": [],
        "nextAfter": 0,
        "through": 0,
        "hasMore": false,
        "pending": [],
    });
    create_server(move |_, request, stream| {
        let request_line = request.lines().next().unwrap();
        if request_line == "POST /repositories HTTP/1.1" {
            assert_eq!(request_body(request)["name"], expected_name);
            respond(stream, "200 OK", &response);
        } else if request_line.starts_with("PUT ") && request_line.contains("/remotes/origin ") {
            assert_eq!(request_body(request)["url"], git_url);
            respond(
                stream,
                "200 OK",
                &serde_json::json!({"remote": {"name": "origin", "url": git_url}}).to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/packs?") {
            respond(
                stream,
                "200 OK",
                r#"{"packs":[],"nextAfter":0,"through":0,"hasMore":false}"#,
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/heads?") {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "cursor": head_cursor,
                    "heads": head.iter().collect::<Vec<_>>(),
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ") && request_line.contains("/objects/inventory ")
        {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({"objects": request_body(request)["objects"]}).to_string(),
            );
        } else if request_line.starts_with("POST ") && request_line.contains("/heads ") {
            let body = request_body(request);
            head = Some(body["newHead"].as_str().unwrap().to_owned());
            head_cursor += 1;
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "cursor": head_cursor,
                    "heads": head.iter().collect::<Vec<_>>(),
                })
                .to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/remotes?") {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({"remotes": [{"name": "origin", "url": git_url}]}).to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/projection?") {
            respond(stream, "200 OK", &projection.to_string());
        } else if request_line.starts_with("POST ") && request_line.contains("/git/fetches ") {
            let body = request_body(request);
            let cursors = body["refs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|fetch_ref| {
                    let index = fetch_ref["proposedState"].as_u64().unwrap() as usize;
                    let state = &fetch_ref["states"][index];
                    serde_json::json!({
                        "remote": body["remote"],
                        "bookmark": fetch_ref["bookmark"],
                        "gitOid": state["gitOid"],
                        "canonicalCommitId": state["canonicalCommitId"],
                        "publicCommitId": state["publicCommitId"],
                        "hiddenSetId": state["hiddenSetId"],
                        "activationSequence": 1,
                    })
                })
                .collect::<Vec<_>>();
            let mappings = cursors
                .iter()
                .map(|cursor| {
                    let mut mapping = cursor.clone();
                    mapping
                        .as_object_mut()
                        .unwrap()
                        .remove("activationSequence");
                    mapping
                })
                .collect::<Vec<_>>();
            projection = serde_json::json!({
                "activationCursor": 1,
                "cursors": cursors,
                "mappings": mappings,
                "nextAfter": 1,
                "through": 1,
                "hasMore": false,
                "pending": [],
            });
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "fetchId": body["fetchId"],
                    "activationCursor": 1,
                })
                .to_string(),
            );
        } else {
            panic!("unexpected import request: {request_line}");
        }
        false
    })
}

#[tokio::test]
async fn init_blank_composes_repository_creation_and_first_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let response = repository_response("blank-project");
    let (base_url, server) = create_server(move |_, request, stream| {
        assert!(request.starts_with("POST /repositories HTTP/1.1"));
        respond(stream, "200 OK", &response);
        true
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("blank-project");

    let output = ds(temp.path(), &config, &["init", checkout.to_str().unwrap()]);
    server.join().unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(checkout.join(".jj/devspace-checkout-owner").is_file());
    let entry = machine_store(temp.path())
        .resolve(&RepositoryName::parse("blank-project").unwrap())
        .unwrap()
        .unwrap();
    assert!(entry.native_repository_path.is_dir());
    assert!(stderr(&output).contains("Repository: blank-project"));
    assert!(
        stderr(&output).contains(&format!(
            "Checkout: {}",
            dunce::canonicalize(&checkout).unwrap().display()
        )),
        "{}",
        stderr(&output)
    );
}

#[test]
fn repo_new_chooses_a_bounded_default_suffix_after_name_rejection() {
    let temp = tempfile::tempdir().unwrap();
    let response = repository_response("new-repo-2");
    let (base_url, server) = create_server(move |index, request, stream| {
        if index == 0 {
            respond(
                stream,
                "409 Conflict",
                r#"{"error":"repository name is already in use","code":"repository-name-taken"}"#,
            );
        } else {
            respond(stream, "200 OK", &response);
        }
        assert!(request.starts_with("POST /repositories HTTP/1.1"));
        index == 1
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["repo", "new"]);
    let requests = server.join().unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains(r#""name":"new-repo""#));
    assert!(requests[1].contains(r#""name":"new-repo-2""#));
    assert!(stderr(&output).contains("Created repository `new-repo-2`."));
}

#[tokio::test]
async fn repo_list_renders_repository_states_and_repairs_an_interrupted_local_rename() {
    let temp = tempfile::tempdir().unwrap();
    let bare_response = format!(
        r#"{{"name":"bare-name","repositoryId":"{}","incarnation":"{}"}}"#,
        "ef".repeat(32),
        "cd".repeat(16)
    );
    let config = write_cli_config(temp.path());
    for (name, response) in [
        ("old-name", repository_response("old-name")),
        ("bare-name", bare_response.clone()),
    ] {
        let (create_url, create_handle) = create_server(move |_, _, stream| {
            respond(stream, "200 OK", &response);
            true
        });
        configure_machine(temp.path(), &create_url);
        let created = ds(temp.path(), &config, &["repo", "new", name]);
        create_handle.join().unwrap();
        assert!(created.status.success(), "{}", stderr(&created));
    }
    let checkouts = [
        temp.path().join("checkout-one"),
        temp.path().join("checkout-two"),
    ];
    for checkout in &checkouts {
        let added = ds(
            temp.path(),
            &config,
            &[
                "add",
                "old-name",
                "-r",
                "root()",
                checkout.to_str().unwrap(),
            ],
        );
        assert!(added.status.success(), "{}", stderr(&added));
    }

    let renamed = repository_response("new-name");
    let cloud_only = format!(
        r#"{{"name":"cloud-only","repositoryId":"{}","incarnation":"{}"}}"#,
        "12".repeat(32),
        "cd".repeat(16)
    );
    let list_body = format!(r#"{{"repositories":[{renamed},{bare_response},{cloud_only}]}}"#);
    let (list_url, list_server) = create_server(move |_, request, stream| {
        assert!(request.starts_with("GET /repositories HTTP/1.1"));
        respond(stream, "200 OK", &list_body);
        true
    });
    configure_machine(temp.path(), &list_url);

    let output = ds(temp.path(), &config, &["repo", "list"]);
    list_server.join().unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "● new-name\n  {}\n  {}\n● bare-name\n○ cloud-only\n",
            dunce::canonicalize(&checkouts[0]).unwrap().display(),
            dunce::canonicalize(&checkouts[1]).unwrap().display()
        )
    );
    assert!(
        machine_store(temp.path())
            .resolve(&RepositoryName::parse("old-name").unwrap())
            .unwrap()
            .is_none()
    );
    assert!(
        machine_store(temp.path())
            .resolve(&RepositoryName::parse("new-name").unwrap())
            .unwrap()
            .is_some()
    );
}

#[test]
fn repo_add_derives_or_overrides_the_name_without_creating_a_checkout() {
    for (remote_basename, requested_name, expected_name) in [
        ("from-basename.git", None, "from-basename"),
        ("override-source.git", Some("chosen-name"), "chosen-name"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let remote = create_git_remote(temp.path(), remote_basename);
        let git_url = remote.to_str().unwrap().to_owned();
        let (base_url, server) = create_import_server(expected_name.to_owned(), git_url.clone());
        configure_machine(temp.path(), &base_url);
        let config = write_cli_config(temp.path());
        let mut args = vec!["repo", "add", git_url.as_str()];
        if let Some(name) = requested_name {
            args.extend(["--name", name]);
        }

        let output = ds(temp.path(), &config, &args);
        let requests = server.join().unwrap();

        assert!(output.status.success(), "{}", stderr(&output));
        assert!(
            machine_store(temp.path())
                .resolve(&RepositoryName::parse(expected_name).unwrap())
                .unwrap()
                .is_some()
        );
        assert!(!temp.path().join(".jj").exists());
        assert!(stderr(&output).contains(&format!("Repository: {expected_name}")));
        assert!(
            requests
                .iter()
                .any(|request| request.contains("/git/fetches "))
        );
    }
}

#[test]
fn repo_add_resumes_only_when_the_registered_origin_matches() {
    let temp = tempfile::tempdir().unwrap();
    let remote = create_git_remote(temp.path(), "resume-origin.git");
    let different_remote = create_git_remote(temp.path(), "different-origin.git");
    let git_url = remote.to_str().unwrap().to_owned();
    let different_url = different_remote.to_str().unwrap().to_owned();
    let name = "resumable-import";
    let (base_url, server) = create_import_server(name.to_owned(), git_url.clone());
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());

    let first = ds(
        temp.path(),
        &config,
        &["repo", "add", &git_url, "--name", name],
    );
    assert!(first.status.success(), "{}", stderr(&first));

    let resumed = ds(
        temp.path(),
        &config,
        &["repo", "add", &git_url, "--name", name],
    );
    assert!(resumed.status.success(), "{}", stderr(&resumed));

    let collision = ds(
        temp.path(),
        &config,
        &["repo", "add", &different_url, "--name", name],
    );
    assert_eq!(collision.status.code(), Some(1));
    assert!(
        stderr(&collision).contains(&format!(
            "repository-name-taken: repository {name} already exists on this machine; existing origin is {git_url}"
        )),
        "{}",
        stderr(&collision)
    );

    let requests = server.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("POST /repositories HTTP/1.1"))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.starts_with("PUT ") && request.contains("/remotes/origin ")
            })
            .count(),
        2
    );
}

#[tokio::test]
async fn list_reports_all_workspace_positions_and_marks_current() {
    let temp = tempfile::tempdir().unwrap();
    let response = repository_response("workspace-list");
    let (base_url, server) = create_server(move |_, _, stream| {
        respond(stream, "200 OK", &response);
        true
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());
    let created = ds(temp.path(), &config, &["repo", "new", "workspace-list"]);
    server.join().unwrap();
    assert!(created.status.success(), "{}", stderr(&created));

    let first = temp.path().join("first");
    let second = temp.path().join("second");
    let first_add = ds(
        temp.path(),
        &config,
        &[
            "add",
            "workspace-list",
            "-r",
            "root()",
            first.to_str().unwrap(),
        ],
    );
    assert!(first_add.status.success(), "{}", stderr(&first_add));
    let second_add = ds(
        &first,
        &config,
        &[
            "add",
            "workspace-list",
            "-r",
            "@-",
            second.to_str().unwrap(),
        ],
    );
    assert!(second_add.status.success(), "{}", stderr(&second_add));
    let described = ds(
        &second,
        &config,
        &["describe", "-m", "second workspace\nmore"],
    );
    assert!(described.status.success(), "{}", stderr(&described));

    let output = ds(&first, &config, &["list"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let output_text = stdout(&output);
    let lines = output_text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{output_text}");
    assert_eq!(lines.iter().filter(|line| line.starts_with('*')).count(), 1);
    assert!(lines.iter().any(|line| line.contains("second workspace")));
    assert!(!output_text.contains("more"));
}

#[cfg(unix)]
#[test]
fn doctor_happy_path_and_insecure_config_failure() {
    let temp = tempfile::tempdir().unwrap();
    let (base_url, server) = create_server(|_, request, stream| {
        assert!(request.starts_with("GET /repositories HTTP/1.1"));
        respond(stream, "200 OK", r#"{"repositories":[]}"#);
        true
    });
    configure_machine(temp.path(), &base_url);
    let config = write_cli_config(temp.path());
    let socket = daemon_socket_path(machine_store(temp.path()).root());
    let mut daemon = ds_command(temp.path(), &config)
        .env("DEVSPACE_DAEMON_TEST_HOOKS", "1")
        .env("DEVSPACE_DAEMON_TEST_POLL_MS", "10000")
        .env("DEVSPACE_DAEMON_TEST_IDLE_MS", "10000")
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(poll_until(Duration::from_secs(10), || socket.exists()));

    let healthy = ds(temp.path(), &config, &["doctor"]);
    server.join().unwrap();
    assert!(healthy.status.success(), "{}", stderr(&healthy));
    assert!(stdout(&healthy).contains("OK machine config:"));
    assert!(stdout(&healthy).contains("OK cloud:"));
    assert!(stdout(&healthy).contains("OK daemon: running"));
    assert!(stdout(&healthy).contains("OK git:"));
    assert!(stdout(&healthy).contains("OK aliases:"));
    daemon.kill().unwrap();
    daemon.wait().unwrap();

    use std::os::unix::fs::PermissionsExt as _;
    let path = machine_store(temp.path()).config_path();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let broken = ds(temp.path(), &config, &["doctor"]);
    assert_eq!(broken.status.code(), Some(1));
    assert!(stdout(&broken).contains("FAIL machine config:"));
    assert!(stdout(&broken).contains("FAIL cloud: machine config is unavailable"));
}
