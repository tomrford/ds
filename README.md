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

The Worker accepts authenticated manifest and chunk uploads under
`/repositories/:repository/packs/:pack`, followed by an explicit install
request. Uploads are quarantined until the Durable Object has checked the
manifest, chunk and whole-pack hashes and revalidated every object through the
Rust/Wasm kernel in one atomic install transaction. Authenticated head reads
and transactions use `/repositories/:repository/heads`, and an authenticated
POST on `/repositories/:repository/initialize` assigns the incarnation. An
incarnation-scoped, paginated GET on `/repositories/:repository/packs` lists
installed logical packs; their exact manifests and chunks are downloadable
from the same pack routes. A bounded POST on
`/repositories/:repository/objects/inventory` returns the installed subset of
the machine's sorted candidate object keys for that incarnation.

## Current boundary

The no-I/O Rust kernel owns canonical protobuf validation, jj-compatible object
IDs, referenced-object extraction, and hidden-path parsing. The Worker owns
request limits and routing. One SQLite-backed Durable Object per repository
owns object persistence, the repository incarnation, authoritative operation
heads, monotonic cursors and idempotency receipts. A head transaction checks
that its complete object closure is installed, adds the new head and removes
only the exact heads observed by that client.

The native machine crate initializes and reloads stock jj repositories. It
rejects repositories whose backend, operation store, operation-head store,
index or submodule-store type differs from the jj 0.42 defaults. Git projection
and sync state live beside this native repository, not inside replacement jj
stores. The same crate discovers deterministic raw-object closures from all
local operation heads, stops at the cloud-accepted operation frontier, and
encodes cloud-missing objects into deterministic, size-bounded, hash-verified
packs.

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
contract over the Worker protocol; the ignored `cloud_live` test converges 2
machines through a real Worker via `DEVSPACE_URL` and `DEVSPACE_TOKEN`.

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
issue cloud requests. End-to-end command latency remains to be measured once
the command runner exists.

When cloud operation objects have been installed locally, the machine validates
their complete closure before adding them to jj's stock operation-head store.
Reloading through jj removes ancestor heads and creates jj's own merge operation
for genuine divergence; Devspace does not implement a parallel view merge.

See [`docs/kernel.md`](docs/kernel.md) for the validation kernel contract,
[`docs/sync.md`](docs/sync.md) for synchronization and convergence, and
[`docs/git-projection.md`](docs/git-projection.md) for Git projection.
