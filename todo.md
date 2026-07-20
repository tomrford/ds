# Open items

- No `ds setup`/`ds login` verb: machine `config.json` (platform data dir,
  0600, `{version, base_url, machine_id, shared_secret}`) is written by hand.
  Needs a verb that generates the machine id, validates the URL/secret against
  the Worker, and writes the file atomically.
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
- `ds context` (grepo parity): external-context management inside ds-managed
  repos; v1 has it, v3 does not. Keeps grepo's existing committed `.repos` /
  `.lock` files and syntax; not folded into any devspace config file.
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
- `ds skill` surface (v1 parity): agent-facing usage docs incl. the private-file
  model, once docs stabilise. Needed before T3 dogfooding.
- Import depth limit (MAX_IMPORT_COMMIT_DEPTH 1,024) blocks `ds init` of
  real histories; raising it needs paged/streaming import so one fetch
  transaction stays bounded. Decide before dogfooding a repo deeper than the
  limit (this repo qualifies).
- Projection idempotency journals grow forever: `projection_fetch_results`
  (one row per fetch) and `projection_batch_results` are never pruned. Port
  the head-store retention + quota pattern (the `*_at_ms` columns exist for
  it). Do early in checkpoint 8, before dogfood data accumulates.
- `projection_states` scaling: no index on `(remote, bookmark)` or
  `pending_batch_id`, and `requireUnambiguousFetchLineage` re-runs a
  full-table `GROUP BY` per fetched OID — hoist the CTE once per request and
  add the two indexes. Pair with the retention work.
- Lift full-tree scans: `apply_pollution_tombstones` and
  `hidden_conflict_paths` walk every tree entry per lifted commit; drive both
  from the public base→tree `MergedTree` diff stream instead. Pair with the
  import-depth raise — same dogfood gate.
- Auto-track discovery walk never sees jj's base ignores (global
  `core.excludesFile`): a globally-ignored directory is descended by our walk
  but pruned by jj's, so hidden matches inside it silently diverge from the
  documented rule. Plumb the same base ignores jj-cli feeds SnapshotOptions
  into `discover_hidden_paths` and pin a golden test.
- Test harness dedup (~1.1k LOC): `settings()` defined 24x, the fake TCP
  Worker written 4x, live-test helpers duplicated between `sync.rs` and
  `sync_live.rs` (move the two `#[ignore]`d live tests there too).
  Opportunistic; zero coverage change.
- Worker version gating: clients now send `x-devspace-client`
  (`ds/<version> encoding/<epoch>`); the Worker ignores it. When enrolment or
  the first encoding bump lands, gate stale epochs with an "upgrade ds" error
  (see AGENTS.md jj bump rollout).
