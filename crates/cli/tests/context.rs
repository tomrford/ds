#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use devspace_machine::{
    MachineRepository, RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName,
};

mod support;

use support::{
    commit_id, configure_machine, ds, ds_command, machine_store, settings, stderr, stdout,
    write_cli_config,
};

async fn owned_checkout(root: &Path, config: &Path, name: &str, checkout: &Path) {
    configure_machine(root, "http://127.0.0.1:1");
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
    add_checkout(root, config, name, "root()", checkout);
}

fn add_checkout(root: &Path, config: &Path, name: &str, revision: &str, checkout: &Path) {
    let output = ds_command(root, config)
        .args(["add", name, "-r", revision])
        .arg(checkout)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

fn context(cwd: &Path, config: &Path, cache: &Path, args: &[&str]) -> Output {
    ds_command(cwd, config)
        .env("DEVSPACE_CACHE_DIR", cache)
        .arg("context")
        .args(args)
        .output()
        .unwrap()
}

fn enable_git_shim(config: &Path) {
    let text = fs::read_to_string(config).unwrap();
    fs::write(config, format!("{text}\n[devspace]\ngit-shim = true\n")).unwrap();
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap()
}

fn git_fixture(root: &Path) -> (PathBuf, String) {
    let source = root.join("reference-source");
    fs::create_dir(&source).unwrap();
    let initialized = git(&source, &["init", "-q", "-b", "main"]);
    assert!(initialized.status.success(), "{}", stderr(&initialized));
    fs::write(source.join("README.md"), "reference\n").unwrap();
    fs::create_dir(source.join("nested")).unwrap();
    fs::write(source.join("nested/data.txt"), "data\n").unwrap();
    let added = git(&source, &["add", "-A"]);
    assert!(added.status.success(), "{}", stderr(&added));
    let committed = git(
        &source,
        &[
            "-c",
            "user.name=Devspace Test",
            "-c",
            "user.email=devspace@example.invalid",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
    );
    assert!(committed.status.success(), "{}", stderr(&committed));
    let head = git(&source, &["rev-parse", "HEAD"]);
    assert!(head.status.success(), "{}", stderr(&head));
    (source, stdout(&head).trim().to_owned())
}

fn git_ls_files(checkout: &Path) -> Vec<String> {
    let output = git(checkout, &["ls-files"]);
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output).lines().map(str::to_owned).collect()
}

#[tokio::test]
async fn add_sync_share_remove_and_gc_context_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    enable_git_shim(&config);
    let cache = temp.path().join("cache");
    let checkout_a = temp.path().join("checkout-a");
    owned_checkout(temp.path(), &config, "context-fixture", &checkout_a).await;
    fs::write(checkout_a.join(".gitignore"), ".repos/*\n!.repos/.lock\n").unwrap();
    let (source, head) = git_fixture(temp.path());
    let url = format!("file://{}", source.display());

    let added = context(
        &checkout_a,
        &config,
        &cache,
        &["add", "reference", "--url", &url],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(stdout(&added).starts_with("added reference -> "));
    assert_eq!(
        fs::read_to_string(checkout_a.join(".repos/.lock")).unwrap(),
        format!("[repos.reference]\nurl = {url:?}\nmode = \"default\"\ncommit = {head:?}\n")
    );

    let link_a = checkout_a.join(".repos/reference");
    assert!(
        fs::symlink_metadata(&link_a)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let snapshot_a = fs::read_link(&link_a).unwrap();
    assert!(snapshot_a.is_absolute());
    assert_eq!(
        fs::read_to_string(link_a.join("README.md")).unwrap(),
        "reference\n"
    );
    assert!(!link_a.join(".git").exists());
    assert_eq!(
        fs::metadata(&snapshot_a).unwrap().permissions().mode() & 0o222,
        0
    );
    assert_eq!(
        fs::metadata(snapshot_a.join("README.md"))
            .unwrap()
            .permissions()
            .mode()
            & 0o222,
        0
    );

    let snapshotted = ds(&checkout_a, &config, &["status"]);
    assert!(snapshotted.status.success(), "{}", stderr(&snapshotted));
    let listed = ds(
        &checkout_a,
        &config,
        &["file", "list", ".repos/.lock", ".repos/reference"],
    );
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert_eq!(stdout(&listed), ".repos/.lock\n");
    let shim_files = git_ls_files(&checkout_a);
    assert!(shim_files.contains(&".repos/.lock".to_owned()));
    assert!(!shim_files.contains(&".repos/reference".to_owned()));

    let revision = commit_id(&checkout_a, &config, "@");
    let checkout_b = temp.path().join("checkout-b");
    add_checkout(
        temp.path(),
        &config,
        "context-fixture",
        &revision,
        &checkout_b,
    );
    assert!(!checkout_b.join(".repos/reference").exists());
    let synced = context(&checkout_b, &config, &cache, &["sync"]);
    assert!(synced.status.success(), "{}", stderr(&synced));
    assert!(stdout(&synced).starts_with("synced reference -> "));
    let link_b = checkout_b.join(".repos/reference");
    assert_eq!(fs::read_link(&link_b).unwrap(), snapshot_a);

    for checkout in [&checkout_a, &checkout_b] {
        let removed = context(checkout, &config, &cache, &["remove", "reference"]);
        assert!(removed.status.success(), "{}", stderr(&removed));
        assert_eq!(stdout(&removed), "removed reference\n");
        assert!(!checkout.join(".repos/reference").exists());
    }

    let collected = context(&checkout_a, &config, &cache, &["gc", "--verbose"]);
    assert!(collected.status.success(), "{}", stderr(&collected));
    assert!(
        stdout(&collected).contains("deleted 1 snapshot(s), 1 remote(s), 0 stale root(s)"),
        "{}",
        stdout(&collected)
    );
    assert!(!snapshot_a.exists());
}

#[tokio::test]
async fn warns_when_context_aliases_are_not_ignored() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    enable_git_shim(&config);
    let cache = temp.path().join("cache");
    let checkout = temp.path().join("checkout");
    owned_checkout(temp.path(), &config, "warning-fixture", &checkout).await;
    fs::create_dir(checkout.join(".repos")).unwrap();
    fs::write(checkout.join(".repos/.lock"), "").unwrap();
    let (source, _) = git_fixture(temp.path());
    let url = format!("file://{}", source.display());

    let added = context(
        &checkout,
        &config,
        &cache,
        &["add", "reference", "--url", &url],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(stderr(&added).contains(".repos/ aliases are not ignored"));

    let snapshotted = ds(&checkout, &config, &["status"]);
    assert!(snapshotted.status.success(), "{}", stderr(&snapshotted));
    let listed = ds(&checkout, &config, &["file", "list", ".repos/reference"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert_eq!(stdout(&listed), ".repos/reference\n");
    assert!(git_ls_files(&checkout).contains(&".repos/reference".to_owned()));

    let removed = context(&checkout, &config, &cache, &["remove", "reference"]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    let collected = context(&checkout, &config, &cache, &["gc"]);
    assert!(collected.status.success(), "{}", stderr(&collected));
}

#[test]
fn refuses_context_commands_in_a_plain_jj_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let plain = temp.path().join("plain");
    let initialized = ds_command(temp.path(), &config)
        .args(["git", "init", "--no-colocate"])
        .arg(&plain)
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{}", stderr(&initialized));

    let output = context(&plain, &config, &temp.path().join("cache"), &["init"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("available only inside a Devspace checkout"),
        "{}",
        stderr(&output)
    );
    assert!(!plain.join(".repos").exists());
}

#[test]
fn context_help_keeps_the_v1_subcommand_surface() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let output = ds(temp.path(), &config, &["context", "--help"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let help = stdout(&output);
    for command in ["init", "add", "list", "sync", "update", "remove", "gc"] {
        assert!(help.contains(command), "missing {command} in:\n{help}");
    }
}

#[tokio::test]
async fn context_rejects_ext_transport_without_running_it() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let cache = temp.path().join("cache");
    let checkout = temp.path().join("checkout");
    owned_checkout(temp.path(), &config, "ext-transport", &checkout).await;
    fs::write(checkout.join(".gitignore"), ".repos/*\n!.repos/.lock\n").unwrap();
    let marker = temp.path().join("ext-ran");
    let script = temp.path().join("ext-helper");
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let url = format!("ext::{}", script.display());

    let output = context(
        &checkout,
        &config,
        &cache,
        &["add", "reference", "--url", &url],
    );

    assert!(!output.status.success());
    assert!(!marker.exists());
    assert!(
        stderr(&output).contains("transport 'ext' not allowed"),
        "{}",
        stderr(&output)
    );
}

#[tokio::test]
async fn context_list_redacts_url_userinfo() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let cache = temp.path().join("cache");
    let checkout = temp.path().join("checkout");
    owned_checkout(temp.path(), &config, "url-redaction", &checkout).await;
    fs::create_dir(checkout.join(".repos")).unwrap();
    fs::write(
        checkout.join(".repos/.lock"),
        "[repos.reference]\nurl = \"https://user:secret@example.test/repository\"\nmode = \"default\"\n",
    )
    .unwrap();

    let output = context(&checkout, &config, &cache, &["list"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains("secret"));
    assert!(stdout(&output).contains("https://example.test/repository"));
}
