use std::collections::BTreeSet;
use std::fs;

use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_heads_store::OpHeadsStoreError;
use jj_lib::settings::UserSettings;

use super::{MachineRepository, ReconcileOperationHeadsError};

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
async fn partial_multi_head_publication_recovers_only_durable_heads() {
    let temp = tempfile::tempdir().unwrap();
    let settings = settings();
    let source = MachineRepository::init(temp.path().join("source"), &settings)
        .await
        .unwrap();
    let first = source.repo().start_transaction();
    let second = source.repo().start_transaction();
    let first_head: [u8; 64] = first
        .commit("first")
        .await
        .unwrap()
        .op_id()
        .as_bytes()
        .try_into()
        .unwrap();
    let second_head: [u8; 64] = second
        .commit("second")
        .await
        .unwrap()
        .op_id()
        .as_bytes()
        .try_into()
        .unwrap();
    let cloud_heads = BTreeSet::from([first_head, second_head]);
    let first_durable_head = *cloud_heads.first().unwrap();
    let absent_head = *cloud_heads.last().unwrap();
    let closure = source.object_closure(&BTreeSet::new()).await.unwrap();

    let mut destination = MachineRepository::init(temp.path().join("destination"), &settings)
        .await
        .unwrap();
    for object in &closure.objects {
        let relative = object.path.strip_prefix(source.path()).unwrap();
        let destination_path = destination.path().join(relative);
        fs::create_dir_all(destination_path.parent().unwrap()).unwrap();
        fs::copy(&object.path, destination_path).unwrap();
    }

    let error = destination
        .reconcile_operation_heads_with_hook(&cloud_heads, |index, operation_id| {
            if index == 1 {
                Err(OpHeadsStoreError::Write {
                    new_op_id: operation_id.clone(),
                    source: std::io::Error::other("injected second head write failure").into(),
                })
            } else {
                Ok(())
            }
        })
        .await
        .unwrap_err();
    let ReconcileOperationHeadsError::PartialPublication { recovered_head, .. } = error else {
        panic!("unexpected error: {error}");
    };

    assert_eq!(recovered_head.as_bytes(), first_durable_head);
    assert_eq!(destination.repo().op_id(), &recovered_head);
    assert_eq!(
        destination
            .repo()
            .op_heads_store()
            .get_op_heads()
            .await
            .unwrap(),
        vec![recovered_head]
    );
    assert_ne!(destination.repo().op_id().as_bytes(), absent_head);
}
