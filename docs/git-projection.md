# Git projection

Git projection maintains 2 invariants across the machine-store path and the
repository Durable Object:

- bytes hidden by the active policy do not enter public Git objects; and
- a push remains recoverable when the remote ref moves but the machine does
  not finalise the cloud journal.

## Machine projection

`GitProjection` owns a rebuildable bare-Git sidecar backed by jj's Git backend.
It translates commits between a stock native `SimpleBackend` store and Git
without making the sidecar authoritative. Export applies the exact hidden-path
set before reading excluded leaves, omits directories made empty by filtering,
and rejects conflicts and Git links with typed errors. Import also rejects Git
links before asking the simple backend to encode a tree.

Translation receipts are external inputs and outputs. The adapter accepts
durable canonical-to-Git or Git-to-public mappings, reuses consistent rows and
rejects conflicting rows. A missing sidecar can therefore be recreated from
the native objects and accepted cloud mappings.

The machine can discover and pack a commit closure that is not reachable from
an operation head. This makes both the selected private commit and each public
shadow commit cloud durable before a pending push is accepted.

## Cloud journal

The existing per-repository Durable Object owns immutable hidden-policy
versions, Git-object receipts, append-only projection states, exact ref
cursors, pending push batches and final results. It does not execute Git.

A batch contains the exact remote and bookmark set, expected old Git IDs, the
full reachable private/public/Git mapping set, one optional proposed state per
ref, the current policy epoch, an owner machine and a monotonic fencing token.
Draft mappings remain quarantined until the complete batch is accepted.
Disjoint ref batches may coexist; an overlapping ref is locked by its pending
batch. A real policy change fails while any batch is pending, while an
idempotent no-op returns the current epoch.

Creating a batch checks that every private and public commit is already in the
cloud object store. Git receipts are repository-wide and immutable: one Git
commit ID cannot later name a different public shadow. Reusing a batch ID with
different canonical input fails.

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

The normal Rust tests prove filtering before leaf reads, scan every blob in a
fresh sidecar for binary private sentinels, rebuild a deleted sidecar from
durable mapping rows, and reject a Git link without changing the native
operation head.

Workers Vitest exercises the authenticated HTTP routes and real SQLite-backed
Durable Object. It covers policy epochs, hidden drafts, eviction, overlapping
batches, stale fences, exact retry receipts, before-push abort, post-push
acceptance, mixed-outcome quarantine, cloud-durability checks and immutable
receipt collisions.

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

Run the live test with:

```sh
nix develop -c pnpm build:wasm
nix develop -c pnpm exec wrangler dev --port 8787 --var DEVSPACE_TOKEN:dev-token
DEVSPACE_URL=http://127.0.0.1:8787 \
DEVSPACE_TOKEN=dev-token \
  nix develop -c cargo test -p devspace-machine \
    --test projection_live -- --ignored --nocapture
```

## Budgets and benchmarks

The journal bounds one request to 4 MiB, 256 refs and 8,192 projection states.
Repository-wide pending and active ref sets are each bounded to 512. Names and
hidden paths are bounded to 256 UTF-8 bytes; one policy is bounded to 4,096
exact paths and 256 KiB of path text. Mapping reads page 256 rows under one
fixed activation high-water. These are current safety limits, not settled
production quotas.

The projection path does not run during warm repository open. The release-only
comparison remains inside the 2 times budget; 3 current local runs measure
1.300, 1.297 and 1.297 times stock jj. The complete command benchmark remains
open because this repository still has no command runner. The required gate is
a representative warm command at no more than 2 times local jj with the
network disabled and zero cloud requests.

The dry-run Worker bundle is 244.31 KiB uncompressed and 75.55 KiB compressed.
The validation Wasm is 163,051 bytes and remains below its 200 KiB build gate.

## Current limitations

- Authentication is one shared bearer token. The fencing protocol establishes
  stale-owner mechanics, not cryptographic machine identity.
- The live test omits the first callback at the protocol boundary; it does not
  launch and hard-kill a separate CLI process because the command runner
  does not exist.
- Fetch-side hidden-lineage lifting, remote replacement and production Git
  credential handling remain open product work outside these invariants.
- Projection width and depth need observational scaling measurements before
  production limits are set. Exercise at least 1,000, 10,000 and 100,000 tree
  entries, and history depths of 1, 100 and 1,000 commits, recording cold
  export, receipt-reuse export and empty-sidecar rebuild separately.
