//! jj-lib 0.42 simple operation-store protobuf shapes.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Timestamp {
    #[prost(int64, tag = "1")]
    pub millis_since_epoch: i64,
    #[prost(int32, tag = "2")]
    pub tz_offset: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct RefConflictLegacy {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub removes: Vec<Vec<u8>>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub adds: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct RefConflict {
    #[prost(message, repeated, tag = "1")]
    pub removes: Vec<RefConflictTerm>,
    #[prost(message, repeated, tag = "2")]
    pub adds: Vec<RefConflictTerm>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct RefConflictTerm {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub value: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct RefTarget {
    #[prost(oneof = "ref_target::Value", tags = "1, 2, 3")]
    pub value: Option<ref_target::Value>,
}

pub(crate) mod ref_target {
    use alloc::vec::Vec;

    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Value {
        #[prost(bytes = "vec", tag = "1")]
        CommitId(Vec<u8>),
        #[prost(message, tag = "2")]
        ConflictLegacy(super::RefConflictLegacy),
        #[prost(message, tag = "3")]
        Conflict(super::RefConflict),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct RefTargetTerm {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub value: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct RemoteBookmark {
    #[prost(string, tag = "1")]
    pub remote_name: String,
    #[prost(message, optional, tag = "2")]
    pub target: Option<RefTarget>,
    #[prost(enumeration = "RemoteRefState", optional, tag = "3")]
    pub state: Option<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Bookmark {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "2")]
    pub local_target: Option<RefTarget>,
    #[prost(message, repeated, tag = "3")]
    pub remote_bookmarks: Vec<RemoteBookmark>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct GitRef {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(bytes = "vec", tag = "2")]
    pub commit_id: Vec<u8>,
    #[prost(message, optional, tag = "3")]
    pub target: Option<RefTarget>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct RemoteRef {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, repeated, tag = "2")]
    pub target_terms: Vec<RefTargetTerm>,
    #[prost(enumeration = "RemoteRefState", tag = "3")]
    pub state: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Tag {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "2")]
    pub target: Option<RefTarget>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct RemoteView {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, repeated, tag = "2")]
    pub bookmarks: Vec<RemoteRef>,
    #[prost(message, repeated, tag = "3")]
    pub tags: Vec<RemoteRef>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct View {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub head_ids: Vec<Vec<u8>>,
    #[prost(bytes = "vec", tag = "2")]
    pub wc_commit_id: Vec<u8>,
    #[prost(message, repeated, tag = "3")]
    pub git_refs: Vec<GitRef>,
    #[prost(message, repeated, tag = "5")]
    pub bookmarks: Vec<Bookmark>,
    #[prost(message, repeated, tag = "6")]
    pub local_tags: Vec<Tag>,
    #[prost(bytes = "vec", tag = "7")]
    pub git_head_legacy: Vec<u8>,
    #[prost(btree_map = "string, bytes", tag = "8")]
    pub wc_commit_ids: BTreeMap<String, Vec<u8>>,
    #[prost(message, optional, tag = "9")]
    pub git_head: Option<RefTarget>,
    #[prost(message, repeated, tag = "11")]
    pub remote_views: Vec<RemoteView>,
    #[prost(bool, tag = "12")]
    pub has_git_refs_migrated_to_remote_tags: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Operation {
    #[prost(bytes = "vec", tag = "1")]
    pub view_id: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub parents: Vec<Vec<u8>>,
    #[prost(message, optional, tag = "3")]
    pub metadata: Option<OperationMetadata>,
    #[prost(message, repeated, tag = "4")]
    pub commit_predecessors: Vec<CommitPredecessors>,
    #[prost(bool, tag = "5")]
    pub stores_commit_predecessors: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct OperationMetadata {
    #[prost(message, optional, tag = "1")]
    pub start_time: Option<Timestamp>,
    #[prost(message, optional, tag = "2")]
    pub end_time: Option<Timestamp>,
    #[prost(string, tag = "3")]
    pub description: String,
    #[prost(string, tag = "4")]
    pub hostname: String,
    #[prost(string, tag = "5")]
    pub username: String,
    #[prost(btree_map = "string, string", tag = "6")]
    pub attributes: BTreeMap<String, String>,
    #[prost(bool, tag = "7")]
    pub is_snapshot: bool,
    #[prost(string, optional, tag = "8")]
    pub workspace_name: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct CommitPredecessors {
    #[prost(bytes = "vec", tag = "1")]
    pub commit_id: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub predecessor_ids: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub(crate) enum RemoteRefState {
    New = 0,
    Tracked = 1,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct StringBytesEntry {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(bytes = "vec", tag = "2")]
    pub value: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct StringStringEntry {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct OrderedView {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub head_ids: Vec<Vec<u8>>,
    #[prost(bytes = "vec", tag = "2")]
    pub wc_commit_id: Vec<u8>,
    #[prost(message, repeated, tag = "3")]
    pub git_refs: Vec<GitRef>,
    #[prost(message, repeated, tag = "5")]
    pub bookmarks: Vec<Bookmark>,
    #[prost(message, repeated, tag = "6")]
    pub local_tags: Vec<Tag>,
    #[prost(bytes = "vec", tag = "7")]
    pub git_head_legacy: Vec<u8>,
    #[prost(message, repeated, tag = "8")]
    pub wc_commit_ids: Vec<StringBytesEntry>,
    #[prost(message, optional, tag = "9")]
    pub git_head: Option<RefTarget>,
    #[prost(message, repeated, tag = "11")]
    pub remote_views: Vec<RemoteView>,
    #[prost(bool, tag = "12")]
    pub has_git_refs_migrated_to_remote_tags: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct OrderedOperation {
    #[prost(bytes = "vec", tag = "1")]
    pub view_id: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub parents: Vec<Vec<u8>>,
    #[prost(message, optional, tag = "3")]
    pub metadata: Option<OrderedOperationMetadata>,
    #[prost(message, repeated, tag = "4")]
    pub commit_predecessors: Vec<CommitPredecessors>,
    #[prost(bool, tag = "5")]
    pub stores_commit_predecessors: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct OrderedOperationMetadata {
    #[prost(message, optional, tag = "1")]
    pub start_time: Option<Timestamp>,
    #[prost(message, optional, tag = "2")]
    pub end_time: Option<Timestamp>,
    #[prost(string, tag = "3")]
    pub description: String,
    #[prost(string, tag = "4")]
    pub hostname: String,
    #[prost(string, tag = "5")]
    pub username: String,
    #[prost(message, repeated, tag = "6")]
    pub attributes: Vec<StringStringEntry>,
    #[prost(bool, tag = "7")]
    pub is_snapshot: bool,
    #[prost(string, optional, tag = "8")]
    pub workspace_name: Option<String>,
}
