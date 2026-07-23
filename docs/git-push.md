# Git push

`ds git push` is the only supported path from canonical Devspace history to a
public Git remote. It projects hidden paths, uploads every required canonical
and public object to the cloud, and binds the Git subprocess result to the cloud
journal.

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

1. read the complete projection snapshot;
2. recover any pending batch that overlaps the requested remote bookmarks;
3. resolve each local bookmark to its canonical Git OID and return immediately
   if every requested cursor is already current;
4. seed projection from active pair states, cursors, and pending identity
   bindings;
5. project hidden paths parent-first, installing cloud packs only if a reached
   seed selects public bytes that are missing locally;
6. ensure the complete canonical and public closures are local, then scan every
   public tree against the canonical hidden policy;
7. upload those closures;
8. begin a durable projection batch with expected old public OIDs;
9. invoke `git push --porcelain` with an exact force-with-lease for each ref;
10. report the observed live OIDs to the Worker;
11. atomically accept all journal cursors or record the batch as aborted;
12. write one native Jujutsu operation that tracks the accepted remote
    bookmarks.

An up-to-date push makes no pack-catalog, manifest, or chunk request. A
non-no-op attempt also skips the catalog when all required closures are already
local. One command retains its installed catalog high-water so later recovery
or retry work downloads only new catalog entries.

An identity projection sends the canonical commit itself. Its cursor is a
projection stop point, but it does not create a pair state or public mirror.
Existing Git signature bytes remain intact. A hidden path or rewritten parent
creates a minimal deterministic public commit in the same Git object database.

The cloud receives both canonical and public closures before the batch begins.
This makes the batch recoverable by any client with the development credential
and repository identity.

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
- public source OIDs from the shared bare object database;
- exact expected-old leases;
- the user's Git configuration and credential helpers.

Devspace parses porcelain output and then observes the remote refs. Process
exit alone is not proof of the final remote state. A foreground push may invoke
a credential helper or prompt in the terminal. Background recovery inherits the
credential configuration but sets `GIT_TERMINAL_PROMPT=0`, so it fails instead
of waiting for interactive input.

The remote URL is stored in the projection journal. Normal Jujutsu Git commands
that bypass this boundary are rejected for owned Devspace repositories.

## Recovery and races

The durable batch is written before `git push`. If the process exits after the
remote accepted refs but before the Worker records them, the next overlapping
push or fetch:

1. claims a new recovery fence;
2. checks the proposed canonical and public closures and downloads cloud packs
   only when objects are missing;
3. replays the exact proposed states and hidden-path scans;
4. repeats the leased Git updates;
5. submits the observed live OIDs;
6. accepts or aborts the original batch.

Batch IDs and request hashes make every transition idempotent. A stale recovery
owner cannot commit after a newer fence is issued. A claim that races with an
abort returns the settled result and does not request a replay.

Projection is deterministic: the same canonical commit and versioned hidden
policy produce the same public bytes and OID on every honest machine. Concurrent
machines therefore propose the same pair. If begin reports an overlapping push,
the client refreshes and retries once. A stale-cursor result is retried once
only when the refreshed snapshot changed. Other conflicts are returned
immediately.

These retries do not weaken recovery or lease checks. Remote-side divergence,
partial multi-ref results, and stale recovery fences still fail closed.

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
- public export of conflicted canonical commits.
