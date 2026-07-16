use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;

use devspace_machine::{
    MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineRepository, MachineStore,
    RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName, SharedSecret,
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

async fn local_repository(root: &Path, repository_name: &str) -> PathBuf {
    let store = machine_store(root);
    store
        .write_config(
            &MachineConfig::new(
                "http://127.0.0.1:1",
                MachineId::parse(MACHINE_ID).unwrap(),
                SharedSecret::new(DEVELOPMENT_SECRET).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
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
