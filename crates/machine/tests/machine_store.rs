use std::sync::{Arc, Barrier};
use std::{fs, thread};

use devspace_machine::{
    MachineStore, MachineStoreError, RepositoryId, RepositoryIdentity, RepositoryIncarnation,
    RepositoryName,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::settings::UserSettings;

mod common;

use common::settings;

fn settings_with_unknown_signing_backend() -> UserSettings {
    let mut config = StackedConfig::with_defaults();
    config.add_layer(
        ConfigLayer::parse(
            ConfigSource::User,
            r#"
                [user]
                name = "Devspace Test"
                email = "devspace@example.invalid"

                [signing]
                backend = "missing"
            "#,
        )
        .unwrap(),
    );
    UserSettings::from_config(config).unwrap()
}

fn name(value: &str) -> RepositoryName {
    RepositoryName::parse(value).unwrap()
}

fn identity(byte: u8, incarnation: u8) -> RepositoryIdentity {
    RepositoryIdentity::new(
        RepositoryId::parse(format!("{byte:02x}").repeat(32)).unwrap(),
        RepositoryIncarnation::parse(format!("{incarnation:02x}").repeat(16)).unwrap(),
    )
}

#[test]
fn catalog_persists_and_native_paths_use_only_opaque_identity() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    let repository_name = name("tenant-visible.name");
    let identity = identity(0x12, 0x34);

    let registered = store
        .register_repository(repository_name.clone(), identity.clone())
        .unwrap();
    assert_eq!(
        registered.native_repository_path,
        temp.path()
            .join("repositories")
            .join("12")
            .join("12".repeat(32))
            .join("34".repeat(16))
            .join("native")
    );
    assert!(
        !registered
            .native_repository_path
            .to_string_lossy()
            .contains(repository_name.as_str())
    );

    let reopened = MachineStore::new(temp.path());
    assert_eq!(
        reopened.resolve(&repository_name).unwrap().unwrap(),
        registered
    );
    assert_eq!(
        reopened
            .register_repository(repository_name, identity)
            .unwrap(),
        registered
    );
}

#[test]
fn catalog_rejects_conflicting_names_identities_and_stale_incarnations() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    let first_name = name("first");
    let first = identity(0x11, 0x22);
    store
        .register_repository(first_name.clone(), first.clone())
        .unwrap();

    assert!(matches!(
        store.register_repository(first_name.clone(), identity(0x33, 0x44)),
        Err(MachineStoreError::ConflictingName { .. })
    ));
    assert!(matches!(
        store.register_repository(first_name.clone(), identity(0x11, 0x55)),
        Err(MachineStoreError::StaleIncarnation { .. })
    ));
    assert!(matches!(
        store.register_repository(name("second"), first.clone()),
        Err(MachineStoreError::ConflictingIdentity { .. })
    ));
    assert!(matches!(
        store.register_repository(name("second"), identity(0x11, 0x66)),
        Err(MachineStoreError::StaleRepositoryIdentity { .. })
    ));
    assert_eq!(store.entries().unwrap().len(), 1);

    assert!(matches!(
        store.unregister_repository(&first_name, &identity(0x11, 0x55)),
        Err(MachineStoreError::StaleRemoval { .. })
    ));
    let removed = store
        .unregister_repository(&first_name, &first)
        .unwrap()
        .unwrap();
    assert_eq!(removed.identity, first);
    assert!(!removed.native_repository_path.exists());
    assert!(store.resolve(&first_name).unwrap().is_none());
}

#[test]
fn catalog_rename_is_atomic_idempotent_and_identity_checked() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    let old_name = name("rename-old");
    let new_name = name("rename-new");
    let current = identity(0x21, 0x22);
    let occupied = identity(0x31, 0x32);
    store
        .register_repository(old_name.clone(), current.clone())
        .unwrap();
    store
        .register_repository(name("occupied"), occupied)
        .unwrap();

    assert!(matches!(
        store.rename_repository(&old_name, name("occupied"), &current),
        Err(MachineStoreError::ConflictingName { .. })
    ));
    assert!(matches!(
        store.rename_repository(&old_name, new_name.clone(), &identity(0x21, 0x23)),
        Err(MachineStoreError::StaleRename { .. })
    ));
    let renamed = store
        .rename_repository(&old_name, new_name.clone(), &current)
        .unwrap();
    assert_eq!(renamed.name, new_name);
    assert!(store.resolve(&old_name).unwrap().is_none());
    assert_eq!(store.resolve(&new_name).unwrap(), Some(renamed.clone()));
    assert_eq!(
        store
            .rename_repository(&new_name, new_name.clone(), &current)
            .unwrap(),
        renamed
    );
}

#[tokio::test]
async fn materialization_requires_the_current_binding_and_unregister_does_not_prune() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    let repository_name = name("materialized");
    let current = identity(0x45, 0x67);
    let stale = identity(0x45, 0x89);
    let entry = store
        .register_repository(repository_name.clone(), current.clone())
        .unwrap();

    assert!(matches!(
        store
            .materialize_repository(&repository_name, &stale, &settings())
            .await,
        Err(MachineStoreError::StaleMaterialization { .. })
    ));
    let repository = store
        .materialize_repository(&repository_name, &current, &settings())
        .await
        .unwrap();
    assert_eq!(repository.path(), entry.native_repository_path);
    drop(repository);
    assert!(
        store
            .open_repository(&repository_name, &settings())
            .await
            .is_ok()
    );

    store
        .unregister_repository(&repository_name, &current)
        .unwrap();
    assert!(entry.native_repository_path.exists());
}

#[tokio::test]
async fn failed_materialization_never_publishes_a_partial_repository() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    let repository_name = name("failed-materialization");
    let identity = identity(0x46, 0x68);
    let entry = store
        .register_repository(repository_name.clone(), identity.clone())
        .unwrap();

    assert!(matches!(
        store
            .materialize_repository(
                &repository_name,
                &identity,
                &settings_with_unknown_signing_backend(),
            )
            .await,
        Err(MachineStoreError::Repository(_))
    ));
    assert!(!entry.native_repository_path.exists());
    let parent = entry.native_repository_path.parent().unwrap();
    let remaining: Vec<_> = fs::read_dir(parent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(remaining, ["native.lock"]);

    let repository = store
        .materialize_repository(&repository_name, &identity, &settings())
        .await
        .unwrap();
    assert_eq!(repository.path(), entry.native_repository_path);
}

#[test]
fn concurrent_materialization_publishes_one_complete_repository() {
    const MATERIALIZERS: usize = 8;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();
    let store = MachineStore::new(&root);
    let repository_name = name("concurrent-materialization");
    let identity = identity(0x47, 0x69);
    let entry = store
        .register_repository(repository_name.clone(), identity.clone())
        .unwrap();
    let barrier = Arc::new(Barrier::new(MATERIALIZERS));

    let handles: Vec<_> = (0..MATERIALIZERS)
        .map(|_| {
            let root = root.clone();
            let repository_name = repository_name.clone();
            let identity = identity.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                let repository = tokio::runtime::Runtime::new().unwrap().block_on(
                    MachineStore::new(root).materialize_repository(
                        &repository_name,
                        &identity,
                        &settings(),
                    ),
                );
                let repository = repository.unwrap();
                (
                    repository.path().to_owned(),
                    repository.repo().op_id().clone(),
                )
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();

    assert!(
        results
            .iter()
            .all(|(path, operation)| path == &results[0].0 && operation == &results[0].1)
    );
    assert_eq!(results[0].0, entry.native_repository_path);
    let parent = entry.native_repository_path.parent().unwrap();
    let mut published_entries: Vec<_> = fs::read_dir(parent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    published_entries.sort();
    assert_eq!(published_entries, ["native", "native.lock"]);
}

#[test]
fn concurrent_process_style_mutations_do_not_lose_catalog_entries() {
    const WRITERS: usize = 24;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();
    let barrier = Arc::new(Barrier::new(WRITERS));
    let handles: Vec<_> = (0..WRITERS)
        .map(|index| {
            let root = root.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let store = MachineStore::new(root);
                barrier.wait();
                store
                    .register_repository(
                        name(&format!("repository-{index}")),
                        identity(index as u8 + 1, 0x10),
                    )
                    .unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    let store = MachineStore::new(root);
    let entries = store.entries().unwrap();
    assert_eq!(entries.len(), WRITERS);
    for index in 0..WRITERS {
        assert!(
            store
                .resolve(&name(&format!("repository-{index}")))
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn incomplete_replacement_file_is_ignored_on_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    store
        .register_repository(name("stable"), identity(0xaa, 0xbb))
        .unwrap();
    fs::write(
        temp.path().join(".repositories.json.crashed.tmp"),
        br#"{"version":1,"repositories":{"partial""#,
    )
    .unwrap();

    let reopened = MachineStore::new(temp.path());
    assert!(reopened.resolve(&name("stable")).unwrap().is_some());
    assert!(reopened.resolve(&name("partial")).unwrap().is_none());
}

#[test]
fn validation_matches_the_cloud_directory_contract() {
    for valid in ["a", "repo-1", "repo.name", "repo_name"] {
        assert!(RepositoryName::parse(valid).is_ok(), "{valid}");
    }
    for invalid in ["", "Upper", "-leading", "slash/name", "space name"] {
        assert!(RepositoryName::parse(invalid).is_err(), "{invalid}");
    }
    assert!(RepositoryId::parse("ab".repeat(32)).is_ok());
    assert!(RepositoryId::parse("AB".repeat(32)).is_err());
    assert!(RepositoryIncarnation::parse("cd".repeat(16)).is_ok());
    assert!(RepositoryIncarnation::parse("cd".repeat(15)).is_err());
}
