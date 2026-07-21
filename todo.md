# Open items

- `ds sync run` is the one ds verb that still prints jj's
  "Cannot define an alias that overrides the built-in command" warning
  (reproducible from any cwd, daemon running or not). The config-migration
  suppression covers every other verb; find which config-load path sync run
  takes that skips it and close the gap.

- Default branch per repo: `ds add` requires an explicit `-r`. jj has no
  stored default-branch concept (`trunk()` just guesses main/master/trunk),
  but import already reads the remote HEAD symref — persist it as a
  committed `devspace.toml` default (overridable there), seed `main` for
  `ds repo new`, and let `ds add` fall back to it when `-r` is omitted.
- `ds git push` races the boundary sync its own preceding commands spawn:
  the per-repo sync lock makes push error "already being synchronized;
  retry" seconds after any ds command. Push (and fetch) should wait
  briefly on the lock instead of failing — bare retry advice is a paper
  cut in scripted/agent flows.
- Machine enrolment endgame: devices become first-class server citizens —
  gh-style device-flow handshake registers the CLI, server issues the
  machine identity (replacing client-generated machine_id) and holds the
  display name. Current design threads a principal type end-to-end so this
  is a swap, not a rework. Pairs with retiring the shared secret. Until it
  lands, machine `config.toml` is written by hand at machine-add time — no
  interim `ds setup` verb (ruled: login replaces it, don't build the stopgap).
- Signed public history: canonical commits can carry jj signatures today
  (byte-exact sync), but projected public Git commits go out unsigned.
  Projection happens locally at export, so running the configured jj
  signing backend over freshly built public commits is feasible — design
  how signing config maps across the boundary and sign at projection time.

- Release binaries built inside `nix develop` link libiconv from the build
  machine's nix store and fail on hosts without it. Fixed today by
  `install_name_tool -change ... /usr/lib/libiconv.2.dylib` + ad-hoc codesign;
  a real distribution story (static iconv, nix-built portable bundle, or an
  OCI/server image for self-host) is a checkpoint-8 decision.

- `ds remove` refuses a checkout that was moved with `mv`: the marker's
  workspace name no longer matches the digest of the new path, and the error
  reports "not a Devspace checkout" without naming the mismatch. Either support
  removal at the moved path or produce an error that explains the move and
  points at the original path.
- `ds add` against an existing checkout whose working-copy commit has advanced
  past the requested base errors with "pass the matching revision" instead of
  reporting that the checkout already exists. Idempotent ensure-checkout
  scripts (`ds add repo -r main path` re-run after edits) hit this; decide
  whether an owned, registered destination should short-circuit to
  already-exists before the revision comparison.
- `ds git push` reports "up to date" from the journal cursor without observing
  the remote, so an externally moved ref goes unnoticed until a real push or
  fetch. Revisit when fetch lands.
- Remote repoint racing an in-flight push OR fetch: the journal is cleared but
  the in-flight machine is not fenced; a push can hit the old URL or lose its
  batch, and a fetch begun against the old URL can be recorded as history from
  the new registration (cursor-less refs especially). Remote-generation
  binding validated transactionally fixes both; production hardening.
- `devspace.toml` (committed, public): run-on-add commands (`pnpm install`)
  and similar repo conventions. Ruling: config lifecycles stay separate —
  committed `devspace.toml` (public conventions), `.repos`/`.lock` (context,
  public), `.dsprivate` (devspace-shared, Git-hidden), machine store
  (user-local). Auto-executing hooks from repo content is the
  agent-plants-a-hook threat; execution needs confirmation or server-side
  arming, design open.
- Multiuser private boundary: `.dsprivate` currently assumes one devspace is
  one user — every devspace collaborator sees all private content. Multiuser
  needs a third boundary between public and private (share some secrets with
  some collaborators, not all). Design item, unscheduled.
- Auto-track discovery walk never sees jj's base ignores (global
  `core.excludesFile`): a globally-ignored directory is descended by our walk
  but pruned by jj's, so hidden matches inside it silently diverge from the
  documented rule. Plumb the same base ignores jj-cli feeds SnapshotOptions
  into `discover_hidden_paths` and pin a golden test.
- Worker version gating: clients now send `x-devspace-client`
  (`ds/<version> encoding/<epoch>`); the Worker ignores it. When enrolment or
  the first encoding bump lands, gate stale epochs with an "upgrade ds" error
  (see AGENTS.md jj bump rollout).
