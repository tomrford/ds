# GitBackend swap decision record

Status: completed; the validated GitBackend stack is now the primary implementation.

The canonical store moved from jj's SimpleBackend to jj's GitBackend, with the
colocated Git object database as the object store. The cutover used a Worker
data wipe and redeploy. No repository data migration code was added.
`wrangler.jsonc` still contains Durable Object class migrations because those
are Cloudflare platform lifecycle declarations, not repository data
migrations.

Three spikes mirrored the repository's original v3 spike phase. Each was
validated in isolation before the integrated cutover.

The verified jj-lib 0.42 foundation was that GitBackend is self-contained in
Git object bytes. Change IDs ride in the `change-id` commit header or derive
deterministically from the commit OID by bit reversal. Conflicted root trees
ride in the `jj:trees` header plus conflict-label headers. The `extra/`
TableStore is a rebuildable cache. Content identity is the SHA-1 of the object
bytes, so kernel validation replaced the proto re-encode check with Git format
parsing.

## Spike 1 — validation kernel on Git formats

The spike began as `crates/kernel-git`; it became `crates/kernel` at cutover.
It proved that the cloud could validate Git objects with no reachable panics,
no jj-lib or gix dependency, and a zero-import WebAssembly module.

- Hand-written no-panic parsers covered Git commit, tree, and blob object bytes:
  header/continuation-line handling (`gpgsig`, `mergetag`, `encoding`,
  `jj:trees`, `change-id`, conflict labels, unknown headers preserved
  opaquely), binary tree entries (mode, name, OID), blob passthrough.
- Reference extraction covered commit → tree + parents and tree → entry OIDs.
- Identity used SHA-1 with collision detection in a pure Rust, `no_std`-capable,
  WebAssembly-safe dependency. The object ID had to equal the hash of the exact
  bytes; validation was parseability plus reference
  extraction + bounds, not re-encoding.
- Golden vectors were regenerated from a real Git
  repository plus jj-lib 0.42 GitBackend as oracle — signed commits, merge
  commits, mergetag headers, non-UTF-8 metadata, conflicted jj commits with
  `jj:trees`, executable files, symlinks, nested trees.
- The mutation suite made every truncation and single-byte mutation of every
  structured vector return without panicking.
- The WebAssembly proof had zero imports, native/WebAssembly ID parity on all
  vectors, and a measured result under the 200 KiB budget.

Standalone tags remained outside the push surface; `mergetag` rides inside
commit bytes. The operation store remained protobuf plus Blake2b.

## Spike 2 — machine store on the Git object database

This spike proved closure discovery, deterministic packs, and cloud sync over
20-byte IDs with the Git object database as the object source. It reran pack
round-trip, exact fresh-machine cloud rebuild, and command-boundary recovery
proofs. It also proved `store/extra` reconstruction from object bytes,
including deterministic synthetic change IDs for imported foreign commits.

## Spike 3 — projection under the colocated shape

This spike proved hidden-path filtering as a Git-to-Git rewrite with an
OID-to-OID journal. Hidden-free ancestry cones used the identity fast path and
created no mapping row. Rewritten public commits became cloud-durable Git
objects in the colocated database. The journal, lease, push, fetch, and recovery
paths moved to 20-byte canonical IDs, including fresh-machine recovery from
cloud packs alone.

## Outcome

All three spike proof suites passed before the CLI and Worker integration
cutover. The final implementation removed the SimpleBackend data model,
retained GitBackend-compatible canonical bytes, and simplified projection and
recovery around the colocated Git object database.

## Open item

Decide whether public commits that projection must rewrite should be signed.
Identity commits already preserve the canonical Git commit and its existing
signature bytes.
