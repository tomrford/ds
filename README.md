# Devspace v3

This repository is a clean implementation of the v3 architecture. The first
spike proves that the jj-compatible validation kernel can stay independent of
`jj-lib`, compile to small WebAssembly, and run inside a Cloudflare Durable
Object.

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

## Spike boundary

The no-I/O Rust kernel owns canonical protobuf validation, jj-compatible object
IDs, referenced-object extraction, and hidden-path parsing. The Worker owns
request limits and routing. One SQLite-backed Durable Object per repository
owns object persistence and coordination.

See [`docs/spike-1.md`](docs/spike-1.md) for the kernel contract and its
verification surface.
