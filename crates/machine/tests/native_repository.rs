use std::fs;

use devspace_machine::{MachineRepository, MachineRepositoryError};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::RemoteRefSymbol;
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

#[tokio::test]
async fn initializes_stock_simple_stores_and_reloads_native_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo");
    let settings = settings();
    let machine_repo = MachineRepository::init(&path, &settings).await.unwrap();

    assert_eq!(
        fs::read_to_string(path.join("store/type")).unwrap(),
        "Simple"
    );
    assert_eq!(
        fs::read_to_string(path.join("op_store/type")).unwrap(),
        "simple_op_store"
    );
    assert_eq!(
        fs::read_to_string(path.join("op_heads/type")).unwrap(),
        "simple_op_heads_store"
    );

    let root_id = machine_repo.repo().store().root_commit_id().clone();
    let symbol = RemoteRefSymbol {
        name: "main".as_ref(),
        remote: "origin".as_ref(),
    };
    let expected = RemoteRef {
        target: RefTarget::normal(root_id),
        state: RemoteRefState::New,
    };
    let mut transaction = machine_repo.repo().start_transaction();
    transaction
        .repo_mut()
        .set_remote_bookmark(symbol, expected.clone());
    let committed = transaction.commit("record native state").await.unwrap();
    let committed_operation = committed.op_id().clone();
    drop(committed);
    drop(machine_repo);

    let reopened = MachineRepository::open(&path, &settings).await.unwrap();
    assert_eq!(reopened.repo().op_id(), &committed_operation);
    assert_eq!(
        reopened.repo().view().get_remote_bookmark(symbol),
        &expected
    );
}

#[tokio::test]
async fn refuses_a_git_backend_repository() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo");
    let settings = settings();
    drop(MachineRepository::init(&path, &settings).await.unwrap());
    fs::write(path.join("store/type"), "git").unwrap();

    let Err(error) = MachineRepository::open(path, &settings).await else {
        panic!("Git backend repository was accepted");
    };
    assert!(matches!(
        error,
        MachineRepositoryError::UnsupportedStore {
            store: "store",
            expected: "Simple",
            actual,
        } if actual == "git"
    ));
}
