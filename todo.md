# Open items

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
- Remote repoint racing an in-flight push: the journal is cleared but the
  in-flight machine is not fenced; it can push the old URL or lose its batch
  mid-flight. Failure is loud and the new registration stays consistent, but
  proper remote-generation fencing belongs in production hardening.
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
