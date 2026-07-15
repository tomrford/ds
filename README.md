# Devspace v3

This repository is a clean implementation of the v3 architecture. The
validation kernel stays independent of `jj-lib`, compiles to small WebAssembly
and runs inside a Cloudflare Durable Object. Phase 2 adds a local machine store
using jj's stock simple backend, operation store and operation-head store.

The v2 checkout is an oracle for wire-format compatibility and fixtures. It is
not a source dependency.

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
and transactions use `/repositories/:repository/heads`. An
incarnation-scoped, paginated GET on `/repositories/:repository/packs` lists
installed logical packs; their exact manifests and chunks are downloadable
from the same pack routes.

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
and sync state will live beside this native repository, not inside replacement
jj stores. The same crate discovers deterministic raw-object closures from all
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
reconcile, uploads the newly discovered local closure, and persists the exact
head request before sending it.

Deleting a fully synchronized machine copy and its sync sidecar is recoverable:
a fresh stock repository downloads the cloud catalog, installs canonical
objects and resolves to the exact previous operation ID and view.

The same sync engine also runs safely without a daemon. At a later command
boundary it rediscovers native operations even when no outbox hint was written,
and it durably replays any exact head request queued by an interrupted boundary.

The warm local repository-open wrapper measures 1.49 to 1.50 times stock jj in
the release-only probe and cannot issue cloud requests. End-to-end command
latency remains to be measured once the v3 command runner exists.

When cloud operation objects have been installed locally, the machine validates
their complete closure before adding them to jj's stock operation-head store.
Reloading through jj removes ancestor heads and creates jj's own merge operation
for genuine divergence; Devspace does not implement a parallel view merge.

See [`docs/spike-1.md`](docs/spike-1.md) for the kernel contract and its
verification surface and [`docs/spike-2.md`](docs/spike-2.md) for the
convergence proof.
