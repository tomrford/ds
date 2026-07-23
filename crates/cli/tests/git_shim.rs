#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use devspace_machine::{RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName};
use devspace_machine_git::MachineGitRepository as MachineRepository;

mod support;

use support::{
    configure_machine, ds, ds_command, machine_store, set_machine_git_shim, settings, stderr,
    write_cli_config,
};

async fn local_repository(root: &Path, name: &str) {
    let store = machine_store(root);
    if store.load_config().is_err() {
        configure_machine(root, "http://127.0.0.1:1");
    }
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
}

fn set_git_shim(config: &Path, enabled: bool) {
    let root = config.parent().unwrap();
    if machine_store(root).load_config().is_err() {
        configure_machine(root, "http://127.0.0.1:1");
    }
    set_machine_git_shim(root, enabled);
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

fn make_git_directories_writable(path: &Path) {
    let metadata = fs::symlink_metadata(path).unwrap();
    if metadata.is_dir() {
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0o700);
        fs::set_permissions(path, permissions).unwrap();
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                make_git_directories_writable(&entry.path());
            }
        }
    }
}

#[tokio::test]
async fn git_shim_is_off_by_default() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    local_repository(temp.path(), "shim-default-off").await;
    let checkout = temp.path().join("checkout");

    let added = add_checkout(temp.path(), &config, "shim-default-off", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(!checkout.join(".git").exists());

    fs::write(checkout.join("public.txt"), "visible\n").unwrap();
    let status = ds(&checkout, &config, &["status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(!checkout.join(".git").exists());
}

#[tokio::test]
async fn add_and_successful_checkout_commands_maintain_a_private_aware_read_only_index() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
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
async fn unchanged_tree_and_policy_do_not_refresh_the_index() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-noop").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-noop", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    let state_before = fs::read_to_string(checkout.join(".jj/devspace-git-shim.state")).unwrap();

    let output = ds_command(&checkout, &config)
        .env("JJ_LOG", "warn")
        .env("DEVSPACE_FAILPOINT", "git_shim_after_exclusion")
        .arg("status")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let state_after = fs::read_to_string(checkout.join(".jj/devspace-git-shim.state")).unwrap();
    assert!(
        !stderr(&output).contains("git_shim_after_exclusion"),
        "{}\nstate before: {state_before:?}\nstate after: {state_after:?}",
        stderr(&output)
    );
    assert_eq!(state_after, state_before);
    assert_git_directories_read_only(&checkout.join(".git"));
}

#[tokio::test]
async fn private_policy_change_invalidates_the_index() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-policy").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-policy", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));

    fs::write(checkout.join(".dsprivate"), "secret.txt\n").unwrap();
    fs::write(checkout.join("secret.txt"), "private\n").unwrap();
    let hidden = ds(&checkout, &config, &["status"]);
    assert!(hidden.status.success(), "{}", stderr(&hidden));
    assert_eq!(git_ls_files(&checkout), Vec::<String>::new());

    fs::write(checkout.join(".dsprivate"), "").unwrap();
    let public = ds(&checkout, &config, &["status"]);
    assert!(public.status.success(), "{}", stderr(&public));
    assert_eq!(git_ls_files(&checkout), ["secret.txt"]);
}

#[tokio::test]
async fn concurrent_commands_serialize_git_shim_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-concurrent").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-concurrent", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    fs::write(checkout.join("concurrent.txt"), "visible\n").unwrap();

    let children = (0..3)
        .map(|_| {
            let mut command = ds_command(&checkout, &config);
            command
                .env("JJ_LOG", "warn")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .arg("status")
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(
            !stderr(&output).contains("index.lock"),
            "{}",
            stderr(&output)
        );
    }
    assert_eq!(git_ls_files(&checkout), ["concurrent.txt"]);
    assert_git_directories_read_only(&checkout.join(".git"));
}

#[tokio::test]
async fn interrupted_refresh_is_repaired_by_the_next_enabled_command() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-recovery").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-recovery", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    fs::write(checkout.join("recovered.txt"), "visible\n").unwrap();

    let interrupted = ds_command(&checkout, &config)
        .env("JJ_LOG", "warn")
        .env("DEVSPACE_FAILPOINT", "git_shim_after_exclusion")
        .arg("status")
        .output()
        .unwrap();
    assert!(interrupted.status.success(), "{}", stderr(&interrupted));
    assert!(
        stderr(&interrupted).contains("git_shim_after_exclusion"),
        "{}",
        stderr(&interrupted)
    );
    assert!(!git_ls_files(&checkout).contains(&"recovered.txt".to_owned()));

    make_git_directories_writable(&checkout.join(".git"));
    fs::write(checkout.join(".git/index.lock"), "interrupted\n").unwrap();
    let repaired = ds(&checkout, &config, &["status"]);
    assert!(repaired.status.success(), "{}", stderr(&repaired));
    assert!(!checkout.join(".git/index.lock").exists());
    assert_eq!(git_ls_files(&checkout), ["recovered.txt"]);
    assert_git_directories_read_only(&checkout.join(".git"));
}

#[tokio::test]
async fn boundary_suppression_skips_git_shim_work() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    local_repository(temp.path(), "shim-suppressed").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-suppressed", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(!checkout.join(".git").exists());

    set_git_shim(&config, true);
    let listed = ds(&checkout, &config, &["list"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(!checkout.join(".git").exists());
}

#[tokio::test]
async fn checkout_removal_unlocks_a_legacy_shim_after_disabling() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-disabled-removal").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-disabled-removal", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    assert_git_directories_read_only(&checkout.join(".git"));

    set_git_shim(&config, false);
    fs::write(checkout.join("disabled.txt"), "not refreshed\n").unwrap();
    let status = ds(&checkout, &config, &["status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(!git_ls_files(&checkout).contains(&"disabled.txt".to_owned()));

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
    set_git_shim(&config, true);
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

#[tokio::test]
async fn pathspec_special_names_are_handled_literally() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-literal").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-literal", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));

    fs::write(checkout.join(".dsprivate"), ":colon-secret\nstar*secret\n").unwrap();
    fs::write(checkout.join(":colon-secret"), "private\n").unwrap();
    fs::write(checkout.join("star*secret"), "private\n").unwrap();
    fs::write(checkout.join(":colon-public"), "visible\n").unwrap();

    let refreshed = ds_command(&checkout, &config)
        .env("JJ_LOG", "warn")
        .arg("status")
        .output()
        .unwrap();
    assert!(refreshed.status.success(), "{}", stderr(&refreshed));
    assert!(
        !stderr(&refreshed).contains("git index shim refresh failed"),
        "{}",
        stderr(&refreshed)
    );
    assert_eq!(git_ls_files(&checkout), [":colon-public"]);
    assert_git_directories_read_only(&checkout.join(".git"));
}

#[tokio::test]
async fn hidden_path_with_newline_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-newline").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-newline", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));

    fs::write(checkout.join("public.txt"), "visible\n").unwrap();
    let refreshed = ds(&checkout, &config, &["status"]);
    assert!(refreshed.status.success(), "{}", stderr(&refreshed));
    assert_eq!(git_ls_files(&checkout), ["public.txt"]);

    // The pattern hides the file, but no info/exclude line can express its
    // name; the refresh must refuse rather than let `add -A` index it.
    fs::write(checkout.join(".dsprivate"), "bad*\n").unwrap();
    fs::write(checkout.join("bad\nname"), "private\n").unwrap();
    let refused = ds_command(&checkout, &config)
        .env("JJ_LOG", "warn")
        .arg("status")
        .output()
        .unwrap();
    assert!(refused.status.success(), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("cannot exclude"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(git_ls_files(&checkout), ["public.txt"]);
    assert_git_directories_read_only(&checkout.join(".git"));

    // Removing the unrepresentable path lets the next refresh recover.
    fs::remove_file(checkout.join("bad\nname")).unwrap();
    fs::write(checkout.join("later.txt"), "visible\n").unwrap();
    let recovered = ds(&checkout, &config, &["status"]);
    assert!(recovered.status.success(), "{}", stderr(&recovered));
    assert_eq!(git_ls_files(&checkout), ["later.txt", "public.txt"]);
}
