use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::rc::Rc;

use devspace_machine::{
    CloudHeads, DownloadedPack, MachineRepository, MachineSyncStore, ObjectId, PackCatalogEntry,
    PackCatalogPage, PackOptions, PendingHeadTransaction, SyncEngine, SyncTransport,
    TransportError,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{OperationId, RefTarget, RemoteRef, RemoteRefState};
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
    commit_offline_operation(&repository, name).await;
    drop(repository);
    MachineRepository::open(path, &settings()).await.unwrap()
}

async fn commit_offline_operation(repository: &MachineRepository, name: &str) {
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FaultBoundary {
    CatalogList,
    PackDownload,
    UploadManifest,
    UploadChunk,
    Install,
    HeadRequest,
    HeadResponse,
}

#[derive(Default)]
struct Upload {
    manifest: Option<Vec<u8>>,
    chunks: BTreeMap<usize, Vec<u8>>,
}

struct FakeCloud {
    object_store: MachineRepository,
    uploads: BTreeMap<ObjectId, Upload>,
    installed: Vec<DownloadedPack>,
    heads: BTreeSet<ObjectId>,
    cursor: u64,
    receipts: BTreeMap<[u8; 16], (PendingHeadTransaction, CloudHeads)>,
}

impl FakeCloud {
    async fn new(path: &std::path::Path) -> Self {
        Self {
            object_store: MachineRepository::init(path, &settings()).await.unwrap(),
            uploads: BTreeMap::new(),
            installed: Vec::new(),
            heads: BTreeSet::new(),
            cursor: 0,
            receipts: BTreeMap::new(),
        }
    }
}

struct FakeTransport {
    cloud: Rc<RefCell<FakeCloud>>,
    fault: Option<FaultBoundary>,
    list_calls: usize,
}

impl FakeTransport {
    fn new(cloud: Rc<RefCell<FakeCloud>>, fault: Option<FaultBoundary>) -> Self {
        Self {
            cloud,
            fault,
            list_calls: 0,
        }
    }

    fn maybe_fail(&mut self, boundary: FaultBoundary) -> Result<(), TransportError> {
        if self.fault == Some(boundary) {
            self.fault = None;
            Err(std::io::Error::other(format!("lost response after {boundary:?}")).into())
        } else {
            Ok(())
        }
    }
}

impl SyncTransport for FakeTransport {
    async fn list_packs(
        &mut self,
        after: u64,
        through: Option<u64>,
    ) -> Result<PackCatalogPage, TransportError> {
        self.list_calls += 1;
        let cloud = self.cloud.borrow();
        let high_water = cloud.installed.len() as u64;
        let through = through.unwrap_or(high_water);
        let packs = cloud
            .installed
            .iter()
            .enumerate()
            .filter_map(|(index, pack)| {
                let sequence = index as u64 + 1;
                (sequence > after && sequence <= through).then_some(PackCatalogEntry {
                    sequence,
                    id: pack.id,
                })
            })
            .take(1)
            .collect::<Vec<_>>();
        let has_more = packs.last().is_some_and(|entry| entry.sequence < through);
        let page = PackCatalogPage {
            next_after: packs.last().map_or(after, |entry| entry.sequence),
            packs,
            through,
            has_more,
        };
        drop(cloud);
        self.maybe_fail(FaultBoundary::CatalogList)?;
        Ok(page)
    }

    async fn download_pack(&mut self, id: ObjectId) -> Result<DownloadedPack, TransportError> {
        let pack = self
            .cloud
            .borrow()
            .installed
            .iter()
            .find(|pack| pack.id == id)
            .cloned()
            .ok_or_else(|| std::io::Error::other("unknown installed pack"))?;
        self.maybe_fail(FaultBoundary::PackDownload)?;
        Ok(pack)
    }

    async fn upload_manifest(&mut self, id: ObjectId, bytes: &[u8]) -> Result<(), TransportError> {
        let mut cloud = self.cloud.borrow_mut();
        let upload = cloud.uploads.entry(id).or_default();
        if let Some(existing) = &upload.manifest {
            if existing != bytes {
                return Err(std::io::Error::other("manifest retry changed bytes").into());
            }
        } else {
            upload.manifest = Some(bytes.to_vec());
        }
        drop(cloud);
        self.maybe_fail(FaultBoundary::UploadManifest)
    }

    async fn upload_chunk(
        &mut self,
        id: ObjectId,
        position: usize,
        bytes: &[u8],
    ) -> Result<(), TransportError> {
        let mut cloud = self.cloud.borrow_mut();
        let existing = cloud
            .uploads
            .entry(id)
            .or_default()
            .chunks
            .entry(position)
            .or_insert_with(|| bytes.to_vec());
        if existing != bytes {
            return Err(std::io::Error::other("chunk retry changed bytes").into());
        }
        drop(cloud);
        self.maybe_fail(FaultBoundary::UploadChunk)
    }

    async fn install_pack(&mut self, id: ObjectId) -> Result<(), TransportError> {
        let mut cloud = self.cloud.borrow_mut();
        if !cloud.installed.iter().any(|pack| pack.id == id) {
            let upload = cloud
                .uploads
                .get(&id)
                .ok_or_else(|| std::io::Error::other("missing upload"))?;
            let manifest = upload
                .manifest
                .clone()
                .ok_or_else(|| std::io::Error::other("missing manifest"))?;
            let chunks = upload.chunks.values().cloned().collect::<Vec<_>>();
            cloud.object_store.install_pack(id, &manifest, &chunks)?;
            cloud.installed.push(DownloadedPack {
                id,
                manifest,
                chunks,
            });
        }
        drop(cloud);
        self.maybe_fail(FaultBoundary::Install)
    }

    async fn get_heads(&mut self) -> Result<CloudHeads, TransportError> {
        let cloud = self.cloud.borrow();
        Ok(CloudHeads {
            cursor: cloud.cursor,
            heads: cloud.heads.clone(),
        })
    }

    async fn transact_heads(
        &mut self,
        pending: &PendingHeadTransaction,
    ) -> Result<CloudHeads, TransportError> {
        self.maybe_fail(FaultBoundary::HeadRequest)?;
        let replay = self
            .cloud
            .borrow()
            .receipts
            .get(&pending.idempotency_key)
            .cloned();
        if let Some((request, response)) = replay {
            if &request != pending {
                return Err(std::io::Error::other("idempotency key changed request").into());
            }
            self.maybe_fail(FaultBoundary::HeadResponse)?;
            return Ok(response);
        }
        self.cloud
            .borrow()
            .object_store
            .object_closure_from_heads(vec![pending.new_head], &BTreeSet::new())?;
        let current_observed = self
            .cloud
            .borrow()
            .heads
            .intersection(&pending.observed_heads)
            .copied()
            .collect::<Vec<_>>();
        let op_store = self.cloud.borrow().object_store.repo().op_store().clone();
        for observed in current_observed {
            if !operation_is_ancestor(&op_store, pending.new_head, observed).await? {
                return Err(std::io::Error::other(
                    "observed current head is not an ancestor of new head",
                )
                .into());
            }
        }
        let mut cloud = self.cloud.borrow_mut();
        for observed in &pending.observed_heads {
            cloud.heads.remove(observed);
        }
        cloud.heads.insert(pending.new_head);
        cloud.cursor += 1;
        let response = CloudHeads {
            cursor: cloud.cursor,
            heads: cloud.heads.clone(),
        };
        cloud
            .receipts
            .insert(pending.idempotency_key, (pending.clone(), response.clone()));
        drop(cloud);
        self.maybe_fail(FaultBoundary::HeadResponse)?;
        Ok(response)
    }
}

async fn operation_is_ancestor(
    op_store: &std::sync::Arc<dyn jj_lib::op_store::OpStore>,
    descendant: ObjectId,
    ancestor: ObjectId,
) -> Result<bool, TransportError> {
    let mut pending = vec![OperationId::new(descendant.to_vec())];
    let mut visited = BTreeSet::new();
    while let Some(operation_id) = pending.pop() {
        if operation_id.as_bytes() == ancestor {
            return Ok(true);
        }
        if visited.insert(operation_id.clone()) {
            pending.extend(op_store.read_operation(&operation_id).await?.parents);
        }
    }
    Ok(false)
}

async fn run_until_success(
    repository: &mut MachineRepository,
    state: &MachineSyncStore,
    packs: &std::path::Path,
    transport: &mut FakeTransport,
) {
    for _ in 0..3 {
        let mut engine =
            SyncEngine::new(repository, state, packs, transport).with_pack_options(PackOptions {
                pack_objects: 1,
                ..PackOptions::default()
            });
        match engine.run().await {
            Ok(_) => return,
            Err(devspace_machine::SyncEngineError::Transport(_)) => {}
            Err(error) => panic!("unexpected sync failure: {error}"),
        }
    }
    panic!("one-shot fault did not recover");
}

#[tokio::test(flavor = "current_thread")]
async fn two_offline_machines_converge_across_every_remote_retry_boundary() {
    for boundary in [
        FaultBoundary::CatalogList,
        FaultBoundary::PackDownload,
        FaultBoundary::UploadManifest,
        FaultBoundary::UploadChunk,
        FaultBoundary::Install,
        FaultBoundary::HeadRequest,
        FaultBoundary::HeadResponse,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut left = offline_machine(&temp.path().join("left-repo"), "left").await;
        let mut right = offline_machine(&temp.path().join("right-repo"), "right").await;
        fs::create_dir(temp.path().join("left-machine")).unwrap();
        fs::create_dir(temp.path().join("right-machine")).unwrap();
        let left_state = MachineSyncStore::open(temp.path().join("left-machine/sync")).unwrap();
        let right_state = MachineSyncStore::open(temp.path().join("right-machine/sync")).unwrap();
        let cloud = Rc::new(RefCell::new(
            FakeCloud::new(&temp.path().join("cloud-objects")).await,
        ));

        let mut left_transport = FakeTransport::new(cloud.clone(), Some(boundary));
        run_until_success(
            &mut left,
            &left_state,
            &temp.path().join("left-packs"),
            &mut left_transport,
        )
        .await;
        let pending = cloud.borrow().receipts.values().next().unwrap().0.clone();
        left_state.save_outbox(&pending).unwrap();
        let mut clear_retry = FakeTransport::new(cloud.clone(), None);
        run_until_success(
            &mut left,
            &left_state,
            &temp.path().join("left-packs"),
            &mut clear_retry,
        )
        .await;
        assert_eq!(cloud.borrow().cursor, 1, "fault: {boundary:?}");

        let mut right_transport = FakeTransport::new(cloud.clone(), Some(boundary));
        run_until_success(
            &mut right,
            &right_state,
            &temp.path().join("right-packs"),
            &mut right_transport,
        )
        .await;
        assert!(right_transport.list_calls > 1, "fault: {boundary:?}");
        let mut final_left = FakeTransport::new(cloud.clone(), None);
        run_until_success(
            &mut left,
            &left_state,
            &temp.path().join("left-packs"),
            &mut final_left,
        )
        .await;

        assert_eq!(
            left.repo().op_id(),
            right.repo().op_id(),
            "fault: {boundary:?}"
        );
        assert_eq!(cloud.borrow().heads.len(), 1);
        assert_eq!(cloud.borrow().cursor, 2);
        assert_eq!(
            left_state.load_state().unwrap().catalog_sequence,
            cloud.borrow().installed.len() as u64
        );
        assert!(left_state.load_outbox().unwrap().is_none());
        assert!(right_state.load_outbox().unwrap().is_none());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_fully_synchronised_machine_rebuilds_exactly_from_cloud_state() {
    let temp = tempfile::tempdir().unwrap();
    let machine = temp.path().join("machine");
    fs::create_dir(&machine).unwrap();
    let mut repository = offline_machine(&machine.join("repo"), "durable").await;
    let state_store = MachineSyncStore::open(machine.join("sync")).unwrap();
    let cloud = Rc::new(RefCell::new(
        FakeCloud::new(&temp.path().join("cloud-objects")).await,
    ));
    let mut transport = FakeTransport::new(cloud.clone(), None);
    run_until_success(
        &mut repository,
        &state_store,
        &machine.join("packs"),
        &mut transport,
    )
    .await;

    let expected_operation = repository.repo().op_id().clone();
    let expected_view = repository.repo().view().store_view().clone();
    let expected_objects = repository
        .object_closure(&BTreeSet::new())
        .await
        .unwrap()
        .objects
        .into_iter()
        .map(|object| object.key)
        .collect::<BTreeSet<_>>();
    drop(repository);
    drop(state_store);
    fs::remove_dir_all(&machine).unwrap();

    fs::create_dir(&machine).unwrap();
    let mut rebuilt = MachineRepository::init(machine.join("repo"), &settings())
        .await
        .unwrap();
    let rebuilt_state = MachineSyncStore::open(machine.join("sync")).unwrap();
    let mut rebuilt_transport = FakeTransport::new(cloud.clone(), None);
    run_until_success(
        &mut rebuilt,
        &rebuilt_state,
        &machine.join("packs"),
        &mut rebuilt_transport,
    )
    .await;

    assert_eq!(rebuilt.repo().op_id(), &expected_operation);
    assert_eq!(rebuilt.repo().view().store_view(), &expected_view);
    assert_eq!(
        rebuilt
            .object_closure(&BTreeSet::new())
            .await
            .unwrap()
            .objects
            .into_iter()
            .map(|object| object.key)
            .collect::<BTreeSet<_>>(),
        expected_objects
    );
    let state = rebuilt_state.load_state().unwrap();
    assert_eq!(state.cloud_cursor, 1);
    assert_eq!(
        state.catalog_sequence,
        cloud.borrow().installed.len() as u64
    );
    assert_eq!(
        state.accepted_heads,
        BTreeSet::from([expected_operation.as_bytes().try_into().unwrap()])
    );
    assert!(rebuilt_state.load_outbox().unwrap().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn command_boundaries_recover_work_with_no_outbox_or_daemon() {
    let temp = tempfile::tempdir().unwrap();
    let machine = temp.path().join("machine");
    fs::create_dir(&machine).unwrap();
    let mut repository = offline_machine(&machine.join("repo"), "first").await;
    let first_operation = repository.repo().op_id().clone();
    let state_store = MachineSyncStore::open(machine.join("sync")).unwrap();
    let cloud = Rc::new(RefCell::new(
        FakeCloud::new(&temp.path().join("cloud-objects")).await,
    ));

    assert!(state_store.load_outbox().unwrap().is_none());
    let mut interrupted_boundary =
        FakeTransport::new(cloud.clone(), Some(FaultBoundary::HeadRequest));
    let mut engine = SyncEngine::new(
        &mut repository,
        &state_store,
        machine.join("packs"),
        &mut interrupted_boundary,
    );
    assert!(matches!(
        engine.run().await,
        Err(devspace_machine::SyncEngineError::Transport(_))
    ));
    drop(engine);

    let queued = state_store.load_outbox().unwrap().unwrap();
    assert_eq!(queued.new_head.as_slice(), first_operation.as_bytes());
    assert_eq!(cloud.borrow().cursor, 0);
    assert!(cloud.borrow().heads.is_empty());

    let mut next_boundary = FakeTransport::new(cloud.clone(), None);
    let mut engine = SyncEngine::new(
        &mut repository,
        &state_store,
        machine.join("packs"),
        &mut next_boundary,
    );
    let state = engine.run().await.unwrap();
    drop(engine);
    let (replayed_request, replayed_response) = cloud
        .borrow()
        .receipts
        .get(&queued.idempotency_key)
        .cloned()
        .unwrap();
    assert_eq!(replayed_request, queued);
    assert_eq!(replayed_response.cursor, 1);
    assert_eq!(replayed_response.heads, state.accepted_heads);
    assert_eq!(state.cloud_cursor, 1);
    assert_eq!(state.accepted_heads, BTreeSet::from([queued.new_head]));
    assert_eq!(state_store.load_state().unwrap(), state);
    assert_eq!(cloud.borrow().heads, state.accepted_heads);
    assert!(state_store.load_outbox().unwrap().is_none());

    commit_offline_operation(&repository, "second").await;
    drop(repository);
    repository = MachineRepository::open(machine.join("repo"), &settings())
        .await
        .unwrap();
    let second_operation = repository.repo().op_id().clone();
    assert_ne!(second_operation, first_operation);
    assert!(state_store.load_outbox().unwrap().is_none());

    let mut following_boundary = FakeTransport::new(cloud.clone(), None);
    let mut engine = SyncEngine::new(
        &mut repository,
        &state_store,
        machine.join("packs"),
        &mut following_boundary,
    );
    let state = engine.run().await.unwrap();
    assert_eq!(state.cloud_cursor, 2);
    assert_eq!(
        state.accepted_heads,
        BTreeSet::from([second_operation.as_bytes().try_into().unwrap()])
    );
    assert_eq!(state_store.load_state().unwrap(), state);
    assert_eq!(cloud.borrow().heads, state.accepted_heads);
    assert!(state_store.load_outbox().unwrap().is_none());
}
