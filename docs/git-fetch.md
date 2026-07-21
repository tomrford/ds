# Git fetch

Fetch moves public Git history into canonical native history: it imports new
remote commits as raw public shadows, then lifts them onto private lineage so
hidden content and private ancestry are preserved. The semantics follow
`hidden.md`.

## Command

The command is available only in a Devspace checkout:

```text
ds git fetch [--remote <name>] [-b <bookmark> ...]
```

The default remote is `origin`. Bookmark arguments are literal Git branch
names. With no bookmark arguments, fetch imports every advertised remote head.
The command runs ordinary repository sync first and holds the repository sync
lock through transport, journal mutation and native view update.

## Import from a remote

`ds repo add` imports an existing Git remote without creating a checkout:

```text
ds repo add <git-url> [--name <name>]
```

The name defaults to the remote URL basename without a trailing `.git`.
`ds init` uses the same import path and adds the first checkout:

```text
ds init <git-url> [<directory>] [--name <name>]
```

Both commands are online-only. They do not convert an existing local Git
repository in place. The checkout directory defaults to `./<name>`.

Import composes the repository creation, remote registry, empty native
materialization and fetch paths. Initialization adds checkout creation. Both
discover the symbolic remote HEAD with `git ls-remote --symref`, import every
advertised head and track the HEAD bookmark at `origin`. Initialization leaves
a new empty working-copy change on top. Empty remotes use `ds repo new`
instead. SHA-256 Git remotes are not supported.

Without a Git URL, `ds init [<directory>]` creates a blank repository named
after the directory and creates its first checkout there.

Repository creation uses the same durable idempotency intent as `ds repo new`.
A retry after a lost cloud response resumes that intent. A later failure keeps
the created cloud repository visible in the error and reports that the local
Git import is incomplete.

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

When a ref has a cursor, its fetched head must descend from the cursor's Git
object in the sidecar graph. A non-descendant head is a rewritten ref and
fails closed; force-pushed history is unsupported. A ref with neither a cursor
nor any receipt-backed seed in its ancestry imports from scratch.

Pending push batches overlapping a fetched ref are recovered before import
(see `git-projection.md`); stale no-op lineage is discarded before unrelated
history is interpreted.

## Import

`import_reachable` translates new Git commits, trees, files and symlinks into
canonical objects, stopping at known raw public shadows supplied from durable
receipts. Import is a translation boundary, not an authority: it publishes no
operation head. Git links fail before the simple backend can encode a tree;
submodules are unsupported. Signatures, which cannot survive translation, are
stripped; raw Git commit data carries a secure-signature field that is cleared
before canonical encoding. Import limits one request to 256 heads, 1,024
commits of ancestry per head, 256 levels of tree recursion and 8,192 entries
in one tree. These are current safety limits aligned with the projection
request bounds, not production quotas. Ordinary supplied mappings are
recomputed and checked; only durable receipt stops terminate translation.

## Lifting

Newly imported public shadows are lifted parent-first. For each public commit
C:

1. each raw public parent is replaced by its already-lifted private commit;
2. the raw public parent trees merge into P, the lifted private parent trees
   merge into Q;
3. the lifted tree is the 3-term merge Q − P + C.tree — the same tree rebase
   jj itself uses;
4. the applicable hidden set is resolved from Q; a conflict in any of Q's
   `.dsprivate` files fails fetch closed;
5. a clean public-introduced value at a hidden path that is absent from Q is
   rewritten to the tombstone conflict defined in `hidden.md`; values supplied
   by Q are not rewritten;
6. the canonical commit is written with the lifted private parents, the
   merged tree and the imported commit's metadata.

Where public trees omit hidden paths, the merged private parents supply their
values unchanged: hidden content, including `.dsprivate` itself, flows to
lifted commits structurally. A public edit at a hidden path becomes a native jj
conflict exactly as `hidden.md` specifies; a Git merge lifts by merging all
public parents and all private parents separately and replaying the delta
between them, preserving parent count and order.

Each lifted commit produces a parent-first state row containing its Git object,
lifted canonical ID, raw public-shadow ID and the identity of the applicable
hidden set resolved from Q. This identity remains deterministic even when the
resulting lifted tree contains conflicts.

Lifted results are deterministic: the same fetched commits, seeds and
hidden-set identities produce identical canonical objects on any machine.

## Journal transaction

Fetch records observed remote history with this idempotent repository Durable
Object mutation:

```text
POST /repositories/:repository/git/fetches
```

The strict request contains `incarnation`, a stable 16-byte `fetchId`, the
authenticated 16-byte `machineId`, a registered `remote`, non-empty `refs` and
`receipts`. Each ref contains `bookmark`, its exact `observedGitOid`, the
nullable `expectedCursorOid`, parent-first projection `states`, and a nullable
`proposedState` index. A null index selects an already-active state for the
same remote, bookmark and observed Git object. Receipts contain `gitOid` and
`publicCommitId`; the array can be empty when every mapping is known. The
route uses the projection body, ref and state limits, and caps receipts at the
same 8,192-entry limit as states.

One synchronous storage transaction validates, in order: the fetch request
hash under `fetchId` (`fetch-request-mismatch`); exact expected cursors
(`fetch-cursor-stale`); absence of an overlapping pending push
(`fetch-overlaps-pending-push`); complete raw-public and lifted-private commit
closures (`fetch-commit-not-durable`); immutable receipts
(`git-receipt-conflict`); receipt coverage for every state
(`fetch-state-receipt-mismatch`); and one lineage per Git object across the
request and the newest active state per bookmark
(`fetch-lineage-ambiguous`). An unregistered remote fails with
`remote-not-found`.

The transaction inserts receipts, appends every new state as active with an
activation sequence, advances each ref cursor and records the response under
`fetchId`. The response is `{fetchId, activationCursor}`. An identical retry
returns that recorded response without inserting states again. No separate
fetch cursor exists: the projection activation cursor pages journal changes
and the per-ref cursor identifies the last accepted state.

The machine uploads and confirms both the raw public and lifted private object
closures before calling the mutation, so the durability gate holds. A lost
response is replayed safely under the same fetch ID.

The first version reads seed state from the existing paged projection
snapshot; a bounded exact-lookup read (by Git ID list) is a later
optimization, not a correctness surface.

## Native view update and recovery

After the journal accepts a fetch, one jj transaction described
`fetch from <remote>` updates each `<bookmark>@<remote>` target from the
journal cursor. Existing tracked remote bookmarks propagate into their local
bookmarks through jj-lib's ref-target merge: an unchanged local bookmark
fast-forwards, concurrent local and remote movement produces a conflicted
bookmark, and no commit is discarded. Newly observed remote bookmarks remain
untracked and do not create or move a local bookmark.

The journal cursor is the source of truth for this update. An up-to-date Git
observation still repairs a missing or stale native remote bookmark. If the
process stops after `record_fetch` but before committing the view transaction,
the next fetch performs that repair without another journal mutation.

## Pollution warning

Lifting reports every conflict at a path hidden by the applicable hidden set,
including both inserted tombstones and natural merge conflicts. Fetch prints
one prominent warning listing those paths. The warning states that public
bytes remain on the Git remote until its history is rewritten externally;
resolving the native conflict does not erase already-published Git objects.

## Exporter interaction

Post-fetch history legitimately carries hidden-path conflicts, but export
never needs to re-encode those commits: the fetch transaction records the
binding between each lifted commit and its already-existing public Git
counterpart, so export of later work stops at the mapped lifted parent.
Export of a commit that itself carries a conflict — hidden or public — fails
closed. Hidden-path conflicts present as ordinary jj conflicts; the boundary
error names the hidden path. The exporter change the fetch path requires is
therefore mapping-aware traversal plus path-aware rejection, not conflict-term
filtering.
