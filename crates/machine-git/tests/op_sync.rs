use std::collections::{BTreeMap, BTreeSet};

use devspace_kernel_git::ops::{OpObjectKind as KernelOpObjectKind, OpReferenceKind, validate_op};
use devspace_machine_git::{
    CloudOpHeads, MachineGitRepository, OpId, OpObjectKey, OpObjectKind, OpSyncEngine, OpSyncStore,
    OpSyncTransport, OpTransportError, PendingOpHeadTransaction,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{Operation, RefTarget, RemoteRef, RemoteRefState, View};
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
                name = "Operation Sync Test"
                email = "op-sync@example.invalid"
            "#,
        )
        .unwrap(),
    );
    UserSettings::from_config(config).unwrap()
}

async fn offline_machine(path: &std::path::Path, name: &str) -> MachineGitRepository {
    let repository = MachineGitRepository::init(path, &settings()).await.unwrap();
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
    MachineGitRepository::open(path, &settings()).await.unwrap()
}

#[derive(Default)]
struct FakeCloud {
    objects: BTreeMap<OpObjectKey, Vec<u8>>,
    heads: BTreeSet<OpId>,
    cursor: u64,
    transactions: usize,
    fail_next_transaction: bool,
    receipts: BTreeMap<[u8; 16], (PendingOpHeadTransaction, CloudOpHeads)>,
}

impl OpSyncTransport for FakeCloud {
    async fn inventory_op_objects(
        &mut self,
        candidates: &[OpObjectKey],
    ) -> Result<BTreeSet<OpObjectKey>, OpTransportError> {
        Ok(candidates
            .iter()
            .copied()
            .filter(|key| self.objects.contains_key(key))
            .collect())
    }

    async fn download_op_object(&mut self, key: OpObjectKey) -> Result<Vec<u8>, OpTransportError> {
        self.objects
            .get(&key)
            .cloned()
            .ok_or_else(|| std::io::Error::other("missing cloud operation object").into())
    }

    async fn upload_op_object(
        &mut self,
        key: OpObjectKey,
        bytes: &[u8],
    ) -> Result<(), OpTransportError> {
        if let Some(existing) = self.objects.get(&key) {
            if existing != bytes {
                return Err(std::io::Error::other("cloud object clobber").into());
            }
        } else {
            self.objects.insert(key, bytes.to_vec());
        }
        Ok(())
    }

    async fn get_op_heads(&mut self) -> Result<CloudOpHeads, OpTransportError> {
        Ok(CloudOpHeads {
            cursor: self.cursor,
            heads: self.heads.clone(),
        })
    }

    async fn transact_op_heads(
        &mut self,
        pending: &PendingOpHeadTransaction,
    ) -> Result<CloudOpHeads, OpTransportError> {
        if self.fail_next_transaction {
            self.fail_next_transaction = false;
            return Err(std::io::Error::other("simulated offline head request").into());
        }
        if let Some((request, response)) = self.receipts.get(&pending.idempotency_key) {
            if request != pending {
                return Err(std::io::Error::other("idempotency replay mismatch").into());
            }
            return Ok(response.clone());
        }
        for observed in self.heads.intersection(&pending.observed_heads) {
            if !self.is_ancestor(pending.new_head, *observed)? {
                return Err(std::io::Error::other("stale observed head").into());
            }
        }
        for observed in &pending.observed_heads {
            self.heads.remove(observed);
        }
        self.heads.insert(pending.new_head);
        self.cursor += 1;
        self.transactions += 1;
        let response = CloudOpHeads {
            cursor: self.cursor,
            heads: self.heads.clone(),
        };
        self.receipts
            .insert(pending.idempotency_key, (pending.clone(), response.clone()));
        Ok(response)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn offline_head_failure_replays_the_durable_outbox() {
    let temp = tempfile::tempdir().unwrap();
    let mut repository = offline_machine(&temp.path().join("repo"), "offline").await;
    let state = OpSyncStore::open(temp.path().join("sync")).unwrap();
    let mut cloud = FakeCloud {
        fail_next_transaction: true,
        ..FakeCloud::default()
    };

    assert!(
        OpSyncEngine::new(&mut repository, &state, &mut cloud)
            .run()
            .await
            .is_err()
    );
    assert!(state.load_outbox().unwrap().is_some());
    assert_eq!(cloud.objects.len(), 2);
    assert!(cloud.heads.is_empty());

    OpSyncEngine::new(&mut repository, &state, &mut cloud)
        .run()
        .await
        .unwrap();
    assert!(state.load_outbox().unwrap().is_none());
    assert_eq!(cloud.cursor, 1);
    assert_eq!(cloud.heads.len(), 1);
}

impl FakeCloud {
    fn is_ancestor(&self, descendant: OpId, ancestor: OpId) -> Result<bool, OpTransportError> {
        let mut pending = vec![descendant];
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if id == ancestor {
                return Ok(true);
            }
            if id == [0; 64] || !visited.insert(id) {
                continue;
            }
            let key = OpObjectKey {
                kind: OpObjectKind::Operation,
                id,
            };
            let bytes = self
                .objects
                .get(&key)
                .ok_or_else(|| std::io::Error::other("missing operation ancestry"))?;
            let operation = validate_op(KernelOpObjectKind::Operation, bytes)?;
            for reference in operation
                .references
                .into_iter()
                .filter(|reference| reference.kind == OpReferenceKind::Operation)
            {
                pending.push(reference.id.try_into().unwrap());
            }
        }
        Ok(false)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn virgin_git_backend_skips_the_implicit_root_operation() {
    let temp = tempfile::tempdir().unwrap();
    let mut repository = MachineGitRepository::init(temp.path().join("repo"), &settings())
        .await
        .unwrap();
    let state = OpSyncStore::open(temp.path().join("sync")).unwrap();
    let mut cloud = FakeCloud::default();

    let result = OpSyncEngine::new(&mut repository, &state, &mut cloud)
        .run()
        .await
        .unwrap();

    assert_eq!(result, Default::default());
    assert_eq!(cloud.transactions, 0);
    assert!(cloud.objects.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn op_round_trip_two_machine_reconciliation_and_fresh_rebuild_are_exact() {
    let temp = tempfile::tempdir().unwrap();
    let mut left = offline_machine(&temp.path().join("left"), "left").await;
    let mut right = offline_machine(&temp.path().join("right"), "right").await;
    let left_state = OpSyncStore::open(temp.path().join("left-sync")).unwrap();
    let right_state = OpSyncStore::open(temp.path().join("right-sync")).unwrap();
    let mut cloud = FakeCloud::default();

    OpSyncEngine::new(&mut left, &left_state, &mut cloud)
        .run()
        .await
        .unwrap();
    assert_eq!(cloud.heads.len(), 1);
    assert_eq!(cloud.objects.len(), 2);

    OpSyncEngine::new(&mut right, &right_state, &mut cloud)
        .run()
        .await
        .unwrap();
    assert_eq!(cloud.heads.len(), 1);
    assert_eq!(cloud.cursor, 2);

    OpSyncEngine::new(&mut left, &left_state, &mut cloud)
        .run()
        .await
        .unwrap();
    assert_eq!(left.repo().op_id(), right.repo().op_id());
    let expected = op_log(right.repo()).await;

    let mut rebuilt = MachineGitRepository::init(temp.path().join("rebuilt"), &settings())
        .await
        .unwrap();
    let rebuilt_state = OpSyncStore::open(temp.path().join("rebuilt-sync")).unwrap();
    OpSyncEngine::new(&mut rebuilt, &rebuilt_state, &mut cloud)
        .run()
        .await
        .unwrap();

    assert_eq!(rebuilt.repo().op_id(), right.repo().op_id());
    assert_eq!(op_log(rebuilt.repo()).await, expected);
    assert!(rebuilt_state.load_outbox().unwrap().is_none());
}

async fn op_log(
    repo: &std::sync::Arc<jj_lib::repo::ReadonlyRepo>,
) -> BTreeMap<Vec<u8>, (Operation, View)> {
    let op_store = repo.op_store();
    let mut pending = vec![repo.op_id().clone()];
    let mut log = BTreeMap::new();
    while let Some(id) = pending.pop() {
        if id.as_bytes() == [0; 64] || log.contains_key(id.as_bytes()) {
            continue;
        }
        let operation = op_store.read_operation(&id).await.unwrap();
        let view = op_store.read_view(&operation.view_id).await.unwrap();
        pending.extend(operation.parents.iter().cloned());
        log.insert(id.as_bytes().to_vec(), (operation, view));
    }
    log
}
