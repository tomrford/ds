# Synchronization and convergence

Stock jj repositories on 2 machines can diverge offline and converge through
the Cloudflare authority without losing acknowledged state. Git projection is
a separate subsystem described in [`git-projection.md`](git-projection.md).

## Commands and connectivity

Repository work is offline-first: ordinary jj-compatible commands operate on
the local checkout, while `ds sync`, the daemon and command-boundary sync move
native state to and from the cloud. `ds add` creates another checkout from a
local machine repository or resolves and clones it on first use. `ds remove`
discards a checkout while retaining its machine repository. `ds git fetch` and
`ds git push` own the explicit public Git boundary.

Repository directory commands are online-only. `ds repo new [<name>]` creates
an empty cloud repository. `ds repo add <git-url> [--name <name>]` imports a Git
remote without a checkout. `ds repo rename` and `ds repo list` update or read
the cloud directory. `ds init [<git-url>] [<directory>]` composes repository
creation or import with a first checkout. It does not convert a local Git
working copy.

## Native repository boundary

`devspace-machine` initializes and reloads a repository through jj-lib 0.42.0.
The durable native stores are exactly:

- `SimpleBackend`
- `SimpleOpStore`
- `SimpleOpHeadsStore`

The default index and submodule store complete jj's repository layout. The
index is rebuildable. The presence of jj's submodule store does not make Git
submodule tree entries encodable by the simple backend; repositories containing
Git links are unsupported.

The Git import boundary inspects trees and rejects a Git link before asking jj
to encode it; the simple backend's panic is not an acceptable rejection path.
The projection tests cover this boundary with a Git-link fixture.

Devspace sync metadata, cloud cursors, the durable outbox and the rebuildable
Git projection belong beside this repository. They do not replace or extend a
jj store.

The current integration tests prove that initialization writes jj's literal
store type names, a native operation survives a full reload, and a Git-backed
repository is rejected before jj attempts to load it.

## Machine-store catalog

One `MachineStore` owns the platform data root, the local repository catalog
and native repository locations. macOS uses
`~/Library/Application Support/devspace`, Linux uses the XDG data directory and
Windows uses the local application-data directory.
`DEVSPACE_MACHINE_STORE_DIR` exists only for deterministic bring-up and tests.

The versioned JSON catalog binds a validated tenant-visible name to a 64-digit
opaque repository ID and a 32-digit incarnation. Native repositories live under
paths derived from both opaque values, never from the tenant-visible name. A
single machine-local file lock serializes readers and writers. Mutations reread
the catalog while holding that lock, sync a new temporary file, atomically
replace the catalog and, on Unix, sync the root directory entry. A crashed
temporary file is ignored. Conflicting name reuse, reuse of an ID under another
name and stale incarnations fail closed.

The library exposes registration, incarnation-checked materialization,
repository open and incarnation-checked catalog removal. Materialization uses
an opaque per-identity lock, initializes in a temporary sibling and atomically
publishes only a complete native repository. Catalog removal never prunes native
repository data. Protected machine configuration stores the Worker URL, shared
development credential and machine ID in an atomic private file. Repository
creation adds the separate durable journal described below. Cloud first-use and
import remain outside this local boundary.

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
pack budget is an upper candidate, not a settled production choice. Pack and
manifest measurements must inform the final limit.

The binary manifest records the local operation heads, ordered object keys,
byte ranges, ordered chunk ranges, per-chunk Blake2b-512 hashes, total byte
length and a whole-pack Blake2b-512 hash. The pack ID is the Blake2b-512 hash
of the canonical manifest bytes. Counts, integers and reserved fields have one
fixed encoding. A checked private constructor enforces strictly ordered unique
heads and objects, at least one object, contiguous object and chunk ranges,
and every format budget before bytes can be encoded. The same closure, cloud inventory and options
therefore produce the same ordered pack set on different machines.

Packing reopens every source object and derives the recorded range from the
bytes actually read. File leaves are hashed while streaming. Symlinks and all
structured objects are validated by the kernel from that same open file before
their bytes are written. Source objects are currently limited to 1 MiB; this
keeps validation memory bounded until measurements establish final limits.

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

The Workers tests prove frozen Rust-format vectors — a minimal manifest and a
heads-carrying multi-chunk manifest — plus corrupt-chunk rejection,
early-install rejection, retry idempotency, transaction rollback after a later
object fails validation, cross-pack references and eviction between quarantine
and installation.

## Cloud object inventory

An authenticated POST to
`/repositories/:repository/objects/inventory` carries the repository
incarnation and at most 4,096 strictly sorted, unique object keys. The response
contains the sorted subset already installed in the Durable Object. Large
closures are split into independent bounded requests by the machine; the
endpoint never enumerates objects the machine did not ask about.

The response is authoritative for its incarnation. Installed objects are
content-addressed, immutable and protected by no-clobber checks, so a positive
answer remains valid across concurrent installs. A concurrent install can make
a negative answer stale, but the consequence is only an idempotent re-upload.
A lost response likewise leaves the next pass free to repeat negotiation or
upload the same immutable bytes.

The Worker decoder requires exact JSON fields, lowercase IDs, canonical key
ordering and the request byte limit before querying storage. The native HTTP
decoder applies the same ordering, kind, ID and exact-field checks to the
response and rejects any returned key outside the request page.

## Cloud download and native installation

Each installed pack receives an append-only repository-local sequence. An
incarnation-scoped catalog returns at most 256 logical pack IDs per page. The
first page captures a high-water sequence, and later pages are bounded by that
same value, so concurrent installs cannot extend or change an in-progress
catalog traversal. The sequence is a pagination cursor only; pack identity
remains the manifest hash. Client-supplied high-water values above the current
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

## Cloud identity and repository directory

The stateless HTTP auth adapter compares a shared development credential using
SHA-256 and Workers Web Crypto's timing-safe equality. It maps every valid
request to one fixed server-side development user and a validated machine ID
header. Callers cannot supply a user ID. The credential is a Worker secret,
never a source or `wrangler.jsonc` value, and the Worker keeps no mutable
request identity in global state.

Repository names are tenant-local directory entries. An authenticated create
request carries a 128-bit idempotency key. The control plane reserves the name,
assigns a Cloudflare-generated opaque Durable Object ID and random 128-bit
incarnation, initializes that repository object, then activates the record. A
retry with the same key and name resumes or returns the same result. Reusing the
key for another name fails. Activation is a compare-and-set transition from
provisional to active; a receipt whose repository is retiring or deleted can
never reactivate it.

`ds repo new [<name>]` writes its name, control-plane target, machine ID and
random 128-bit key to the machine-store creation journal before sending that
request. A lost response reuses the recorded key. Once a response is available,
the command durably records its opaque repository ID and incarnation before
catalog registration, then atomically publishes an empty stock repository. A
terminal name or key conflict discards the intent. A retired provisional
creation discards the intent and retries once with a fresh key. Authentication,
network and server failures retain the current intent. Successful local
materialization removes it because the catalog is then the durable record.

Deletion first blocks new authorization and retires the repository object. It
then frees the tenant-local name. Recreating the name produces a different ID
and incarnation. The control plane rejects another user's ID and every retired
or stale incarnation without revealing whether another tenant owns it. A
delete retry resumes a retiring row. Repository creation also recovers up to 64
retiring rows for that user, including provisional rows older than 24 hours,
and installs a retired Repository Durable Object tombstone even when
initialization never completed.

The Worker creates a typed `{ userId, machineId }` principal before calling the
control plane. Directory and repository authorization accept only that
principal and never inspect caller-supplied user IDs. The repository Durable
Object also checks the authenticated user, machine-derived authority, opaque ID
and incarnation before access. Synthetic principals test cross-user isolation
below the development-only HTTP adapter.

## Cloud operation heads

The control-plane creation saga initializes the repository Durable Object once
with its identity and incarnation. Head reads and writes require that current
authority and matching request incarnation.

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

The zero operation, zero view and root commit remain implicit jj objects; the
zero operation cannot be published as a head.
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
installed pack-catalog frontier. A second file records an ordered batch of
pending head transactions: each idempotency key and new head plus one shared
pre-batch cloud-head table and a per-entry subset bitmap. The bitmap keeps the
4,096 by 4,096 worst case bounded to about 2.6 MiB rather than repeating full
IDs. Both formats are strict, versioned, bounded to 4,096 heads and reject
noncanonical ordering and trailing bitmap bits.

The sidecar requires an existing machine-store parent; creating it syncs both
that parent and the new directory. Writes use a synced temporary file, atomic
rename and directory sync. Removing the outbox, including a retry after an
ambiguous removal failure, is also followed by a directory sync. The sync
engine holds the sidecar's process lock, writes the complete batch before
sending its first transaction, persists each accepted result before removing
that entry, and repairs and replays an existing batch before deriving new work.
Missing state starts at the zero frontier; malformed state fails closed.

The tests reopen the sidecar to prove cursor, frontier and exact request
survival, prove outbox clearing is idempotent, and reject truncated or
noncanonical files.

## Native sync engine and fault matrix

One generic engine owns the command or daemon sync pass behind the
machine-local process lock. It first negotiates and reuploads any cloud-missing
objects needed by an existing outbox, drains that exact batch and finishes the
pass. Otherwise it pages and installs cloud packs under one catalog high-water,
reads cloud heads, and asks stock jj to resolve all local and cloud operation
heads. It discovers the remaining local closure, negotiates its installed
object subset in 4,096-key pages, and builds and uploads only the missing
objects. The engine rereads the stock head set after upload and repeats this
snapshot at most 4 times when a concurrent local operation changes it.

Every unaccepted head in the final snapshot gets one transaction. The engine
walks its local operation ancestry and includes only pre-batch cloud heads
proven to be its ancestors. Siblings are absent from that table, so sequential
transactions cannot remove one another; a stale concurrent head also never
claims a cloud head outside its ancestry. One shared ordered head table plus
per-entry bitmaps keeps the outbox compact. The whole batch is durable before
the first request. Each accepted cursor and head set is persisted before that
entry is removed, so a crash between sibling transactions resumes with the
exact remaining suffix.

After a batch is fully accepted, or a pass finds no head work, the engine
removes at most 256 local pack directories. It deletes and syncs the manifest
first, then removes bounded chunk files, syncs the pack directory, removes it
and syncs the pack root. A crash can therefore leave only a manifest-less
directory, which cannot be reused as a valid immutable pack and is eligible for
the next cleanup pass. Cloud-installed packs remain the rebuild authority.

The transport boundary matches the Worker protocol but remains independent of
HTTP. Its head authority enforces the same observed-head ancestry rule. A
deterministic fault transport uses one-object packs and one-pack catalog pages,
and exercises catalog listing, pack download, a lost inventory response, lost
responses after manifest upload, chunk upload, pack install and head mutation,
plus failure before the head mutation. Re-presenting an already accepted
request from the outbox proves that a crash before outbox clearing replays the
receipt without advancing the cloud cursor again. Separate tests prove an
unchanged 64-object closure uploads no objects, concurrent divergent local
heads remain cloud siblings, a crash between their transactions preserves both,
and manifest-first pack cleanup recovers after an injected fault.

For every boundary, 2 stock repositories create different offline operations.
The first uploads, the second downloads and merges through jj before uploading
the merge, and the first downloads it back. Both finish at the same operation
ID, the cloud has one head and exactly 2 accepted head transactions, and no
outbox remains. The machines share only the fault transport; they never copy
objects directly or contact one another.

## Live Worker transport

`HttpTransport` implements the same transport contract over the Worker's HTTP
protocol: shared-secret authentication with a distinct machine ID, repository
authority,
incarnation-scoped inventory and catalog, manifest and chunk routes, and the
JSON head transaction. The ignored `cloud_live` cross-language probe contains
the two-machine flow. It remains a manual deployment probe because it requires
explicit Worker credentials and repository authority. The in-repo fault matrix
is the hermetic convergence proof.

## Exact cloud rebuild

The rebuild test synchronizes a stock repository, records its operation ID,
view and complete canonical object-key set, then deletes the entire machine
copy: native repository, sync sidecar and local packs. A newly initialized
stock repository starts with no cursor, downloads every logical
cloud pack and authoritative head through the same engine, and reconstructs
the exact operation ID, view and object set. Its rebuilt cursor and catalog
frontier match the cloud and no outbox remains.

## Command-boundary recovery

After each successful repository command, `ds` records every distinct machine
repository it opened. It sends `sync <name>` to the machine daemon over a
private local socket with a short bounded connect. A running daemon queues the
repository once. If the socket is unavailable, `ds` starts `ds daemon run`
detached, retries the notification for a bounded interval and starts a detached
`ds sync run --repository <name>` one-shot if notification still fails. The
command does not wait for Worker I/O. Command failures, `ds daemon` and `ds
sync` do not create another boundary.

`ds git` owns the Git boundary in Devspace checkouts. Its remote registry and
bookmark push commands replace stock jj Git behavior; all other Git subcommands
are fenced. `ds git fetch` and `ds git push` suppress the detached
command-boundary notification because they run the same in-process
sync work unit under the repository sync lock before projection or Git contact.

The daemon holds one machine-local singleton lock. It removes a stale socket,
drains every complete catalog repository on startup, processes notifications,
polls the catalog for remote work and exits after an idle timeout. Repository
sync still uses the existing per-repository and sidecar locks, so auto-start
races and overlapping one-shots are harmless. The exact outbox is written
before cloud mutation and each accepted entry is removed only after its cloud
state is durable. SIGKILL can leave a stale socket but cannot remove native
operations, accepted state or pending requests; restart replaces the socket and
replays the remaining work idempotently.

`DEVSPACE_DAEMON=0` selects detached one-shots at command boundaries. Windows
uses the same degraded behavior because the daemon socket is Unix-only.
`DEVSPACE_BOUNDARY_SYNC=0` disables all boundary work. The engine rediscovers
operations from jj's stock operation heads even when the sidecar has no outbox,
so re-enabling either mode at a later command boundary still converges local
work.

The single `ds status` sync line is unchanged and reads only local operation
heads, accepted heads and the outbox. `ds sync status` reads the same state for
every catalog repository, reports incomplete clones and pending counts, and
adds one `daemon: running` or `daemon: not running` line from a short local
`ping`. Neither status command constructs an HTTP transport or contacts the
Worker.

## Warm latency

The release-only probe compares `MachineRepository::open()` with stock jj's
`RepoLoader::load_at_head()` on the same warm stock repository, whose fixture
holds 64 chained operations and a 64-bookmark view so the open reads a real
operation and view. It alternates the order of 5 warm-up batches and 21
measured batches, takes the median batch time, and amortizes each over 20
opens. Three consecutive runs measured the Devspace wrapper at 1.309, 1.306
and 1.304 times stock jj, inside the 2 times budget for this shared subpath.
The probe is ignored by default and run explicitly; it does not gate CI.

The wrapper validates the 5 stock store-type markers and delegates to jj's
loader. This code path has no cloud client or `SyncTransport` value, so the
probe makes zero cloud requests by construction.

A supporting process probe exercised the same direct bare-repository shape:
open the repository, walk and render 50 commits with ignore-working-copy
semantics. Against an equivalent stock jj-lib program, including process
startup, its worst ratio was 1.0135 times stock jj-lib on 64-operation and
1,000-operation fixtures, with no Devspace-specific cost growing with
repository size. A network-denied run and a call-graph check confirmed zero
cloud requests. This probe did not invoke the public `ds` and pinned `jj` CLIs,
so it supports the design but does not close the complete-command budget. The
public CLI no-pain smoke check is tracked in
[issue 22](https://github.com/tomrford/devspace/issues/22).

The `ds` command runner resolves `ds -R <name> log` through the machine-store
catalog and opens the resulting bare repository directly through a custom jj
workspace loader. Its in-memory working-copy sentinel has no selected checkout,
so jj skips snapshotting and `@` is unavailable. The runner accepts only
repo-targeted `log` at this boundary and rejects commands that mutate or depend
on a working copy. The read-only loader prunes ancestor operation heads in
memory and requires exactly one remaining head; sync reconciles genuinely
divergent heads before command execution. Bare roots with jj config markers are
rejected, and raw native paths are not product-facing identities. A missing
local name fails without a cloud request on this warm read path. `ds add`
handles cloud first use by resolving the name, synchronizing a staged native
repository and publishing the complete clone. Normal jj workspaces, including
explicit `-R` workspace paths, continue through jj's stock loader.

Inside an owned Devspace checkout, the runner intercepts the parsed `git`
subcommand before stock dispatch. Ordinary jj workspaces continue to use stock
Git commands. Product Git commands require a checkout; repository-targeted bare
mode remains read-only `log` only.
