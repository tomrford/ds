# Design memo: signing public Git commits at projection time

## Recommendation

Sign once at first export and make the signature part of the recorded projection (option a). Persist the signature bytes inside the public shadow commit, which is already cloud-durable before any journal batch exists. Every later re-derivation — sidecar rebuild, second-machine replay, recovery — replays the stored bytes through a `SigningFn` instead of signing again. This keeps the projection a deterministic function of its recorded inputs, which is the invariant the whole receipt model stands on.

Deterministic signing (option b) is feasible only for SSH ed25519 keys and could not replace stored-byte replay anyway. Signing "on the wire" (option c) does not exist: `git push` transfers objects verbatim, so it collapses to option a. Not signing (option d) is the current, documented state.

## 1. How the projection writes public Git commits today

The projection sidecar is a bare Git repository managed through jj-lib's `GitBackend`, which writes objects with gix:

- `crates/machine/src/git_projection.rs:270` — `GitBackend::init_internal(settings, &store_path)` creates the sidecar store; `GitProjection::from_backend` wraps it in a jj `Store` and already builds a `Signer::from_settings(settings)` at line 288, currently unused for writes.
- `crates/machine/src/git_projection.rs:624-635` — the translation walk builds each public commit as a `BackendCommit` with `secure_sig: None` (line 633) and writes it with `target.write_commit(target_commit, None)` (line 635). The `None` is the `sign_with: Option<&mut SigningFn>` parameter.
- jj-lib 0.42 `git_backend.rs:1252-1258` (`write_commit`) asserts `contents.secure_sig.is_none()` — you cannot pass a pre-made signature through the commit struct. At lines 1350-1360, if `sign_with` is `Some`, it serialises the unsigned commit, calls the function, and appends the result as the `gpgsig` extra header before gix writes the object. So the signature attaches exactly where Git expects it, and the only hook is the `SigningFn` closure: `pub type SigningFn<'a> = dyn FnMut(&[u8]) -> SignResult<Vec<u8>> + Send + 'a` (jj-lib `backend.rs:182`).

The sidecar is disposable by design: `crates/cli/src/git/projection_sidecar.rs:16-24` deletes and rebuilds an invalid sidecar, and `docs/git-projection.md` states "A missing sidecar can therefore be recreated from the native objects and accepted cloud mappings". Cloud packs carry native objects only (`crates/machine/src/object_closure.rs`, `pack_manifest.rs` — kernel `ObjectKind`s). No durable store holds exported Git bytes today. Rebuild works only because projection is deterministic.

## 2. What jj-lib 0.42 gives us

The signing stack is fully reusable and already half-wired in:

- `jj_lib::signing::Signer` wraps `SigningBackend` implementations (gpg, gpgsm, ssh, plus a test backend). `Signer::sign(data, key)` is a plain pass-through to the configured backend (`signing.rs:222-229`) — it takes arbitrary bytes and needs no jj-store commit. We can call it directly with projected commit bytes.
- Config keys (parsed by `UserSettings`, which devspace already builds): `signing.backend` (`none`/`gpg`/`gpgsm`/`ssh`), `signing.key`, `signing.behavior` (`drop`/`keep`/`own`/`force`), `signing.backends.gpg.program`, `signing.backends.gpg.allow-expired-keys`, `signing.backends.ssh.program`, `signing.backends.ssh.allowed-signers`, `signing.backends.ssh.revocation-list`.
- Stock jj sign-on-push: jj-cli 0.42 `commands/git/push.rs:547` checks `git.sign-on-push` (default false in `config/misc.toml:30`), then `sign_commits_before_push` (lines 803-840) rewrites the unsigned commits in the jj store with `SignBehavior::Own` and rebases descendants. It signs by mutating canonical history before export.
- The actual signing call site is `commit_builder.rs:456-465`: `let sign_fn = |data| store.signer().sign(data, sign_settings.key.as_deref())`, passed to `write_commit`.

Devspace should reuse the `Signer`, the config keys and the `sign_fn` shape, but not jj's rewrite-then-push behaviour. Devspace's canonical store is not Git-backed; the natural place to sign is the projected public commit, which leaves canonical history untouched — no rebase churn, no `SignBehavior` interaction. `SignBehavior` and `jj sign` stay about canonical jj signatures, which are a separate feature (byte-exact replicated; kernel `proto.rs:66`).

## 3. Determinism: what breaks if projected bytes vary per run

GPG signatures are always nondeterministic: OpenPGP embeds the signature creation time in the hashed subpackets, so `gpg -abu <key>` (`gpg_signing.rs:213-219`) gives different bytes each run. SSH signatures (`ssh-keygen -Y sign -n git`, `ssh_signing.rs:221-248`) have no timestamp in the SSHSIG format, so they are deterministic for ed25519 and RSA keys, but not for ECDSA or FIDO `-sk` keys (per-signature nonce or counter).

If a re-projection can produce different bytes than the first one, these invariants fail:

- Receipt-checked re-export fails closed. Export seeds `ExportMappings` from accepted journal rows. `TranslationStops::contains` (`git_projection.rs:536-561`) reuses a mapping only if the Git object is readable in the sidecar; after a sidecar rebuild it is not, so the walk re-derives the commit and `record_mapping` (`git_projection.rs:109-128`) raises `ConflictingMapping` when the new id differs from the receipt. Every push from that machine on that bookmark is then wedged.
- Recovery replay fails closed, permanently. `rebuild_replay_heads` (`crates/cli/src/git/push.rs:589-644`) re-runs `export_reachable` and requires `exported.git_heads == expected_git_head` (lines 621-628). Replay is re-derivation, not stored-byte playback. A pending batch whose head cannot be re-derived byte-identically can never be recovered — and `push_with_cloud` (lines 136-142) refuses to start new work on that bookmark while the batch is pending. One crash after `begin_push` bricks the bookmark for the whole fleet.
- Second-machine recovery has the same failure with fewer local resources. `recover_pending_batch` (push.rs:538-548) handles `ObjectNotFound` by downloading all cloud packs and re-exporting — packs restore canonical objects and shadows, but nothing restores the signature, so machine B cannot reproduce machine A's bytes.
- Public history diverges across bookmarks and machines. Export seeding filters mappings per remote and bookmark (push.rs:307-311). Today determinism makes independent exports of the same canonical commit converge on one `git_oid`. With per-run signatures, two fresh exports mint two different Git commits for one canonical commit. Receipts are keyed by `git_oid` (`docs/git-projection.md`: "Git receipts are repository-wide and immutable: one Git commit ID cannot later name a different public shadow"), so both would be accepted — silent divergence, not a 409.
- Fetch seed matching stays sound but only for bytes that never need re-derivation. Seed selection matches exact `git_oid`s from receipts (`docs/git-fetch.md`, seed selection). Those ids are stable once recorded; the failure mode is upstream, in producing them again.

The hidden-path scan is unaffected — signatures do not touch trees.

One extra hazard inside jj-lib: `write_commit`'s change-id collision loop (`git_backend.rs:1372-1392`) decrements the committer timestamp until the extras table agrees, which both changes the bytes being signed and re-invokes the signing function. This is a pre-existing determinism edge (it needs two canonical commits with identical projected content but different change ids) and it fails closed today. Note it; do not design around it yet.

## 4. Evaluating the options

### Option a — sign once, persist the signed bytes as the projection (recommended)

The pack and sidecar do not already persist exported Git objects durably, so the signature needs a durable home. Two candidates:

1. Journal `ProjectionState` rows. Rejected: a GPG signature is around 1 KiB, the journal caps a request at 4 MiB with up to 8,192 states (`docs/git-projection.md`, budgets), so signatures could alone exceed the budget, and it needs Worker schema and Wasm validation changes.
2. The public shadow commit. Recommended. The shadow is a native commit created by importing the projected Git commit (push.rs:346-354). It is uploaded to cloud packs before `begin_push` — the durability gate requires "every private and public commit is already in the cloud object store" (`docs/git-projection.md`) — and the receipt immutably binds `git_oid` to `public_commit_id`. Recovery already downloads packs on `ObjectNotFound` (push.rs:538-548). So the byte authority devspace needs already has durability, immutability binding and a recovery download path; it just does not carry the signature yet.

Two code facts block the naive version and shape the design:

- The kernel refuses foreign payloads in `secure_sig`: `crates/kernel/src/backend.rs:340-346` requires the signed payload to equal the commit's canonical proto encoding. A `gpgsig` signs Git bytes, not proto bytes, so it cannot ride in `secure_sig`. Add a separate optional proto field (say `git_sig: Option<Vec<u8>>`) on the kernel commit (`crates/kernel/src/proto.rs`), included in canonical encoding so the shadow id binds the signature bytes.
- Import currently strips signatures: the translation builds every target commit with `secure_sig: None` (`git_projection.rs:633`) and `docs/git-fetch.md` documents "Signatures, which cannot survive translation, are stripped". Change the import direction to copy the Git commit's `gpgsig` into the shadow's `git_sig`. Import stays deterministic (the signature is part of the input Git bytes), and the shadow becomes self-sufficient for byte replay.

Export then passes a per-commit `SigningFn` at `git_projection.rs:635`:

- if the canonical commit has a recorded shadow (receipt row) and that shadow carries `git_sig`, return the stored bytes — deterministic replay
- otherwise, if signing is enabled, call `store.signer().sign(data, key)` — first export, fresh signature
- otherwise pass `None` — unsigned, as today

Correctness is self-checking: replayed bytes must hash to the receipt's `git_oid`, and the existing equality checks (`record_mapping`, `rebuild_replay_heads`) enforce exactly that.

History is not rewritten. Export stops at receipt-mapped commits that exist in the sidecar, so enabling signing signs only commits exported after the switch; old receipts and oids stand.

### Option b — deterministic signing

Only SSH ed25519 (and RSA) qualify; GPG never does, and ECDSA and `-sk` SSH keys do not. Even in the best case, the private key becomes an input to the projection function: every machine must hold the same key, and rotating it makes historical re-derivation impossible — the same wedge as option a without stored bytes. Verdict: acceptable as a hardening constraint ("prefer ed25519") but not a substitute for stored-byte replay. Do not build the design on it.

### Option c — sign at the push boundary

There is no such boundary. The signature lives in the commit object; `git push` moves objects verbatim from the sidecar (`observed_push` runs `devspace_machine::push` against `projection.git_repo_path()`, push.rs:699-721). This collapses to option a.

### Option d — do not sign

The current state. `docs/git-push.md` already lists signing as outside the native surface. GitHub's "verified" badge requires a signature in the object (or commits created through the web UI or API); there is no way to mark plain pushed commits verified after the fact. If the owner wants signed public history, option a is the only sound route.

## 5. Multi-machine: first exporter wins, everyone else replays

This is already the model — with one gap. The docs say a claimant can "download the already durable native objects, rebuild the exact Git objects and perform that push" (`docs/git-projection.md`, cloud journal), and the code enforces exactness:

```rust
// crates/cli/src/git/push.rs:621
if exported.git_heads.as_slice() != std::slice::from_ref(&expected_git_head) {
    return Err(devspace_machine::ProjectionError::ConflictingMapping { ... });
}
```

But "rebuild the exact Git objects" today means re-derive, relying on determinism. With signatures, replay must come from stored bytes. The shadow-carried signature closes the gap: machine B downloads packs (which include shadows), the `SigningFn` replays A's signature, and the oid check passes without B holding any key.

A fresh export of the same canonical commit on B would mint a different signature and oid. Two changes keep the fleet convergent:

- widen export seeding from per-bookmark mapping rows (push.rs:307-311) to all receipts for the repository, so any already-projected canonical commit replays its recorded signed bytes on every bookmark and machine
- in the normal export path, mirror the recovery fallback: on `ObjectNotFound` for a referenced shadow, download packs before failing (today only `recover_pending_batch` does this)

Same-bookmark races are already serialised by cursors, leases and pending-batch locking, and the journal's receipt immutability rejects any attempt to rebind a `git_oid` to a different shadow (`git-receipt-conflict`).

## 6. Config surface

Reuse jj's `signing.*` table wholesale — the sidecar store already builds its `Signer` from the same `UserSettings` (`git_projection.rs:288`), so `signing.backend`, `signing.key` and `signing.backends.*` work unchanged. Add one devspace gate rather than reusing `git.sign-on-push`, whose stock semantics (rewrite canonical commits) devspace deliberately does not follow: for example `git.sign-exports = true` (machine-local). `signing.behavior` stays scoped to canonical jj signatures.

Per-remote signing is not meaningful under the receipt model: receipts are repository-wide, so a commit's public bytes are fixed at first export regardless of remote. A per-repository cloud policy flag (require signed exports) is possible later; it belongs in the remote registry or repository record, not the machine.

## 7. Implementation sites

1. `crates/kernel/src/proto.rs` and `backend.rs` — optional `git_sig` field on the commit proto, included in canonical encoding, exempt from the `secure_sig` canonical-payload check.
2. `crates/machine/src/git_projection.rs` — export arm: per-commit `SigningFn` (replay from shadow `git_sig`, else fresh sign via the sidecar store's `Signer`); import arm: copy `gpgsig` from the source Git commit into the shadow's `git_sig`. Both directions need the canonical-to-shadow lookup threaded in (a callback or a mapping parameter).
3. `crates/cli/src/git/push.rs` — thread the shadow lookup into `prepare_updates` and `rebuild_replay_heads`; widen export seeding to repository-wide receipts; add the pack-download fallback to the normal export path.
4. `crates/cli/src/git/fetch.rs` — no seed or journal changes; imported foreign signatures now flow into shadows via the shared translation change.
5. Worker — no schema change; states and receipts are untouched. Confirm pack chunking absorbs the slightly larger shadow commits.
6. Docs — `git-push.md` (remove signing from the unsupported list, describe the gate), `git-projection.md` (the determinism contract becomes "deterministic given recorded signatures"), `git-fetch.md` (signatures preserved in shadows, no longer stripped).

## 8. Open questions

- Change-id collision loop: `write_commit` mutates the committer timestamp and re-signs on extras collisions (`git_backend.rs:1372-1392`), which invalidates a replayed signature. Rare and fail-closed; decide whether to detect and error explicitly.
- Mixed fleets: a keyless machine can replay and recover signed batches (no key needed — a genuinely good property of this design), but its own new exports are unsigned, interleaving signed and unsigned public history. Is that acceptable, or should a repository-level policy refuse unsigned exports?
- Foreign fetched commits: preserving their `gpgsig` in shadows changes shadow ids versus today (pre-release, no migration needed) and improves round-trip fidelity, but full byte reproduction of arbitrary foreign commits (other extra headers, encodings) stays out of scope — sidecar rebuild still relies on re-fetching foreign objects from the remote. Worth stating in `git-projection.md`.
- Key rotation: old signatures replay from shadows, so rotation is safe for history; only fresh exports use the new key. Confirm no code path ever verifies "signed by the current key".
- Journal budgets: shadows grow by roughly signature size; states and receipts do not. Re-measure the 4 MiB request budget assumptions for pack upload paths only.

## Addendum (Tom's review): option (e) — remote-as-persistence, now the leading option

Git and jj never face the re-sign problem because the signed object is the
persisted authority from birth. Apply the same principle without touching
the kernel schema: the git remote + journal ARE the persistence.
- Incremental export needs parent OIDs (receipts/journal), never parent
  signed bytes; trees are unsigned and deterministic.
- Recovery: remote received the push → fetch the signed objects back
  instead of re-deriving; remote didn't → journal aborts, fresh push is a
  legitimate new first-sign (new objects in effect).
- Racing exporters: the bookmark lease picks a winner; the loser's
  fetch-before-retry must ADOPT the winner's mapping (accepted-mappings
  stop rule) and discard its own divergent local mapping — needs explicit
  design + test.
- Cost: push recovery gains a remote-reachability dependency (inherent to
  pushing); fresh-machine replay can no longer run from cloud packs alone.
- Wins: no git_sig kernel field, no schema-doctrine amendment, no import
  preservation change; canonical store stays exactly jj's schema.
