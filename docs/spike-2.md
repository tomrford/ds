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
File and symlink leaves are retained as a path and advisory length. The pack
writer opens each path once and verifies the bytes against the object ID it is
packing. Structured objects are rejected above the current 1 MiB Wasm
validation limit before they are read into memory.

The zero operation, zero view and root commit are implicit jj objects with no
canonical file. An exact cloud-accepted operation head stops traversal of that
branch. All current local heads remain in the result, including divergent
heads; no reconciliation happens during discovery.

Accepted-head pruning cuts only the operation-parent chain. Every unaccepted
operation's view still reaches the full commit graph, so discovery opens and
validates the complete reachable object set on every run. Cloud object
inventory removes known objects before packing, and pack metrics distinguish
discovered, skipped and written objects. Discovery cost still scales with
repository size and bears directly on the warm-latency budget; it must be
measured rather than assumed.

The integration tests create 2 offline operations from one base, prove both
closures remain reachable, prove one accepted head does not hide the other,
exercise the exact commit, tree, file and symlink paths, and prove missing
leaves and missing, corrupt or oversized structured objects fail closed.

## Pack format

`devspace-machine` turns a closure into an ordered set of immutable packs.
Objects are ordered by kind and ID, and objects already present in the supplied
cloud inventory are omitted. No object crosses a pack boundary. The remaining
canonical bytes are concatenated within each pack and split into fixed size
chunks without padding.

The provisional budgets are 64 MiB and 65,536 objects per pack, 4,096 operation
heads per manifest, and 64 KiB to 8 MiB per chunk with a 1 MiB default. These
bounds keep every pack, manifest allocation and retry scan finite. The 64 MiB
pack budget is the largest candidate from the v3 plan, not a settled production
choice; the spike measures pack and manifest sizes before that choice is made.

The binary manifest records the local operation heads, ordered object keys,
byte ranges, ordered chunk ranges, per-chunk Blake2b-512 hashes, total byte
length and a whole-pack Blake2b-512 hash. The pack ID is the Blake2b-512 hash
of the canonical manifest bytes. Counts, integers and reserved fields have one
fixed encoding. A checked private constructor enforces strictly ordered unique
heads and objects, contiguous object and chunk ranges, and every format budget
before bytes can be encoded. The same closure, cloud inventory and options
therefore produce the same ordered pack set on different machines.

Packing reopens every source object and derives the recorded range from the
bytes actually read. File leaves are hashed while streaming. Symlinks and all
structured objects are validated by the kernel from that same open file before
their bytes are written. Source objects are currently limited to 1 MiB; this
keeps validation memory bounded until the measured spike establishes final
limits.

Chunks and the manifest are written into a temporary directory and made
visible under the pack ID only after all files have been synced. Rebuilding an
existing pack verifies its manifest and every chunk before reuse. The tests
prove byte-for-byte determinism, cloud-known filtering, deterministic
multi-pack boundaries, bounded chunking, source revalidation and
corrupted-pack rejection.

## Remaining proof

The remaining vertical slices are:

1. Upload the manifest and chunks, then validate and install the pack through
   the Durable Object without baking SQLite or R2 locations into the protocol.
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
