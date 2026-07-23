use std::fs;

use devspace_machine::{MachineConfig, MachineConfigError, MachineId, MachineStore, SharedSecret};

fn config(secret: &str) -> MachineConfig {
    MachineConfig::new(
        "https://worker.example.test/api/",
        MachineId::parse("ab".repeat(16)).unwrap(),
        SharedSecret::new(secret).unwrap(),
    )
    .unwrap()
}

fn named_config(secret: &str, name: &str) -> MachineConfig {
    config(secret).with_machine_name(name).unwrap()
}

#[test]
fn config_round_trips_through_the_user_config_file() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("isolated-machine");
    let store = MachineStore::new(&root);
    let expected = config("local-development-secret");

    store.write_config(&expected).unwrap();
    assert_eq!(store.load_config().unwrap(), expected);
    assert_eq!(expected.base_url(), "https://worker.example.test/api");
    assert_eq!(expected.machine_id().as_str(), "ab".repeat(16));
    assert_eq!(expected.machine_name(), None);
    let persisted: toml::Value = toml::from_slice(&fs::read(store.config_path()).unwrap()).unwrap();
    assert!(persisted.get("version").is_none());
    assert!(persisted.get("machine_name").is_none());
    assert_eq!(persisted["git_shim"].as_bool(), Some(false));
    assert_eq!(persisted["context"]["auto-sync"].as_bool(), Some(false));
    assert!(!expected.context_auto_sync());
    let mut files = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    files.sort();
    assert_eq!(files, ["config.toml", "config.toml.lock"]);
}

#[test]
fn absent_boolean_settings_default_to_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    fs::write(
        store.config_path(),
        format!(
            "base_url = \"https://worker.example.test\"\nmachine_id = \"{}\"\nshared_secret = \"secret\"\n",
            "ab".repeat(16)
        ),
    )
    .unwrap();

    let config = store.load_config().unwrap();
    assert!(!config.git_shim());
    assert!(!config.context_auto_sync());
}

#[test]
fn named_config_and_boolean_settings_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    let expected = named_config("local-development-secret", "Tom-Mac_1")
        .with_git_shim(true)
        .with_context_auto_sync(true);

    store.write_config(&expected).unwrap();

    assert_eq!(store.load_config().unwrap(), expected);
    assert_eq!(expected.machine_name(), Some("Tom-Mac_1"));
    assert!(expected.git_shim());
    assert!(expected.context_auto_sync());
    let persisted: toml::Value = toml::from_slice(&fs::read(store.config_path()).unwrap()).unwrap();
    assert_eq!(persisted["machine_name"].as_str(), Some("Tom-Mac_1"));
    assert_eq!(persisted["git_shim"].as_bool(), Some(true));
    assert_eq!(persisted["context"]["auto-sync"].as_bool(), Some(true));
}

#[test]
fn machine_name_accepts_only_the_configured_length_and_ascii_set() {
    let longest_valid = "a".repeat(32);
    for valid in ["a", "-", "machine_01", longest_valid.as_str()] {
        assert!(config("secret").with_machine_name(valid).is_ok(), "{valid}");
    }
    let too_long = "a".repeat(33);
    for invalid in ["", "has space", "dot.name", "mächine", too_long.as_str()] {
        assert!(
            matches!(
                config("secret").with_machine_name(invalid),
                Err(MachineConfigError::InvalidMachineName)
            ),
            "{invalid}"
        );
    }
}

#[test]
fn concurrent_updates_preserve_independent_settings() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    store.write_config(&config("secret")).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

    let git_store = store.clone();
    let git_barrier = barrier.clone();
    let git = std::thread::spawn(move || {
        git_barrier.wait();
        git_store
            .update_config(|config| config.with_git_shim(true))
            .unwrap();
    });
    let context_store = store.clone();
    let context_barrier = barrier.clone();
    let context = std::thread::spawn(move || {
        context_barrier.wait();
        context_store
            .update_config(|config| config.with_context_auto_sync(true))
            .unwrap();
    });
    barrier.wait();
    git.join().unwrap();
    context.join().unwrap();

    let config = store.load_config().unwrap();
    assert!(config.git_shim());
    assert!(config.context_auto_sync());
}

#[test]
fn write_config_replaces_the_previous_contents() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    let first = config("first-sensitive-value");
    let second = config("second-sensitive-value");
    store.write_config(&first).unwrap();
    store.write_config(&second).unwrap();
    assert_eq!(store.load_config().unwrap(), second);
    assert!(format!("{second:?}").contains("second-sensitive-value"));
}

#[test]
fn decode_errors_include_the_toml_diagnostic() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    store.write_config(&config("initial-secret")).unwrap();
    let exposed_secret = "secret-that-must-not-appear";
    fs::write(
        store.config_path(),
        format!(
            "base_url = \"https://worker.example.test\"\nmachine_id = \"{}\"\nshared_secret = {exposed_secret}\n",
            "ab".repeat(16)
        ),
    )
    .unwrap();

    let error = store.load_config().unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("line 3"), "{message}");
    assert!(message.contains(exposed_secret), "{message}");
}

#[cfg(unix)]
#[test]
fn loads_a_user_config_with_normal_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    store.write_config(&config("development-secret")).unwrap();
    fs::set_permissions(store.config_path(), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(store.load_config().unwrap(), config("development-secret"));
}
