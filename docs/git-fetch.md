# Git fetch

`ds git fetch` imports public Git history through the projection journal. It
fetches exact public objects into the canonical bare Git object database, lifts
the inherited hidden overlay onto new history, makes both public and canonical
closures cloud-durable, and updates remote-tracking bookmarks.

## Command

Fetch every branch from the default remote:

```sh
ds git fetch
```

Select a remote or literal branch names:

```sh
ds git fetch --remote origin
ds git fetch --remote origin --branch main --branch release
```

The remote must be registered with `ds git remote add`. Arbitrary refspecs and
unregistered URLs are outside this boundary.

## Fetch flow

One fetch holds the repository sync lock and performs this sequence:

1. read the complete projection snapshot;
2. recover any pending push batch that overlaps the requested bookmarks;
3. refresh the snapshot if recovery settled a batch;
4. install the current cloud pack catalog;
5. run `git fetch` into temporary refs in the shared bare object database;
6. read the fetched public head OIDs;
7. seed overlay-lift with journaled canonical/public pairs;
8. replay canonical hidden state over each newly reached public commit;
9. print every data-disclosure warning;
10. upload the Git closure of all public and canonical heads;
11. record one idempotent fetch transaction in the Worker;
12. update Jujutsu remote-tracking bookmarks to the canonical heads in one
    native operation.

Public objects retain their exact remote bytes and Git OIDs.

## Seed selection

The journal snapshot supplies active and historical pair states:

```text
canonicalOid  publicOid  hiddenSetId?
```

Active pair states and cursors are stop points for overlay-lift. Their canonical
OIDs and hidden-set identities supply the lineage from which new descendants
continue. Identity cursors use one OID on both sides and carry no hidden-set
identity.

If a fetched ref has an active cursor, its new public head must descend from
the cursor's public OID. Devspace rejects a rewritten remote ref instead of
guessing which canonical hidden lineage should own it.

An untracked public history with no hidden policy can start from identity. A
history that requires private overlay needs an unambiguous journal seed.

## Overlay lift

Overlay-lift walks foreign commits parent-first. For each commit it maps public
parents to canonical parents, compares the public-parent and canonical-parent
base trees, and applies the public change over the canonical base.

If the parents are unchanged, no hidden policy exists, and no hidden path is
published, the commit is an identity:

```text
canonicalOid == publicOid
```

No mirror commit and no pair row are created. This preserves the exact foreign
commit, including its signatures and unknown headers.

If hidden state or rewritten parents require a mirror, Devspace creates a
canonical Git commit while retaining the original public commit as the pair's
`publicOid`. The canonical mirror inherits private paths and `.dsprivate`
policy from its canonical parents. Both objects remain in the same Git object
database and become cloud-durable.

Merges replay all parent lineages through Jujutsu's merged-tree semantics.
Hidden conflicts remain canonical conflicts; they are never flattened into a
public tree.

## Disclosure warning

A foreign commit can publish a path that the inherited `.dsprivate` policy
marks hidden. That content is already visible on the Git remote. Fetch prints:

```text
WARNING: DATA DISCLOSURE: foreign commit <oid> contains hidden path `<path>`;
that foreign version is publicly visible on the remote
```

Devspace does not silently choose the public or private value. It creates a
canonical Jujutsu tree conflict between the foreign content and a deterministic
tombstone that explains the collision. The user must resolve the conflict.

Fetch cannot retract the disclosed bytes. Rotate or revoke any exposed secret
outside Devspace.

## Journal transaction

Each fetched bookmark records:

- the observed public OID;
- the expected active cursor OID, if one exists;
- newly created canonical/public pair states reachable from that bookmark;
- either the proposed pair index or an `identityOid`.

`identityOid` must equal the observed public OID and cannot accompany pair
states. The Worker verifies every public and canonical commit is durable and
checks request-wide binding and hidden-lineage consistency before mutation.

The fetch ID and canonical request hash make retries idempotent. Reusing the ID
for different bytes is rejected. Cursor advancement is transactional across
the request.

## Native view update and recovery

The cloud journal is committed before the local Jujutsu remote-tracking
operation. If the process stops between those steps, the next fetch reads the
same journal result and can repeat the local update.

A fetch first recovers overlapping pending pushes because their final public
OIDs determine the correct seed lineage. Recovery uses the original leases and
fencing rules described in [Git push](git-push.md).

Downloaded public and mirrored canonical objects are ordinary Git objects.
Fresh-machine recovery obtains them from the cloud pack catalog. One command
tracks the installed catalog high-water and downloads only later entries when
another recovery step needs them.

Snapshot activation high-waters do not make concurrent remote repointing
consistent. That remains the remote-generation work in issue #15.

## Exporter interaction

The next push starts from the active cursor. Locally created canonical
descendants project from that seed:

- an unchanged hidden-free descendant remains identity;
- a descendant with hidden paths gets a new public mirror;
- the lease expects the public OID recorded by fetch.

This round trip preserves the public lineage while the canonical lineage keeps
its private overlay.
