> Archived probe report (2026-07-15). Conclusions are folded into the product
> specs and subsystem docs; this file is evidence, not current guidance.

# Product-shaped Git lease push probe

## Recommendation

Use a structured Git subprocess for both push and remote-ref observation.
Make `--atomic` part of the product contract. Pass one exact
`--force-with-lease=<ref>:<expected>` argument per ref, use unforced refspecs,
parse `git push --porcelain`, then observe the complete requested ref set with
`git ls-remote --refs` after every push attempt.

This matches jj 0.42's operational model. jj-lib does not implement the wire
push itself. Its private runner executes `git push --porcelain --no-verify`,
adds one exact force-with-lease argument per ref and deliberately removes the
leading `+` from each refspec because that would bypass the lease. It parses
each porcelain line into pushed, rejected and remote-rejected refs.

The probe adds 2 requirements which jj-lib's public API does not provide:

- atomic multi-ref batches
- a complete, side-effect-free observation of the requested remote refs

The implementation is in
`crates/machine/src/projection_push_probe.rs`. The normal integration test is
`crates/machine/tests/projection_push_probe.rs`. The ignored live recovery test
also uses the probe function in place of its raw push and `rev-parse` calls.

## Candidate assessment

| Candidate | Finding | Decision |
| --- | --- | --- |
| jj-lib 0.42 | `jj_lib::git::push_updates()` is public and provides exact expected-old targets and per-ref results. It requires a full jj `Repo` with a Git backend and a configured named remote. Its public options expose remote push options only. The underlying command does not request `--atomic`. jj-lib has no pure ls-refs API; its fetch path also shells out to Git and updates local state. | Follow its subprocess design, but do not use this API as the product boundary. |
| gix 0.84 | The locked `gix::push` module only models `push.default`. There is no send-pack or remote push implementation. The remote connection implementation contains fetch and ref-map support only. The locked jj feature graph also does not enable a gix network client. gix can model and fetch remote refs when built with networking, but it cannot perform this push. | Not usable for the complete mechanism. Do not split push and observation across 2 credential stacks. |
| Git subprocess | Git 2.54.0 in the Nix shell provides exact leases, porcelain per-ref results, deletion refspecs, atomic push and ls-remote observation. The fixture proves the required outcomes. | Use this mechanism. |

Adding `git2` and libgit2 would add a new Rust and native dependency plus a
second credential implementation. This probe did not test it. There is no
reason to choose it without a separate proof of exact expected-old leases,
atomic capability handling and credential parity.

## Proven behaviour

The fixture uses a bare sidecar and a separate bare remote. It proves:

- creation succeeds with an empty expected value and reports each ref as
  updated
- moving `refs/heads/main` before a 2-ref push makes that ref report
  `LeaseRejected`
- `--atomic` makes the companion ref report `atomic push failed` and leaves it
  absent
- a diverged update succeeds when the expected old OID still matches, because
  the lease permits the forced non-fast-forward update
- setting `receive.denyNonFastForwards=true` produces a per-ref remote rejection
  with reason `non-fast-forward`, not a lease rejection
- a deletion-only batch succeeds with an exact lease and observes the ref as
  absent
- `git ls-remote --refs` returns the complete requested map when missing refs
  are filled with `None`

The process exit code remains useful as a command diagnostic. It is not the
journal result. The product must make protocol decisions from the complete
post-attempt observation because `projection_store.ts` accepts only an exact
all-proposed set, aborts an unclaimed all-expected set and quarantines mixed or
ambiguous values.

The probe sets `LC_ALL=C` and distinguishes Git's `stale info` rejection from
other porcelain failures. That classification helps diagnostics and tests.
Recovery must still use observed OIDs, so it does not trust human-readable
reason text.

## Atomic batches and partial failure

The product should always request `--atomic`. A server which does not advertise
atomic push support makes the whole command fail. The module must then observe
all requested refs and leave the journal pending unless the observation itself
settles the batch.

Do not silently retry without `--atomic`. Without it, Git can update some refs
and reject others. The current journal handles that safely by quarantining a
mixed observation. A later exact replay can still advance refs which remain at
their expected values, while refs already at their proposed values may report
stale leases. The complete observation can eventually accept an all-proposed
set. This is recoverable but creates more ambiguous intermediate states and can
remain pending when one ref has a permanent policy rejection.

Porcelain output is per-ref for ordinary lease and receive-pack failures. A
transport, authentication or unsupported-capability failure may produce no
per-ref lines. The interface therefore needs a `NotReported` state plus the
complete observed ref map. If observation also fails, the caller cannot submit
recovery and must retain the pending batch.

## Credentials

Keep authentication inside Git's established credential paths. Do not put a
token in the remote URL, command arguments or structured logs.

For HTTPS:

- inherit configured `credential.helper` support for existing user sessions
- pass a scoped `GIT_ASKPASS` environment when Devspace owns a token; the token
  is normally supplied as Git's password and the provider-specific username is
  supplied separately
- set `GIT_TERMINAL_PROMPT=0` for background recovery so a replay cannot hang
- allow a foreground command to use an intentional askpass UI instead of a
  terminal prompt

For SSH:

- inherit the user's SSH config and `SSH_AUTH_SOCK` so ssh-agent works
- allow scoped `GIT_SSH_COMMAND` or `core.sshCommand` configuration for a
  Devspace-managed key or host policy
- use `SSH_ASKPASS` only when an intentional UI is available; background
  recovery must fail and remain pending instead of waiting for input

The process wrapper should accept an explicit executable path and a per-command
environment map. This mirrors jj-lib's `GitSubprocessOptions`, whose environment
field exists for values such as `GIT_ASKPASS` without process-wide mutation.
Redact remote URLs and sideband or stderr content before logging because remote
helpers and servers can emit sensitive text.

## Product interface

The product boundary should be equivalent to:

```rust
struct LeaseUpdate {
    expected_old_oid: Option<[u8; 20]>,
    new_oid: Option<[u8; 20]>, // None means deletion
}

fn push(
    sidecar_git_dir: &Path,
    remote: &ResolvedRemote,
    updates: &BTreeMap<QualifiedRef, LeaseUpdate>,
    credentials: &GitProcessEnvironment,
) -> Result<PushReport, PushError>;

struct PushReport {
    process_succeeded: bool,
    refs: BTreeMap<QualifiedRef, RefReport>,
    command_diagnostic: Option<RedactedDiagnostic>,
}

struct RefReport {
    push_status: PushStatus,
    observed_oid: Option<[u8; 20]>,
}
```

`PushStatus` needs updated, deleted, up-to-date, lease-rejected,
remote-rejected, other-rejected and not-reported cases. The report must contain
every input ref even when Git emits no porcelain line.

The adapter from the journal is direct:

- resolve `ProjectionReplay.remote` to a push URL or configured remote
- qualify each bookmark as `refs/heads/<bookmark>`
- copy `expected_old_oid`
- resolve `states[proposed_state].git_oid`, or use `None` for deletion
- push the whole map atomically
- map every observed qualified ref back to a `ProjectionObservation`
- submit the complete observation set to `recover_push`

There are 3 protocol mismatches to settle before production:

1. The journal stores a remote string such as `origin`. A fresh rebuilt
   sidecar may not have that named remote or its credentials. Product config
   needs a durable remote identity-to-push-URL resolver available to every
   recovery machine.
2. The journal stores bookmarks, while Git operates on qualified refs. Keep
   the journal branch-only for now and validate the qualification centrally.
3. The journal and probe use 20-byte SHA-1 OIDs. A SHA-256 remote must be
   rejected explicitly or the protocol must gain an object-format field and
   variable-length OIDs.

## Run the proof

Run the focused, non-ignored fixture test with:

```sh
nix develop -c cargo test -p devspace-machine \
  --test projection_push_probe -- --nocapture
```

Run the requested package gate with:

```sh
nix develop -c cargo test -p devspace-machine
```

`projection_live` remains ignored because it requires a Worker or deployment.
It now compiles against and calls the probe adapter. Run it with the Worker
environment documented in `docs/spike-3.md` when that service is available.
