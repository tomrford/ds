use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use devspace_machine::{
    MachineRepository, RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName,
};
use jj_lib::default_index::DefaultIndexStore;
use jj_lib::default_submodule_store::DefaultSubmoduleStore;
use jj_lib::op_store::RefTarget;
use jj_lib::repo::Repo as _;
use jj_lib::simple_backend::SimpleBackend;
use jj_lib::simple_op_heads_store::SimpleOpHeadsStore;
use jj_lib::simple_op_store::SimpleOpStore;

mod support;

use support::{ds, ds_with_env, machine_store, settings, stderr, stdout};

const FIXTURE_DESCRIPTION: &str = "bare repository fixture";

fn write_fixture_cli_config(root: &Path) -> PathBuf {
    let path = root.join("jj-config.toml");
    fs::write(
        &path,
        r#"
            [user]
            name = "Devspace Test"
            email = "devspace@example.invalid"

            [aliases]
            fixture = ["log", "--no-graph"]
            catalog-fixture = ["-R", "machine-repo", "log", "--no-graph"]

            [revsets]
            log = "main"

            [templates]
            log = '"configured " ++ description.first_line() ++ "\n"'

            [ui]
            color = "never"
        "#,
    )
    .unwrap();
    path
}

fn write_colliding_alias_config(root: &Path) -> PathBuf {
    let path = root.join("jj-config.toml");
    fs::write(
        &path,
        r#"
            [user]
            name = "Devspace Test"
            email = "devspace@example.invalid"

            [aliases]
            init = ["git", "init"]
            unrelated = ["version"]

            [ui]
            color = "never"
        "#,
    )
    .unwrap();
    path
}

fn write_watchman_config(root: &Path) -> PathBuf {
    let path = root.join("watchman-config.toml");
    fs::write(
        &path,
        r#"
            [user]
            name = "Devspace Test"
            email = "devspace@example.invalid"

            [fsmonitor]
            backend = "watchman"

            [ui]
            color = "never"
        "#,
    )
    .unwrap();
    path
}

fn repository_identity(repository_name: &str) -> RepositoryIdentity {
    let discriminator = repository_name
        .bytes()
        .fold(1_u8, |accumulator, byte| accumulator.wrapping_add(byte));
    RepositoryIdentity::new(
        RepositoryId::parse(format!("{discriminator:02x}").repeat(32)).unwrap(),
        RepositoryIncarnation::parse("34".repeat(16)).unwrap(),
    )
}

async fn catalog_repository(
    root: &Path,
    repository_name: &str,
) -> (PathBuf, jj_lib::op_store::OperationId) {
    let entry = machine_store(root)
        .register_repository(
            RepositoryName::parse(repository_name).unwrap(),
            repository_identity(repository_name),
        )
        .unwrap();
    let operation = bare_repository(&entry.native_repository_path).await;
    (entry.native_repository_path, operation)
}

async fn bare_repository(path: &Path) -> jj_lib::op_store::OperationId {
    let repository = MachineRepository::init(path, &settings()).await.unwrap();
    let repo = repository.repo();
    let root_id = repo.store().root_commit_id().clone();
    let tree = repo.store().empty_merged_tree();
    let mut transaction = repo.start_transaction();
    let commit = transaction
        .repo_mut()
        .new_commit(vec![root_id], tree)
        .set_description(FIXTURE_DESCRIPTION)
        .write()
        .await
        .unwrap();
    transaction
        .repo_mut()
        .set_local_bookmark_target("main".as_ref(), RefTarget::normal(commit.id().clone()));
    transaction
        .commit("create bare CLI fixture")
        .await
        .unwrap()
        .op_id()
        .clone()
}

async fn operation_id(path: &Path) -> jj_lib::op_store::OperationId {
    MachineRepository::open(path, &settings())
        .await
        .unwrap()
        .repo()
        .op_id()
        .clone()
}

async fn add_divergent_operation_heads(path: &Path) {
    let repository = MachineRepository::open(path, &settings()).await.unwrap();
    let repo = repository.repo();
    let first = repo.start_transaction();
    let second = repo.start_transaction();
    first.commit("first concurrent operation").await.unwrap();
    second.commit("second concurrent operation").await.unwrap();
    assert_eq!(repo.op_heads_store().get_op_heads().await.unwrap().len(), 2);
}

async fn add_redundant_operation_head(path: &Path) {
    let repository = MachineRepository::open(path, &settings()).await.unwrap();
    let repo = repository.repo();
    let ancestor = repo.op_id().clone();
    repo.start_transaction()
        .commit("descendant operation")
        .await
        .unwrap();
    let op_heads_store = repo.op_heads_store();
    let _lock = op_heads_store.lock().await.unwrap();
    op_heads_store
        .update_op_heads(&[], &ancestor)
        .await
        .unwrap();
    assert_eq!(op_heads_store.get_op_heads().await.unwrap().len(), 2);
}

#[derive(Debug, PartialEq, Eq)]
enum SnapshotEntry {
    Directory,
    File(Vec<u8>),
}

fn repository_storage_snapshot(path: &Path) -> BTreeMap<PathBuf, SnapshotEntry> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, SnapshotEntry>) {
        let relative = path.strip_prefix(root).unwrap().to_owned();
        if path.is_dir() {
            snapshot.insert(relative, SnapshotEntry::Directory);
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), snapshot);
            }
        } else {
            snapshot.insert(relative, SnapshotEntry::File(fs::read(path).unwrap()));
        }
    }

    let mut snapshot = BTreeMap::new();
    for directory in ["store", "op_store", "op_heads"] {
        visit(path, &path.join(directory), &mut snapshot);
    }
    snapshot
}

fn directory_snapshot(path: &Path) -> BTreeMap<PathBuf, SnapshotEntry> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, SnapshotEntry>) {
        let relative = path.strip_prefix(root).unwrap().to_owned();
        if path.is_dir() {
            snapshot.insert(relative, SnapshotEntry::Directory);
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), snapshot);
            }
        } else {
            snapshot.insert(relative, SnapshotEntry::File(fs::read(path).unwrap()));
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(path, path, &mut snapshot);
    snapshot
}

#[cfg(unix)]
fn set_repository_read_only(path: &Path, read_only: bool) {
    fn collect(path: &Path, paths: &mut Vec<PathBuf>) {
        paths.push(path.to_owned());
        if path.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                collect(&entry.unwrap().path(), paths);
            }
        }
    }

    let mut paths = Vec::new();
    collect(path, &mut paths);
    for path in paths {
        let mode = match (read_only, path.is_dir()) {
            (true, true) => 0o555,
            (true, false) => 0o444,
            (false, true) => 0o755,
            (false, false) => 0o644,
        };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
}

fn add_bare_marker_collision(path: &Path) {
    for (directory, store_type) in [
        ("store", SimpleBackend::name()),
        ("op_store", SimpleOpStore::name()),
        ("op_heads", SimpleOpHeadsStore::name()),
        ("index", DefaultIndexStore::name()),
        ("submodule_store", DefaultSubmoduleStore::name()),
    ] {
        let directory = path.join(directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("type"), store_type).unwrap();
    }
}

#[test]
fn root_help_is_devspace_first() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_fixture_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["--help"]);

    assert!(output.status.success(), "{}", stderr(&output));
    let help = stdout(&output);
    assert!(
        help.starts_with("Cloudflare-native development workspaces"),
        "{help}"
    );
    for command in [
        "add", "init", "list", "doctor", "remove", "repo", "skill", "sync", "config", "git", "jj",
        "status",
    ] {
        assert!(
            help.lines()
                .any(|line| line.trim_start().starts_with(&format!("{command} "))),
            "missing `{command}` from root help:\n{help}"
        );
    }
    assert!(help.contains("ds help jj"), "{help}");
    assert!(!help.contains("  abandon "), "{help}");
}

#[test]
fn skill_prints_agent_guidance_without_a_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_fixture_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["skill"]);

    assert!(output.status.success(), "{}", stderr(&output));
    let guidance = stdout(&output);
    assert!(guidance.contains("best-effort Git index shim is off by default"));
    assert!(guidance.contains("ds config set git-shim true"));
    assert!(guidance.contains("Git writes and all `jj` commands"));
    assert!(guidance.contains("Git is a projection boundary"));
    assert!(guidance.contains("It is not an ignore file"));
}

#[test]
fn skill_topics_render_without_a_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_fixture_cli_config(temp.path());

    for (topic, sentinel) in [
        ("core", "# Devspace"),
        ("private", "# Private paths with `.dsprivate`"),
        ("context", "# Pinned repository context"),
        ("jj", "# Devspace and jj"),
    ] {
        let output = ds(temp.path(), &config, &["skill", topic]);
        assert!(output.status.success(), "{topic}: {}", stderr(&output));
        assert!(stdout(&output).contains(sentinel), "{topic}");
    }
}

#[test]
fn skill_rejects_unknown_topic_with_available_topics() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_fixture_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["skill", "unknown"]);

    assert_eq!(output.status.code(), Some(2));
    let error = stderr(&output);
    assert!(error.contains("invalid value 'unknown'"), "{error}");
    for topic in ["core", "private", "context", "jj"] {
        assert!(error.contains(topic), "missing `{topic}` from: {error}");
    }
}

#[test]
fn jj_help_lists_the_embedded_command_surface() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_fixture_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["help", "jj"]);

    assert!(output.status.success(), "{}", stderr(&output));
    let help = stdout(&output);
    for command in ["abandon", "describe", "log", "new"] {
        assert!(
            help.lines()
                .any(|line| line.trim_start().starts_with(&format!("{command} "))),
            "missing `{command}` from jj help:\n{help}"
        );
    }
}

#[test]
fn embedded_jj_command_help_still_works() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_fixture_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["log", "--help"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Show revision history"));
}

#[test]
fn colliding_alias_is_silenced_without_affecting_other_aliases_or_devspace_command() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_colliding_alias_config(temp.path());

    let alias = ds(temp.path(), &config, &["unrelated"]);
    assert!(alias.status.success(), "{}", stderr(&alias));
    assert!(stdout(&alias).contains("ds 0.1.0"), "{}", stdout(&alias));

    let init_help = ds(temp.path(), &config, &["init", "--help"]);
    assert!(init_help.status.success(), "{}", stderr(&init_help));
    assert!(
        stdout(&init_help).contains("Initialize a Devspace repository from a Git remote"),
        "{}",
        stdout(&init_help)
    );

    for output in [alias, init_help] {
        let stderr = stderr(&output);
        assert!(!stderr.contains("Cannot define an alias"), "{stderr}");
        assert!(!stderr.contains("Deprecated"), "{stderr}");
    }
}

#[tokio::test]
async fn bare_log_uses_stock_alias_revset_template_and_graph_without_workspace_state() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "machine-repo";
    let (repository_path, expected_operation) =
        catalog_repository(temp.path(), repository_name).await;
    let config = write_fixture_cli_config(temp.path());

    let configured = ds(temp.path(), &config, &["-R", repository_name, "fixture"]);
    assert!(configured.status.success(), "{}", stderr(&configured));
    assert_eq!(stdout(&configured), "configured bare repository fixture\n");

    let alias_selected = ds(temp.path(), &config, &["catalog-fixture"]);
    assert!(
        alias_selected.status.success(),
        "{}",
        stderr(&alias_selected)
    );
    assert_eq!(
        stdout(&alias_selected),
        "configured bare repository fixture\n"
    );

    let graphed = ds(
        temp.path(),
        &config,
        &[
            "-R",
            repository_name,
            "log",
            "-r",
            "main",
            "-T",
            "description.first_line()",
        ],
    );
    assert!(graphed.status.success(), "{}", stderr(&graphed));
    assert!(stdout(&graphed).contains(FIXTURE_DESCRIPTION));
    assert!(stdout(&graphed).contains('○'));
    assert!(!stdout(&graphed).contains('@'));

    assert_eq!(operation_id(&repository_path).await, expected_operation);
    assert!(!repository_path.join(".jj").exists());
    assert!(!repository_path.join("working_copy").exists());
}

#[tokio::test]
async fn local_name_log_has_no_network_dependency() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "offline-repository";
    catalog_repository(temp.path(), repository_name).await;
    let config = write_fixture_cli_config(temp.path());

    let output = ds_with_env(
        temp.path(),
        temp.path(),
        &config,
        &["-R", repository_name, "log", "-r", "main"],
        &[
            ("HTTP_PROXY", "http://127.0.0.1:1"),
            ("HTTPS_PROXY", "http://127.0.0.1:1"),
            ("ALL_PROXY", "http://127.0.0.1:1"),
            ("NO_PROXY", ""),
        ],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains(FIXTURE_DESCRIPTION));
}

#[tokio::test]
async fn bare_log_accepts_all_repository_option_spellings() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "machine-repo";
    catalog_repository(temp.path(), repository_name).await;
    let config = write_fixture_cli_config(temp.path());

    for args in [
        vec!["-R".to_owned(), repository_name.to_owned()],
        vec!["--repository".to_owned(), repository_name.to_owned()],
        vec![format!("-R={repository_name}")],
        vec![format!("-R{repository_name}")],
        vec![format!("--repository={repository_name}")],
    ] {
        let mut args = args;
        args.extend(["log".to_owned(), "-r".to_owned(), "main".to_owned()]);
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = ds(temp.path(), &config, &args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        assert!(stdout(&output).contains(FIXTURE_DESCRIPTION));
    }
}

#[test]
fn missing_local_name_points_to_add_without_network() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_fixture_cli_config(temp.path());

    let output = ds(temp.path(), &config, &["-R", "not-local", "log"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("Repository `not-local` is not present in this machine store"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("run `ds add`"),
        "{}",
        stderr(&output)
    );
}

#[tokio::test]
async fn bare_log_reports_that_at_is_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "machine-repo";
    catalog_repository(temp.path(), repository_name).await;
    let config = write_fixture_cli_config(temp.path());

    let output = ds(
        temp.path(),
        &config,
        &["-R", repository_name, "log", "-r", "@"],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("doesn't have a working-copy commit"),
        "{}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bare_log_reads_non_writable_repository_without_touching_lock_path() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "machine-repo";
    let (repository_path, _) = catalog_repository(temp.path(), repository_name).await;
    let lock_path = repository_path.join("op_heads/heads/lock");
    fs::create_dir(&lock_path).unwrap();
    let before = repository_storage_snapshot(&repository_path);
    let config = write_fixture_cli_config(temp.path());
    set_repository_read_only(&repository_path, true);

    let output = ds(
        temp.path(),
        &config,
        &["-R", repository_name, "log", "-r", "main"],
    );
    let after = repository_storage_snapshot(&repository_path);
    set_repository_read_only(&repository_path, false);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains(FIXTURE_DESCRIPTION));
    assert_eq!(after, before);
    assert!(lock_path.is_dir());
}

#[tokio::test]
async fn bare_log_rejects_divergent_heads_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_fixture_cli_config(temp.path());
    let repository_name = "divergent";
    let (repository_path, _) = catalog_repository(temp.path(), repository_name).await;
    add_divergent_operation_heads(&repository_path).await;
    fs::create_dir(repository_path.join("op_heads/heads/lock")).unwrap();
    let before = repository_storage_snapshot(&repository_path);

    let output = ds(temp.path(), &config, &["-R", repository_name, "log"]);

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("has 2 operation heads"),
        "{}",
        stderr(&output)
    );
    assert_eq!(repository_storage_snapshot(&repository_path), before);
}

#[tokio::test]
async fn bare_log_prunes_redundant_heads_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_fixture_cli_config(temp.path());
    let repository_name = "redundant";
    let (repository_path, _) = catalog_repository(temp.path(), repository_name).await;
    add_redundant_operation_head(&repository_path).await;
    fs::create_dir(repository_path.join("op_heads/heads/lock")).unwrap();
    let before = repository_storage_snapshot(&repository_path);

    let output = ds(temp.path(), &config, &["-R", repository_name, "log"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains(FIXTURE_DESCRIPTION));
    assert_eq!(repository_storage_snapshot(&repository_path), before);
}

#[tokio::test]
async fn bare_config_markers_are_rejected_before_loading_or_writing() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_fixture_cli_config(temp.path());
    let pager_marker = temp.path().join("pager-ran");
    let config_home = temp.path().join("config-home");
    let home = temp.path().join("home");
    fs::create_dir(&config_home).unwrap();
    fs::create_dir(&home).unwrap();

    for marker in [
        "config.toml",
        "config-id",
        ".jj/workspace-config.toml",
        ".jj/workspace-config-id",
    ] {
        let repository_name = format!("marker-{}", marker.replace(['.', '/'], "-"));
        let (repository_path, _) = catalog_repository(temp.path(), &repository_name).await;
        let contents = if marker.ends_with(".toml") {
            format!(
                "[ui]\npaginate = \"always\"\npager = [\"sh\", \"-c\", \"touch {}; cat\"]\n",
                pager_marker.display()
            )
        } else {
            "planted-config-id\n".to_owned()
        };
        let marker_path = repository_path.join(marker);
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        fs::write(marker_path, contents).unwrap();
        let before = directory_snapshot(temp.path());

        let output = ds_with_env(
            temp.path(),
            temp.path(),
            &config,
            &["-R", &repository_name, "log"],
            &[
                ("XDG_CONFIG_HOME", config_home.to_str().unwrap()),
                ("HOME", home.to_str().unwrap()),
            ],
        );

        assert_eq!(
            output.status.code(),
            Some(1),
            "{marker}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("native repository is invalid"),
            "{marker}: {}",
            stderr(&output)
        );
        assert_eq!(directory_snapshot(temp.path()), before, "{marker}");
        assert!(!pager_marker.exists(), "{marker}");
    }
}

#[tokio::test]
async fn raw_native_repository_paths_are_not_product_repository_identities() {
    let temp = tempfile::tempdir().unwrap();
    let (repository_path, _) = catalog_repository(temp.path(), "machine-repo").await;
    let config = write_fixture_cli_config(temp.path());

    for (cwd, args) in [
        (repository_path.as_path(), vec!["log"]),
        (
            temp.path(),
            vec!["-R", repository_path.to_str().unwrap(), "log"],
        ),
    ] {
        let output = ds(cwd, &config, &args);
        assert_eq!(output.status.code(), Some(1));
        assert!(
            stderr(&output).contains("There is no jj repo in"),
            "{}",
            stderr(&output)
        );
    }
}

#[tokio::test]
async fn bare_repository_rejects_mutating_and_working_copy_commands() {
    let temp = tempfile::tempdir().unwrap();
    let repository_name = "machine-repo";
    let (repository_path, expected_operation) =
        catalog_repository(temp.path(), repository_name).await;
    let config = write_fixture_cli_config(temp.path());

    for command in ["new", "status"] {
        let output = ds(temp.path(), &config, &["-R", repository_name, command]);
        assert_eq!(output.status.code(), Some(1), "{command}");
        assert!(
            stderr(&output).contains(
                "Repository-targeted mode currently supports only read-only `log`; this command requires a checkout."
            ),
            "{command}: {}",
            stderr(&output)
        );
    }

    assert_eq!(operation_id(&repository_path).await, expected_operation);
    assert!(!repository_path.join(".jj").exists());
    assert!(!repository_path.join("working_copy").exists());
}

#[test]
fn normal_jj_workspaces_keep_stock_command_behavior() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let config = write_fixture_cli_config(temp.path());

    let init = ds(
        temp.path(),
        &config,
        &["git", "init", workspace.to_str().unwrap()],
    );
    assert!(init.status.success(), "{}", stderr(&init));
    assert!(workspace.join(".jj").is_dir());

    let new = ds(
        temp.path(),
        &config,
        &[
            "-R",
            workspace.to_str().unwrap(),
            "new",
            "-m",
            "stock workspace change",
        ],
    );
    assert!(new.status.success(), "{}", stderr(&new));

    let log = ds(
        temp.path(),
        &config,
        &[
            "-R",
            workspace.to_str().unwrap(),
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "description.first_line() ++ \"\\n\"",
        ],
    );
    assert!(log.status.success(), "{}", stderr(&log));
    assert_eq!(stdout(&log), "stock workspace change\n");

    let relative = ds(
        temp.path(),
        &config,
        &[
            "-R",
            "workspace",
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "description.first_line() ++ \"\\n\"",
        ],
    );
    assert!(relative.status.success(), "{}", stderr(&relative));
    assert_eq!(stdout(&relative), "stock workspace change\n");
}

#[test]
fn normal_workspace_marker_names_are_not_misclassified_from_cwd_or_repository() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let config = write_fixture_cli_config(temp.path());
    let init = ds(
        temp.path(),
        &config,
        &["git", "init", workspace.to_str().unwrap()],
    );
    assert!(init.status.success(), "{}", stderr(&init));
    add_bare_marker_collision(&workspace);

    for (cwd, args) in [
        (workspace.as_path(), vec!["log", "-r", "root()"]),
        (
            temp.path(),
            vec!["-R", workspace.to_str().unwrap(), "log", "-r", "root()"],
        ),
    ] {
        let output = ds(cwd, &config, &args);
        assert!(output.status.success(), "{}", stderr(&output));
    }
}

#[test]
fn normal_workspace_accepts_configured_watchman_backend() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let config = write_watchman_config(temp.path());
    let init = ds(
        temp.path(),
        &config,
        &["git", "init", workspace.to_str().unwrap()],
    );
    assert!(init.status.success(), "{}", stderr(&init));

    let status = ds(&workspace, &config, &["status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(
        !stderr(&status).contains("not compiled with the `watchman` feature"),
        "{}",
        stderr(&status)
    );
}
