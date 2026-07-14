# Validation kernel

Spike 1 of the Cloudflare-native v3 plan: prove that object validation can run
inside a TypeScript Durable Object through a narrow Rust kernel compiled to
Wasm. Every bar the plan set for this spike holds:

- Narrow dependency graph: the kernel depends on `prost` and `blake2` only —
  no `jj-lib`. It is a maintained mini-fork of jj's simple backend and
  op-store storage formats (see the crate docs in `crates/kernel`); every jj
  format change must be mirrored there.
- No reachable panic path: protobuf conversion returns `Result` throughout,
  with no panic-catching. The mutation suite validates every truncation and
  single-byte mutation of each structured golden vector.
- Small Wasm binary: the optimized module is ~140 KiB with zero imports; the
  build fails above 200 KiB.
- v2 ID parity: every golden vector produces the same ID and the same
  accept/reject outcome in the frozen v2 codec, the native kernel, and the
  Wasm kernel inside the Durable Object.

The validation kernel is a no-I/O Rust crate with two dependencies: `prost` for
the jj-compatible protobuf envelope and `blake2` for object IDs. It does not
depend on `jj-lib`.

The kernel validates canonical bytes, calculates the content ID, and returns the
object references needed for closure checks. It covers files, symlinks, trees,
commits, views and operations. Hidden-path parsing lives in the same no-I/O
crate; tree traversal and Git projection do not.

Unlike the v2 server, which normalizes legacy encodings on ingest, the kernel
rejects any non-canonical bytes. Both stores hold only canonical bytes; v3
moves normalization to the machine because replication is byte-exact, so the
cloud must never rewrite what a client uploaded.

`kernel-wasm` exposes a small allocation and validation ABI plus an incremental
raw Blake2b-512 state used for pack and chunk verification. The release profile
uses `panic = "abort"`. Checked conversion replaces panic-catching at the
protobuf boundary, so malformed object bytes return an error. The optimized
module has no imports, and the build rejects modules larger than 200 KiB.

One SQLite-backed `Repository` Durable Object owns each repository name. It
quarantines bounded pack manifests and chunks, then runs the Wasm validator
before inserting immutable object bytes and their references in one synchronous
install transaction. The Worker applies authentication, repository name
validation and endpoint-specific request bounds before each RPC call.

## Verification

`crates/kernel/tests/v2_golden.txt` contains 32 frozen objects and IDs. Most
come from importing a real repository (mint, ~90 commits) through v2 and
walking the stored history. The remaining vectors cover jj simple-store edge
cases that import does not produce: signed commits, conflicted root trees with
labels, merge commits with predecessors, executable files, symlinks and nested
trees. Every vector uses the unextended jj-lib 0.42.0 simple backend or simple
operation-store schema.

The Rust suite and Workers Vitest suite validate all six object kinds against
the same vectors. The malformed-input suite exercises every truncation and
single-byte mutation of each structured vector without panic-catching.

Workers Vitest also covers canonical rejection, reference extraction,
idempotent insertion, bounded requests, authentication, quarantine and install
retries, and SQLite persistence across Durable Object eviction.

The original per-object HTTP spike surface has been removed. Phase 2 uses pack
manifests and chunks; the kernel and per-object reference index remain the
validation boundary beneath that protocol. Head transactions, Git projection
and machine ownership remain separate work.
