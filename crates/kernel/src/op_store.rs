//! Mirrors `jj_lib::op_store` values and `jj_lib::simple_op_store` encoding.

use std::collections::{BTreeMap, BTreeSet};

use blake2::Blake2b512;
use prost::Message;

use crate::error::{Context, ValidationError};
use crate::hash::{ContentHash, content_id, hash_map};
use crate::proto;
use crate::{ObjectKind, ObjectReference, ValidatedObject};

const OBJECT_ID_LENGTH: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Id(Vec<u8>);

impl ContentHash for Id {
    fn hash(&self, state: &mut Blake2b512) {
        self.0.hash(state);
    }
}

fn id(label: &str, bytes: Vec<u8>) -> Result<Id, ValidationError> {
    if bytes.len() != OBJECT_ID_LENGTH {
        return Err(ValidationError::new(format!(
            "{label} id must be {OBJECT_ID_LENGTH} bytes, got {}",
            bytes.len()
        )));
    }
    Ok(Id(bytes))
}

fn reference(kind: ObjectKind, value: &Id) -> ObjectReference {
    let mut id = [0; OBJECT_ID_LENGTH];
    id.copy_from_slice(&value.0);
    ObjectReference { kind, id }
}

#[derive(Clone, Debug)]
struct RefTarget(Vec<Option<Id>>);

impl RefTarget {
    fn absent() -> Self {
        Self(vec![None])
    }

    fn is_present(&self) -> bool {
        self.0.iter().any(Option::is_some)
    }
}

impl ContentHash for RefTarget {
    fn hash(&self, state: &mut Blake2b512) {
        self.0.hash(state);
    }
}

#[derive(Clone, Copy, Debug)]
enum RemoteRefState {
    New,
    Tracked,
}

impl ContentHash for RemoteRefState {
    fn hash(&self, state: &mut Blake2b512) {
        match self {
            Self::New => 0_u32.hash(state),
            Self::Tracked => 1_u32.hash(state),
        }
    }
}

#[derive(Clone, Debug)]
struct RemoteRef {
    target: RefTarget,
    state: RemoteRefState,
}

impl ContentHash for RemoteRef {
    fn hash(&self, state: &mut Blake2b512) {
        self.target.hash(state);
        self.state.hash(state);
    }
}

#[derive(Clone, Debug, Default)]
struct RemoteView {
    bookmarks: BTreeMap<String, RemoteRef>,
    tags: BTreeMap<String, RemoteRef>,
}

impl ContentHash for RemoteView {
    fn hash(&self, state: &mut Blake2b512) {
        hash_map(self.bookmarks.iter(), state);
        hash_map(self.tags.iter(), state);
    }
}

#[derive(Clone, Debug)]
struct View {
    head_ids: BTreeSet<Id>,
    local_bookmarks: BTreeMap<String, RefTarget>,
    local_tags: BTreeMap<String, RefTarget>,
    remote_views: BTreeMap<String, RemoteView>,
    git_refs: BTreeMap<String, RefTarget>,
    git_head: RefTarget,
    wc_commit_ids: BTreeMap<String, Id>,
}

impl ContentHash for View {
    fn hash(&self, state: &mut Blake2b512) {
        (self.head_ids.len() as u64).hash(state);
        for id in &self.head_ids {
            id.hash(state);
        }
        hash_map(self.local_bookmarks.iter(), state);
        hash_map(self.local_tags.iter(), state);
        hash_map(self.remote_views.iter(), state);
        hash_map(self.git_refs.iter(), state);
        self.git_head.hash(state);
        hash_map(self.wc_commit_ids.iter(), state);
    }
}

fn interleave<T>(removes: Vec<T>, adds: Vec<T>) -> Result<Vec<T>, ValidationError> {
    if adds.len() != removes.len() + 1 {
        return Err(ValidationError::new(
            "ref target must have exactly one more add than remove",
        ));
    }
    let mut adds = adds.into_iter();
    let mut values = Vec::with_capacity(removes.len() * 2 + 1);
    if let Some(first) = adds.next() {
        values.push(first);
    }
    for (remove, add) in removes.into_iter().zip(adds) {
        values.push(remove);
        values.push(add);
    }
    Ok(values)
}

fn ref_target_from_proto(value: Option<proto::RefTarget>) -> Result<RefTarget, ValidationError> {
    let Some(value) = value.and_then(|value| value.value) else {
        return Ok(RefTarget::absent());
    };
    let terms = match value {
        proto::ref_target::Value::CommitId(value) => {
            vec![Some(id("view ref commit", value)?)]
        }
        proto::ref_target::Value::ConflictLegacy(conflict) => {
            let removes = conflict
                .removes
                .into_iter()
                .map(|value| id("view ref commit", value).map(Some))
                .collect::<Result<Vec<_>, _>>()?;
            let adds = conflict
                .adds
                .into_iter()
                .map(|value| id("view ref commit", value).map(Some))
                .collect::<Result<Vec<_>, _>>()?;
            interleave(removes, adds)?
        }
        proto::ref_target::Value::Conflict(conflict) => {
            let convert = |term: proto::RefConflictTerm| {
                term.value
                    .map(|value| id("view ref commit", value))
                    .transpose()
            };
            let removes = conflict
                .removes
                .into_iter()
                .map(convert)
                .collect::<Result<Vec<_>, _>>()?;
            let adds = conflict
                .adds
                .into_iter()
                .map(convert)
                .collect::<Result<Vec<_>, _>>()?;
            interleave(removes, adds)?
        }
    };
    Ok(RefTarget(terms))
}

fn ref_target_to_proto(value: &RefTarget) -> proto::RefTarget {
    let term = |value: &Option<Id>| proto::RefConflictTerm {
        value: value.as_ref().map(|id| id.0.clone()),
    };
    proto::RefTarget {
        value: Some(proto::ref_target::Value::Conflict(proto::RefConflict {
            removes: value.0.iter().skip(1).step_by(2).map(term).collect(),
            adds: value.0.iter().step_by(2).map(term).collect(),
        })),
    }
}

fn state_from_proto(value: i32) -> Result<RemoteRefState, ValidationError> {
    match proto::RemoteRefState::try_from(value) {
        Ok(proto::RemoteRefState::New) => Ok(RemoteRefState::New),
        Ok(proto::RemoteRefState::Tracked) => Ok(RemoteRefState::Tracked),
        Err(_) => Err(ValidationError::new(format!(
            "invalid remote ref state: {value}"
        ))),
    }
}

fn state_to_proto(value: RemoteRefState) -> i32 {
    match value {
        RemoteRefState::New => proto::RemoteRefState::New as i32,
        RemoteRefState::Tracked => proto::RemoteRefState::Tracked as i32,
    }
}

fn remote_refs_from_proto(
    values: Vec<proto::RemoteRef>,
) -> Result<BTreeMap<String, RemoteRef>, ValidationError> {
    let mut output = BTreeMap::new();
    for value in values {
        if value.target_terms.is_empty() || value.target_terms.len().is_multiple_of(2) {
            return Err(ValidationError::new(format!(
                "remote ref {:?} must have an odd number of target terms",
                value.name
            )));
        }
        let terms = value
            .target_terms
            .into_iter()
            .map(|term| {
                term.value
                    .map(|value| id("view ref commit", value))
                    .transpose()
            })
            .collect::<Result<Vec<_>, _>>()?;
        output.insert(
            value.name,
            RemoteRef {
                target: RefTarget(terms),
                state: state_from_proto(value.state)?,
            },
        );
    }
    Ok(output)
}

fn remote_refs_to_proto(values: &BTreeMap<String, RemoteRef>) -> Vec<proto::RemoteRef> {
    values
        .iter()
        .map(|(name, value)| proto::RemoteRef {
            name: name.clone(),
            target_terms: value
                .target
                .0
                .iter()
                .map(|term| proto::RefTargetTerm {
                    value: term.as_ref().map(|id| id.0.clone()),
                })
                .collect(),
            state: state_to_proto(value.state),
        })
        .collect()
}

fn view_from_proto(value: proto::View) -> Result<View, ValidationError> {
    let mut wc_commit_ids = BTreeMap::new();
    if !value.wc_commit_id.is_empty() {
        wc_commit_ids.insert(
            "default".to_owned(),
            id("workspace commit", value.wc_commit_id)?,
        );
    }
    for (name, value) in value.wc_commit_ids {
        wc_commit_ids.insert(name, id("workspace commit", value)?);
    }
    let head_ids = value
        .head_ids
        .into_iter()
        .map(|value| id("view head commit", value))
        .collect::<Result<BTreeSet<_>, _>>()?;

    let mut local_bookmarks = BTreeMap::new();
    let mut legacy_remote_views: BTreeMap<String, RemoteView> = BTreeMap::new();
    for bookmark in value.bookmarks {
        let local = ref_target_from_proto(bookmark.local_target)?;
        for remote in bookmark.remote_bookmarks {
            let remote_view = legacy_remote_views.entry(remote.remote_name).or_default();
            remote_view.bookmarks.insert(
                bookmark.name.clone(),
                RemoteRef {
                    target: ref_target_from_proto(remote.target)?,
                    state: remote
                        .state
                        .map(state_from_proto)
                        .transpose()?
                        .unwrap_or(RemoteRefState::New),
                },
            );
        }
        if local.is_present() {
            local_bookmarks.insert(bookmark.name, local);
        }
    }

    let mut local_tags = BTreeMap::new();
    for tag in value.local_tags {
        local_tags.insert(tag.name, ref_target_from_proto(tag.target)?);
    }

    let mut remote_views = legacy_remote_views;
    if !value.remote_views.is_empty() {
        remote_views.clear();
        for remote in value.remote_views {
            remote_views.insert(
                remote.name,
                RemoteView {
                    bookmarks: remote_refs_from_proto(remote.bookmarks)?,
                    tags: remote_refs_from_proto(remote.tags)?,
                },
            );
        }
    }

    let mut git_refs = BTreeMap::new();
    for git_ref in value.git_refs {
        let target = if git_ref.target.is_some() {
            ref_target_from_proto(git_ref.target)?
        } else {
            RefTarget(vec![Some(id("view ref commit", git_ref.commit_id)?)])
        };
        git_refs.insert(git_ref.name, target);
    }
    let git_head = if value.git_head.is_some() {
        ref_target_from_proto(value.git_head)?
    } else if !value.git_head_legacy.is_empty() {
        RefTarget(vec![Some(id("view ref commit", value.git_head_legacy)?)])
    } else {
        RefTarget::absent()
    };

    Ok(View {
        head_ids,
        local_bookmarks,
        local_tags,
        remote_views,
        git_refs,
        git_head,
        wc_commit_ids,
    })
}

fn view_to_proto(view: &View) -> proto::CanonicalView {
    let mut all_bookmarks = BTreeSet::new();
    all_bookmarks.extend(view.local_bookmarks.keys().cloned());
    for remote in view.remote_views.values() {
        all_bookmarks.extend(remote.bookmarks.keys().cloned());
    }
    let bookmarks = all_bookmarks
        .into_iter()
        .map(|name| {
            let local = view
                .local_bookmarks
                .get(&name)
                .cloned()
                .unwrap_or_else(RefTarget::absent);
            let remote_bookmarks = view
                .remote_views
                .iter()
                .filter_map(|(remote_name, remote)| {
                    remote
                        .bookmarks
                        .get(&name)
                        .map(|remote_ref| proto::RemoteBookmark {
                            remote_name: remote_name.clone(),
                            target: Some(ref_target_to_proto(&remote_ref.target)),
                            state: Some(state_to_proto(remote_ref.state)),
                        })
                })
                .collect();
            proto::Bookmark {
                name,
                local_target: Some(ref_target_to_proto(&local)),
                remote_bookmarks,
            }
        })
        .collect();

    proto::CanonicalView {
        head_ids: view.head_ids.iter().map(|id| id.0.clone()).collect(),
        wc_commit_id: Vec::new(),
        git_refs: view
            .git_refs
            .iter()
            .map(|(name, target)| proto::GitRef {
                name: name.clone(),
                commit_id: Vec::new(),
                target: Some(ref_target_to_proto(target)),
            })
            .collect(),
        bookmarks,
        local_tags: view
            .local_tags
            .iter()
            .map(|(name, target)| proto::Tag {
                name: name.clone(),
                target: Some(ref_target_to_proto(target)),
            })
            .collect(),
        git_head_legacy: Vec::new(),
        wc_commit_ids: view
            .wc_commit_ids
            .iter()
            .map(|(key, value)| proto::StringBytesEntry {
                key: key.clone(),
                value: value.0.clone(),
            })
            .collect(),
        git_head: Some(ref_target_to_proto(&view.git_head)),
        remote_views: view
            .remote_views
            .iter()
            .map(|(name, remote)| proto::RemoteView {
                name: name.clone(),
                bookmarks: remote_refs_to_proto(&remote.bookmarks),
                tags: remote_refs_to_proto(&remote.tags),
            })
            .collect(),
        has_git_refs_migrated_to_remote_tags: true,
    }
}

fn collect_target_references(value: &RefTarget, output: &mut Vec<ObjectReference>) {
    output.extend(
        value
            .0
            .iter()
            .flatten()
            .map(|id| reference(ObjectKind::Commit, id)),
    );
}

pub(crate) fn validate_view(bytes: &[u8]) -> Result<ValidatedObject, ValidationError> {
    let value = proto::View::decode(bytes).context("decode view object")?;
    let view = view_from_proto(value)?;
    if view_to_proto(&view).encode_to_vec() != bytes {
        return Err(ValidationError::new(
            "view object is not canonically encoded",
        ));
    }

    let mut references = Vec::new();
    references.extend(
        view.head_ids
            .iter()
            .map(|id| reference(ObjectKind::Commit, id)),
    );
    references.extend(
        view.wc_commit_ids
            .values()
            .map(|id| reference(ObjectKind::Commit, id)),
    );
    for target in view
        .local_bookmarks
        .values()
        .chain(view.local_tags.values())
        .chain(view.git_refs.values())
    {
        collect_target_references(target, &mut references);
    }
    collect_target_references(&view.git_head, &mut references);
    for remote in view.remote_views.values() {
        for remote_ref in remote.bookmarks.values().chain(remote.tags.values()) {
            collect_target_references(&remote_ref.target, &mut references);
        }
    }
    references.sort_unstable();
    references.dedup();
    Ok(ValidatedObject {
        id: content_id(&view),
        references,
    })
}

#[derive(Clone, Copy)]
struct Timestamp {
    millis: i64,
    offset: i32,
}

impl ContentHash for Timestamp {
    fn hash(&self, state: &mut Blake2b512) {
        self.millis.hash(state);
        self.offset.hash(state);
    }
}

struct TimestampRange {
    start: Timestamp,
    end: Timestamp,
}

impl ContentHash for TimestampRange {
    fn hash(&self, state: &mut Blake2b512) {
        self.start.hash(state);
        self.end.hash(state);
    }
}

struct OperationMetadata {
    time: TimestampRange,
    description: String,
    hostname: String,
    username: String,
    is_snapshot: bool,
    workspace_name: Option<String>,
    attributes: BTreeMap<String, String>,
}

impl ContentHash for OperationMetadata {
    fn hash(&self, state: &mut Blake2b512) {
        self.time.hash(state);
        self.description.hash(state);
        self.hostname.hash(state);
        self.username.hash(state);
        self.is_snapshot.hash(state);
        self.workspace_name.hash(state);
        hash_map(self.attributes.iter(), state);
    }
}

struct Operation {
    view_id: Id,
    parents: Vec<Id>,
    metadata: OperationMetadata,
    commit_predecessors: Option<BTreeMap<Id, Vec<Id>>>,
}

impl ContentHash for Operation {
    fn hash(&self, state: &mut Blake2b512) {
        self.view_id.hash(state);
        self.parents.hash(state);
        self.metadata.hash(state);
        match &self.commit_predecessors {
            None => 0_u32.hash(state),
            Some(values) => {
                1_u32.hash(state);
                hash_map(values.iter(), state);
            }
        }
    }
}

fn timestamp(value: Option<proto::Timestamp>) -> Timestamp {
    let value = value.unwrap_or_default();
    Timestamp {
        millis: value.millis_since_epoch,
        offset: value.tz_offset,
    }
}

fn timestamp_proto(value: Timestamp) -> proto::Timestamp {
    proto::Timestamp {
        millis_since_epoch: value.millis,
        tz_offset: value.offset,
    }
}

pub(crate) fn validate_operation(bytes: &[u8]) -> Result<ValidatedObject, ValidationError> {
    let stored = proto::Operation::decode(bytes).context("decode operation object")?;
    if stored.parents.is_empty() {
        return Err(ValidationError::new(
            "root-like operations are synthesized and cannot be stored",
        ));
    }
    let metadata = stored.metadata.clone().unwrap_or_default();
    let mut commit_predecessors = BTreeMap::new();
    for entry in &stored.commit_predecessors {
        commit_predecessors.insert(
            id("commit predecessor key", entry.commit_id.clone())?,
            entry
                .predecessor_ids
                .iter()
                .cloned()
                .map(|value| id("commit predecessor", value))
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    let operation = Operation {
        view_id: id("operation view", stored.view_id.clone())?,
        parents: stored
            .parents
            .iter()
            .cloned()
            .map(|value| id("parent operation", value))
            .collect::<Result<Vec<_>, _>>()?,
        metadata: OperationMetadata {
            time: TimestampRange {
                start: timestamp(metadata.start_time.clone()),
                end: timestamp(metadata.end_time.clone()),
            },
            description: metadata.description.clone(),
            hostname: metadata.hostname.clone(),
            username: metadata.username.clone(),
            is_snapshot: metadata.is_snapshot,
            workspace_name: metadata.workspace_name.clone(),
            attributes: metadata.attributes.into_iter().collect(),
        },
        commit_predecessors: stored
            .stores_commit_predecessors
            .then_some(commit_predecessors),
    };

    let canonical_metadata = proto::CanonicalOperationMetadata {
        start_time: Some(timestamp_proto(operation.metadata.time.start)),
        end_time: Some(timestamp_proto(operation.metadata.time.end)),
        description: operation.metadata.description.clone(),
        hostname: operation.metadata.hostname.clone(),
        username: operation.metadata.username.clone(),
        attributes: operation
            .metadata
            .attributes
            .iter()
            .map(|(key, value)| proto::StringStringEntry {
                key: key.clone(),
                value: value.clone(),
            })
            .collect(),
        is_snapshot: operation.metadata.is_snapshot,
        workspace_name: operation.metadata.workspace_name.clone(),
    };
    let canonical_predecessors = operation
        .commit_predecessors
        .as_ref()
        .map(|values| {
            values
                .iter()
                .map(|(commit_id, predecessors)| proto::CommitPredecessors {
                    commit_id: commit_id.0.clone(),
                    predecessor_ids: predecessors.iter().map(|id| id.0.clone()).collect(),
                })
                .collect()
        })
        .unwrap_or_default();
    let canonical = proto::CanonicalOperation {
        view_id: operation.view_id.0.clone(),
        parents: operation.parents.iter().map(|id| id.0.clone()).collect(),
        metadata: Some(canonical_metadata),
        commit_predecessors: canonical_predecessors,
        stores_commit_predecessors: operation.commit_predecessors.is_some(),
    };
    if canonical.encode_to_vec() != bytes {
        return Err(ValidationError::new(
            "operation object is not canonically encoded",
        ));
    }

    let mut references = vec![reference(ObjectKind::View, &operation.view_id)];
    references.extend(
        operation
            .parents
            .iter()
            .map(|id| reference(ObjectKind::Operation, id)),
    );
    if let Some(predecessors) = &operation.commit_predecessors {
        for (commit, values) in predecessors {
            references.push(reference(ObjectKind::Commit, commit));
            references.extend(values.iter().map(|id| reference(ObjectKind::Commit, id)));
        }
    }
    references.sort_unstable();
    references.dedup();
    Ok(ValidatedObject {
        id: content_id(&operation),
        references,
    })
}
