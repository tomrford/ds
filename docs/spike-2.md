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

## Cloud installation

The Worker exposes authenticated manifest and chunk PUTs under a pack ID and a
separate install POST. The pack ID must match the Blake2b-512 manifest hash.
The Durable Object decodes the Rust manifest format strictly, including all
reserved fields, ordering, range and size bounds, before it creates quarantine
state. Manifest and chunk retries are byte-exact and idempotent.

Quarantine is an implementation detail. SQLite values are split into 1 MiB
parts below the row limit, but neither the manifest nor any HTTP identifier
contains a SQLite or R2 location. A later storage move therefore preserves the
wire protocol and content identities.

Install requires every declared chunk. In one synchronous Durable Object
transaction it rereads bounded chunk parts, verifies each chunk and the
whole-pack hash incrementally, reconstructs at most one 1 MiB object at a time,
and validates every object and ID through the Wasm kernel. Only then does it
promote the canonical manifest plus head, object and chunk indexes to installed
state and discard the transient uploaded chunk parts. The object table and
installed index can reproduce the pack without making its identity depend on
the current byte-store layout. Any error rolls back the object writes. An
installed pack is also an idempotency record, so a lost install response is
safe to retry; later manifest and chunk retries are checked against its retained
index before they are acknowledged.

Pack installation deliberately does not require every reference to exist yet
and never advances a head. Deterministic multi-pack splitting can place a
same-kind parent in a later pack. The head transaction is the correct closure
gate: it can advance only after the complete referenced graph exists, while
individual packs remain independently uploadable in any order.

The Workers tests prove a frozen Rust-format vector, corrupt-chunk rejection,
early-install rejection, retry idempotency, transaction rollback after a later
object fails validation, cross-pack references and eviction between quarantine
and installation.

## Cloud download and native installation

Each installed pack receives an append-only repository-local sequence. An
incarnation-scoped catalog returns at most 256 logical pack IDs per page. The
first page captures a high-water sequence, and later pages are bounded by that
same value, so concurrent installs cannot extend or change an in-progress
catalog traversal. The sequence is a pagination cursor only; pack identity
remains the manifest hash. A separate catalog table is backfilled
idempotently in bounded, retryable batches for Durable Objects created before
download support existed. Client-supplied high-water values above the current
catalog frontier are rejected.

Authenticated GETs return the exact installed manifest and its logical chunks.
The Durable Object reconstructs each chunk from canonical objects and the
installed pack index, then checks the chunk hash before returning it. Stored
manifest parts are also rehashed against the requested pack ID. Neither the
catalog nor the pack URLs reveal whether those bytes currently live in SQLite,
R2 or another byte store.

`devspace-machine` strictly decodes the same manifest format, checks the
manifest ID, every chunk and the whole-pack hash, and validates every object
and ID through the kernel. Canonical objects are installed into jj's stock
simple stores using synced no-clobber writes. An exact existing object makes a
retry idempotent and retries its parent-directory sync; different bytes at an
existing content path fail closed.
Valid immutable objects installed before a later pack failure may remain as a
safe cache, but pack installation never publishes an operation head.

The Worker test reconstructs a multi-object pack whose object crosses a chunk
boundary and compares every downloaded byte with the upload. The native test
builds a real repository pack, installs it into a fresh stock repository,
retries it idempotently and only then reconciles its operation head. Corrupt
download bytes are rejected without changing the stock head store.

## Cloud operation heads

The repository Durable Object is initialized once with a 128-bit incarnation.
Initialization is currently a service-binding RPC because the control plane is
outside this spike. Repeating the same initialization is safe; a different
incarnation is rejected. Head reads and writes require the matching
incarnation.

An authenticated head transaction carries one new operation head, the exact
set of heads observed by the client and a 128-bit idempotency key. The request
surface and resulting authoritative head set are bounded at 4,096 heads. The
observed set is sorted before its canonical request hash is computed. In one
bounded maintenance transaction the Durable Object prunes expired receipts.
It then uses one SQLite transaction to:

1. checks the incarnation and any existing idempotency receipt;
2. walks the new operation's complete reference graph and rejects the update
   if any non-implicit object is absent;
3. proves that every observed head which is still current is in the new
   operation's parent ancestry;
4. removes only those observed heads, adds the new head and advances a
   monotonic cursor; and
5. stores the cursor and resulting ordered head set as the exact retry result.

The zero operation, zero view and root commit remain implicit jj objects.
Failed closure or ancestry checks do not consume the idempotency key, so a
client can install a missing pack and retry the same logical request. Complete
object closures are recorded once; later descendants stop at that immutable
proven frontier instead of rescanning the repository. A successful replay
within the 7-day receipt window returns its original cursor and head set even
if later transactions changed the repository. A reused key with different
canonical input is rejected. Receipt and stored-head quotas bound SQLite use,
and expired receipts are removed in bounded batches. A retry after expiry can
at worst restore an ancestral head as explicit divergence: the ancestry rule
still prevents it from deleting unrelated acknowledged work.

The Workers tests exercise real branch and merge operation ancestry, stale
concurrent clients, rejection of unrelated-head removal, exact-observation
removal, ordered convergence, exact retry replay, conflicting key reuse,
incarnation isolation, transitive closure failures followed by pack
installation, Durable Object eviction and protocol bounds.

## Native reconciliation

After cloud objects have been installed in a native repository,
`devspace-machine` validates their complete closure from the supplied operation
heads before changing jj's stock operation-head store. It then holds the stock
head-store lock while adding every cloud head without removing any local head.
The lock is released before the repository reloads through jj's
`RepoLoader::load_at_head()`.

Structured objects are rechecked through the kernel during closure discovery.
At this authority boundary, file leaves are additionally hashed while streaming
and symlink bytes are checked for both their ID and UTF-8 validity. A mid-batch
head-store write can leave an earlier immutable head durably added because the
stock file store has no batch API. In that case the method reloads and
reconciles every successfully visible head before returning an explicit partial
publication error, so its exposed repository cannot remain behind its durable
head store.

That reload uses jj's own operation resolver. Ancestor heads are removed without
creating another operation. Genuine divergent heads are merged through jj's
operation and view machinery, and the resulting merge operation becomes the
single stock head. Reapplying an already merged ancestor therefore resolves to
the existing merge operation rather than producing another one.

The integration tests create 2 stock repositories, make different offline
operations, install one native closure into the other and prove jj creates a
merge containing both views. Installing the merged closure back into the first
repository converges both to the same operation ID and view. A separate test
copies an operation without its view and proves validation fails before the
cloud head is published locally. Further tests corrupt file and symlink leaves,
and inject a failure after the first of 2 cloud-head writes to prove recovery
keeps the exposed repository aligned with the durable stock head store.

## Durable machine sync state

`MachineSyncStore` owns a machine-local sidecar, separate from every stock jj
store. One file records the last accepted cloud cursor and head set plus the
installed pack-catalog frontier. A second file records the exact pending head
transaction: its idempotency key, new head and observed heads. Both formats are
strict, versioned, bounded to 4,096 heads and reject noncanonical ordering.

The sidecar requires an existing machine-store parent; creating it syncs both
that parent and the new directory. Writes use a synced temporary file, atomic
rename and directory sync. Removing the outbox, including a retry after an
ambiguous removal failure, is also followed by a directory sync. The sync engine will
hold the sidecar's process lock, write the outbox before sending a head
transaction, persist the accepted result before clearing it, and repair and
replay an existing outbox before deriving new work. Missing state starts at the zero
frontier; malformed state fails closed.

The tests reopen the sidecar to prove cursor, frontier and exact request
survival, prove outbox clearing is idempotent, and reject truncated or
noncanonical files.

## Native sync engine and fault matrix

One generic engine now owns the command or daemon sync pass behind the
machine-local process lock. It first reuploads the complete closure for an
existing outbox and replays that exact request, then pages and
installs cloud packs under one catalog high-water, reads cloud heads, and asks
stock jj to resolve all local and cloud operation heads. It then discovers the
remaining local closure, builds and uploads deterministic packs, durably writes
the exact head request, sends it, persists the returned cursor and heads, and
only then clears the outbox.

The transport boundary matches the Worker protocol but remains independent of
HTTP. Its head authority enforces the same observed-head ancestry rule. A
deterministic fault transport uses one-object packs and one-pack catalog pages,
and exercises catalog listing, pack download, lost responses after manifest
upload, chunk upload, pack install and head mutation, plus failure before the
head mutation. Re-presenting an already accepted request from the outbox proves
that a crash before outbox clearing replays the receipt without advancing the
cloud cursor again.

For every boundary, 2 stock repositories create different offline operations.
The first uploads, the second downloads and merges through jj before uploading
the merge, and the first downloads it back. Both finish at the same operation
ID, the cloud has one head and exactly 2 accepted head transactions, and no
outbox remains. The machines share only the fault transport; they never copy
objects directly or contact one another.

## Exact cloud rebuild

The rebuild test synchronizes a stock repository, records its operation ID,
view and complete canonical object-key set, then deletes the entire machine
copy: native repository, sync sidecar and local packs. A newly initialized
stock repository starts with no cursor or inventory, downloads every logical
cloud pack and authoritative head through the same engine, and reconstructs
the exact operation ID, view and object set. Its rebuilt cursor and catalog
frontier match the cloud and no outbox remains.

## Command-boundary recovery

The command-boundary test runs no daemon. A native jj transaction commits an
operation while the sidecar has no outbox entry, modelling a crash before the
hint is written. The next engine invocation rediscovers the operation from
jj's stock operation heads, uploads its closure and writes the exact head
request before a fault prevents cloud mutation. A separate invocation repairs
and replays that queued request, then a later native transaction is likewise
discovered and published at the following command boundary. Both accepted
cursors and heads persist and no outbox remains.

## Remaining proof

The remaining vertical slice measures warm local command latency with the
network disabled. It must stay within 2 times local jj and make zero cloud
requests.

Pack size, chunk count and SQLite versus R2 placement are measured outputs of
this spike. The protocol must not bake in a storage-provider-specific object
location.
