use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use devspace_machine::{
    MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineRepository, RepositoryId,
    RepositoryIdentity, RepositoryIncarnation, RepositoryName, SharedSecret,
};

mod support;

use support::{ds, machine_store, settings, stderr, stdout, write_cli_config};

const DEVELOPMENT_SECRET: &str = "cli-development-secret";

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
    fs::write(checkout.join(".gitignore"), "*.env\n.dsprivate\n").unwrap();
    fs::write(checkout.join(".dsprivate"), "*.env\n").unwrap();
    fs::write(checkout.join("secret.env"), "private\n").unwrap();

    let tracked = ds(
        &checkout,
        &config,
        &["file", "list", ".dsprivate", "secret.env"],
    );
    assert!(tracked.status.success(), "{}", stderr(&tracked));
    assert_eq!(stdout(&tracked), ".dsprivate\nsecret.env\n");

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
    fs::write(checkout.join(".dsprivate"), "private/\n").unwrap();
    fs::create_dir(checkout.join("private")).unwrap();
    fs::write(checkout.join("private/secret"), "private\n").unwrap();

    let tracked = ds(&checkout, &config, &["file", "list", "private/secret"]);
    assert!(tracked.status.success(), "{}", stderr(&tracked));
    assert_eq!(stdout(&tracked), "private/secret\n");
}

#[tokio::test]
async fn nested_dsprivate_tracks_itself_and_a_gitignored_secret() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "nested-hidden-file").await;
    fs::write(
        checkout.join(".gitignore"),
        "sub/.dsprivate\nsub/secret.env\n",
    )
    .unwrap();
    fs::create_dir(checkout.join("sub")).unwrap();
    fs::write(checkout.join("sub/.dsprivate"), "secret.env\n").unwrap();
    fs::write(checkout.join("sub/secret.env"), "private\n").unwrap();

    let tracked = ds(
        &checkout,
        &config,
        &["file", "list", "sub/.dsprivate", "sub/secret.env"],
    );
    assert!(tracked.status.success(), "{}", stderr(&tracked));
    assert_eq!(stdout(&tracked), "sub/.dsprivate\nsub/secret.env\n");
}

#[tokio::test]
async fn gitignored_directory_is_not_searched_for_nested_dsprivate() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "ignored-policy-directory").await;
    fs::write(checkout.join(".gitignore"), "node_modules/\n").unwrap();
    fs::create_dir(checkout.join("node_modules")).unwrap();
    fs::write(
        checkout.join("node_modules/.dsprivate"),
        "generated-secret\n",
    )
    .unwrap();
    fs::write(checkout.join("node_modules/generated-secret"), "private\n").unwrap();

    let untracked = ds(
        &checkout,
        &config,
        &[
            "file",
            "list",
            "node_modules/.dsprivate",
            "node_modules/generated-secret",
        ],
    );
    assert!(untracked.status.success(), "{}", stderr(&untracked));
    assert_eq!(stdout(&untracked), "");
}

#[tokio::test]
async fn removing_hidden_pattern_keeps_file_tracked() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "hidden-removal").await;
    fs::write(checkout.join(".gitignore"), "secret.env\n").unwrap();
    fs::write(checkout.join(".dsprivate"), "secret.env\n").unwrap();
    fs::write(checkout.join("secret.env"), "private\n").unwrap();
    let tracked = ds(&checkout, &config, &["file", "list", "secret.env"]);
    assert!(tracked.status.success(), "{}", stderr(&tracked));
    assert_eq!(stdout(&tracked), "secret.env\n");

    fs::write(checkout.join(".dsprivate"), "").unwrap();
    let still_tracked = ds(&checkout, &config, &["file", "list", "secret.env"]);
    assert!(still_tracked.status.success(), "{}", stderr(&still_tracked));
    assert_eq!(stdout(&still_tracked), "secret.env\n");
}

#[tokio::test]
async fn missing_dsprivate_leaves_gitignored_files_untracked() {
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
    fs::write(plain.join(".gitignore"), ".dsprivate\nsecret.env\n").unwrap();
    fs::write(plain.join(".dsprivate"), "secret.env\n").unwrap();
    fs::write(plain.join("secret.env"), "private\n").unwrap();

    let untracked = ds(
        &plain,
        &config,
        &["file", "list", ".dsprivate", "secret.env"],
    );
    assert!(untracked.status.success(), "{}", stderr(&untracked));
    assert_eq!(stdout(&untracked), "");
}
