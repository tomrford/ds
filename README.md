# Devspace

Devspace synchronizes stock jj repositories through Cloudflare. Its validation
kernel stays independent of `jj-lib`, compiles to small WebAssembly and runs
inside a Cloudflare Durable Object. The machine store uses jj's stock simple
backend, operation store and operation-head store. A rebuildable Git sidecar
projects public history while the Durable Object owns its policy and recovery
journal.

## Development

Enter the pinned toolchain and run the full gate:

```sh
nix develop
pnpm install
pnpm types
pnpm check
pnpm test
pnpm build
```

During development, every machine uses the same Worker secret and sends its
locally configured machine ID. The HTTP auth adapter maps that credential to
one fixed server-side user; callers cannot select a user. An authenticated
POST to `/repositories` reserves a tenant-local name under a 128-bit
idempotency key and returns an opaque repository ID and 128-bit incarnation.
GET and DELETE on `/repositories/:name` resolve and retire that directory
record. Directory API failures return `error` prose and include a stable
kebab-case `code` when callers can act on a specific rejection. Deleting and
recreating a name assigns a new ID and incarnation.

The Worker accepts repository-authorized manifest and chunk uploads under
`/repositories/:repository-id/packs/:pack`, followed by an explicit install
request. Every data-plane request carries the current incarnation. Uploads are
quarantined until the Durable Object has checked the
manifest, chunk and whole-pack hashes and revalidated every object through the
Rust/Wasm kernel in one atomic install transaction. Authenticated head reads
and transactions use `/repositories/:repository-id/heads`. An
incarnation-scoped, paginated GET on `/repositories/:repository/packs` lists
installed logical packs; their exact manifests and chunks are downloadable
from the same pack routes. A bounded POST on
`/repositories/:repository/objects/inventory` returns the installed subset of
the machine's sorted candidate object keys for that incarnation.

## Current boundary

The no-I/O Rust kernel owns canonical protobuf validation, jj-compatible object
IDs, referenced-object extraction, and hidden-path parsing. The Worker owns
request limits and stateless routing. One SQLite-backed control-plane Durable
Object owns the per-user name directory and repository-creation receipts. One
SQLite-backed Durable Object per opaque repository ID
owns object persistence, the repository incarnation, authoritative operation
heads, monotonic cursors and idempotency receipts. A head transaction checks
that its complete object closure is installed, adds the new head and removes
only the exact heads observed by that client.

The shared credential is configured as the `DEVSPACE_SHARED_SECRET` Worker
secret with `wrangler secret put`; it is absent from source and
`wrangler.jsonc`.
`DEVSPACE_DEVELOPMENT_USER_ID` is the non-secret fixed identity. This adapter is
development-only. Dogfooding replaces it with real multi-user authentication
without changing typed principals, repository authorization or transports. The
control plane scopes every directory and data-plane query by user ID, but with
one fixed development user that isolation is exercised only by unit tests;
demonstrating it end to end is an open gate for machine enrolment.

## Offline behavior

Reads and edits in an existing checkout are local jj operations against the
machine store. After a successful repository command, `ds` notifies the
machine sync daemon once for each repository the command opened. The local
socket notification is bounded and the command never waits for cloud work. If
the daemon is not running, `ds` starts it detached, retries the notification
briefly and falls back to a detached one-shot sync if startup fails. The daemon
drains every complete catalog repository on startup, responds to command
notifications, polls for remote work and exits after an idle timeout. Work
created offline converges when a later command boundary or daemon poll reaches
the Worker. jj's operation merge absorbs divergence created on other machines.

Set `DEVSPACE_DAEMON=0` to use detached one-shot sync at command boundaries.
Windows uses this degraded mode because the local daemon socket is Unix-only.
Set `DEVSPACE_BOUNDARY_SYNC=0` to disable daemon notification, auto-start and
one-shots entirely. Local operations and durable pending requests remain in the
machine store, so a later enabled boundary still converges them.

`ds status` adds one local-only sync line to jj's status output. It reports
whether the checkout has never synchronized, has operations pending upload, or
matches the cloud heads recorded by the last successful sync. The indicator
never contacts the Worker. `ds sync status` gives the local picture for every
catalog repository: incomplete clones, pending operation counts and the last
successful-sync guidance available from the existing sidecar. Its only IPC is
a short daemon `ping`; it never contacts the Worker. `ds sync run --repository
<name>` remains the plumbing command used by degraded boundary sync and manual
recovery.

`ds repo new` requires connectivity because reserving a tenant-local name is a
global operation on the directory. Offline repository creation is outside the
current boundary; names are directory bindings rather than repository
identities, so a future offline path can create under a provisional local
identity and bind the name at first contact.

The native machine crate initializes and reloads stock jj repositories. It
rejects repositories whose backend, operation store, operation-head store,
index or submodule-store type differs from the jj 0.42 defaults. Git projection
and sync state live beside this native repository, not inside replacement jj
stores. The same crate discovers deterministic raw-object closures from all
local operation heads, stops at the cloud-accepted operation frontier, and
encodes cloud-missing objects into deterministic, size-bounded, hash-verified
packs.

One `MachineStore` owns the platform data directory, a durable local repository
catalog, the repository-creation journal and native repository locations. The
catalog binds a tenant-visible name to an opaque repository ID and incarnation;
native paths contain those 2 opaque values and never the name. Catalog and
repository-creation-journal changes use machine-local file locks and synced
atomic replacement. Registration, materialization and catalog-only removal are
library seams; removal does not prune native repository data. On macOS the
default root is `~/Library/Application Support/devspace`; Linux uses the XDG
data directory and Windows uses the local application-data directory.
`DEVSPACE_MACHINE_STORE_DIR` is a bring-up and test-only root override.

The `ds` binary embeds jj-cli 0.42 as its parser and command engine. `ds repo
new <name>` durably records a random 128-bit idempotency key before claiming the
cloud name. A retry replays only that exact request, persists the returned
opaque identity, registers it in the catalog and atomically publishes an empty
stock-format bare repository. Network, authentication and server failures leave
the pending key available for a later replay. A terminal cloud conflict discards
the intent; an expired provisional creation is retried once immediately with a
fresh key. Successful local materialization removes the intent because the
catalog is then the durable record, and repeating the command reports that the
repository already exists on this machine. Normal jj workspaces use jj's stock
loader and command behavior. `ds -R <name> log` resolves the name through the
local catalog and opens its stock bare repository directly, with no temporary
workspace or working-copy snapshot. There is no selected checkout, so `@` is
unavailable. Other commands against a bare machine repository are rejected;
this read path constructs no Devspace sync transport or HTTP client. Bare roots
with jj repository or workspace config markers are rejected, and raw native
paths are not repository identities. A missing local name fails locally because
cloud first-use belongs to the repository lifecycle commands rather than the
warm read path. Existing workspace paths, including explicit `-R` paths, retain
stock jj behavior.

`ds add <name> -r <revision> <path>` creates a working checkout. On first use on
a machine, it resolves the cloud directory entry, synchronizes a staged native
repository and publishes the complete clone before creating the checkout. This
first use requires connectivity. Later checkouts use the local machine
repository. Both the revision and destination are explicit. Named revisions
resolve against the locally accepted repository; plain `@`, `@-` and `@+`
resolve only when the command runs inside another checkout of the same
repository. The workspace identity is the machine ID plus a stable digest of
the canonical destination path, so the same machine and path always select the
same workspace. The checkout carries a self-describing
`.jj/devspace-checkout-owner` marker and a stock `.jj/repo` pointer to the shared
native repository. Its working-copy state stays in the checkout.

`ds add` decides retries from the repository view and destination directory. A
matching destination, workspace and requested parent is already complete; the
command refreshes the workspace-path record and succeeds. If the destination is
absent but that workspace is registered at the requested parent, the command
rebuilds the checkout at its current working-copy commit. A workspace registered
at another parent must be requested with that matching revision. Any destination
without the matching ownership marker is left untouched and rejected. These
checks defend against accidental collisions, concurrent creators and stale
leftovers; they are not a security boundary against other local processes,
which own the same files the checkout does.

Fresh creation writes the working-copy commit and workspace registration in one
repository transaction. The checkout is built from scratch in a deterministic
sibling staging directory, synchronized and published with a no-replace rename.
Stale staging with the matching marker is disposable and rebuilt wholesale. A
machine-local per-destination lock rejects simultaneous creators; its file has no
state. Removing a completed checkout directory and repeating the same command
rebuilds it from the registered workspace. Use `ds remove` for normal removal
so the workspace identity is freed.

`ds remove <path>` removes one checkout without removing its native repository
or catalog entry. The command accepts only a directory with a valid Devspace
ownership marker whose repository identity is in the local catalog. It
snapshots the checkout first, including stale working-copy recovery, so final
file edits remain in the shared store. It then forgets the workspace, removes
its workspace-path record and deletes the checkout directory. Other checkouts
and repository data remain available. `--json` prints the removed repository,
workspace and root identity.

Removal also converges from interrupted states. A marked directory whose
workspace is already forgotten is deleted. If the directory is already gone,
the deterministic workspace identity and stored path allow the command to
forget its remaining registration. An unmarked directory or a marker whose
repository is absent from the catalog is left untouched.

Downloaded packs are decoded and hash-checked again before the machine installs
their canonical objects into the stock simple stores with no-clobber writes.
Pack installation never publishes an operation head; complete-closure
validation and native reconciliation remain the authority boundary.

Machine-local sync progress is a separate, locked sidecar. It records the
accepted cloud cursor and heads, installed catalog frontier, and the exact
pending head transaction needed to replay an ambiguous response. These files
do not replace jj's operation-head store.

The native sync engine uses that sidecar around one transport contract. It
replays pending head work first, installs new cloud packs, asks stock jj to
reconcile, negotiates and uploads only cloud-missing objects, and persists an
ordered transaction batch for every unaccepted local head before sending it.
Each transaction observes only pre-batch cloud heads in its own ancestry, so
publishing one sibling does not remove another. Accepted batches trigger
bounded, manifest-first local pack cleanup. `HttpTransport` implements that
contract over the Worker protocol. The ignored live convergence and projection
probes accept the shared credential, distinct machine IDs and an explicitly
supplied repository authority. They remain manual deployment probes rather than
part of the hermetic gate.

Deleting a fully synchronized machine copy and its sync sidecar is recoverable:
a fresh stock repository downloads the cloud catalog, installs canonical
objects and resolves to the exact previous operation ID and view.

The same sync engine runs in the daemon and detached one-shots. At a later
command boundary it rediscovers native operations even when no outbox hint was
written, and it durably replays any exact head request queued by an interrupted
pass. Killing the daemon cannot remove native operations or its durable sync
sidecar; restart drains the surviving work under the same per-repository lock.

`GitProjection` translates between the native store and a rebuildable bare-Git
sidecar. Exact hidden paths are removed before excluded leaves are read, and
Git links fail before native tree encoding. The Durable Object assigns hidden
policy epochs and journals immutable Git receipts, quarantined projection
states, exact ref cursors and fenced pending batches. A second machine can
recover a remote ref move after the first machine omits finalisation, then
rebuild an empty sidecar from cloud objects and accepted mappings.

The warm local repository-open wrapper measures 1.297 to 1.300 times stock jj in
the release-only probe against a 64-operation fixture repository and cannot
issue cloud requests. The end-to-end read-command probe measured at most
1.0135 times its stock jj-lib equivalent; the `ds` runner preserves that direct
bare-repository shape.

When cloud operation objects have been installed locally, the machine validates
their complete closure before adding them to jj's stock operation-head store.
Reloading through jj removes ancestor heads and creates jj's own merge operation
for genuine divergence; Devspace does not implement a parallel view merge.

See [`docs/kernel.md`](docs/kernel.md) for the validation kernel contract,
[`docs/sync.md`](docs/sync.md) for synchronization and convergence, and
[`docs/git-projection.md`](docs/git-projection.md) for Git projection.
[`docs/hidden.md`](docs/hidden.md), [`docs/git-push.md`](docs/git-push.md)
and [`docs/git-fetch.md`](docs/git-fetch.md) specify the hidden-file model
and the Git transport surfaces for the product build-out; probe reports
backing them are archived under [`docs/archive/`](docs/archive/).
