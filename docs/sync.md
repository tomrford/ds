# Synchronization and convergence

Devspace replicates a Jujutsu repository without replicating a working copy.
Each machine owns a bare Git repository and Jujutsu operation store. The cloud
owns immutable object bytes, installed pack metadata, and the authoritative set
of operation heads.

The protocol is content-addressed and retry-safe. No synchronization request
rewrites an existing object.

## Commands and connectivity

`ds` records a successful repository mutation before it starts background
synchronization. The boundary process runs the same synchronization engine as
the hidden `ds sync run --repository-name <name>` command.

`ds sync status` reports local catalog sequence, accepted cloud operation
heads, pending outbox entries, and failures for every machine repository. It
does not contact Git remotes.

Commands that need the cloud authenticate with:

- `x-devspace-machine`;
- `x-devspace-secret`;
- `x-devspace-incarnation`;
- `x-devspace-client: ds/<version> git-pack/2`.

The control plane checks the enrolled machine and active repository
incarnation before forwarding a request to `RepositoryGit`.

## Native repository boundary

Every checkout for one logical repository shares one bare Git object database.
It contains:

- canonical Git blobs, trees, and commits;
- public projection commits created for Git remotes;
- Jujutsu's operation and view files;
- `store/extra`, the rebuildable GitBackend metadata cache.

Only Git object bytes and operation-store objects are replicated. Workspace
state, working-copy files, locks, indexes, and `store/extra` stay local.

Opening a repository reconstructs missing GitBackend cache entries from the
Git commit headers. A fresh clone therefore needs no private side database to
recover change IDs or conflicted-tree metadata.

## Machine-store catalog

The machine store maps a validated repository name to:

- the cloud repository ID;
- its current incarnation;
- the local bare repository path.

Catalog creation is staged behind a durable creation intent. A crash after the
cloud allocates a repository but before local materialization resumes the same
intent. It does not allocate another repository. Per-repository locks serialize
materialization, synchronization, and destructive removal.

The cloud control plane is the authority for names and incarnations. A deleted
incarnation cannot authorize an old repository Durable Object.

## Git object closure

The Git closure starts from commit heads and walks:

- commit to root tree and parent commits;
- tree to every referenced blob or child tree.

Object keys are `(kind, oid)`, where kind is blob, tree, or commit and the OID
is the 20-byte Git SHA-1. The validator recomputes the exact Git object
preimage, rejects collisions, parses structured objects, and records their
references.

Closure discovery rejects:

- a missing referenced object;
- a referenced object of the wrong kind;
- malformed Git bytes;
- an object whose supplied OID does not match its bytes;
- an object beyond the configured byte or closure bounds.

The machine reads canonical and projected objects from the same Git object
database.

## Git pack format

Git objects travel in deterministic `DSPK` version 2 packs. This is a
Devspace transport container, not Git's packfile format.

The manifest contains:

- the `DSPK` magic and version;
- chunk size, total pack length, and 64-byte Blake2b pack digest;
- ordered 20-byte head OIDs;
- ordered object entries with kind, OID, offset, and length;
- ordered chunk entries with offset, length, and 64-byte Blake2b digest.

The payload concatenates exact Git object payloads in manifest order. The same
closure and chunk size produce the same manifest, payload, and pack ID.

The Worker receives a manifest and bounded chunk parts in quarantine. Install
is one Durable Object transaction:

1. verify manifest, part layout, chunk digests, and full pack digest;
2. validate every Git object with the WebAssembly kernel;
3. verify every reference is present in the pack or already installed;
4. insert objects and reference rows with no-clobber checks;
5. publish the installed pack at the next catalog sequence;
6. remove its quarantine rows.

A repeated install of the same pack is idempotent. A different byte sequence
for an existing object key is a hard error.

## Operation object closure

Jujutsu operation history uses a separate content-addressed graph:

- an operation references its parent operations and one view;
- a view references canonical Git commits and other jj view state.

View and operation IDs are 64-byte Blake2b values. The machine validates local
objects before upload. The Worker validates them again before insertion.

Inventory requests are bounded batches of `(kind, id)` keys. The Worker returns
the missing keys; the machine uploads or downloads only those objects. Git
commit references in views must become durable through the Git-pack path
before an operation head can advance.

## Cloud operation heads

The cloud stores a set of operation heads and a monotonically increasing
cursor. A head transaction contains:

- a stable idempotency key;
- one proposed new head;
- the previously observed heads that the new operation supersedes.

The Worker verifies that the new operation and its closure are durable,
removes only observed ancestor heads, inserts the new head, and records the
result under the idempotency key. Replaying the same request returns the same
result. Reusing a key for different bytes is rejected.

Concurrent machines can add distinct heads. They do not overwrite one another.
The next machine to synchronize downloads both closures and lets Jujutsu create
the native merge operation.

## Synchronization run

One run holds the repository synchronization lock and follows this order:

1. load local sync state;
2. if an outbox batch exists, re-upload its reachable Git and operation
   closures, replay its head transactions, and stop;
3. read cloud operation heads;
4. download Git packs after the local catalog sequence;
5. download the missing operation closures;
6. reconcile multiple cloud heads in the native Jujutsu repository;
7. persist the accepted heads and catalog sequence;
8. discover the current local operation heads;
9. upload their reachable Git closure;
10. upload their operation closure;
11. write a durable outbox batch for new head transactions;
12. apply each transaction and remove each acknowledged outbox entry.

The outbox is written only after every referenced byte is durable in the
cloud. On retry, the machine uploads the closure again before replaying the
transaction. This ordering makes a local crash, network timeout, or lost
response recoverable without guessing whether the cloud committed.

## Convergence

Objects converge by immutable content identity. Operation heads converge by
set reconciliation.

If machines A and B write concurrently:

1. A and B upload disjoint objects and operation heads;
2. the cloud retains both heads;
3. a later sync downloads both operation closures;
4. Jujutsu merges the operations locally;
5. that merge operation is uploaded as a new head;
6. its transaction removes the observed ancestors.

No last-writer-wins register exists for repository state. A transaction can
remove only heads it proves it observed.

## Exact cloud rebuild

A new machine can recover a repository using only:

- its control-plane repository identity;
- the installed Git pack catalog and bytes;
- operation objects and cloud operation heads.

It installs every missing Git pack, downloads each operation closure, rebuilds
the GitBackend cache from commit bytes, and opens a checkout at the recovered
operation. The recovered canonical Git OIDs and Jujutsu operation IDs match the
source machine exactly.

Projection journal state is cloud data in the same repository Durable Object.
It is needed for Git remote continuity, not for canonical repository recovery.

## Command-boundary recovery

Mutation commands recover the native repository before exposing it to
Jujutsu. They serialize against sync, finish any durable operation-head outbox,
and reject an inconsistent or retired repository identity.

Git push and fetch add their own projection-journal recovery boundary. See
[Git push](git-push.md) and [Git fetch](git-fetch.md).

The following are deliberate hard failures:

- installed bytes conflict with an existing object ID;
- a pack or operation closure is incomplete;
- cloud authorization names a stale incarnation;
- an idempotency key is reused for a different request;
- projection state would bind one canonical commit to two public commits;
- hidden-path scanning detects public disclosure.
