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

The Worker accepts `PUT /repositories/:repository/objects/:kind`, where `kind`
is `file`, `symlink`, `tree`, `commit`, `view`, or `operation`. The body is
validated by the Rust/Wasm kernel before the Durable Object stores it.

## Current boundary

The no-I/O Rust kernel owns canonical protobuf validation, jj-compatible object
IDs, referenced-object extraction, and hidden-path parsing. The Worker owns
request limits and routing. One SQLite-backed Durable Object per repository
owns object persistence and coordination.

The native machine crate initializes and reloads stock jj repositories. It
rejects repositories whose backend, operation store, operation-head store,
index or submodule-store type differs from the jj 0.42 defaults. Git projection
and sync state will live beside this native repository, not inside replacement
jj stores. The same crate discovers deterministic raw-object closures from all
local operation heads and stops at the cloud-accepted operation frontier.

See [`docs/spike-1.md`](docs/spike-1.md) for the kernel contract and its
verification surface and [`docs/spike-2.md`](docs/spike-2.md) for the
convergence proof.
