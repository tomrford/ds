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
async fn hidden_add_remove_list_round_trip_and_force_tracks_ignored_files() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "hidden-round-trip").await;
    fs::write(checkout.join(".gitignore"), "*.env\n").unwrap();
    fs::write(checkout.join(".dshide"), "# keep this comment\n").unwrap();
    fs::write(checkout.join("secret.env"), "private\n").unwrap();

    let added = ds(&checkout, &config, &["hidden", "add", "*.env"]);
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(!stderr(&added).contains("not covered by gitignore"));
    let listed = ds(&checkout, &config, &["hidden", "list"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert_eq!(stdout(&listed), "# keep this comment\n*.env\n");

    let tracked = ds(&checkout, &config, &["file", "list", "secret.env"]);
    assert!(tracked.status.success(), "{}", stderr(&tracked));
    assert_eq!(stdout(&tracked), "secret.env\n");

    let duplicate = ds(&checkout, &config, &["hidden", "add", "*.env"]);
    assert!(duplicate.status.success(), "{}", stderr(&duplicate));
    assert!(stderr(&duplicate).contains("already in .dshide"));
    assert_eq!(
        fs::read_to_string(checkout.join(".dshide")).unwrap(),
        "# keep this comment\n*.env\n"
    );

    let removed = ds(&checkout, &config, &["hidden", "remove", "*.env"]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(stderr(&removed).contains("eligible for future Git publication"));
    let listed = ds(&checkout, &config, &["hidden", "list"]);
    assert_eq!(stdout(&listed), "# keep this comment\n");
}

#[tokio::test]
async fn hidden_add_warns_when_matching_content_is_not_gitignored() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "hidden-warning").await;
    fs::write(checkout.join("secret.txt"), "private\n").unwrap();

    let added = ds(&checkout, &config, &["hidden", "add", "secret.txt"]);
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(stderr(&added).contains("not covered by gitignore"));
}

#[test]
fn hidden_commands_require_a_devspace_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let output = ds(temp.path(), &config, &["hidden", "list"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("There is no jj repo"));
}
