# Git push

`ds git push` is the only supported path from canonical Devspace history to a
public Git remote. It projects hidden paths, uploads both sides of every
required canonical/public pair to the cloud, and binds the Git subprocess
result to the cloud journal.

## Commands

Register a remote:

```sh
ds git remote add origin git@example.com:owner/project.git
ds git remote list
```

Push literal bookmark names:

```sh
ds git push --remote origin --bookmark main
ds git push --remote origin --bookmark main --bookmark release
```

Delete every tracked remote bookmark whose local bookmark is absent:

```sh
ds git push --remote origin --deleted
```

`--deleted` can be combined with explicit bookmarks. Devspace rejects inferred
or unjournaled deletions.

## Push flow

One push holds the repository sync lock and performs this sequence:

1. recover any pending batch that overlaps the requested remote bookmarks;
2. read the complete projection snapshot;
3. resolve each local bookmark to its canonical Git OID;
4. seed projection with existing canonical/public pairs;
5. project hidden paths parent-first;
6. scan each complete public tree against the canonical hidden policy;
7. upload the Git closure of every canonical and public head;
8. begin a durable projection batch with expected old public OIDs;
9. invoke `git push --porcelain` with an exact force-with-lease for each ref;
10. report the observed live OIDs to the Worker;
11. atomically accept all journal cursors or record the batch as aborted;
12. write one native Jujutsu operation that tracks the accepted remote
    bookmarks.

An identity projection sends the canonical commit itself. No public mirror or
mapping row exists, and existing Git signature bytes remain intact. A hidden
path or rewritten parent creates a minimal public commit in the same Git
object database.

The cloud receives both canonical and public closures before the batch begins.
This makes the batch recoverable by any enrolled machine with the repository
identity.

## Leases and atomic journal state

Each requested bookmark carries an expected old public OID from the active
journal cursor. The Git subprocess uses that value as its lease. A missing
cursor permits only creation, not an unverified overwrite.

A multi-ref Git server can accept some refs and reject others. Devspace treats
the journal update as one unit: the Worker accepts the batch only when every
observed ref equals its proposed public OID. Otherwise it aborts the batch and
does not advance any cursor.

The remote may therefore contain a partial Git-side result after a rejected
multi-ref push. The next command observes that state under the same leases
instead of fabricating success.

## Git subprocess

The subprocess receives:

- the registered remote URL;
- literal `refs/heads/<bookmark>` destinations;
- canonical public source OIDs from the shared bare object database;
- exact expected-old leases;
- isolated credential and prompt settings.

Devspace parses porcelain output and then observes the remote refs. Process
exit alone is not proof of the final remote state.

The remote URL is stored in the projection journal. Normal Jujutsu Git commands
that bypass this boundary are rejected for owned Devspace repositories.

## Recovery

The durable batch is written before `git push`. If the process exits after the
remote accepted refs but before the Worker records them, the next overlapping
push or fetch:

1. claims a new recovery fence;
2. downloads any missing cloud Git packs;
3. replays the exact proposed states and hidden-path scans;
4. repeats the leased Git updates;
5. submits the observed live OIDs;
6. accepts or aborts the original batch.

Batch IDs and request hashes make every transition idempotent. A stale recovery
owner cannot commit after a newer fence is issued.

## Credentials and diagnostics

Git credentials come from the user's Git configuration and credential helpers.
Devspace does not store remote passwords or tokens in the cloud journal.

Diagnostics name the failed phase: projection, hidden-path scan, cloud upload,
batch creation, Git subprocess, live-ref observation, or journal recovery.
Sensitive credential material is not printed.

## Unsupported surface

The boundary deliberately excludes:

- arbitrary refspecs and tag pushes;
- bypassing projection with raw `git push`;
- force without an exact journal lease;
- partial journal acceptance for multi-ref pushes;
- public export of conflicted canonical commits;
- signing newly rewritten public projection commits.

Identity commits keep signatures because their exact canonical bytes are
pushed. Signing a public commit that projection must rewrite remains the only
open signing question.
