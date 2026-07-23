# GitBackend swap — spike plan

The canonical store moves from jj's SimpleBackend to jj's GitBackend
(colocated git odb as the object store). Clean cut: the worker is wiped and
redeployed; no migration code, ever. Three spikes mirror the repository's
original v3 spike phase: each is validated in isolation with its own proof
tests before any integrated intermediate state exists.

Verified foundation (jj-lib 0.42): GitBackend is self-contained in git object
bytes — change IDs ride in the `change-id` commit header or derive
deterministically from the commit OID (bit reversal); conflicted root trees
ride in the `jj:trees` header plus conflict-label headers; the `extra/`
TableStore is a rebuildable cache. Content identity is the SHA-1 of the
object bytes, so kernel validation loses the proto re-encode check and gains
git-format parsing.

## Spike 1 — validation kernel on git formats

New crate `crates/kernel-git` (the existing kernel stays untouched until
cutover). Proves the cloud can validate git objects under the existing
doctrine: no reachable panics, no jj-lib/gix dependency, zero-import wasm.

- Hand-written no-panic parsers for git commit, tree, and blob object bytes:
  header/continuation-line handling (`gpgsig`, `mergetag`, `encoding`,
  `jj:trees`, `change-id`, conflict labels, unknown headers preserved
  opaquely), binary tree entries (mode, name, OID), blob passthrough.
- Reference extraction: commit → tree + parents; tree → entry OIDs.
- Identity: SHA-1 with collision detection (dependency chosen by health
  check; pure Rust, no_std-capable, wasm-safe). Object ID must equal the
  hash of the exact bytes; validation is parseability + reference
  extraction + bounds, not re-encoding.
- Golden vectors regenerated the original way: walked out of a real git
  repository plus jj-lib 0.42 GitBackend as oracle — signed commits, merge
  commits, mergetag headers, non-UTF-8 metadata, conflicted jj commits with
  `jj:trees`, executable files, symlinks, nested trees.
- Mutation suite: every truncation and single-byte mutation of every
  structured vector errors without panicking.
- Wasm: zero imports, native/wasm ID parity on all vectors, measured against
  the 200 KiB budget (SHA-1DC code size is a known risk — measure, do not
  assume).

Standalone tags are out of scope (tags are outside the push surface;
`mergetag` rides inside commit bytes). The op store stays proto + Blake2 and
is untouched.

## Spike 2 — machine store on the git odb

Proves closure discovery, deterministic packs, and cloud sync over 20-byte
IDs with the git odb as the object source. Re-runs the original proofs:
pack round trip, exact cloud rebuild from a fresh machine, command-boundary
recovery. Extras-table reconstruction from object bytes is proven here
(imported foreign commits get deterministic synthetic change IDs).

## Spike 3 — projection under the colocated shape

Proves hidden-path filtering as a git-to-git rewrite with an OID→OID mapping
table: identity fast path for hidden-free ancestry cones (mapping row only
where filtering rewrote), filtered public commits stored as first-class
canonical objects (cloud-durable — the byte-custody problem dissolves),
journal/lease/recovery machinery re-plumbed to 20-byte canonical IDs.
Push/fetch/recovery proofs re-run, including fresh-machine recovery from
cloud packs alone.

## Gates

Each spike lands only when its proof tests are green in isolation. Anything
that meaningfully diverges from this plan stops for discussion before
implementation continues. After all three: integration (CLI cutover, worker
wipe/redeploy), then a simplification pass over semi-legacy machinery whose
assumptions the swap invalidates (sign-on-export custody, rewrite handling,
shim fabrication), then dogfooding resumes.
