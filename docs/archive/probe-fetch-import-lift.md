> Archived probe report (2026-07-15). Conclusions are folded into the product
> specs and subsystem docs; this file is evidence, not current guidance.

# Fetch import and hidden-lineage lifting findings

## Result

Fetch-side lifting is implementable with jj's existing tree merge model. The
probe imports real Git commits, attaches them to private native ancestry and
preserves both public and hidden changes.

The proof covers:

- a private canonical head containing `secrets/.env`
- export and push to a temporary bare Git remote
- a plain Git collaborator changing public files on 2 branches and merging them
- a plain Git collaborator adding, editing and deleting `secrets/.env`
- a real Git fetch into the projection sidecar
- import of new Git commits as raw public jj shadows
- lifting every imported commit onto the private lineage
- a native hidden-path conflict on the public add and edit
- cleanup of that conflict on the later public deletion
- sanitized re-export with no reachable hidden path or hidden bytes
- immutable Git-to-public receipt checks

The test is `crates/machine/tests/fetch_import_lift.rs`. It is not ignored and
needs no network or Worker.

## Lifting algorithm

### Select the private seed from exact ancestry

For each fetched ref, walk backwards from the exact fetched Git head. A Git ID
can seed private history only when the cloud journal has both:

- its immutable receipt from Git ID to raw public jj shadow
- an active projection state for the relevant remote and bookmark, binding the
  same Git ID and public shadow to one private canonical commit and policy epoch

Do not use a cursor merely because it belongs to the same ref. The exact Git ID
must occur in the fetched ancestry. If the same Git ID is reachable through
several bookmarks, use the specification's rule: the newest state for each
bookmark must agree on one private commit and policy epoch. Otherwise fail.

`ImportMappings` receives the durable Git-to-public receipts. This lets
`import_reachable` stop at known raw public shadows and import only new public
objects.

### Replay each public delta onto private parents

Process newly imported public shadows in parent-first order. For each public
commit `C`:

1. Replace each raw public parent with its already lifted private commit.
2. Recursively merge the raw public parent trees to make `P`.
3. Recursively merge the lifted private parent trees to make `Q`.
4. Use jj's native merged-tree operation to calculate `Q - P + C.tree`.
5. Write a canonical commit with the lifted private parents, the merged tree
   and the imported commit metadata.

In code, step 4 is the same 3-term tree rebase used by jj itself:

```text
adds:    merged private parents Q, public child C.tree
removes: merged public parents P
```

The probe uses `merge_commit_trees()` for both parent sets and
`MergedTree::merge()` for the 3-term replay. This is preferable to copying
hidden values from one parent because it preserves native conflicts and works
for all paths, not only the hidden set.

### Hidden content behavior

The proof matches the `hidden-files.md` rules for a seeded hidden path:

- if the public child leaves the hidden path absent, the private value stays
  resolved and unchanged
- if the public child adds the hidden path, jj produces an add/add file
  conflict containing the private value and the public value
- if the next public commit edits that path, the private side remains and the
  public conflict term changes to the new public value
- if a later public commit deletes the path, the public side disappears and
  the private value becomes clean again

The probe does not implement the unseeded-path tombstone. The specification
requires a modify-delete conflict against one of 2 fixed synthetic objects.
Production needs canonical definitions for those objects and tests proving
that their selection cannot collide with the public file ID.

### Merge commits

A Git merge keeps its parent count and order, but each parent becomes the
corresponding lifted private commit. The algorithm first merges all raw public
parents and all private parents separately. It then replays the public merge
commit's delta between those 2 merged bases.

The test proves a clean 2-parent Git merge. Both branch changes and the private
hidden value appear in the lifted merge. `merge_commit_trees()` also supports
octopus merges and recursive merge bases, but the probe does not test those.
Hidden disagreement between private parents remains a native jj conflict under
this algorithm, as required by the specification, but needs a dedicated
product test.

### Re-export after a hidden-path public edit

An external Git commit can publish a path that Devspace currently hides. Those
bytes already exist in the external remote and enter the sidecar during fetch.
Devspace cannot make those old objects cease to exist immediately.

The probe rebuilds a sanitized public lineage from the last safe mapped Git
ancestor. It removes the hidden path from each new public shadow and reparents
each shadow to the sanitized parent. It records canonical-to-sanitized-Git
mappings for the lifted commits that contain hidden-only native conflicts. The
normal exporter then exports the final clean lifted commit.

The resulting public head has no reachable commit containing `secrets/.env`.
No reachable blob contains the pre-existing private bytes or either public
hidden value. The original collaborator values remain in unreachable objects
in the bare remote and sidecar until normal Git retention and garbage
collection remove them.

This sanitization is probe code, not the proposed production abstraction. It
does prove that a rewritten public lineage can preserve native conflict history
without exposing any hidden term.

## Cloud journal state and protocol

### State fetch needs

Fetch needs the state already described by the specification:

- immutable repository-wide Git ID to raw public shadow receipts
- active per-ref states containing Git ID, raw public shadow, private canonical
  commit and policy epoch
- the exact current per-ref cursor
- every historical policy version referenced by an active seed state
- pending push batches, because recovery must finish before unrelated import

The current `GET /repositories/:repository/projection` response can provide
active mappings, cursors, policy and pending batches. It does not provide a way
to commit import receipts or lifted states. `src/worker.ts` has no fetch or
import mutation route.

### Minimal protocol addition

Add one idempotent mutation route:

```text
POST /repositories/:repository/git/fetches
```

The request should contain:

- a stable fetch ID and machine ID
- remote name and fetched ref names
- the exact observed Git head for each ref
- the expected prior cursor or absence for each ref
- the policy epoch used for lifting
- every new immutable Git-to-raw-public receipt
- parent-first per-ref states binding Git, public and lifted private IDs
- the proposed final state for each ref

The Durable Object should validate in one transaction:

- authentication, incarnation and idempotent request hash
- exact expected cursors
- no unresolved overlapping push batch
- unchanged policy epoch
- object closure for every new raw public and private commit
- immutable receipt consistency
- one unambiguous lineage per reached Git object

It should then insert receipts, append active projection states and advance the
existing ref cursors atomically. A separate fetch cursor is unnecessary. The
projection activation cursor already pages journal changes, and the per-ref
cursor already identifies the last accepted Git and private state.

For scale, add an exact lookup endpoint later, or extend the projection read to
accept a bounded list of Git IDs and candidate refs. The first product version
can use the existing paged projection snapshot before adding this read
optimization. The mutation route is the missing correctness surface.

The machine must upload and confirm both raw public and lifted private object
closures before it calls the fetch mutation. A stable fetch ID makes a retry
safe if the machine loses the response.

## Gaps and hazards in import_reachable

`import_reachable` is an object translator, not a fetch implementation. The
probe found these gaps:

- it always uses an empty hidden filter and imports the remote tree literally
- it creates raw public shadows only; it does not select a private seed, lift
  parents, merge trees, update refs or commit an operation
- it writes immutable objects before a later error, although it leaves the
  operation head unchanged
- it trusts a supplied mapping once the target commit exists; it does not
  recompute and compare the translation
- `reached_mappings` mixes preloaded durable mappings with mappings created
  earlier in the same call and reached again through another merge branch
- it rejects Git links, but has no product decision for other remote values at
  an exact hidden file path, especially a directory
- it drops copy IDs, predecessors, conflict labels and secure signatures during
  translation
- Git commit signing cannot survive a rewritten sanitized commit; raw
  `GitBackend` commit data also has `secure_sig` set and panics if passed back to
  `write_commit` without clearing it
- it has no protocol or resource limits for commit depth, tree width or input
  head count
- recursive tree copying needs separate adversarial depth testing
- it has no special handling for non-UTF-8 names, case collisions or paths that
  cannot materialize on a client platform

Git submodules already fail before simple-backend encoding, which is correct
for the accepted canonical schema. Files, executable bits and symlinks work in
the exercised translator. The probe does not cover annotated tags, replace
refs, shallow history, partial clones, signed pushes or Git notes.

The largest export-side gap blocks the specified fetch behavior: the current
exporter rejects any commit with a conflicted root tree before it filters hidden
paths. A public edit at a seeded hidden path must create a native conflict, so a
later export cannot traverse that history without pre-recorded sanitized
mappings. The test directly asserts this failure.

Production should let the exporter discard conflict terms only at exact hidden
paths before it rejects remaining public conflicts. It must still fail on any
conflict that can affect a public path.

## Product decisions required before implementation

> Decision 1: choose whether fetch can rewrite externally published hidden
> history.

The specification says hidden paths are absent from every Git projection, but
it does not say how to handle a collaborator who publishes a currently-hidden
path. Keeping that Git commit as an ancestor leaves the hidden path reachable.
The probe chooses a sanitized rewrite from the last safe mapped ancestor. This
requires a lease-protected force update on the next push. If this is not the
product behavior, narrow the guarantee explicitly.

> Decision 2: define which policy epoch applies to new fetched commits.

The specification says exact seed states retain their historical policy epoch.
The v3 summary says fetch merges under the current hidden rules. These differ
when policy changed after the last push. Decide whether lift uses the seed
epoch, current epoch, or an explicit transition between both.

> Decision 3: define the synthetic tombstones.

The unseeded hidden addition rule depends on 2 fixed negative objects, but the
specification does not define their bytes, type selection or deterministic
choice. This is part of canonical semantics and must be fixed before machines
can converge.

> Decision 4: define non-file collisions at hidden paths.

A Git collaborator can replace an exact hidden file path with a directory,
symlink or Git link. The current exporter rejects a hidden directory and import
rejects a Git link. Decide which cases become native conflicts and which fail
closed.

## Product implementation recommendation

Build the product path in this order:

1. Settle the 4 decisions above and update `hidden-files.md` first.
2. Add exact seed lookup and the idempotent fetch journal mutation.
3. Split raw public import from lineage lifting in the machine API. Return
   preloaded seed hits separately from within-call translation reuse.
4. Implement lifting as a pure parent-first engine using
   `merge_commit_trees()` and `MergedTree::merge()`.
5. Implement the synthetic tombstone and hidden-path type rules.
6. Make export filter exact hidden paths from conflicted trees before rejecting
   any remaining public conflict.
7. Generate and journal sanitized shadows for externally introduced hidden
   paths, with exact leases for the later remote rewrite.
8. Commit native refs and operations, sync both object closures, then commit the
   cloud fetch transaction.
9. Add Worker and real-remote tests for retries, cursor races, policy changes,
   pending push recovery, rewritten refs, ambiguous multi-ref seeds, octopus
   merges and hidden parent disagreement.

Reuse the tree-rebase formula, receipt discipline and end-to-end fixture from
the probe. Rebuild the test-local sanitizer, raw backend writes and in-memory
maps as production machine and journal components. Do not copy them as a
service boundary.

## How to run the proof

Run only the fetch proof:

```sh
nix develop -c cargo test -p devspace-machine \
  --test fetch_import_lift -- --nocapture
```

Run the requested machine gate:

```sh
nix develop -c cargo test -p devspace-machine
```

The full gate passes. Existing live cloud, live projection and timing tests
remain ignored unless run with their documented environments.
