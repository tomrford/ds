use std::fs;
use std::process::Command;

mod support;

use support::{configure_machine, ds, machine_store, stderr, stdout, write_cli_config};

#[test]
fn devspace_config_path_get_and_set_use_the_machine_config() {
    let temp = tempfile::tempdir().unwrap();
    let jj_config = write_cli_config(temp.path());
    configure_machine(temp.path(), "https://worker.example.test");
    let store = machine_store(temp.path());

    let path = ds(temp.path(), &jj_config, &["config", "path"]);
    assert!(path.status.success(), "{}", stderr(&path));
    assert_eq!(
        stdout(&path).trim(),
        store.config_path().display().to_string()
    );

    let initial = ds(temp.path(), &jj_config, &["config", "get", "git-shim"]);
    assert!(initial.status.success(), "{}", stderr(&initial));
    assert_eq!(stdout(&initial), "false\n");

    let enabled = ds(
        temp.path(),
        &jj_config,
        &["config", "set", "git-shim", "true"],
    );
    assert!(enabled.status.success(), "{}", stderr(&enabled));
    assert!(stdout(&enabled).is_empty());
    assert!(stderr(&enabled).is_empty());

    let config = store.load_config().unwrap();
    assert!(config.git_shim());
    assert_eq!(config.base_url(), "https://worker.example.test");
}

#[test]
fn config_path_uses_xdg_then_home_without_a_machine_store_override() {
    let temp = tempfile::tempdir().unwrap();
    let jj_config = write_cli_config(temp.path());
    let xdg = temp.path().join("xdg");
    let home = temp.path().join("home");

    let run = |xdg_home: Option<&std::path::Path>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ds"));
        command
            .current_dir(temp.path())
            .env("JJ_CONFIG", &jj_config)
            .env("HOME", &home)
            .env_remove("DEVSPACE_MACHINE_STORE_DIR")
            .env_remove("XDG_CONFIG_HOME")
            .args(["config", "path"]);
        if let Some(xdg_home) = xdg_home {
            command.env("XDG_CONFIG_HOME", xdg_home);
        }
        command.output().unwrap()
    };

    let from_xdg = run(Some(&xdg));
    assert!(from_xdg.status.success(), "{}", stderr(&from_xdg));
    assert_eq!(
        stdout(&from_xdg).trim(),
        xdg.join("devspace/config.toml").display().to_string()
    );

    let from_home = run(None);
    assert!(from_home.status.success(), "{}", stderr(&from_home));
    let expected_home = home
        .join(".config/devspace/config.toml")
        .display()
        .to_string();
    assert_eq!(stdout(&from_home).trim(), expected_home);

    let relative_xdg = run(Some(std::path::Path::new("relative")));
    assert!(relative_xdg.status.success(), "{}", stderr(&relative_xdg));
    assert_eq!(stdout(&relative_xdg).trim(), expected_home);
}

#[test]
fn jj_config_is_available_only_through_the_jj_escape_hatch() {
    let temp = tempfile::tempdir().unwrap();
    let jj_config = write_cli_config(temp.path());

    let devspace_help = ds(temp.path(), &jj_config, &["help", "config"]);
    assert!(devspace_help.status.success(), "{}", stderr(&devspace_help));
    assert!(stdout(&devspace_help).contains("Manage Devspace configuration"));
    assert!(stdout(&devspace_help).contains("Set a Devspace config value"));

    let nested_help = ds(temp.path(), &jj_config, &["help", "config", "set"]);
    assert!(nested_help.status.success(), "{}", stderr(&nested_help));
    assert!(stdout(&nested_help).contains("Set a Devspace config value"));
    assert!(stdout(&nested_help).contains("possible values: git-shim"));
    assert!(!stdout(&nested_help).contains("--user"));

    let jj_help = ds(temp.path(), &jj_config, &["jj", "config", "--help"]);
    assert!(jj_help.status.success(), "{}", stderr(&jj_help));
    assert!(stdout(&jj_help).contains("Operates on jj configuration"));

    let jj = ds(
        temp.path(),
        &jj_config,
        &["jj", "config", "get", "user.name"],
    );
    assert!(jj.status.success(), "{}", stderr(&jj));
    assert_eq!(stdout(&jj), "Devspace Test\n");

    let text = fs::read_to_string(&jj_config).unwrap();
    fs::write(
        &jj_config,
        format!("{text}\n[aliases]\ncfg = [\"config\", \"get\", \"user.name\"]\njj = [\"log\"]\n"),
    )
    .unwrap();
    let protected = ds(
        temp.path(),
        &jj_config,
        &["jj", "config", "get", "user.name"],
    );
    assert!(protected.status.success(), "{}", stderr(&protected));
    assert_eq!(stdout(&protected), "Devspace Test\n");

    let aliased = ds(temp.path(), &jj_config, &["cfg"]);
    assert!(!aliased.status.success());
    assert!(stderr(&aliased).contains("ds jj config"));

    let flag_prefixed = ds(
        temp.path(),
        &jj_config,
        &["--quiet", "config", "get", "user.name"],
    );
    assert!(!flag_prefixed.status.success());
    assert!(stderr(&flag_prefixed).contains("ds jj config"));

    let unsupported = ds(temp.path(), &jj_config, &["jj", "log"]);
    assert!(!unsupported.status.success());
    assert!(stderr(&unsupported).contains("only `ds jj config` is supported"));
}

#[test]
fn missing_machine_config_disables_runtime_shim_but_config_get_fails() {
    let temp = tempfile::tempdir().unwrap();
    let jj_config = write_cli_config(temp.path());

    let version = ds(temp.path(), &jj_config, &["version"]);
    assert!(version.status.success(), "{}", stderr(&version));

    let get = ds(temp.path(), &jj_config, &["config", "get", "git-shim"]);
    assert!(!get.status.success());
    assert!(stderr(&get).contains("config.toml"));
}

#[test]
fn invalid_boolean_is_rejected_without_rewriting_config() {
    let temp = tempfile::tempdir().unwrap();
    let jj_config = write_cli_config(temp.path());
    configure_machine(temp.path(), "https://worker.example.test");
    let before = fs::read(machine_store(temp.path()).config_path()).unwrap();

    let output = ds(
        temp.path(),
        &jj_config,
        &["config", "set", "git-shim", "yes"],
    );

    assert!(!output.status.success());
    assert_eq!(
        fs::read(machine_store(temp.path()).config_path()).unwrap(),
        before
    );
}
