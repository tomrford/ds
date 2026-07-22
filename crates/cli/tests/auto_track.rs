use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use devspace_machine::{
    MACHINE_STORE_OVERRIDE, MachineRepository, RepositoryId, RepositoryIdentity,
    RepositoryIncarnation, RepositoryName,
};

mod support;

use support::{
    commit_id, configure_machine, ds, ds_with_env, machine_store, set_machine_git_shim, settings,
    stderr, stdout, write_cli_config,
};

async fn checkout(root: &Path, config: &Path, name: &str) -> PathBuf {
    configure_machine(root, "http://127.0.0.1:1");
    set_machine_git_shim(root, true);
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

fn git_files(checkout: &Path) -> String {
    let output = Command::new("git")
        .current_dir(checkout)
        .args(["ls-files"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output)
}

#[cfg(unix)]
fn assert_git_directories_are_read_only(checkout: &Path) {
    let mut pending = vec![checkout.join(".git")];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() {
            assert_eq!(
                metadata.permissions().mode() & 0o222,
                0,
                "{} is writable",
                path.display()
            );
            pending.extend(
                fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        }
    }
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
async fn globally_ignored_paths_stay_out_of_the_git_shim() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "global-ignored-parent").await;

    // Make one private tree canonical before its parent becomes globally
    // ignored. jj must keep tracking it, while the shim must retain only its
    // public path.
    fs::create_dir(checkout.join("vendored")).unwrap();
    fs::write(checkout.join("vendored/.dsprivate"), "secret.env\n").unwrap();
    fs::write(checkout.join("vendored/secret.env"), "private\n").unwrap();
    fs::write(checkout.join("vendored/public.txt"), "public\n").unwrap();
    fs::write(checkout.join("public.txt"), "public\n").unwrap();
    let initially_tracked = ds(&checkout, &config, &["status"]);
    assert!(
        initially_tracked.status.success(),
        "{}",
        stderr(&initially_tracked)
    );

    // A global excludes file is part of jj's snapshot base ignores; hidden
    // discovery and the Git shim must resolve the same chain without allowing
    // Git to read ambient configuration itself.
    let global_ignore = temp.path().join("global-ignore");
    fs::write(&global_ignore, "vendored/\nignored-untracked/\n").unwrap();
    let global_config = temp.path().join("global-gitconfig");
    fs::write(
        &global_config,
        format!("[core]\n\texcludesFile = {}\n", global_ignore.display()),
    )
    .unwrap();
    let environment = [
        ("GIT_CONFIG_GLOBAL", global_config.to_str().unwrap()),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ];

    // An ignored untracked parent must not acquire policy power or enter the
    // shim. An ordinary sibling proves hidden discovery still works elsewhere.
    for parent in ["ignored-untracked", "app"] {
        fs::create_dir(checkout.join(parent)).unwrap();
        fs::write(checkout.join(parent).join(".dsprivate"), "secret.env\n").unwrap();
        fs::write(checkout.join(parent).join("secret.env"), "private\n").unwrap();
        fs::write(checkout.join(parent).join("public.txt"), "public\n").unwrap();
    }

    let refreshed = ds_with_env(&checkout, temp.path(), &config, &["status"], &environment);
    assert!(refreshed.status.success(), "{}", stderr(&refreshed));

    let tracked = ds_with_env(
        &checkout,
        temp.path(),
        &config,
        &["file", "list"],
        &environment,
    );
    assert!(tracked.status.success(), "{}", stderr(&tracked));
    assert_eq!(
        stdout(&tracked),
        "app/.dsprivate\napp/public.txt\napp/secret.env\npublic.txt\nvendored/.dsprivate\nvendored/public.txt\nvendored/secret.env\n"
    );

    let git_files = Command::new("git")
        .current_dir(&checkout)
        .args(["ls-files"])
        .output()
        .unwrap();
    assert!(git_files.status.success(), "{}", stderr(&git_files));
    assert_eq!(
        stdout(&git_files),
        "app/public.txt\npublic.txt\nvendored/public.txt\n"
    );

    let exclude = fs::read_to_string(checkout.join(".git/info/exclude")).unwrap();
    for pattern in [
        "/app/.dsprivate",
        "/app/secret.env",
        "/ignored-untracked",
        "/vendored",
    ] {
        assert!(exclude.lines().any(|line| line == pattern), "{exclude}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_dsprivate_under_a_base_ignored_parent_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "ignored-symlink-policy").await;
    fs::create_dir(checkout.join("vendored")).unwrap();
    fs::write(checkout.join("vendored/.dsprivate"), "secret\n").unwrap();
    fs::write(checkout.join("vendored/secret"), "private\n").unwrap();
    fs::write(checkout.join("vendored/public"), "public\n").unwrap();
    let tracked = ds(&checkout, &config, &["status"]);
    assert!(tracked.status.success(), "{}", stderr(&tracked));

    fs::write(checkout.join("policy-target"), "public\n").unwrap();
    fs::remove_file(checkout.join("vendored/.dsprivate")).unwrap();
    symlink("../policy-target", checkout.join("vendored/.dsprivate")).unwrap();
    let global_ignore = temp.path().join("global-ignore-symlink");
    fs::write(&global_ignore, "vendored/\n").unwrap();
    let global_config = temp.path().join("global-gitconfig-symlink");
    fs::write(
        &global_config,
        format!("[core]\n\texcludesFile = {}\n", global_ignore.display()),
    )
    .unwrap();
    let environment = [
        ("GIT_CONFIG_GLOBAL", global_config.to_str().unwrap()),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ];

    let refreshed = ds_with_env(&checkout, temp.path(), &config, &["status"], &environment);
    assert!(refreshed.status.success(), "{}", stderr(&refreshed));
    assert_eq!(git_files(&checkout), "policy-target\n");
    assert_git_directories_are_read_only(&checkout);
}

#[tokio::test]
async fn conflicted_dsprivate_under_a_base_ignored_parent_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "ignored-conflicted-policy").await;
    fs::create_dir(checkout.join("vendored")).unwrap();
    fs::write(checkout.join("vendored/public"), "public\n").unwrap();
    fs::write(checkout.join("vendored/secret"), "private\n").unwrap();
    let base = ds(&checkout, &config, &["new", "-m", "base"]);
    assert!(base.status.success(), "{}", stderr(&base));
    let base_id = commit_id(&checkout, &config, "@-");

    fs::write(checkout.join("vendored/.dsprivate"), "secret\n").unwrap();
    let left = ds(&checkout, &config, &["new", "-m", "left"]);
    assert!(left.status.success(), "{}", stderr(&left));
    let left_id = commit_id(&checkout, &config, "@-");

    let right_base = ds(&checkout, &config, &["new", &base_id]);
    assert!(right_base.status.success(), "{}", stderr(&right_base));
    fs::write(checkout.join("vendored/.dsprivate"), "public\n").unwrap();
    let right = ds(&checkout, &config, &["new", "-m", "right"]);
    assert!(right.status.success(), "{}", stderr(&right));
    let right_id = commit_id(&checkout, &config, "@-");

    let global_ignore = temp.path().join("global-ignore-conflict");
    fs::write(&global_ignore, "vendored/\n").unwrap();
    let global_config = temp.path().join("global-gitconfig-conflict");
    fs::write(
        &global_config,
        format!("[core]\n\texcludesFile = {}\n", global_ignore.display()),
    )
    .unwrap();
    let environment = [
        ("GIT_CONFIG_GLOBAL", global_config.to_str().unwrap()),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ];
    let merged = ds_with_env(
        &checkout,
        temp.path(),
        &config,
        &["new", &left_id, &right_id, "-m", "merge"],
        &environment,
    );
    assert!(merged.status.success(), "{}", stderr(&merged));

    let conflicts = ds_with_env(
        &checkout,
        temp.path(),
        &config,
        &["resolve", "--list"],
        &environment,
    );
    assert!(conflicts.status.success(), "{}", stderr(&conflicts));
    assert_eq!(
        stdout(&conflicts),
        "vendored/.dsprivate    2-sided conflict\n"
    );
    assert_eq!(git_files(&checkout), "");
}

#[tokio::test]
async fn base_ignored_public_correction_is_batched() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "ignored-batched-correction").await;
    fs::create_dir(checkout.join("vendored")).unwrap();
    fs::write(checkout.join("vendored/.dsprivate"), "*.secret\n").unwrap();
    for index in 0..300 {
        fs::write(
            checkout.join(format!("vendored/public-{index:04}")),
            "public\n",
        )
        .unwrap();
    }
    fs::write(checkout.join("vendored/hidden.secret"), "private\n").unwrap();
    let tracked = ds(&checkout, &config, &["status"]);
    assert!(tracked.status.success(), "{}", stderr(&tracked));

    let global_ignore = temp.path().join("global-ignore-batches");
    fs::write(&global_ignore, "vendored/\n").unwrap();
    let global_config = temp.path().join("global-gitconfig-batches");
    fs::write(
        &global_config,
        format!("[core]\n\texcludesFile = {}\n", global_ignore.display()),
    )
    .unwrap();
    let environment = [
        ("GIT_CONFIG_GLOBAL", global_config.to_str().unwrap()),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ];
    let refreshed = ds_with_env(&checkout, temp.path(), &config, &["status"], &environment);
    assert!(refreshed.status.success(), "{}", stderr(&refreshed));

    let files = git_files(&checkout);
    assert_eq!(files.lines().count(), 300);
    assert!(
        files
            .lines()
            .all(|path| path.starts_with("vendored/public-")),
        "{files}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn injected_refresh_error_repairs_exclusions_and_restores_permissions() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = checkout(temp.path(), &config, "shim-error-guard").await;
    fs::create_dir(checkout.join("vendored")).unwrap();
    fs::write(checkout.join("vendored/.dsprivate"), "secret\n").unwrap();
    fs::write(checkout.join("vendored/secret"), "private\n").unwrap();
    fs::write(checkout.join("vendored/public"), "public\n").unwrap();
    let initially_tracked = ds(&checkout, &config, &["status"]);
    assert!(
        initially_tracked.status.success(),
        "{}",
        stderr(&initially_tracked)
    );
    assert_eq!(git_files(&checkout), "vendored/public\n");

    let global_ignore = temp.path().join("global-ignore-error-guard");
    fs::write(&global_ignore, "vendored/\n").unwrap();
    let global_config = temp.path().join("global-gitconfig-error-guard");
    fs::write(
        &global_config,
        format!("[core]\n\texcludesFile = {}\n", global_ignore.display()),
    )
    .unwrap();
    let failed_refresh = ds_with_env(
        &checkout,
        temp.path(),
        &config,
        &["status"],
        &[
            ("GIT_CONFIG_GLOBAL", global_config.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("DEVSPACE_FAILPOINT", "git_shim_after_exclusion"),
        ],
    );
    assert!(
        failed_refresh.status.success(),
        "{}",
        stderr(&failed_refresh)
    );
    // The injected error occurs after removing the ignored root but before
    // restoring its canonical public file.
    assert_eq!(git_files(&checkout), "");
    let exclude = fs::read_to_string(checkout.join(".git/info/exclude")).unwrap();
    assert!(exclude.lines().any(|line| line == "/vendored"), "{exclude}");
    assert_git_directories_are_read_only(&checkout);

    let repaired = ds_with_env(
        &checkout,
        temp.path(),
        &config,
        &["status"],
        &[
            ("GIT_CONFIG_GLOBAL", global_config.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ],
    );
    assert!(repaired.status.success(), "{}", stderr(&repaired));
    assert_eq!(git_files(&checkout), "vendored/public\n");
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
