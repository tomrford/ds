# Git fetch

Fetch moves public Git history into canonical native history: it imports new
remote commits as raw public shadows, then lifts them onto private lineage so
hidden content and private ancestry are preserved. The semantics follow
`hidden.md`; the lifting algorithm and conflict behavior are proven by the
fetch probe against a real remote. This document specifies the product
implementation.

## Seed selection

For each fetched ref, the machine walks backwards from the exact fetched Git
head. A Git commit can seed private history only when the cloud journal holds
both its immutable Git-to-public receipt and an active projection state
binding that Git object and public shadow to one private canonical commit and
hidden-set identity for the relevant remote and bookmark.

Lineage comes only from exact Git objects reached in the imported ancestry. A
cursor or pending state for the same ref is not evidence that rewritten or
unrelated history descends from private state. When the same Git object is
recorded through several bookmarks, the newest state per bookmark must agree
on one private commit and hidden-set identity; otherwise the seed is
ambiguous and the fetch fails closed.

Pending push batches overlapping a fetched ref are recovered before import
(see `git-projection.md`); stale no-op lineage is discarded before unrelated
history is interpreted.

## Import

`import_reachable` translates new Git commits, trees, files and symlinks into
canonical objects, stopping at known raw public shadows supplied from durable
receipts. Import is a translation boundary, not an authority: it publishes no
operation head. Git links fail before the simple backend can encode a tree;
submodules are unsupported. Signatures, which cannot survive translation, are
stripped; raw Git commit data carries a secure-signature field that must be
cleared before canonical encoding. Import enforces explicit bounds on commit
depth, tree width and input head count, and recomputes any supplied mapping
rather than trusting it.

## Lifting

Newly imported public shadows are lifted parent-first. For each public commit
C:

1. each raw public parent is replaced by its already-lifted private commit;
2. the raw public parent trees merge into P, the lifted private parent trees
   merge into Q;
3. the lifted tree is the 3-term merge Q − P + C.tree — the same tree rebase
   jj itself uses;
4. the canonical commit is written with the lifted private parents, the
   merged tree and the imported commit's metadata.

Because hidden paths are absent from every public tree, the merged private
parents supply their values unchanged: hidden content, including `.dsprivate`
itself, flows to lifted commits structurally. A public edit at a hidden path
becomes a native jj conflict exactly as `hidden.md` specifies; a Git merge
lifts by merging all public parents and all private parents separately and
replaying the delta between them, preserving parent count and order.

Lifted results are deterministic: the same fetched commits, seeds and
hidden-set identities produce identical canonical objects on any machine.

## Journal transaction

Fetch needs one new idempotent mutation on the repository Durable Object:

```text
POST /repositories/:repository/git/fetches
```

The request carries a stable fetch ID and machine ID, the remote identity,
the fetched refs with their exact observed Git heads, the expected prior
cursor per ref, the hidden-set identity used for lifting, every new immutable
Git-to-public receipt, parent-first per-ref states binding Git, public and
lifted private IDs, and the proposed final state per ref.

In one transaction the Durable Object validates authentication, incarnation
and the idempotent request hash; exact expected cursors; the absence of an
unresolved overlapping push batch; object closure for every new raw public
and private commit; immutable receipt consistency; and one unambiguous
lineage per reached Git object. It then inserts receipts, appends active
projection states and advances the per-ref cursors atomically. No separate
fetch cursor exists: the projection activation cursor pages journal changes
and the per-ref cursor identifies the last accepted state.

The machine uploads and confirms both the raw public and lifted private
object closures through ordinary sync before calling the mutation, so the
durability gate holds. A lost response is replayed safely under the same
fetch ID.

The first version reads seed state from the existing paged projection
snapshot; a bounded exact-lookup read (by Git ID list) is a later
optimization, not a correctness surface.

## Exporter interaction

Post-fetch history legitimately carries hidden-path conflicts, but export
never needs to re-encode those commits: the fetch transaction records the
binding between each lifted commit and its already-existing public Git
counterpart, so export of later work stops at the mapped lifted parent.
Export of a commit that itself carries a conflict — hidden or public — fails
closed; hidden-involved conflicts are labeled distinctly (see `hidden.md`).
The exporter change the fetch path requires is therefore mapping-aware
traversal plus labeled rejection, not conflict-term filtering.

## Open items

- The journal route, its Worker schema and the fault-matrix coverage: lost
  responses at fetch-mutation, cursor races, policy-bearing (`.dsprivate`)
  commits, rewritten refs, ambiguous multi-bookmark seeds, octopus merges and
  hidden parent disagreement.
- Adversarial depth testing for recursive tree translation.
- Non-UTF-8 names, case collisions and paths that cannot materialize on a
  client platform have no defined handling.
- Annotated tags, replace refs, shallow history, partial clones and Git notes
  are out of scope for the native surface.
