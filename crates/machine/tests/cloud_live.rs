//! Live machine-to-Worker convergence over HTTP.
//!
//! Run against `wrangler dev` (or a deployed Worker):
//!
//! ```sh
//! wrangler dev --port 8787 --var SPIKE_TOKEN:dev-token &
//! DEVSPACE_SPIKE_URL=http://127.0.0.1:8787 DEVSPACE_SPIKE_TOKEN=dev-token \
//!   cargo test -p devspace-machine --test cloud_live -- --ignored --nocapture
//! ```
//!
//! Each run uses a fresh repository name, so a persistent Worker keeps old
//! test repositories around; `wrangler dev` state is disposable.

use std::collections::BTreeSet;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use blake2::{Blake2b512, Digest as _};
use devspace_machine::{HttpTransport, MachineRepository, MachineSyncStore, SyncEngine};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteRefSymbol};
use jj_lib::repo::Repo as _;
use jj_lib::settings::UserSettings;

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
#[ignore = "live cloud test; needs DEVSPACE_SPIKE_URL and DEVSPACE_SPIKE_TOKEN"]
async fn two_machines_converge_through_a_live_worker() {
    let base_url = std::env::var("DEVSPACE_SPIKE_URL").expect("set DEVSPACE_SPIKE_URL");
    let token = std::env::var("DEVSPACE_SPIKE_TOKEN").expect("set DEVSPACE_SPIKE_TOKEN");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let repository_name = format!("spike-live-{nanos:x}-{:x}", std::process::id());
    let incarnation: [u8; 16] = Blake2b512::digest(repository_name.as_bytes())[..16]
        .try_into()
        .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let mut left = offline_machine(&temp.path().join("left-repo"), "left").await;
    let mut right = offline_machine(&temp.path().join("right-repo"), "right").await;
    fs::create_dir(temp.path().join("left-machine")).unwrap();
    fs::create_dir(temp.path().join("right-machine")).unwrap();
    let left_state = MachineSyncStore::open(temp.path().join("left-machine/sync")).unwrap();
    let right_state = MachineSyncStore::open(temp.path().join("right-machine/sync")).unwrap();

    let mut left_transport =
        HttpTransport::new(&base_url, &repository_name, &token, incarnation).unwrap();
    let mut right_transport =
        HttpTransport::new(&base_url, &repository_name, &token, incarnation).unwrap();
    left_transport.initialize().await.unwrap();
    left_transport.initialize().await.unwrap();

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
