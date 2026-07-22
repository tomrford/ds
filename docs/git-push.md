# Git push

`ds git push` publishes hidden-safe Git history from a native Devspace checkout.
The repository Durable Object journals exact expected-old-OID leases and decides
the result from the observed remote refs. The Git process exit code is never the
authority for whether a push succeeded.

## Commands

The Git boundary is available only in Devspace checkouts:

```text
ds git remote add <name> <url>
ds git remote list
ds git fetch [--remote <name>] [-b <bookmark> ...]
ds git push [-b <bookmark> ...] [--deleted] [--remote <name>]
```

The default remote is `origin`. Bookmark arguments are literal Git branch names,
not patterns. Tags, push options and the remaining stock jj Git commands are
fenced because the native store is not Git-backed.

The repository Durable Object owns the remote registry, so a fresh recovery
machine resolves the same remote without inheriting another machine's Git
configuration. A same-URL registration is idempotent. Changing a URL clears
that remote's projection states, cursors and pending work while retaining the
repository-wide immutable Git receipts. Password-bearing URL userinfo is
rejected; credentials do not live in the registry.

## Push flow

A push acquires the per-repository sync lock and performs the ordinary
in-process repository sync before resolving requested local bookmarks from the
locked repository state. A conflicted bookmark fails before Git contact.
An absent local bookmark is a deletion only when the selected remote has a
projection cursor for it; otherwise the command reports `no such bookmark`.
`--deleted` selects every cursor for the selected remote whose local bookmark
is absent and whose local remote bookmark is tracked. It excludes untracked
remote bookmarks, present local bookmarks and cursors for other remotes. An
explicit push refuses to overwrite a present untracked remote bookmark and
advises tracking it first. A creation with no remote bookmark still starts
tracking automatically. `--deleted` can be combined with explicit `-b`
arguments.

After sync, the command:

1. pages one projection snapshot at a fixed activation high-water and loads the
   accepted mappings, cursors and pending batches;
2. opens or creates the rebuildable Git sidecar at the machine repository's
   `projection/` directory;
3. supplies the repository's accepted mappings to `export_reachable`, exports
   its canonical target and scans the resulting public head again under the
   target commit's hidden set;
4. imports the public Git commits as native public shadows and assembles each
   new journal state from the Git OID, canonical commit, public commit and the
   canonical commit's hidden-set identity;
5. discovers the canonical target and public-shadow commit closure, negotiates
   cloud inventory, and uploads and installs the missing immutable packs;
6. creates a random 128-bit journal batch carrying one update per bookmark,
   the cursor's expected old OID and the proposed head state, or no proposed
   state for deletion;
7. performs one foreground, atomic, lease-protected Git push through the
   registered URL and Git's normal credential stack;
8. observes the complete requested ref set and submits those values to journal
   recovery; and
9. reports success only when the journal accepts the batch; and
10. records one jj operation that moves each pushed `<bookmark>@<remote>` to
    the pushed canonical commit in tracked state, or removes it after deletion.

A cursor already binding the bookmark to the selected canonical commit is
up-to-date and creates no journal batch. The command still repairs a stale
local remote-tracking bookmark from that cursor in the same recorded operation.
Creation pushes automatically track the new remote bookmark. Successful output
is one line per requested ref: creation, deletion and up-to-date results are
named directly; updates show the old and new short Git OIDs. Projection, pack
and Git plumbing output stays captured.

The journal decision remains authoritative if the local view transaction
fails. The command prints the successful push lines, exits successfully and
warns the operator to repair creations and updates with `ds git fetch`, or to
remove a landed deletion from the view with
`ds bookmark forget <bookmark> --include-remotes`.

## Git subprocess

Every non-empty batch uses:

- `git push --porcelain --no-verify --atomic`;
- one `--force-with-lease=<ref>:<expected-oid>` per ref, with an empty
  expectation for creation;
- unforced OID-to-ref refspecs, or deletion refspecs; and
- `LC_ALL=C` for stable porcelain parsing.

After every attempt, successful or not, `git ls-remote --refs` observes all
requested refs. An atomic-capability or remote-policy rejection fails the whole
batch. A lease rejection tells the operator to fetch the remote move before
retrying the push.

The subprocess wrapper retains one structured report entry for every requested
ref, including refs Git did not mention. If remote observation fails, the
journal batch remains pending because absence cannot be inferred from missing
output. If the push process fails but observation shows every proposed value,
the journal accepts the batch; if observation shows every expected value, an
unclaimed batch aborts. Mixed or otherwise ambiguous values remain
quarantined.

## Recovery

Before creating a new batch, the command checks for pending batches overlapping
the requested remote and bookmarks. It also refreshes this check when
`begin_push` loses a race to another pending owner.

The recovery machine claims the complete pending batch, reads its exact replay
payload and repeats the recorded multi-ref lease push. Active mappings plus the
replay's quarantined mappings rebuild missing Git objects in an empty sidecar.
If replay exposes a missing canonical object, the machine downloads and
installs the cloud pack catalog through the normal pack path, then re-exports.
Every rebuilt public head passes the hidden-path scan before Git contact.

The command observes the complete replayed ref set and calls `recover_push`
with the new fencing token. Only an accepted journal outcome completes
recovery. This lets a machine with a fresh native clone and no sidecar finish a
push after another machine moved the remote ref and stopped before finalising
the journal.

`ds doctor` surfaces this state. For each registered repository it reads the
projection journal and warns about every pending batch with its remote,
bookmark names and owner machine, noting that pushes to those bookmarks stay
blocked until a push recovers the batch. When the cloud is unreachable the
check degrades to a non-fatal warning.

## Credentials and diagnostics

HTTPS pushes inherit configured credential helpers. SSH pushes inherit the
user's SSH configuration and `SSH_AUTH_SOCK`. Foreground pushes may prompt.
Remote URLs never appear in arguments retained for diagnostics: the safe
command shape uses `<remote>`. Diagnostic stderr is bounded and removes lines
containing the remote URL, its authority or injected credential environment
values. Callers pass the registry URL only through the redacting `RemoteUrl`
wrapper and never format it themselves.

The journal and wire protocol carry 20-byte SHA-1 OIDs. SHA-256 remotes remain
unsupported because the journal does not yet carry an object-format field or
variable-length OIDs.

## Unsupported surface

Push options, tags, signing and SHA-256 remotes are outside the native Git
surface.
