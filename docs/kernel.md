# Validation kernel

Object validation runs inside the TypeScript Durable Object through a narrow
Rust kernel compiled to Wasm. The kernel has these constraints:

- Narrow dependency graph: the kernel depends on `prost` and `blake2` only —
  no `jj-lib`. It is a maintained mini-fork of jj's simple backend and
  op-store storage formats (see the crate docs in `crates/kernel`); every jj
  format change must be mirrored there.
- No reachable panic path: protobuf conversion returns `Result` throughout,
  with no panic-catching. The mutation suite validates every truncation and
  single-byte mutation of each structured golden vector.
- Small Wasm binary: the optimized module is ~140 KiB with zero imports; the
  build fails above 200 KiB.
- jj ID parity: every golden vector produces the canonical jj-lib 0.42.0 ID
  and the same accept/reject outcome in the native kernel and the Wasm kernel
  inside the Durable Object.

The validation kernel is a no-I/O Rust crate with two dependencies: `prost` for
the jj-compatible protobuf envelope and `blake2` for object IDs. It does not
depend on `jj-lib`.

The kernel validates canonical bytes, calculates the content ID, and returns the
object references needed for closure checks. It covers files, symlinks, trees,
commits, views and operations. Hidden-path parsing lives in the same no-I/O
crate; tree traversal and Git projection do not.

The kernel rejects non-canonical bytes. Both stores hold only canonical bytes;
normalization belongs on the machine because replication is byte-exact, so the
cloud must never rewrite what a client uploaded.

`kernel-wasm` exposes a small allocation and validation ABI plus an incremental
raw Blake2b-512 state used for pack and chunk verification. The release profile
uses `panic = "abort"`. Checked conversion replaces panic-catching at the
protobuf boundary, so malformed object bytes return an error. The optimized
module has no imports, and the build rejects modules larger than 200 KiB.

One SQLite-backed `Repository` Durable Object owns each opaque repository ID. It
quarantines bounded pack manifests and chunks, then runs the Wasm validator
before inserting immutable object bytes and their references in one synchronous
install transaction. The Worker authenticates a typed machine principal and
resolves the current repository incarnation through the control-plane Durable Object before each
typed RPC. The repository object independently rechecks the user, repository ID
and incarnation before reading or mutating state.

## Verification

`crates/kernel/tests/jj_golden.txt` contains 32 frozen objects and IDs. Most
come from walking the stored history of a real repository (mint, about 90
commits). The remaining vectors cover jj simple-store edge cases that import
does not produce: signed commits, conflicted root trees with labels, merge
commits with predecessors, executable files, symlinks and nested trees. Every
vector uses the unextended jj-lib 0.42.0 simple backend or simple
operation-store schema.

The Rust suite and Workers Vitest suite validate all six object kinds against
the same vectors. The malformed-input suite exercises every truncation and
single-byte mutation of each structured vector without panic-catching.

Workers Vitest also covers canonical rejection, reference extraction,
idempotent insertion, bounded requests, authentication, quarantine and install
retries, and SQLite persistence across Durable Object eviction.

The Worker uses pack manifests and chunks. The kernel and per-object reference
index remain the validation boundary beneath that protocol. Head transactions,
Git projection and machine ownership sit outside the kernel.
