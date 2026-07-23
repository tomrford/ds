# Git projection

Devspace keeps one canonical Git history and exposes an explicit public Git
boundary. Projection removes paths selected by `.dsprivate` while preserving
all other Git semantics that can remain byte-identical.

Canonical and public objects live in the same bare Git object database and use
ordinary 20-byte Git OIDs. Public objects are cloud-durable Git objects, not
temporary export artifacts.

## Machine projection

Projection walks canonical commits parent-first. An existing pair or identity
cursor is a stop point: its public commit already defines that canonical
lineage. A reached identity cursor remains only a traversal seed; the machine
does not serialize it as a pair state.

For each unseeded commit the machine:

1. resolves the inherited `.dsprivate` files from its canonical merged tree;
2. removes `.dsprivate` and every matched path from the Git tree;
3. substitutes the public OID for any rewritten parent;
4. reuses every unchanged tree object;
5. writes a new commit only if its tree or parent list changed.

If neither the tree nor any parent changes, the commit takes the identity fast
path. Its public OID equals its canonical OID. No new object and no mapping row
are produced. Signed commits on an entirely identity lineage retain their exact
`gpgsig` and `mergetag` bytes.

A rewritten commit retains the canonical author, committer, message, encoding,
`change-id`, and foreign non-signature headers. Projection drops `gpgsig`,
`gpgsig-sha256`, and `mergetag` because changed tree or parent bytes invalidate
them. It embeds no canonical-OID metadata because that would disclose the
existence of private history.

Projection writes each rewritten commit directly to the object database and
records its canonical/public pair. A seeded public commit must have a complete
local object closure. If it is missing, push installs the cloud catalog and
retries projection once from those exact seed bytes.

Tree and commit rewriting is deterministic. The same canonical commit and
effective hidden policy produce byte-identical public objects on every machine.
The cache key includes the source tree and effective hidden-policy chain, so
identical subtrees under the same policy reuse one public tree OID.

This determinism pins public OIDs across machines. Any change to the rewrite
algorithm therefore needs an explicit compatibility and rollout design.

Projection rejects conflicted canonical commits, malformed objects, missing
seed closures, and a proposed public tree that still contains a hidden path.

## Overlay lift

Fetch runs the inverse operation over foreign public history. Existing
canonical/public pairs seed the traversal. For every new public commit,
overlay-lift:

1. maps its public parents to canonical parents;
2. merges the public parents and canonical parents separately;
3. resolves the hidden policy and hidden content from the canonical base;
4. applies the public tree change over that canonical base;
5. writes a canonical mirror only when the parents or hidden overlay require
   one.

A hidden-free commit whose parents remain identical takes the identity fast
path and produces no mapping. Otherwise the resulting pair is
`canonicalOid`/`publicOid`; the public object remains unchanged.

If foreign history publishes a path that the inherited policy marks hidden,
overlay-lift emits a `WARNING: DATA DISCLOSURE` diagnostic. It preserves the
public content in a Jujutsu tree conflict against a deterministic tombstone so
the canonical result is explicit and resolvable. The warning means the foreign
bytes are already public on the remote; fetch cannot retract them.

## Cloud journal

The journal stores pair-shaped projection state:

```text
canonicalOid  publicOid  hiddenSetId?
```

An active remote bookmark cursor selects one binding. Identity history stores
the one shared OID in an identity cursor. Pending identity pushes expose that
OID as `identityOid`. Rewritten history stores the pair and the nullable
64-byte identity of the effective hidden set.

Before mutation, the Worker checks request capacity, object durability,
request-wide canonical bindings, hidden-set lineage, and expected cursors. It
rejects identity-shaped pair states. The same canonical OID cannot be presented
with conflicting public OIDs or hidden-set lineages in one request or against
stored state.

Repository history is bounded to 65,536 pair-state rows. Snapshot responses
page activated mappings in groups of 256 under one fixed activation high-water.
The first page also carries at most 512 cursors and pending batches. The native
transport applies a 1 MiB JSON-response cap, validates cursor order and bounds,
and permits at most 256 pages.

The activation high-water stabilizes append-only state rows. It does not make a
multi-page snapshot consistent with concurrent remote repointing, which deletes
that remote's old rows. Remote generations in issue #15 remain required for
that guarantee.

Push updates use durable batches. Each batch contains:

- remote and bookmark;
- expected old public OID;
- proposed pair state, identity OID, or deletion;
- owner machine, fencing token, request hash, and idempotency key.

The batch begins before the Git subprocess runs. Recovery claims the fence,
replays the exact lease updates, observes the live remote refs, and either
activates every cursor atomically or records an aborted result. A partially
accepted multi-ref push never becomes a partially committed journal update.

Fetch records observed public refs, any new pair states, and either a proposed
state index or `identityOid`. The Worker verifies that all referenced commits
are already durable before it advances cursors.

## Verification

The projection suite proves:

- signed identity history remains byte-exact;
- hidden files and `.dsprivate` never enter public trees;
- only affected trees and commits are rewritten;
- rewritten commits are deterministic and unsigned;
- invalidated signature headers and mergetags are removed while opaque header
  order is preserved;
- merges preserve public parent order and canonical hidden lineage;
- overlay-lift preserves hidden files through public edits and deletions;
- disclosure collisions become explicit conflicts and warnings;
- push and fetch recover after process failure without journal drift;
- fresh-machine recovery succeeds using cloud packs and journal state;
- identity cursors stop traversal without creating identity-shaped pair rows;
- a settled aborted claim refreshes state without an unnecessary replay.

The Worker checks journal mutations transactionally and rejects stale
incarnations, stale leases, missing durable commits, identity-shaped pair
states, conflicting bindings, ambiguous lineage, and request-key reuse. The
native client validates snapshot page structure but relies on deterministic
projection instead of negotiating between different machine-minted public
objects.

## Budgets and measurements

The integrated validation module and Worker dry-run currently measure:

- `dist/kernel.wasm`: 193,056 bytes, zero imports;
- Worker upload: 904.19 KiB;
- Worker upload gzip: 183.19 KiB.

The WebAssembly build enforces a 200 KiB limit. `pnpm build` performs a Worker
dry run only; deployment is a separate operator action.
