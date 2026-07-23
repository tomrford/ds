# Night report — 2026-07-23

Status at hand-back: **all three swap spikes complete and green; stopped at
the integration boundary** per your standing orders, because integration
opens design decisions that are yours to make. Nothing diverged from
`docs/swap-spikes.md`.

## Shipped to `main` before the spikes (709274f)

`ds git push` jj-parity (remote-tracking bookmark moves with the bookmark in
one recorded op, auto-track, `--deleted` with tracked-only selection,
untracked-overwrite refusal, stock op descriptions), doctor pending-batch
surfacing, `MissingMappedObject` strict/replay export modes, repo-wide
export seeding. Two BLOCK-verdict adversarial reviews fully resolved.
Dogfood-verified by pushing itself. The self-dogfooding devspace is torn
down; this working copy is a plain git clone. `mint`, `cantraceviewer`,
`unum` remain for your push-and-delete pass.

## Spike verdicts (branch `spike/git-backend`, one proof commit each)

1. **Kernel** — `crates/kernel-git` + `kernel-git-wasm`: no_std panic-free
   parsers, `sha1-checked` identity; 33 oracle vectors from a real repo,
   zero parser rejections over 156 objects, jj-lib agrees on every jj
   header; 24,778 mutation cases; native/wasm parity on all 40 vectors;
   **wasm = 71,783 bytes, zero imports** — under half the 200 KiB gate.
2. **Machine store + cloud** — `crates/machine-git` + `RepositoryGit` DO:
   closure discovery proven against `git rev-list`; deterministic DSPK v2
   packs; kernel-validated no-clobber installs; **extras-free rebuild
   proven** (change IDs, conflicts, labels all reconstruct with `store/extra`
   deleted — the "extras are a cache" foundation holds); live wrangler
   round trip incl. fresh-machine rebuild and interrupted-upload retry.
3. **Projection + journal + push/fetch** — git→git projection with
   **identity fast path proven at byte level** (hidden-free cones: zero
   objects, zero rows, signatures/mergetags untouched); minimal rewrite
   set; public commits are first-class canonical objects (pack/install
   unmodified); pair-shaped journal (`canonical-oid-diverged` enforced one
   way, public OIDs deliberately many-to-one the other, fetch-receipt
   arrays obsoleted); live proofs all PASS: hidden-safe push (every remote
   blob sentinel-scanned), **byte-identical signed push end-to-end**,
   **fresh-machine crash recovery from cloud packs alone** (failpoint after
   git push; second machine claims, rebuilds, recovers — no remote
   fetch-back), fetch with canonical-parent graft.

The three properties the swap was sold on — identity projection for foreign
history, custody dissolution, signatures for free — are now demonstrated,
not argued.

## Decisions before integration starts (in rough dependency order)

1. **Op-store sync shape.** Ops/views are still proto + Blake2; the old
   kernel validates them and the old sync engine ships them. Options:
   (a) keep the old kernel permanently, narrowed to ops/views only, old
   sync path intact — least work, two kernels forever; (b) port op-store
   sync onto the v2 surface (new op tables in `RepositoryGit`, old
   Repository DO deleted entirely) — one DO, one sync engine, more
   integration work. My lean: (b), it is the honest clean cut and the op
   protocol is small.
2. **Lift depth for fetch.** The spike proved the canonical-parent graft.
   The old `git_lift` three-way tree-lift/tombstone machinery covers
   richer cases (remote moves atop rewritten history with hidden edits in
   flight). Decide whether parent-graft is the v2 contract (simpler, my
   lean, revisit if dogfooding hurts) or the full lift ports.
3. **Worker wipe sequencing.** Blocked on your push-and-delete of the
   other three devspaces. After that: wipe, deploy v2-only, re-init this
   repo as the first new-world devspace and resume dogfooding.
4. **Branch handling.** `spike/git-backend` is 10 commits of proofs on top
   of main. Squash-merge per your usual preference, or keep the proof
   commits? (They mirror the original v3 spike history, which you kept.)
5. **Semi-legacy sweep scope** (your pre-bed item). Confirmed candidates
   found tonight: sign-on-export memo + design docs (custody machinery
   obsolete — signing now happens on canonical commits, rewritten-commit
   signing is the only residual question), `git_lift`/sidecar/subprocess
   modules, the git shim's fabrication layer (v2 serves reads from the
   real odb behind the same read-only fence), the stale bundle-size figure
   in `git-projection.md`, and the `MissingMappedObject`/hydration
   machinery shipped tonight in 709274f (dies with the old projection —
   it earned its keep for the interim dogfooding window regardless).

## Integration work list (mechanical once 1-2 are decided)

CLI dispatch onto `MachineGitRepository` (sync lock, bookmark selection,
view transactions, output, failpoints — the Phase 1 semantics tests carry
over as the acceptance gate); delete superseded SimpleBackend projection,
lift, sidecar, subprocess, and v1 worker surfaces; wipe + redeploy worker;
true up README, kernel/git-push/git-fetch/git-projection/hidden docs.
