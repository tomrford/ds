//! Live machine-to-Worker convergence over HTTP.
//!
//! This manual probe requires an explicitly supplied live repository authority.

use devspace_machine::{
    HttpTransport, MachineConfig, MachineId, MachineRepository, MachineSyncStore, SharedSecret,
    SyncEngine,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteRefSymbol};
use jj_lib::repo::Repo as _;
use jj_lib::settings::UserSettings;
use std::collections::BTreeSet;
use std::fs;

fn settings() -> UserSettings {
    let mut config = StackedConfig::with_defaults();
    config.add_layer(
        ConfigLayer::parse(
            ConfigSource::User,
            r#"
                [user]
                name = "Devspace Test"
                email = "devspace@example.invalid"
            "#,
        )
        .unwrap(),
    );
    UserSettings::from_config(config).unwrap()
}

async fn offline_machine(path: &std::path::Path, name: &str) -> MachineRepository {
    let repository = MachineRepository::init(path, &settings()).await.unwrap();
    let mut transaction = repository.repo().start_transaction();
    transaction.repo_mut().set_remote_bookmark(
        RemoteRefSymbol {
            name: RefName::new(name),
            remote: "origin".as_ref(),
        },
        RemoteRef {
            target: RefTarget::normal(repository.repo().store().root_commit_id().clone()),
            state: RemoteRefState::New,
        },
    );
    transaction.commit(format!("offline {name}")).await.unwrap();
    drop(repository);
    MachineRepository::open(path, &settings()).await.unwrap()
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires live Worker credentials and repository authority"]
async fn two_machines_converge_through_a_live_worker() {
    let base_url = std::env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
    let shared_secret =
        std::env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
    let repository_id =
        std::env::var("DEVSPACE_REPOSITORY_ID").expect("set DEVSPACE_REPOSITORY_ID");
    let incarnation = parse_incarnation(
        &std::env::var("DEVSPACE_INCARNATION").expect("set DEVSPACE_INCARNATION"),
    );
    let first_config = MachineConfig::new(
        &base_url,
        MachineId::parse("11".repeat(16)).unwrap(),
        SharedSecret::new(&shared_secret).unwrap(),
    )
    .unwrap();
    let second_config = MachineConfig::new(
        &base_url,
        MachineId::parse("22".repeat(16)).unwrap(),
        SharedSecret::new(&shared_secret).unwrap(),
    )
    .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let mut left = offline_machine(&temp.path().join("left-repo"), "left").await;
    let mut right = offline_machine(&temp.path().join("right-repo"), "right").await;
    fs::create_dir(temp.path().join("left-machine")).unwrap();
    fs::create_dir(temp.path().join("right-machine")).unwrap();
    let left_state = MachineSyncStore::open(temp.path().join("left-machine/sync")).unwrap();
    let right_state = MachineSyncStore::open(temp.path().join("right-machine/sync")).unwrap();

    let mut left_transport =
        HttpTransport::new(&first_config, &repository_id, incarnation).unwrap();
    let mut right_transport =
        HttpTransport::new(&second_config, &repository_id, incarnation).unwrap();
    left_transport.probe_access().await.unwrap();

    SyncEngine::new(
        &mut left,
        &left_state,
        temp.path().join("left-packs"),
        &mut left_transport,
    )
    .run()
    .await
    .unwrap();
    SyncEngine::new(
        &mut right,
        &right_state,
        temp.path().join("right-packs"),
        &mut right_transport,
    )
    .run()
    .await
    .unwrap();
    let final_state = SyncEngine::new(
        &mut left,
        &left_state,
        temp.path().join("left-packs"),
        &mut left_transport,
    )
    .run()
    .await
    .unwrap();

    assert_eq!(left.repo().op_id(), right.repo().op_id());
    assert_eq!(final_state.cloud_cursor, 2);
    assert_eq!(
        final_state.accepted_heads,
        BTreeSet::from([left.repo().op_id().as_bytes().try_into().unwrap()])
    );
    assert!(left_state.load_outbox().unwrap().is_none());
    assert!(right_state.load_outbox().unwrap().is_none());

    let origin: &jj_lib::ref_name::RemoteName = "origin".as_ref();
    let bookmarks = left
        .repo()
        .view()
        .store_view()
        .remote_views
        .get(origin)
        .map(|remote| remote.bookmarks.len());
    assert_eq!(bookmarks, Some(2));
}

fn parse_incarnation(value: &str) -> [u8; 16] {
    assert_eq!(
        value.len(),
        32,
        "DEVSPACE_INCARNATION must be 32 hex characters"
    );
    std::array::from_fn(|index| {
        u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .expect("DEVSPACE_INCARNATION must be lowercase hex")
    })
}
