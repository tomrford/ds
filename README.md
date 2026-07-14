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
and transactions use `/repositories/:repository/heads`.

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

When cloud operation objects have been installed locally, the machine validates
their complete closure before adding them to jj's stock operation-head store.
Reloading through jj removes ancestor heads and creates jj's own merge operation
for genuine divergence; Devspace does not implement a parallel view merge.

See [`docs/spike-1.md`](docs/spike-1.md) for the kernel contract and its
verification surface and [`docs/spike-2.md`](docs/spike-2.md) for the
convergence proof.
