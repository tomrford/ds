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

## Remaining proof

The next vertical slices are:

1. Discover the immutable object closure between a local operation frontier
   and the last cloud-accepted frontier.
2. Encode that closure as a deterministic manifest plus bounded chunks, then
   install and validate it through the Durable Object.
3. Atomically add a new cloud operation head while removing only the exact
   heads observed by the client, with an incarnation and idempotency key.
4. Reconcile concurrent cloud heads into each native repository using jj's
   operation machinery.
5. Run the 2-machine fault matrix at every upload, install, cursor and outbox
   boundary; retries must be idempotent and acknowledged state must survive.
6. Delete one fully synchronised machine store and rebuild it exactly from the
   cloud.
7. Run the same engine only at command boundaries and prove queued work is
   rediscovered even when the outbox hint is missing.
8. Measure warm local command latency with the network disabled. It must stay
   within 2 times local jj and make zero cloud requests.

Pack size, chunk count and SQLite versus R2 placement are measured outputs of
this spike. The protocol must not bake in a storage-provider-specific object
location.
