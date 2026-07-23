# Validation kernel

Devspace has one validation kernel for every byte stored by the repository
Durable Object. The native Rust library and zero-import WebAssembly build share
the same parsers, identity functions, bounds, and reference extraction.

The kernel does not depend on `jj-lib`, `gix`, or a system Git library. It
implements only the canonical formats that the cloud must validate.

## Git objects

Canonical repository objects use Git's object formats:

- blobs are opaque bytes;
- trees are ordered binary entries containing a mode, raw name, and 20-byte
  object ID;
- commits contain a tree, zero or more parents, identity headers, a message,
  and preserved continuation-line headers such as `gpgsig` and `mergetag`.

Commit validation recognizes GitBackend metadata including `change-id`,
`jj:trees`, and conflict-label headers. Unknown well-formed headers remain
opaque. The validator rejects malformed modes, duplicate or unsorted tree
entries, invalid object IDs, broken headers, oversized inputs, and references
whose type is not permitted at that position.

An object's identity is the SHA-1 of Git's exact
`"<type> <length>\\0<payload>"` preimage. The SHA-1 implementation performs
collision detection. Validation never normalizes or re-encodes an accepted Git
object; byte identity is the contract.

Standalone tag objects are outside the repository transport. A signed tag
embedded in a commit's `mergetag` header remains part of the commit bytes.

## Operation store

Jujutsu views and operations use the simple operation-store protobuf schema.
Their identities are 64-byte Blake2b hashes of the canonical semantic content.
The kernel decodes the protobuf, validates every field and referenced object,
reconstructs the semantic value, and rejects non-canonical encodings.

The accepted schema is exactly jj's operation and view format. Devspace adds no
fields to these objects.

Git objects and operation objects have separate namespaces, routes, and
closure rules. A commit references a tree and parents; a tree references its
entries; an operation references its parent operations and view; a view
references Git commits through jj's commit IDs.

## WebAssembly boundary

`crates/kernel-wasm` exports validation for Git and operation objects from one
module. The Worker calls it before an object can enter durable storage.
Malformed input returns a typed error and cannot trap the Worker.

The current release build is:

- `dist/kernel.wasm`: 193,056 bytes;
- imports: zero;
- Worker dry-run bundle: 904.19 KiB raw, 183.19 KiB gzip.

`scripts/build-wasm.mjs` builds exactly this one module and enforces the
200 KiB WebAssembly budget.

## Verification

Golden vectors come from real Git repositories and jj-lib 0.42.0 stores. They
cover signed and merged commits, non-UTF-8 metadata, GitBackend conflicted
trees, executable files, symlinks, nested trees, operation merges, and
repository views.

Native and WebAssembly validators must return the same identity and references
for every vector. Structured vectors are also checked under every truncation
and single-byte mutation. Pack installation repeats identity validation and
rejects missing references or no-clobber violations.

Run the complete proof:

```sh
nix develop -c pnpm check
nix develop -c pnpm test
```
