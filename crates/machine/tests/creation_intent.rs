use std::sync::{Arc, Barrier};
use std::thread;

use devspace_machine::{
    MachineConfig, MachineId, MachineStore, RepositoryCreationIntentError, RepositoryCreationKey,
    RepositoryCreationTarget, RepositoryId, RepositoryIdentity, RepositoryIncarnation,
    RepositoryName, SharedSecret,
};

fn name(value: &str) -> RepositoryName {
    RepositoryName::parse(value).unwrap()
}

fn identity(byte: u8, incarnation: u8) -> RepositoryIdentity {
    RepositoryIdentity::new(
        RepositoryId::parse(format!("{byte:02x}").repeat(32)).unwrap(),
        RepositoryIncarnation::parse(format!("{incarnation:02x}").repeat(16)).unwrap(),
    )
}

fn target(url: &str, machine: u8) -> RepositoryCreationTarget {
    let config = MachineConfig::new(
        url,
        MachineId::parse(format!("{machine:02x}").repeat(16)).unwrap(),
        SharedSecret::new("development-secret").unwrap(),
    )
    .unwrap();
    RepositoryCreationTarget::from_config(&config)
}

#[test]
fn intent_reuses_the_original_request_and_retains_a_completed_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    let repository_name = name("retry-safe");
    let creation_target = target("https://worker.example.test", 0x12);
    let first = store
        .begin_repository_creation(
            repository_name.clone(),
            creation_target.clone(),
            RepositoryCreationKey::new([0x34; 16]),
        )
        .unwrap();
    assert_eq!(first.key().bytes(), [0x34; 16]);
    assert!(!format!("{first:?}").contains(&"34".repeat(16)));

    let reopened = MachineStore::new(temp.path());
    let resumed = reopened
        .begin_repository_creation(
            repository_name.clone(),
            creation_target.clone(),
            RepositoryCreationKey::new([0x56; 16]),
        )
        .unwrap();
    assert_eq!(resumed.key().bytes(), [0x34; 16]);

    let cloud_identity = identity(0x78, 0x9a);
    let recorded = reopened
        .record_repository_created(&resumed, cloud_identity.clone())
        .unwrap();
    assert_eq!(recorded.identity(), Some(&cloud_identity));
    reopened
        .register_repository(repository_name.clone(), cloud_identity.clone())
        .unwrap();
    let completed = reopened.complete_repository_creation(&recorded).unwrap();
    assert!(completed.is_complete());

    let after_completion = MachineStore::new(temp.path())
        .begin_repository_creation(
            repository_name,
            creation_target,
            RepositoryCreationKey::new([0xbc; 16]),
        )
        .unwrap();
    assert_eq!(after_completion.key().bytes(), [0x34; 16]);
    assert_eq!(after_completion.identity(), Some(&cloud_identity));
    assert!(after_completion.is_complete());
}

#[test]
fn intent_refuses_changed_authority_and_cloud_identity() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    let repository_name = name("fixed-authority");
    let intent = store
        .begin_repository_creation(
            repository_name.clone(),
            target("https://first.example.test", 0x11),
            RepositoryCreationKey::new([0x22; 16]),
        )
        .unwrap();

    assert!(matches!(
        store.begin_repository_creation(
            repository_name.clone(),
            target("https://second.example.test", 0x11),
            RepositoryCreationKey::new([0x33; 16]),
        ),
        Err(RepositoryCreationIntentError::TargetChanged(_))
    ));
    let recorded = store
        .record_repository_created(&intent, identity(0x44, 0x55))
        .unwrap();
    assert!(matches!(
        store.record_repository_created(&recorded, identity(0x66, 0x77)),
        Err(RepositoryCreationIntentError::DifferentCloudIdentity(_))
    ));
}

#[test]
fn existing_catalog_binding_is_not_adopted_as_a_create_retry() {
    let temp = tempfile::tempdir().unwrap();
    let store = MachineStore::new(temp.path());
    let repository_name = name("existing");
    store
        .register_repository(repository_name.clone(), identity(0x11, 0x22))
        .unwrap();

    assert!(matches!(
        store.begin_repository_creation(
            repository_name,
            target("https://worker.example.test", 0x33),
            RepositoryCreationKey::new([0x44; 16]),
        ),
        Err(RepositoryCreationIntentError::AlreadyRegistered(_))
    ));
}

#[test]
fn concurrent_starters_publish_one_request_key() {
    const STARTERS: usize = 12;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();
    let barrier = Arc::new(Barrier::new(STARTERS));
    let handles: Vec<_> = (0..STARTERS)
        .map(|index| {
            let root = root.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                MachineStore::new(root)
                    .begin_repository_creation(
                        name("concurrent"),
                        target("https://worker.example.test", 0x55),
                        RepositoryCreationKey::new([index as u8; 16]),
                    )
                    .unwrap()
                    .key()
                    .bytes()
            })
        })
        .collect();
    let keys: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert!(keys.iter().all(|key| key == &keys[0]));
}
