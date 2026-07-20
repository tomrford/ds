# Git projection

Git projection maintains 2 invariants across the machine-store path and the
repository Durable Object:

- bytes matched by each canonical commit's hidden set do not enter public Git
  objects; and
- a push remains recoverable when the remote ref moves but the machine does
  not finalise the cloud journal.

## Machine projection

`GitProjection` owns a rebuildable bare-Git sidecar backed by jj's Git backend.
It translates commits between a stock native `SimpleBackend` store and Git
without making the sidecar authoritative. Export resolves each commit's root
`.dsprivate` blob as a gitignore matcher, caches matchers by `FileId`, prunes
matching directories without descent and filters matching leaves before
reading them. The root `.dsprivate` is always excluded. Export rejects conflicts,
non-file root `.dsprivate` entries and Git links with typed errors. Import is
unfiltered and rejects Git links before asking the simple backend to encode a
tree.

Translation receipts are external inputs and outputs. The adapter accepts
durable canonical-to-Git or Git-to-public mappings, reuses consistent rows and
rejects conflicting rows. A missing sidecar can therefore be recreated from
the native objects and accepted cloud mappings.

The machine can discover and pack a commit closure that is not reachable from
an operation head. This makes both the selected private commit and each public
shadow commit cloud durable before a pending push is accepted.

## Cloud journal

The per-repository Durable Object owns Git-object receipts, append-only
projection states, exact ref cursors, pending push batches and final results.
Each state binds a Git object ID, private canonical commit, public shadow
commit and nullable hidden-set identity. It does not execute Git.

A batch contains the exact remote and bookmark set, expected old Git IDs, the
full reachable state set, one optional proposed state per ref, an owner machine
and a monotonic fencing token. The hidden-set binding belongs to each state,
not the batch. Draft mappings remain quarantined until the complete batch is
accepted. Disjoint ref batches may coexist; an overlapping ref is locked by
its pending batch.

Creating a batch checks that every private and public commit is already in the
cloud object store. Git receipts are repository-wide and immutable: one Git
commit ID cannot later name a different public shadow. Reusing a batch ID with
different canonical input or hidden-set identity fails.

Any authenticated machine may claim a pending batch. Claiming assigns a new
fencing token, so a callback from the previous owner cannot finalise it. A
claimant cannot finalise through the normal callback and cannot abort a batch
while the remote still matches the expected refs: an already-running push from
the previous owner could still land. Instead, the claimant repeats the exact
lease-protected push recorded by the batch, then recovers from the observed
remote values. The replay endpoint returns the bounded, quarantined mapping
set and proposed-state positions, so a fresh claimant can download the already
durable native objects, rebuild the exact Git objects and perform that push.
Recovery compares the complete observed ref set with the journal:

- all refs at their proposed values accepts every draft and advances or clears
  every cursor atomically, including deletion-only batches;
- all refs at their expected values aborts an unclaimed batch, while a claimed
  batch remains pending until its exact push is replayed;
- mixed or otherwise ambiguous values retain the batch and fail closed.

The Durable Object accepts only exact Git IDs. It does not accept a client
claim that an unrecorded Git commit is a descendant; that requires imported
ancestry and a durable projection state first.

## Verification

The normal Rust tests pin the gitignore matcher behavior, resolve different
`.dsprivate` blobs across one history walk, prove filtering before leaf reads,
scan every blob in a fresh sidecar for binary private sentinels, rebuild a
deleted sidecar from durable mapping rows, and reject a Git link without
changing the native operation head.

Workers Vitest exercises the authenticated HTTP routes and real SQLite-backed
Durable Object. It covers nullable and concrete hidden-set identities through
replay and accepted snapshots, malformed identity rejection, identity-bound
batch retries, eviction, overlapping batches, stale fences, before-push abort,
post-push acceptance, mixed-outcome quarantine, cloud-durability checks and
immutable receipt collisions.

Rust transport coverage proves both the sync and projection clients use the
hardened HTTP client and terminate when a Worker accepts a request but never
responds.

The ignored `projection_live` test crosses the Rust/TypeScript boundary against
`wrangler dev` or a deployment. It creates private native history, exports and
scans a public Git graph, uploads private and public commit closures, creates a
pending batch, pushes with an exact lease to a real bare Git remote and omits
the first machine's final callback. A second client claims the batch, reads its
durable replay mappings, downloads the cloud pack into a fresh native store,
rebuilds an empty sidecar, observes the remote and recovers the batch. The test
then claims a second batch before its remote ref moves, proves recovery retains
the unchanged batch, reads the replay payload, performs the exact lease push
from the rebuilt sidecar and finalises it from the observed remote value. It
scans every blob in both the remote and rebuilt sidecar for both private values.

The live probe accepts the shared development credential with machine IDs `11`
repeated 16 times and `22` repeated 16 times. It remains a manual deployment
probe because its repository authority and Git remote are supplied explicitly.

## Budgets and benchmarks

The journal bounds one request to 4 MiB, 256 refs and 8,192 projection states.
Repository-wide pending and active ref sets are each bounded to 512. Remote
and bookmark names are bounded to 256 UTF-8 bytes. Mapping reads page 256 rows
under one fixed activation high-water. These are current safety limits, not
settled production quotas.

The projection path does not run during warm repository open. The release-only
comparison remains inside the 2 times budget; 3 current local runs measure
1.300, 1.297 and 1.297 times stock jj. The embedded command runner connects
checkout `ds git push` execution to the projection journal while warm
repository open and bare-repository `log` remain local-only paths.

The dry-run Worker bundle is 270.78 KiB uncompressed and 80.67 KiB compressed.
The validation Wasm is 142,859 bytes and remains below its 200 KiB build gate.

## Current limitations

- The authenticated machine-ID header binds projection ownership and fencing
  callbacks to the configured machine. A payload cannot claim another machine
  ID.
- Fetch-side hidden-lineage lifting is specified in
  [`git-fetch.md`](git-fetch.md) but is not implemented. Push uses the remote
  registry, structured Git subprocess and per-commit hidden model described in
  [`git-push.md`](git-push.md) and [`hidden.md`](hidden.md).
- Projection width and depth need observational scaling measurements before
  production limits are set. Exercise at least 1,000, 10,000 and 100,000 tree
  entries, and history depths of 1, 100 and 1,000 commits, recording cold
  export, receipt-reuse export and empty-sidecar rebuild separately.
