#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output};

use devspace_machine::{
    MachineConfig, MachineId, MachineRepository, RepositoryId, RepositoryIdentity,
    RepositoryIncarnation, RepositoryName, SharedSecret,
};

mod support;

use support::{ds, ds_command, machine_store, settings, stderr, write_cli_config};

const DEVELOPMENT_SECRET: &str = "cli-development-secret";

async fn local_repository(root: &Path, name: &str) {
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
}

fn add_checkout(root: &Path, config: &Path, name: &str, checkout: &Path) -> Output {
    ds_command(root, config)
        .args(["add", name, "-r", "root()"])
        .arg(checkout)
        .output()
        .unwrap()
}

fn git_ls_files(checkout: &Path) -> Vec<String> {
    let output = Command::new("git")
        .current_dir(checkout)
        .args(["ls-files"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn assert_git_directories_read_only(path: &Path) {
    let metadata = fs::symlink_metadata(path).unwrap();
    assert_eq!(
        metadata.permissions().mode() & 0o222,
        0,
        "{} is writable",
        path.display()
    );
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            assert_git_directories_read_only(&entry.path());
        }
    }
}

#[tokio::test]
async fn add_and_successful_checkout_commands_maintain_a_private_aware_read_only_index() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    local_repository(temp.path(), "shim-contract").await;
    let checkout = temp.path().join("checkout");

    let added = add_checkout(temp.path(), &config, "shim-contract", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(checkout.join(".git").is_dir());

    fs::write(checkout.join(".gitignore"), "ignored-dir/\n").unwrap();
    fs::create_dir(checkout.join("ignored-dir")).unwrap();
    fs::write(checkout.join("ignored-dir/generated"), "ignored\n").unwrap();
    fs::write(checkout.join("normal.txt"), "visible\n").unwrap();
    fs::write(checkout.join(".dsprivate"), "secret.txt\nprivate-dir/\n").unwrap();
    fs::write(checkout.join("secret.txt"), "private\n").unwrap();
    fs::create_dir(checkout.join("private-dir")).unwrap();
    fs::write(checkout.join("private-dir/nested"), "private\n").unwrap();
    fs::create_dir(checkout.join("sub")).unwrap();
    fs::write(checkout.join("sub/.dsprivate"), "nested-secret\n").unwrap();
    fs::write(checkout.join("sub/nested-secret"), "private\n").unwrap();
    fs::write(checkout.join("sub/public.txt"), "visible\n").unwrap();

    let refreshed = ds(&checkout, &config, &["status"]);
    assert!(refreshed.status.success(), "{}", stderr(&refreshed));
    assert_eq!(
        git_ls_files(&checkout),
        [".gitignore", "normal.txt", "sub/public.txt"]
    );
    let exclude = fs::read_to_string(checkout.join(".git/info/exclude")).unwrap();
    for pattern in [
        ".jj/",
        "/.dsprivate",
        "/private-dir",
        "/secret.txt",
        "/sub/.dsprivate",
        "/sub/nested-secret",
    ] {
        assert!(exclude.lines().any(|line| line == pattern), "{exclude}");
    }
    assert_git_directories_read_only(&checkout.join(".git"));

    fs::write(checkout.join("next.txt"), "visible\n").unwrap();
    let next = ds(&checkout, &config, &["status"]);
    assert!(next.status.success(), "{}", stderr(&next));
    assert!(git_ls_files(&checkout).contains(&"next.txt".to_owned()));

    fs::write(
        checkout.join(".dsprivate"),
        "secret.txt\nprivate-dir/\nnormal.txt\n",
    )
    .unwrap();
    let newly_private = ds(&checkout, &config, &["status"]);
    assert!(newly_private.status.success(), "{}", stderr(&newly_private));
    assert!(!git_ls_files(&checkout).contains(&"normal.txt".to_owned()));
    assert_git_directories_read_only(&checkout.join(".git"));

    let removed = ds_command(temp.path(), &config)
        .arg("remove")
        .arg(&checkout)
        .output()
        .unwrap();
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(!checkout.exists());
}

#[tokio::test]
async fn plain_jj_checkout_never_gets_a_git_shim() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("plain-jj");
    let initialized = ds_command(temp.path(), &config)
        .args(["git", "init", "--no-colocate"])
        .arg(&checkout)
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{}", stderr(&initialized));
    assert!(!checkout.join(".git").exists());

    let status = ds(&checkout, &config, &["status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(!checkout.join(".git").exists());
}

#[tokio::test]
async fn missing_git_warns_without_failing_add() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    local_repository(temp.path(), "missing-git").await;
    let checkout = temp.path().join("checkout");
    let empty_path = temp.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();

    let output = ds_command(temp.path(), &config)
        .env("PATH", &empty_path)
        .env("JJ_LOG", "warn")
        .args(["add", "missing-git", "-r", "root()"])
        .arg(&checkout)
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(checkout.is_dir());
    assert!(!checkout.join(".git").exists());
    assert!(
        stderr(&output).contains("git index shim refresh failed"),
        "{}",
        stderr(&output)
    );
}
