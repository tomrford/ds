# Git push

Devspace pushes projected public commits from the rebuildable bare sidecar to
real Git remotes, journaled by the repository Durable Object with exact
expected-old-OID leases (see `git-projection.md`). This document specifies the
product push mechanism. The recoverable-push invariant and journal protocol
are already implemented and tested; the mechanism below replaces the
test-harness shell-out used by the live proof.

## Mechanism

Push and remote observation use a structured `git` subprocess. This matches
jj's own operational model at the pinned version: jj-lib performs the wire
push by executing `git push --porcelain --no-verify` with one exact
`--force-with-lease=<ref>:<expected>` argument per ref and unforced refspecs,
then parses per-ref porcelain results. Devspace follows that design directly
rather than calling jj-lib's API, which exposes neither atomic batches nor a
side-effect-free ref observation.

The vendored gix cannot push: the locked version has no send-pack or remote
push implementation. libgit2 would add a native dependency and a second
credential stack without a capability the subprocess lacks.

Rules:

- every push requests `--atomic`; the journal batches multiple refs and a
  partial update multiplies ambiguous recovery states. A server that does not
  advertise atomic push fails the whole command; Devspace does not silently
  retry without it
- each ref carries an exact lease: `--force-with-lease=<ref>:<expected-oid>`,
  with the zero OID for creation and a deletion refspec for removal
- refspecs are never force-prefixed; the leading `+` would bypass the lease
- output is parsed from `--porcelain` with `LC_ALL=C`; the lease rejection
  (`stale info`) is distinguished from remote policy rejections such as
  `non-fast-forward`
- after every attempt, successful or not, the complete requested ref set is
  observed with `git ls-remote --refs`; absent refs map to explicit absence

The journal decides from the complete post-attempt observation, never from
the process exit code: `projection_store.ts` accepts only an exact
all-proposed set, aborts an unclaimed all-expected set, and quarantines mixed
or ambiguous values. A transport or authentication failure can produce no
per-ref porcelain lines at all, so the report must distinguish not-reported
refs; if observation itself fails, the batch stays pending.

## Interface

```rust
struct LeaseUpdate {
    expected_old_oid: Option<GitOid>, // None means creation
    new_oid: Option<GitOid>,          // None means deletion
}

fn push(
    sidecar_git_dir: &Path,
    remote: &ResolvedRemote,
    updates: &BTreeMap<QualifiedRef, LeaseUpdate>,
    environment: &GitProcessEnvironment,
) -> Result<PushReport, PushError>;
```

`PushReport` carries one entry per input ref — status (updated, deleted,
up-to-date, lease-rejected, remote-rejected, other-rejected, not-reported)
plus the observed OID — and a redacted command diagnostic. Every input ref
appears in the report even when Git emits no line for it.

The adapter from the journal is direct: resolve the batch's remote identity
to a push URL, qualify each bookmark as `refs/heads/<bookmark>`, copy the
expected old OID, resolve the proposed state's Git OID or `None` for
deletion, push the map atomically, and submit the complete observation set to
the recovery route.

## Remote identity

The journal stores a remote identity, not a URL. The repository Durable
Object owns a remote registry mapping that identity to a fetch/push URL, so a
fresh machine running recovery can resolve `origin` without inheriting
another machine's git configuration. Changing a remote's URL clears that
remote's private projection states, cursors and pending journal before new
history is fetched. Credentials never live in the registry.

## Credentials

Authentication stays inside Git's established credential paths; tokens never
appear in URLs, arguments or logs.

- HTTPS: inherit configured credential helpers; when Devspace owns a token,
  pass a scoped `GIT_ASKPASS` through the per-command environment. Background
  recovery sets `GIT_TERMINAL_PROMPT=0` so a replay can never hang on a
  prompt; a foreground command may present an intentional askpass UI
- SSH: inherit the user's SSH config and `SSH_AUTH_SOCK`; a Devspace-managed
  key or host policy uses a scoped `GIT_SSH_COMMAND`. Background recovery
  fails and stays pending rather than waiting for input

The subprocess wrapper accepts an explicit git executable path and a
per-command environment map, mirroring jj-lib's subprocess options. Remote
URLs, sideband progress and stderr are redacted before logging; servers and
credential helpers can emit sensitive text.

## Object format

The journal and wire protocol carry 20-byte SHA-1 OIDs. A SHA-256 remote is
rejected explicitly at remote registration; supporting it requires an
object-format field and variable-length OIDs in the journal protocol first.

## Open items

- The remote registry routes and schema in the repository Durable Object.
- Bookmark-name validation is centralized where bookmarks are qualified into
  `refs/heads/` refs; the journal stays branch-only.
- Push options, tags and signing are not part of the native Git surface.
