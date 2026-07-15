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

#[test]
fn config_round_trips_through_an_atomic_private_file() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("isolated-machine");
    let store = MachineStore::new(&root);
    let expected = config("local-development-secret");

    store.write_config(&expected).unwrap();
    assert_eq!(store.load_config().unwrap(), expected);
    assert_eq!(expected.base_url(), "https://worker.example.test/api");
    assert_eq!(expected.machine_id().as_str(), "ab".repeat(16));
    assert_eq!(
        fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        ["config.json"]
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(store.config_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[test]
fn replacement_is_complete_and_secret_is_redacted() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    let first = config("first-sensitive-value");
    let second = config("second-sensitive-value");
    store.write_config(&first).unwrap();
    store.write_config(&second).unwrap();
    assert_eq!(store.load_config().unwrap(), second);
    assert!(!format!("{second:?}").contains("second-sensitive-value"));
    assert!(!format!("{:?}", second.shared_secret()).contains("second-sensitive-value"));
}

#[cfg(unix)]
#[test]
fn refuses_to_load_a_group_or_world_readable_secret() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    store
        .write_config(&config("private-until-permissions-change"))
        .unwrap();
    fs::set_permissions(store.config_path(), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        store.load_config(),
        Err(MachineConfigError::InsecurePermissions(_))
    ));
}
