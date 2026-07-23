# Git projection

Devspace keeps one canonical Git history and exposes an explicit public Git
boundary. Projection removes paths selected by `.dsprivate` while preserving
all other Git semantics that can remain byte-identical.

Canonical and public objects live in the same bare Git object database and use
ordinary 20-byte Git OIDs. Public objects are cloud-durable Git objects, not
temporary export artifacts.

## Machine projection

Projection walks canonical commits parent-first. A journal mapping is a stop
point: its public commit already defines the projection of that canonical
lineage.

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
and opaque headers that remain valid. Projection removes GitBackend-private
headers and records the canonical OID in the rewritten commit metadata. It
rejects conflicted canonical commits, malformed objects, and a proposed public
tree that still contains a hidden path.

Tree rewriting is minimal and deterministic. The cache key includes the source
tree and effective hidden-policy chain, so identical subtrees under the same
policy reuse one public tree OID.

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

An active remote bookmark cursor selects one pair. For identity history the
cursor can store the one shared OID without adding a receipt row. Rewritten
history stores the pair and the nullable 64-byte identity of the effective
hidden set.

The receipt invariant is one-way: one canonical OID cannot map to two public
OIDs. The Worker rejects a conflicting pair. A public OID can be reached from
more than one canonical lineage only when fetch can resolve that lineage
without ambiguity.

Push updates use durable batches. Each batch contains:

- remote and bookmark;
- expected old public OID;
- proposed pair state or deletion;
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

- hidden-free signed history creates no mirror objects or mapping rows;
- hidden files and `.dsprivate` never enter public trees;
- only affected trees and commits are rewritten;
- merges preserve public parent order and canonical hidden lineage;
- repeated projection is deterministic;
- overlay-lift preserves hidden files through public edits and deletions;
- disclosure collisions become explicit conflicts and warnings;
- push and fetch recover after process failure without journal drift;
- fresh-machine recovery succeeds using cloud packs and journal state.

The Worker checks all journal mutations transactionally and rejects stale
incarnations, stale leases, missing durable commits, ambiguous mappings, and
request-key reuse.

## Budgets and measurements

The integrated validation module and Worker dry-run currently measure:

- `dist/kernel.wasm`: 192,676 bytes, zero imports;
- Worker upload: 897.01 KiB;
- Worker upload gzip: 181.92 KiB.

The WebAssembly build enforces a 200 KiB limit. `pnpm build` performs a Worker
dry run only; deployment is a separate operator action.
