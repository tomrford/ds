use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use devspace_machine::{
    CatalogEntry, MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineRepository,
    MachineStore, MachineSyncStore, RepositoryId, RepositoryIdentity, RepositoryIncarnation,
    RepositoryName, SharedSecret,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::object_id::ObjectId as _;
use jj_lib::settings::UserSettings;

const DEVELOPMENT_SECRET: &str = "cli-development-secret";
const FIRST_MACHINE_ID: &str = "12121212121212121212121212121212";
const SECOND_MACHINE_ID: &str = "34343434343434343434343434343434";

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

fn machine_store(root: &Path) -> MachineStore {
    MachineStore::new(root.join("machine-store"))
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

fn ds(cwd: &Path, home: &Path, config: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ds"));
    command
        .current_dir(cwd)
        .env(MACHINE_STORE_OVERRIDE, home.join("machine-store"))
        .env("JJ_CONFIG", config)
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

async fn local_repository(root: &Path, name: &str, base_url: &str) -> CatalogEntry {
    configure_machine(root, base_url, FIRST_MACHINE_ID, DEVELOPMENT_SECRET);
    let store = machine_store(root);
    let entry = store
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
    entry
}

#[tokio::test]
async fn sync_run_skips_when_the_repository_lock_is_held() {
    let temp = tempfile::tempdir().unwrap();
    let entry = local_repository(temp.path(), "locked", "http://127.0.0.1:1").await;
    let config = write_cli_config(temp.path());
    let _guard = machine_store(temp.path())
        .try_lock_repository_sync(&entry.identity)
        .unwrap();

    let output = ds(
        temp.path(),
        temp.path(),
        &config,
        &["sync", "run", "--repository", "locked"],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).is_empty());
    assert_eq!(
        stderr(&output),
        "Repository `locked` is already being synchronized; skipping.\n"
    );
}

#[test]
fn sync_run_rejects_an_unknown_repository_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(
        temp.path(),
        "http://127.0.0.1:1",
        FIRST_MACHINE_ID,
        DEVELOPMENT_SECRET,
    );
    let config = write_cli_config(temp.path());

    let output = ds(
        temp.path(),
        temp.path(),
        &config,
        &["sync", "run", "--repository", "missing-repository"],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output)
            .contains("Repository `missing-repository` is not present in this machine store."),
        "{}",
        stderr(&output)
    );
}

#[test]
fn sync_run_points_an_incomplete_clone_back_to_add() {
    let temp = tempfile::tempdir().unwrap();
    configure_machine(
        temp.path(),
        "http://127.0.0.1:1",
        FIRST_MACHINE_ID,
        DEVELOPMENT_SECRET,
    );
    let store = machine_store(temp.path());
    store
        .register_repository(
            RepositoryName::parse("incomplete").unwrap(),
            RepositoryIdentity::new(
                RepositoryId::parse("ab".repeat(32)).unwrap(),
                RepositoryIncarnation::parse("cd".repeat(16)).unwrap(),
            ),
        )
        .unwrap();
    let config = write_cli_config(temp.path());

    let output = ds(
        temp.path(),
        temp.path(),
        &config,
        &["sync", "run", "--repository", "incomplete"],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains(
            "Repository `incomplete` has an incomplete clone; run `ds add` again to finish it."
        ),
        "{}",
        stderr(&output)
    );
}

#[tokio::test]
async fn sync_run_reports_offline_transport_failure_without_mutating_durable_state() {
    let temp = tempfile::tempdir().unwrap();
    let entry = local_repository(temp.path(), "offline", "http://127.0.0.1:1").await;
    let store = machine_store(temp.path());
    let config = write_cli_config(temp.path());

    drop(store.try_lock_repository_sync(&entry.identity).unwrap());
    let sync_store = MachineSyncStore::open(store.repository_sync_path(&entry.identity)).unwrap();
    drop(sync_store.lock().unwrap());
    let before = snapshot_files(store.root());
    let operation_before = MachineRepository::open(&entry.native_repository_path, &settings())
        .await
        .unwrap()
        .repo()
        .op_id()
        .clone();

    let output = ds(
        temp.path(),
        temp.path(),
        &config,
        &["sync", "run", "--repository", "offline"],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("error sending request"),
        "{}",
        stderr(&output)
    );
    assert_eq!(snapshot_files(store.root()), before);
    let repository = MachineRepository::open(&entry.native_repository_path, &settings())
        .await
        .unwrap();
    assert_eq!(repository.repo().op_id(), &operation_before);
    assert!(sync_store.load_outbox().unwrap().is_none());
}

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

fn seal_commit(cwd: &Path, home: &Path, config: &Path, description: &str) {
    let described = ds(cwd, home, config, &["describe", "-m", description]);
    assert!(described.status.success(), "{}", stderr(&described));
    let sealed = ds(cwd, home, config, &["new"]);
    assert!(sealed.status.success(), "{}", stderr(&sealed));
}

fn commit_id(cwd: &Path, home: &Path, config: &Path, revision: &str) -> String {
    let output = ds(
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

fn repository_commit_ids(home: &Path, config: &Path, name: &str) -> Vec<String> {
    let output = ds(
        home,
        home,
        config,
        &[
            "-R",
            name,
            "log",
            "-r",
            "all()",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output).lines().map(str::to_owned).collect()
}

async fn operation_heads(repository_path: &Path) -> Vec<String> {
    let repository = MachineRepository::open(repository_path, &settings())
        .await
        .unwrap();
    repository
        .repo()
        .op_heads_store()
        .get_op_heads()
        .await
        .unwrap()
        .into_iter()
        .map(|head| head.hex())
        .collect()
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
    format!("sync-live-{}-{suffix}", std::process::id())
}

fn snapshot_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }

    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}
