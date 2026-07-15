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
record. Deleting and recreating a name assigns a new ID and incarnation.

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
without changing typed principals, repository authorization or transports.

The native machine crate initializes and reloads stock jj repositories. It
rejects repositories whose backend, operation store, operation-head store,
index or submodule-store type differs from the jj 0.42 defaults. Git projection
and sync state live beside this native repository, not inside replacement jj
stores. The same crate discovers deterministic raw-object closures from all
local operation heads, stops at the cloud-accepted operation frontier, and
encodes cloud-missing objects into deterministic, size-bounded, hash-verified
packs.

One `MachineStore` owns the platform data directory, a durable local repository
catalog, creation journals and native repository locations. The
catalog binds a tenant-visible name to an opaque repository ID and incarnation;
native paths contain those 2 opaque values and never the name. Catalog and
creation-journal changes use machine-local file locks and synced atomic
replacement. Registration, materialization and catalog-only removal are
library seams; removal does not prune native repository data. On macOS the
default root is `~/Library/Application Support/devspace`; Linux uses the XDG
data directory and Windows uses the local application-data directory.
`DEVSPACE_MACHINE_STORE_DIR` is a bring-up and test-only root override.

The `ds` binary embeds jj-cli 0.42 as its parser and command engine. `ds repo
new <name>` durably records a random 128-bit idempotency key before claiming the
cloud name. A retry replays only that exact request, persists the returned
opaque identity, registers it in the catalog and atomically publishes an empty
stock-format bare repository. The completed receipt remains local so a retry
after the final write is distinct from adopting an unrelated catalog entry.
Normal jj workspaces use jj's stock loader and command behavior. `ds -R <name>
log` resolves the name through the local catalog and opens its stock bare
repository directly, with no temporary workspace or working-copy snapshot.
There is no selected checkout, so `@` is unavailable. Other commands against a
bare machine repository are rejected; this read path constructs no Devspace
sync transport or HTTP client. Bare roots with jj repository or workspace
config markers are rejected, and raw native paths are not repository
identities. A missing local name fails locally because cloud first-use belongs
to the repository lifecycle commands rather than the warm read path. Existing
workspace paths, including explicit `-R` paths, retain stock jj behavior.

`ds add <name> -r <revision> <path>` creates a new working checkout for an
existing local machine repository. Both the revision and destination are
explicit. Named revisions resolve against the locally accepted repository;
plain `@`, `@-` and `@+` resolve only when the command runs inside another
checkout of the same repository. Each checkout has a machine-scoped workspace
identity and stock `.jj/repo` pointer to the shared native repository. Its
working-copy state stays in the checkout. Before creating workspace state,
`ds add` journals the original revision expression, its resolved base, the exact
planned working-copy commit, a stable workspace ID, staging name and ownership
token. It stages the whole checkout beside the absent destination and publishes
the directory in one no-replace filesystem operation. A retry resumes the same
registration, staging or publication step while the journaled parent remains
stable. If an ancestor is renamed after the publication inode check, the
anchored rename can land under that moved parent. The requested path then fails
ownership, any foreign replacement remains untouched and the intent remains
staged. Devspace cannot discover the checkout at an arbitrary renamed ancestor
location. If another process creates or replaces the destination, `ds add`
fails without adopting, replacing or deleting it. Removing a checkout path
cannot follow repository data into the machine store.

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

The same sync engine also runs safely without a daemon. At a later command
boundary it rediscovers native operations even when no outbox hint was written,
and it durably replays any exact head request queued by an interrupted boundary.

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
