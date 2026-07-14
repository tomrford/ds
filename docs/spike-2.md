# Spike 2: convergence proof

Phase 2 proves that stock jj repositories on 2 machines can diverge offline
and converge through the Cloudflare authority without losing acknowledged
state. It does not implement Git projection.

## Native repository boundary

`devspace-machine` initializes and reloads a repository through jj-lib 0.42.0.
The durable native stores are exactly:

- `SimpleBackend`
- `SimpleOpStore`
- `SimpleOpHeadsStore`

The default index and submodule store complete jj's repository layout. The
index is rebuildable. The presence of jj's submodule store does not make Git
submodule tree entries encodable by the simple backend; repositories containing
Git links are unsupported and must be rejected before Git import.

Devspace sync metadata, cloud cursors, the durable outbox and the rebuildable
Git projection belong beside this repository. They do not replace or extend a
jj store.

The current integration tests prove that initialization writes jj's literal
store type names, a native operation survives a full reload, and a Git-backed
repository is rejected before jj attempts to load it.

## Object closure

The sync input is derived from the stock operation-head store, not from the
outbox. Starting from every local operation head, `devspace-machine` walks raw
canonical files under the simple backend and operation store. Structured
objects are validated by `devspace-kernel`, which also returns their references.
File and symlink leaves are retained as a path and length so the pack writer can
stream them in the next slice. Leaves are not hash-verified at discovery: the
pack writer must verify each leaf against its ID while streaming and treat the
recorded length as advisory. Structured objects are rejected above the current
1 MiB Wasm validation limit before they are read into memory.

The zero operation, zero view and root commit are implicit jj objects with no
canonical file. An exact cloud-accepted operation head stops traversal of that
branch. All current local heads remain in the result, including divergent
heads; no reconciliation happens during discovery.

Accepted-head pruning cuts only the operation-parent chain. Every unaccepted
operation's view still reaches the full commit graph, so discovery opens and
validates the complete reachable object set on every run. Two obligations
follow: the manifest must deduplicate against objects the cloud already holds,
or uploads are O(repo) per sync; and discovery cost scales with repository
size, which bears directly on the warm-latency budget. Both are measured, not
assumed.

The integration tests create 2 offline operations from one base, prove both
closures remain reachable, prove one accepted head does not hide the other,
exercise the exact commit, tree, file and symlink paths, and prove missing
leaves and missing, corrupt or oversized structured objects fail closed.

## Remaining proof

The next vertical slices are:

1. Encode the discovered closure as a deterministic manifest plus bounded
   chunks, deduplicated against objects the cloud already holds, with leaf
   hashes verified during streaming, then install and validate it through the
   Durable Object.
2. Atomically add a new cloud operation head while removing only the exact
   heads observed by the client, with an incarnation and idempotency key.
3. Reconcile concurrent cloud heads into each native repository using jj's
   operation machinery.
4. Run the 2-machine fault matrix at every upload, install, cursor and outbox
   boundary; retries must be idempotent and acknowledged state must survive.
5. Delete one fully synchronised machine store and rebuild it exactly from the
   cloud.
6. Run the same engine only at command boundaries and prove queued work is
   rediscovered even when the outbox hint is missing.
7. Measure warm local command latency with the network disabled. It must stay
   within 2 times local jj and make zero cloud requests.

Pack size, chunk count and SQLite versus R2 placement are measured outputs of
this spike. The protocol must not bake in a storage-provider-specific object
location.
