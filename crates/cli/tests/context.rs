#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use devspace_machine::MachineGitRepository as MachineRepository;
use devspace_machine::{RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName};

mod support;

use support::{
    commit_id, configure_machine, ds, ds_command, machine_store, set_machine_git_shim, settings,
    stderr, stdout, write_cli_config,
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

fn enable_git_shim(root: &Path, config: &Path, checkout: &Path) {
    set_machine_git_shim(root, true);
    let refreshed = ds(checkout, config, &["status"]);
    assert!(refreshed.status.success(), "{}", stderr(&refreshed));
}

fn set_context_auto_sync(root: &Path, config: &Path, enabled: bool) {
    let configured = ds(
        root,
        config,
        &[
            "config",
            "set",
            "context.auto-sync",
            if enabled { "true" } else { "false" },
        ],
    );
    assert!(configured.status.success(), "{}", stderr(&configured));
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
    let cache = temp.path().join("cache");
    let checkout_a = temp.path().join("checkout-a");
    owned_checkout(temp.path(), &config, "context-fixture", &checkout_a).await;
    enable_git_shim(temp.path(), &config, &checkout_a);
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
async fn context_auto_sync_follows_working_copy_movements_when_enabled() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let cache = temp.path().join("cache");
    let source_checkout = temp.path().join("source-checkout");
    owned_checkout(
        temp.path(),
        &config,
        "context-auto-sync-movement",
        &source_checkout,
    )
    .await;
    fs::write(
        source_checkout.join(".gitignore"),
        ".repos/*\n!.repos/.lock\n",
    )
    .unwrap();
    let (source, _) = git_fixture(temp.path());
    let url = format!("file://{}", source.display());
    let added_context = context(
        &source_checkout,
        &config,
        &cache,
        &["add", "reference", "--url", &url],
    );
    assert!(added_context.status.success(), "{}", stderr(&added_context));
    let snapshot = fs::read_link(source_checkout.join(".repos/reference")).unwrap();
    let snapshotted = ds(&source_checkout, &config, &["status"]);
    assert!(snapshotted.status.success(), "{}", stderr(&snapshotted));
    let revision = commit_id(&source_checkout, &config, "@");

    let disabled_checkout = temp.path().join("disabled-checkout");
    let disabled = ds_command(temp.path(), &config)
        .env("DEVSPACE_CACHE_DIR", &cache)
        .args(["add", "context-auto-sync-movement", "-r", &revision])
        .arg(&disabled_checkout)
        .output()
        .unwrap();
    assert!(disabled.status.success(), "{}", stderr(&disabled));
    assert!(!disabled_checkout.join(".repos/reference").exists());
    assert!(!stderr(&disabled).contains("context auto-sync:"));

    set_context_auto_sync(temp.path(), &config, true);
    let enabled_checkout = temp.path().join("enabled-checkout");
    let enabled = ds_command(temp.path(), &config)
        .env("DEVSPACE_CACHE_DIR", &cache)
        .args(["add", "context-auto-sync-movement", "-r", &revision])
        .arg(&enabled_checkout)
        .output()
        .unwrap();
    assert!(enabled.status.success(), "{}", stderr(&enabled));
    assert_eq!(
        fs::read_link(enabled_checkout.join(".repos/reference")).unwrap(),
        snapshot
    );
    assert_eq!(
        stderr(&enabled)
            .matches("context auto-sync: synced reference")
            .count(),
        1,
        "{}",
        stderr(&enabled)
    );

    fs::remove_file(enabled_checkout.join(".repos/reference")).unwrap();
    let read_only = ds_command(&enabled_checkout, &config)
        .env("DEVSPACE_CACHE_DIR", &cache)
        .arg("status")
        .output()
        .unwrap();
    assert!(read_only.status.success(), "{}", stderr(&read_only));
    assert!(!enabled_checkout.join(".repos/reference").exists());
    assert!(!stderr(&read_only).contains("context auto-sync:"));

    let restored = context(&enabled_checkout, &config, &cache, &["sync"]);
    assert!(restored.status.success(), "{}", stderr(&restored));
    assert_eq!(
        fs::read_link(enabled_checkout.join(".repos/reference")).unwrap(),
        snapshot
    );

    let moved_away = ds_command(&enabled_checkout, &config)
        .env("DEVSPACE_CACHE_DIR", &cache)
        .args(["new", "root()"])
        .output()
        .unwrap();
    assert!(moved_away.status.success(), "{}", stderr(&moved_away));
    assert!(!enabled_checkout.join(".repos/.lock").exists());
    assert!(!enabled_checkout.join(".repos/reference").exists());
    assert!(stderr(&moved_away).contains("context auto-sync: removed"));

    let moved_back = ds_command(&enabled_checkout, &config)
        .env("DEVSPACE_CACHE_DIR", &cache)
        .args(["new", &revision])
        .output()
        .unwrap();
    assert!(moved_back.status.success(), "{}", stderr(&moved_back));
    assert_eq!(
        fs::read_link(enabled_checkout.join(".repos/reference")).unwrap(),
        snapshot
    );
    assert_eq!(
        stderr(&moved_back)
            .matches("context auto-sync: synced reference")
            .count(),
        1,
        "{}",
        stderr(&moved_back)
    );
}

#[tokio::test]
async fn context_auto_sync_preserves_a_tracked_symlink_in_a_lockless_destination() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let cache = temp.path().join("cache");
    let checkout = temp.path().join("checkout");
    owned_checkout(
        temp.path(),
        &config,
        "context-auto-sync-tracked-symlink",
        &checkout,
    )
    .await;
    fs::write(checkout.join(".gitignore"), ".repos/*\n!.repos/.lock\n").unwrap();
    let (source, _) = git_fixture(temp.path());
    let url = format!("file://{}", source.display());
    let added_context = context(
        &checkout,
        &config,
        &cache,
        &["add", "reference", "--url", &url],
    );
    assert!(added_context.status.success(), "{}", stderr(&added_context));
    let snapshot = fs::read_link(checkout.join(".repos/reference")).unwrap();
    let snapshotted = ds(&checkout, &config, &["status"]);
    assert!(snapshotted.status.success(), "{}", stderr(&snapshotted));
    let context_revision = commit_id(&checkout, &config, "@");

    let moved_to_root = ds(&checkout, &config, &["new", "root()"]);
    assert!(moved_to_root.status.success(), "{}", stderr(&moved_to_root));
    assert!(!checkout.join(".repos/.lock").exists());
    assert_eq!(
        fs::read_link(checkout.join(".repos/reference")).unwrap(),
        snapshot
    );
    fs::remove_file(checkout.join(".repos/.gitignore")).unwrap();
    let tracked = ds(&checkout, &config, &["status"]);
    assert!(tracked.status.success(), "{}", stderr(&tracked));
    let tracked_revision = commit_id(&checkout, &config, "@");
    let listed = ds(&checkout, &config, &["file", "list", ".repos/reference"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert_eq!(stdout(&listed), ".repos/reference\n");

    let returned = ds(&checkout, &config, &["new", &context_revision]);
    assert!(returned.status.success(), "{}", stderr(&returned));
    let restored = context(&checkout, &config, &cache, &["sync"]);
    assert!(restored.status.success(), "{}", stderr(&restored));
    set_context_auto_sync(temp.path(), &config, true);

    let moved_to_tracked = ds_command(&checkout, &config)
        .env("DEVSPACE_CACHE_DIR", &cache)
        .args(["new", &tracked_revision])
        .output()
        .unwrap();
    assert!(
        moved_to_tracked.status.success(),
        "{}",
        stderr(&moved_to_tracked)
    );
    assert!(!checkout.join(".repos/.lock").exists());
    assert_eq!(
        fs::read_link(checkout.join(".repos/reference")).unwrap(),
        snapshot
    );
    assert!(stderr(&moved_to_tracked).contains("context auto-sync: removed"));
    let listed = ds(&checkout, &config, &["file", "list", ".repos/reference"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert_eq!(stdout(&listed), ".repos/reference\n");
}

#[tokio::test]
async fn context_auto_sync_warns_after_moving_away_from_an_invalid_lock() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("checkout");
    owned_checkout(
        temp.path(),
        &config,
        "context-auto-sync-invalid-previous-lock",
        &checkout,
    )
    .await;
    fs::write(checkout.join(".gitignore"), ".repos/*\n!.repos/.lock\n").unwrap();
    fs::create_dir(checkout.join(".repos")).unwrap();
    fs::write(
        checkout.join(".repos/.lock"),
        "[repos.reference]\nurl = \"https://user:secret@example.invalid/repository\"\nmode = [\n",
    )
    .unwrap();
    let local_target = temp.path().join("machine-local-reference");
    symlink(&local_target, checkout.join(".repos/reference")).unwrap();
    let snapshotted = ds(&checkout, &config, &["status"]);
    assert!(snapshotted.status.success(), "{}", stderr(&snapshotted));
    set_context_auto_sync(temp.path(), &config, true);

    let moved = ds(&checkout, &config, &["new", "root()"]);
    assert!(moved.status.success(), "{}", stderr(&moved));
    assert!(!checkout.join(".repos/.lock").exists());
    assert_eq!(
        fs::read_link(checkout.join(".repos/reference")).unwrap(),
        local_target
    );
    assert!(
        stderr(&moved).contains("could not clear context before working-copy movement")
            && stderr(&moved).contains("context state may be inconsistent")
            && stderr(&moved).contains("rewrite or rebuild it manually"),
        "{}",
        stderr(&moved)
    );
    assert!(!stderr(&moved).contains("secret"), "{}", stderr(&moved));
}

#[tokio::test]
async fn context_auto_sync_clear_leaves_a_user_retargeted_symlink_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let cache = temp.path().join("cache");
    let checkout = temp.path().join("checkout");
    owned_checkout(
        temp.path(),
        &config,
        "context-auto-sync-user-symlink",
        &checkout,
    )
    .await;
    fs::write(checkout.join(".gitignore"), ".repos/*\n!.repos/.lock\n").unwrap();
    let (source, _) = git_fixture(temp.path());
    let url = format!("file://{}", source.display());
    let added_context = context(
        &checkout,
        &config,
        &cache,
        &["add", "reference", "--url", &url],
    );
    assert!(added_context.status.success(), "{}", stderr(&added_context));
    let snapshotted = ds(&checkout, &config, &["status"]);
    assert!(snapshotted.status.success(), "{}", stderr(&snapshotted));

    fs::remove_file(checkout.join(".repos/reference")).unwrap();
    let user_target = temp.path().join("user-owned-reference");
    symlink(&user_target, checkout.join(".repos/reference")).unwrap();
    set_context_auto_sync(temp.path(), &config, true);

    let moved = ds_command(&checkout, &config)
        .env("DEVSPACE_CACHE_DIR", &cache)
        .args(["new", "root()"])
        .output()
        .unwrap();
    assert!(moved.status.success(), "{}", stderr(&moved));
    assert_eq!(
        fs::read_link(checkout.join(".repos/reference")).unwrap(),
        user_target
    );
    assert!(
        stderr(&moved).contains("left")
            && stderr(&moved).contains("target is not the snapshot recorded"),
        "{}",
        stderr(&moved)
    );
}

#[tokio::test]
async fn context_auto_sync_warns_when_the_destination_lock_is_invalid() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let source_checkout = temp.path().join("source-checkout");
    owned_checkout(
        temp.path(),
        &config,
        "context-auto-sync-invalid-destination-lock",
        &source_checkout,
    )
    .await;
    fs::create_dir(source_checkout.join(".repos")).unwrap();
    fs::write(
        source_checkout.join(".repos/.lock"),
        "[repos.reference]\nurl = \"https://user:secret@example.invalid/repository\"\nmode = [\n",
    )
    .unwrap();
    let snapshotted = ds(&source_checkout, &config, &["status"]);
    assert!(snapshotted.status.success(), "{}", stderr(&snapshotted));
    let revision = commit_id(&source_checkout, &config, "@");
    set_context_auto_sync(temp.path(), &config, true);

    let destination = temp.path().join("destination");
    let added = ds_command(temp.path(), &config)
        .env("DEVSPACE_CACHE_DIR", temp.path().join("cache"))
        .args([
            "add",
            "context-auto-sync-invalid-destination-lock",
            "-r",
            &revision,
        ])
        .arg(&destination)
        .output()
        .unwrap();
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(
        stderr(&added).contains("could not sync context")
            && stderr(&added).contains("context state may be inconsistent")
            && stderr(&added).contains("rewrite or rebuild it manually"),
        "{}",
        stderr(&added)
    );
    assert!(!stderr(&added).contains("secret"), "{}", stderr(&added));
}

#[tokio::test]
async fn context_auto_sync_failure_warns_without_failing_transition_or_polluting_json() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let cache = temp.path().join("cache");
    let source_checkout = temp.path().join("source-checkout");
    owned_checkout(
        temp.path(),
        &config,
        "context-auto-sync-warning",
        &source_checkout,
    )
    .await;
    fs::create_dir(source_checkout.join(".repos")).unwrap();
    fs::write(
        source_checkout.join(".repos/.lock"),
        "[repos.reference]\nurl = \"https://user:secret@127.0.0.1:1/repository\"\nmode = \"default\"\n",
    )
    .unwrap();

    let manual = ds_command(&source_checkout, &config)
        .env("DEVSPACE_CACHE_DIR", &cache)
        .args(["context", "sync"])
        .output()
        .unwrap();
    assert!(!manual.status.success());
    assert!(stdout(&manual).is_empty(), "{}", stdout(&manual));
    assert!(
        stderr(&manual).contains("Warning: failed to sync reference"),
        "{}",
        stderr(&manual)
    );
    assert!(
        stderr(&manual).contains(".repos/ aliases are not ignored"),
        "{}",
        stderr(&manual)
    );
    assert!(!stderr(&manual).contains("secret"), "{}", stderr(&manual));

    let snapshotted = ds(&source_checkout, &config, &["status"]);
    assert!(snapshotted.status.success(), "{}", stderr(&snapshotted));
    let revision = commit_id(&source_checkout, &config, "@");
    set_context_auto_sync(temp.path(), &config, true);

    let created_checkout = temp.path().join("created-checkout");
    let added = ds_command(temp.path(), &config)
        .env("DEVSPACE_CACHE_DIR", &cache)
        .args(["add", "context-auto-sync-warning", "-r", &revision])
        .arg(&created_checkout)
        .arg("--json")
        .output()
        .unwrap();

    assert!(added.status.success(), "{}", stderr(&added));
    serde_json::from_slice::<serde_json::Value>(&added.stdout).unwrap();
    assert!(
        fs::symlink_metadata(created_checkout.join(".repos"))
            .unwrap()
            .is_dir()
    );
    assert!(
        fs::symlink_metadata(created_checkout.join(".repos/.lock"))
            .unwrap()
            .is_file()
    );
    assert!(
        stderr(&added).contains("context auto-sync: warning: failed to sync reference")
            && stderr(&added).contains("completed with warnings")
            && stderr(&added).contains(".repos/ aliases are not ignored"),
        "{}",
        stderr(&added)
    );
    assert!(!stderr(&added).contains("secret"), "{}", stderr(&added));
}

#[tokio::test]
async fn context_auto_sync_does_not_follow_a_symlinked_project_directory() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let source_checkout = temp.path().join("source-checkout");
    owned_checkout(
        temp.path(),
        &config,
        "context-auto-sync-symlink",
        &source_checkout,
    )
    .await;
    fs::write(source_checkout.join(".lock"), "").unwrap();
    fs::write(source_checkout.join("target"), "target\n").unwrap();
    symlink("target", source_checkout.join("keep")).unwrap();
    symlink(".", source_checkout.join(".repos")).unwrap();
    let snapshotted = ds(&source_checkout, &config, &["status"]);
    assert!(snapshotted.status.success(), "{}", stderr(&snapshotted));
    let revision = commit_id(&source_checkout, &config, "@");
    set_context_auto_sync(temp.path(), &config, true);

    let created_checkout = temp.path().join("created-checkout");
    let added = ds_command(temp.path(), &config)
        .env("DEVSPACE_CACHE_DIR", temp.path().join("cache"))
        .args(["add", "context-auto-sync-symlink", "-r", &revision])
        .arg(&created_checkout)
        .output()
        .unwrap();

    assert!(added.status.success(), "{}", stderr(&added));
    assert!(
        fs::symlink_metadata(created_checkout.join("keep"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!stderr(&added).contains("context auto-sync:"));
}

#[tokio::test]
async fn warns_when_context_aliases_are_not_ignored() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let cache = temp.path().join("cache");
    let checkout = temp.path().join("checkout");
    owned_checkout(temp.path(), &config, "warning-fixture", &checkout).await;
    enable_git_shim(temp.path(), &config, &checkout);
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
fn context_help_lists_the_supported_subcommands() {
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
