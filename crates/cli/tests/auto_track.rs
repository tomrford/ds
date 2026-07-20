use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use devspace_machine::{
    MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineRepository, MachineStore,
    RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName, SharedSecret,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::settings::UserSettings;

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

fn machine_store(root: &Path) -> MachineStore {
    MachineStore::new(root.join("machine-store"))
}

fn ds(cwd: &Path, config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ds"))
        .current_dir(cwd)
        .env(
            MACHINE_STORE_OVERRIDE,
            config.parent().unwrap().join("machine-store"),
        )
        .env("JJ_CONFIG", config)
        .env("DEVSPACE_BOUNDARY_SYNC", "0")
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

async fn checkout(root: &Path, config: &Path, name: &str) -> PathBuf {
    let store = machine_store(root);
    store
        .write_config(
            &MachineConfig::new(
                "http://127.0.0.1:1",
                MachineId::parse("12".repeat(16)).unwrap(),
                SharedSecret::new(DEVELOPMENT_SECRET).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
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
    let checkout = root.join("checkout");
    let output = Command::new(env!("CARGO_BIN_EXE_ds"))
        .current_dir(root)
        .env(MACHINE_STORE_OVERRIDE, root.join("machine-store"))
        .env("JJ_CONFIG", config)
        .env("DEVSPACE_BOUNDARY_SYNC", "0")
        .env("NO_COLOR", "1")
        .args(["add", name, "-r", "root()"])
        .arg(&checkout)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    checkout
}

#[tokio::test]
async fn ordinary_snapshot_tracks_gitignored_hidden_file_and_sealed_change() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "hidden-file").await;
    fs::write(checkout.join(".gitignore"), "*.env\n.dshide\n").unwrap();
    fs::write(checkout.join(".dshide"), "*.env\n").unwrap();
    fs::write(checkout.join("secret.env"), "private\n").unwrap();

    let tracked = ds(
        &checkout,
        &config,
        &["file", "list", ".dshide", "secret.env"],
    );
    assert!(tracked.status.success(), "{}", stderr(&tracked));
    assert_eq!(stdout(&tracked), ".dshide\nsecret.env\n");

    let sealed = ds(&checkout, &config, &["new", "-m", "after secret"]);
    assert!(sealed.status.success(), "{}", stderr(&sealed));
    let committed = ds(
        &checkout,
        &config,
        &["file", "list", "-r", "@-", "secret.env"],
    );
    assert!(committed.status.success(), "{}", stderr(&committed));
    assert_eq!(stdout(&committed), "secret.env\n");
}

#[tokio::test]
async fn ordinary_snapshot_tracks_files_beneath_hidden_directory() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "hidden-directory").await;
    fs::write(checkout.join(".gitignore"), "private/\n").unwrap();
    fs::write(checkout.join(".dshide"), "private/\n").unwrap();
    fs::create_dir(checkout.join("private")).unwrap();
    fs::write(checkout.join("private/secret"), "private\n").unwrap();

    let tracked = ds(&checkout, &config, &["file", "list", "private/secret"]);
    assert!(tracked.status.success(), "{}", stderr(&tracked));
    assert_eq!(stdout(&tracked), "private/secret\n");
}

#[tokio::test]
async fn removing_hidden_pattern_keeps_file_tracked() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "hidden-removal").await;
    fs::write(checkout.join(".gitignore"), "secret.env\n").unwrap();
    fs::write(checkout.join(".dshide"), "secret.env\n").unwrap();
    fs::write(checkout.join("secret.env"), "private\n").unwrap();
    let tracked = ds(&checkout, &config, &["file", "list", "secret.env"]);
    assert!(tracked.status.success(), "{}", stderr(&tracked));
    assert_eq!(stdout(&tracked), "secret.env\n");

    fs::write(checkout.join(".dshide"), "").unwrap();
    let still_tracked = ds(&checkout, &config, &["file", "list", "secret.env"]);
    assert!(still_tracked.status.success(), "{}", stderr(&still_tracked));
    assert_eq!(stdout(&still_tracked), "secret.env\n");
}

#[tokio::test]
async fn missing_dshide_leaves_gitignored_files_untracked() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "no-hidden-policy").await;
    fs::write(checkout.join(".gitignore"), "secret.env\n").unwrap();
    fs::write(checkout.join("secret.env"), "private\n").unwrap();

    let untracked = ds(&checkout, &config, &["file", "list", "secret.env"]);
    assert!(untracked.status.success(), "{}", stderr(&untracked));
    assert_eq!(stdout(&untracked), "");
}

#[test]
fn plain_jj_repository_gets_no_hidden_auto_tracking() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let plain = temp.path().join("plain");
    let initialized = ds(temp.path(), &config, &["git", "init", "plain"]);
    assert!(initialized.status.success(), "{}", stderr(&initialized));
    fs::write(plain.join(".gitignore"), ".dshide\nsecret.env\n").unwrap();
    fs::write(plain.join(".dshide"), "secret.env\n").unwrap();
    fs::write(plain.join("secret.env"), "private\n").unwrap();

    let untracked = ds(&plain, &config, &["file", "list", ".dshide", "secret.env"]);
    assert!(untracked.status.success(), "{}", stderr(&untracked));
    assert_eq!(stdout(&untracked), "");
}
