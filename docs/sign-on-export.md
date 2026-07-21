# Design: sign-on-export option (e) — remote-as-persistence

Companion to `sign-on-export-memo.md`. Options (a)–(d) are settled there; this
doc completes option (e). All file:line references are against the current
tree (master, pre-release; no migration concerns).

## 0. The invariant shift

Option (a) kept "the projection is a deterministic function of recorded
inputs" by storing signature bytes in the kernel. Option (e) drops that
invariant for commit objects and replaces it with two weaker ones that the
existing journal already almost enforces:

- **E1 — OIDs are decisions, bytes are on the remote.** The journal's
  `projection_states` and `git_receipts` rows record which Git OID a canonical
  commit projected to. The signed bytes behind that OID are persisted by the
  Git remote that received the push, and nowhere else. Devspace never
  re-derives a mapped commit's bytes; it either has them (sidecar), fetches
  them back (remote), or the mapping is dead.
- **E2 — the shadow is signature-independent.** Import builds every target
  commit with `secure_sig: None` (`crates/machine/src/git_projection.rs:633`,
  shared translate path), and jj-lib reads a `gpgsig` header into
  `secure_sig` on the way in (jj-lib 0.42 `git_backend.rs:659-668`), so the
  stripped shadow of a signed commit is byte-identical to the shadow of the
  same commit unsigned. Two differently-signed exports of the same canonical
  commit produce different `git_oid`s but the **same** `public_commit_id`
  (parents map through import mappings to the same public parents, so the
  property is transitive). This is what makes receipts, durability checks and
  race recovery sound below.

Determinism is still required — but only for **import** (git → shadow, used to
verify adopted bytes) and **trees** (unsigned, derived from canonical trees).
Export determinism is no longer load-bearing anywhere, which also downgrades
the memo §8 change-id-collision-loop hazard from "decide whether to detect"
to a non-issue: the loop just changes which OID gets minted once.

Kernel: zero changes. `crates/kernel/src/proto.rs:66` `secure_sig` stays the
only signature field and stays scoped to canonical jj signatures. No packs or
pack manifests change; cloud packs continue to carry canonical objects only.

## 1. Export flow with signing

### 1.1 Where the signature is made

`translate_reachable` writes each projected commit at
`crates/machine/src/git_projection.rs:635` via
`target.write_commit(target_commit, None)`. The `None` is jj-lib's
`sign_with: Option<&mut SigningFn>` hook (`SigningFn = dyn FnMut(&[u8]) ->
SignResult<Vec<u8>>`, jj-lib `backend.rs:182`); when `Some`, jj serialises the
unsigned commit, calls the closure, and appends the result as the `gpgsig`
header before gix writes the object (`git_backend.rs:1350-1361`).

Change: `GitProjection` grows an export-signing mode resolved once at
open/init from `UserSettings` (the `Signer` is already built and stored at
`git_projection.rs:288`):

- gate: new machine-local config `git.sign-exports = true|false` (default
  false). Do not reuse jj's `git.sign-on-push`, whose stock semantics rewrite
  canonical history.
- backend/key: jj's stock `signing.backend`, `signing.key`,
  `signing.backends.*` — already parsed by the `UserSettings` devspace builds.
- the closure, only when `direction == TranslationDirection::Export` and the
  gate is on: `|data| self.store.signer().sign(data, key)`.

There is **no replay arm**. Unlike option (a), the signing function is called
at most once per freshly-minted commit; commits with an existing mapping never
reach `write_commit` at all (1.3).

### 1.2 Ordering against the `.dsprivate` hidden scan

The signature must cover final public bytes, after hidden filtering. Ordering
in `prepare_updates` (`crates/cli/src/git/push.rs:302-354`) already gives
this, with one clarification:

1. `copy_tree` filters the hidden set during translation — the tree the commit
   references is already scrubbed (`git_projection.rs:845-899`).
2. `write_commit` signs those bytes (the signature therefore covers exactly
   the filtered tree, the mapped parent OIDs, and the copied metadata).
3. The defense-in-depth `scan_hidden_paths` runs on the **signed** head
   (`push.rs:331-339` scans `git_head`, which under signing is the signed
   commit's OID — the scan reads the tree through that commit).
4. Only after the scan passes does the head reach `begin_push` and transport.

So the literal order is filter → sign → scan → publish. The scan does not need
to precede signing: it validates the identical tree the signature covers, and
a scan failure aborts before anything is recorded or pushed — a signed object
that failed the scan is unreferenced sidecar garbage, never published, never
journalled. Invariant to document in `git-push.md`: **no signed object is
published or recorded unless the exact signed head passed the hidden scan.**
`rebuild_replay_heads` keeps its own scan (`push.rs:629-641`, `StaleMapping`),
and adoption re-scans fetched bytes (§3.4), so the invariant holds on every
path that can move a signed object toward a remote.

### 1.3 Incremental export: children on top of parent OIDs

Constructing a child commit needs only its parents' **OIDs**: `translate_reachable`
maps parents via `mapped_commit_id` (`git_projection.rs:722-736`), a pure
lookup in the seeded mapping table — no parent bytes are read. Trees are
derived from canonical trees. So a machine that has never held the signed
parent bytes can still mint correctly-parented children. Bytes are needed only
at push time, for pack connectivity, and fetch-back supplies them (§2).

Two changes make the walk respect E1:

**(i) Stop on mapping, fail loud on missing bytes.** Today
`TranslationStops::contains` (`git_projection.rs:535-561`) treats a mapped
commit whose Git object is unreadable as unmapped (`ObjectNotFound => Ok(false)`,
line 557) and re-derives it — the exact behavior that only worked under
determinism. Under (e), export direction changes that arm to return a new
typed error:

```rust
ProjectionError::MissingMappedObject { canonical_id, git_id }
```

so the caller can fetch-back and retry instead of silently minting a
divergent object. (Import direction keeps today's behavior; its stop table is
receipts + accepted mappings and import re-derivation is still deterministic.)

**(ii) Repository-wide seeding.** `prepare_updates` seeds `ExportMappings`
from rows filtered per `(remote, bookmark)` (`push.rs:307-311`). Widen to all
mapping rows in the snapshot (all bookmarks; see open question 1 on
cross-remote scope). Any canonical commit ever accepted anywhere then stops
the walk, which is both the incremental-export optimisation and the
divergence-avoidance mechanism (§3).

### 1.4 What gets recorded where

Nothing new. The journal record shapes are unchanged:

- `ProjectionState { gitOid, canonicalCommitId, publicCommitId, hiddenSetId }`
  (`src/projection_protocol.ts:43-48`) — the `gitOid` is now the signed
  object's OID; the `publicCommitId` is the stripped shadow, identical to the
  unsigned world by E2.
- `git_receipts (git_oid -> public_commit_id)`, immutable
  (`src/projection_store.ts:1088-1108`).
- cursors, batches, fences, batch results: untouched.

The durability gate at `begin` (`projection_store.ts:472-474`,
`requireDurableState`) still checks canonical + public commits in cloud packs
— correct, because those are exactly the objects the cloud still persists.
The signed Git bytes are *deliberately not* checked durable by the journal:
their durability authority is the remote, established by the very push the
batch exists to fence. Journal budgets (`docs/git-projection.md`, 4 MiB / 256
refs / 8,192 states) are unaffected — no signature bytes cross the wire.

Push flow end to end (`push_with_cloud`, `push.rs:115-222`), deltas marked:

1. snapshot + recover overlapping pending — recovery semantics updated (§2, §3).
2. `prepare_updates`: repo-wide seeds Δ, export with `SigningFn` Δ,
   fetch-back-on-`MissingMappedObject` retry loop Δ, hidden scan, shadow
   import, state assembly — otherwise unchanged.
3. upload canonical closure (unchanged).
4. `begin_push` — journal adds the canonical-divergence check Δ (§3.2).
5. `observed_push` lease push (unchanged; `git_subprocess.rs:254`).
6. `recover_push` with observations (unchanged accept path; new `abandon`
   flag on the recovery path only, §2.3).

## 2. Recovery matrix

Notation: J = journal has the mapping/state, L = local sidecar has the Git
bytes, R = remote has the Git bytes.

### 2.1 J ∧ ¬L ∧ R — fetch-back (the common cell)

Rebuilt sidecar (`crates/cli/src/git/projection_sidecar.rs:9-25` deletes and
recreates invalid sidecars), second machine, or fresh clone. Export raises
`MissingMappedObject { git_id }`. Resolution, mirroring the existing
`ObjectNotFound → download_all_packs → retry` shape at `push.rs:538-548`:

1. Pick fetch sources: the mapping rows carrying that `git_oid` name their
   `(remote, bookmark)` (`MappingRow`, `projection_store.ts:86-94`); the OID
   is an ancestor of that bookmark's cursor unless the remote was rewritten.
   Fetch those bookmark refs into the sidecar via the existing
   `devspace_machine::fetch` (`git_subprocess/fetch.rs:78`).
2. If the object is still absent (bookmark since rewritten/deleted), attempt
   fetch-by-SHA (`git fetch <url> <oid>`; works where the server enables
   reachable-SHA-in-want — GitHub does; not universal).
3. Re-run export. Fetched foreign bytes get the same verification as adoption
   (§3.4) before anything built on them is pushed.

A fresh machine may need both halves: `download_all_packs` for canonical
objects and shadows (existing, `push.rs:646-674`) *and* fetch-back for Git
bytes. This is the addendum's accepted cost: fresh-machine replay can no
longer run from cloud packs alone; push recovery gains a remote-reachability
dependency, which is inherent to pushing.

### 2.2 J ∧ ¬L ∧ ¬R — remote lost the bytes (force-push, GC)

Fetch-back exhausts both sources. Two sub-cases with different answers:

- **Pending (never-accepted) mapping** — this is the addendum's "journal
  aborts, fresh push is a legitimate first-sign" cell and it works: abort the
  batch (§2.3), which deletes the quarantined states
  (`projection_store.ts:919-924`), then export fresh. New signature, new OID,
  no conflict: the divergence check (§3.2) consults `projection_states`, and
  the aborted rows are gone. The orphaned `git_receipts` rows from `begin`
  survive abort (receipts are never deleted; `clearRemoteJournal`,
  `projection_store.ts:942-953`, doesn't touch them) — harmless, because a
  receipt is a content-addressed identity statement (that OID, should its
  bytes ever reappear, means that shadow) and by E2 it can never conflict.
- **Accepted mapping** — fail closed with a client-side error naming the OID
  and mapping ("remote no longer has signed public object … for …; the remote
  history was rewritten or pruned outside devspace"). Do **not** fresh-sign:
  an accepted mapping losing its bytes is definitionally an external history
  rewrite, and rewritten remote history is already an unsupported, fail-closed
  path fleet-wide (`GitLiftError::RefRewritten`,
  `crates/cli/src/git/fetch.rs:567-572`: "fetching rewritten history is not
  supported yet"). Retiring an accepted mapping needs an observation-gated
  journal verb and belongs to the future rewritten-history feature (open
  question 2). Note the honest divergence from the task's framing: "abort that
  mapping and fresh-sign" is correct for pending mappings; for accepted ones
  today's system has no sound unilateral abort, and inventing one here would
  contradict receipt immutability for no current gain.

### 2.3 Pending batch recovery (crash between `begin_push` and finalize)

Today `recover_pending_batch` (`push.rs:516-564`) claims the batch (fresh
fence — `claim`, `projection_store.ts:736-777` — so the previous owner's late
callback is rejected by `requireFence`, `projection_store.ts:973-985`),
re-derives the exact heads via `rebuild_replay_heads` (`push.rs:589-644`),
replays the lease push, and recovers from observations. Re-derivation is the
broken part. By remote state:

**R at proposed (the push landed).** Claimant: fetch-back the proposed OIDs
(§2.1; the ref is *at* them, so a plain ref fetch suffices), verify (§3.4),
then run `observed_push` — the lease push fails (ref already moved) but the
observation reports the proposed OID (`push.rs:718` returns the report on
`PushFailed`), and `recover_push` sees all-proposed → accepted
(`projection_store.ts:806-808`). No signing key needed — the memo's "keyless
machines can recover" property survives in (e) form.

**R at expected, same machine, sidecar intact.** The signed bytes exist
locally. `rebuild_replay_heads` already seeds the walk with the batch's own
state rows (`push.rs:609-612`), so with stop-on-mapping the export
short-circuits at the head without re-deriving anything, the equality check at
`push.rs:621-628` passes trivially, and the exact push replays. Unchanged
behavior; only the bytes-missing arm changes.

**R at expected, claimant without the bytes (different machine, or sidecar
lost).** The bytes exist nowhere durable. Exact replay is impossible — and
the journal currently *requires* it: `recover` with all-expected on a claimed
batch returns 409 `projection-replay-required`
(`projection_store.ts:809-815`). That guard's premise was determinism ("a
claimant can always rebuild the exact objects", `docs/git-projection.md`
cloud-journal section). Required journal change:

- `recoverProjectionBatchSchema` (`src/projection_protocol.ts:107-109`) gains
  an optional `abandon: boolean` (default false).
- In `recover`: `allExpected && claimed && !abandon` → 409
  `projection-replay-required` (unchanged); `allExpected && claimed && abandon`
  → finish as `aborted`. `allProposed` still accepts and mixed still
  quarantines (`projection-remote-state-ambiguous`) regardless of the flag.
  Fencing is untouched: abandon is only honored from the current fence owner.

Claimant protocol around abandon (the zombie-push analysis that makes it
sound is in §3.5):

1. observe the remote (the `observed_push` report, or `ls-remote`); if at
   proposed → R-at-proposed path above.
2. at expected → re-observe once after a short delay (narrow the
   push-in-flight window), then `recover(abandon: true)` → batch aborted,
   quarantined states deleted.
3. **post-abort observation gate**: before any fresh `begin_push` on those
   bookmarks, observe again. If the ref moved off expected in the interim
   (the zombie landed), run the normal fetch path first — journal cursors are
   still at the old value, so this is the standard
   remote-moved-outside-devspace reconciliation (`remote_moved`,
   `push.rs:765-777`; fetch lift, §3.5). Only then export fresh (legitimate
   new first-sign) and push.

**R at neither (third value).** External interference; existing
`projection-remote-state-ambiguous` quarantine, unchanged
(`projection_store.ts:819-824`).

## 3. Racing exporters (the open problem)

Setup: machines M_A and M_B concurrently export the same canonical commit C.
Signatures are nondeterministic, so they mint different signed objects G_A ≠
G_B — while, by E2, both import to the same shadow P, so
`git_receipts` rows `G_A→P` and `G_B→P` can *both* be stored without tripping
`projection-receipt-mismatch`. The journal race is decided by existing
machinery; the design problem is making the loser converge instead of wedging.

### 3.1 Same bookmark: the journal already picks the winner

(One flag on the addendum: it says "the bookmark lease picks a winner" — for
same-bookmark races the **journal** picks the winner at `begin`, before any
Git lease is taken. The lease is the last-line fence against non-devspace
writers and zombie pushes, not the arbiter between devspace machines.)

Serialisation points, in order:

- `projection_batch_refs` uniqueness → 409 `push-in-progress`
  (`projection_store.ts:508-526`) when the winner's batch is still pending;
- `requireExpectedCursors` → 409 `projection-cursor-stale`
  (`projection_store.ts:1110-1139`) when the winner already finalized.

**Loser detection** is therefore the `begin_push` error itself. Crucially, in
a same-bookmark race the loser has *not pushed anything*: `observed_push` runs
only after a successful `begin` (`push.rs:165-175`). Its divergent signed
objects G_B exist only in its disposable sidecar — unreferenced garbage,
cleared by rebuild or `git gc`, no remote cleanup needed. (The only wasted
work is the signatures themselves; acceptable.)

**Loser adoption protocol** (all machinery reused, one behavior change):

1. `push_with_cloud`'s existing begin-error arm (`push.rs:205-219`) refreshes
   the snapshot; if the winner's batch is pending, the loser becomes its
   claimant via `recover_pending_batch` — claim/fence/replay/recover exactly
   as §2.3. It drives the winner's batch to accepted (fetch-back of G_A if the
   winner's push landed) or aborted (abandon, if it didn't).
   Note this can fence out a *live* winner mid-push — the winner's own
   `recover_push` then gets `projection-owner-stale` and its CLI errors while
   the loser completes the batch. That is today's claim-immediately behavior,
   unchanged by (e); the winner's retry finds the bookmark up to date.
2. Loop: reload snapshot, `prepare_updates` again. Seeds now include the
   accepted rows `C→G_A` (repo-wide seeding, §1.3). The walk stops at C.
3. Sidecar lacks G_A's bytes → `MissingMappedObject` → fetch-back (§2.1) →
   retry. Export now builds the loser's remaining commits as children of G_A.
4. Verify adopted bytes (§3.4), scan, `begin_push` with the advanced
   `expected_old_oid`, push, finalize.

If the winner finalized before the loser's `begin` (cursor-stale case), step 1
is skipped — there is no pending batch, `push_with_cloud` surfaces the error
and today's contract is that the user reruns the push, which then follows
steps 2–4. (Auto-retry on cursor-stale is a pre-existing UX gap, not a signing
issue; out of scope.)

### 3.2 Cross-bookmark, same canonical commit: close the silent-divergence hole

Memo §3 bullet 4 stands: with disjoint ref sets both batches are legal today,
both pushes land, and one canonical commit gets two live public OIDs —
receipts keyed by `git_oid` accept both. Under determinism the OIDs coincide;
under signing they diverge permanently and transitively (descendants inherit
divergent parent OIDs). Two layers:

**Layer 1 (client, required):** repo-wide seeding + fetch-back (§1.3) makes
any *sequenced* export converge — only true concurrent first-exports can
diverge.

**Layer 2 (journal, small):** a mint-time uniqueness check in `begin` only —
one Git OID per canonical commit. No new table: `projection_states` already
holds `(canonical_commit_id, git_oid)` for both pending and active rows, and
abort deletes pending rows, giving exactly the right lifecycle (a binding
whose bytes never became remote-durable dies with its batch; an accepted
binding is permanent). For each submitted state:

```sql
SELECT git_oid FROM projection_states
WHERE canonical_commit_id = ? AND git_oid <> ? LIMIT 1
```

(needs an index on `canonical_commit_id` in `src/schema.ts`). A hit → new 409,
kebab-case per the constraint:

```
canonical-commit-diverged
```

with payload `{ gitOid, pendingBatchId | null }` so the loser can act without
scanning. The check runs at `begin` **only** — never in `recordFetch`: fetch
records what the remote world already is, and refusing to record it would
break reconciliation. (`recordFetch`'s existing `fetch-lineage-ambiguous`
guards the *other* direction, one lineage per Git OID —
`projection_store.ts:1345-1401`.) The DO is single-threaded
(`transactionSync`), so two concurrent begins serialize and the second always
sees the first's pending rows: the race is closed at the authority, before
either loser pushes.

**Loser handling for `canonical-commit-diverged`:** if `pendingBatchId` is
set, claim-and-recover that batch (same §2.3 machinery — it is not caught by
`overlapping_pending`, `push.rs:497-513`, since the bookmark differs, hence
the payload field), then retry; the retry either adopts the accepted mapping
via seeds + fetch-back, or — if the batch aborted — mints fresh as a
legitimate first-sign. If `pendingBatchId` is null the binding is active:
reload snapshot, re-prepare, adopt.

### 3.3 What "adopting the winner's mapping" changes locally

Nothing is persisted machine-side beyond the sidecar (consistent with "never
persist state derivable from the world"): the journal snapshot *is* the
mapping state, re-read every push, and the sidecar gains the fetched objects.
The loser's own pre-race `ExportMappings` were request-scoped values built
from the snapshot; there is no stale local table to fix. Its divergent
objects: same-bookmark and cross-bookmark losers never pushed them (§3.1,
§3.2 — layer 2 rejects before push), so "the loser's already-pushed objects"
reduce to exactly one case, the abandoned-then-landed zombie, analysed in
§3.5.

### 3.4 What adoption verifies

On fetching G_A the loser verifies, in order:

1. **Content address** — Git itself: the fetched object hashes to G_A.
2. **Shadow identity** — deterministic import of G_A must equal the journal's
   `public_commit_id`. This is the existing expected-mapping mechanism:
   `import_reachable_with_stops` with receipts as `expected` raises
   `ConflictingMapping` on mismatch (`git_projection.rs:568-569, 636-645`).
   This is the load-bearing check: it proves the remote's bytes carry exactly
   the recorded public content.
3. **Hidden scan** — `scan_hidden_paths` of the fetched head against the
   canonical commit's hidden set, the `StaleMapping` shape
   (`push.rs:629-641`): a hostile or corrupted remote cannot smuggle hidden
   content in under an adopted mapping. (For commits below the head this is
   subsumed by 2: the shadow equality covers the whole filtered tree.)
4. **Not signature validity.** No key, no allowed-signers file is needed to
   adopt; identity is bound by 1–3. Signature *validity* is the remote
   platform's concern (the "verified" badge). Optional future hardening: run
   jj's `Signer::verify` when `signing.backends.ssh.allowed-signers` is
   configured; off by default.

Replay verification in `rebuild_replay_heads` changes the same way: when the
bytes were fetched rather than locally present, the head check becomes
"import of fetched head == state's `public_commit_id`" instead of "re-derived
export head == proposed OID" (`push.rs:621-628`), which re-derivation can no
longer satisfy.

### 3.5 The zombie push, end to end

The one path where a loser's (or crashed winner's) objects reach the remote
outside an accepted batch: claimant abandons at R-at-expected (§2.3), and the
original owner's in-flight `git push` lands *afterwards*. Why this is
degraded-but-convergent rather than unsound:

- Journal state after abandon: batch gone, quarantined states deleted, cursor
  still at the old OID, receipts `G_A→P` **persisted** (stored at `begin`,
  `projection_store.ts:488`, surviving abort).
- Zombie lands: remote ref = G_A, journal cursor = old. Every subsequent
  devspace interaction classifies this as "remote moved outside devspace"
  (`remote_moved`, `push.rs:765-777`; push says fetch first).
- The fetch path reconciles: G_A is imported fresh (not in accepted
  mappings), and `recordFetch` requires its state to match the persisted
  immutable receipt (`fetch-state-receipt-mismatch`,
  `projection_store.ts:627-642`) — which it does, by E2. The lift
  (`git_lift.rs::select_seeds` / `lift_imported`) grafts hidden lineage from
  the accepted parent seed and produces a canonical commit L for G_A. L is
  content-equivalent to the original C but generally a different canonical
  commit, so the user may see a diverged bookmark (C local, L remote) —
  standard external-move resolution.
- If instead the claimant's fresh-signed push wins the race to the remote,
  the zombie's `--force-with-lease=<expected_old>` fails at the remote and
  its objects never land (or land unreferenced and are GC'd). The Git lease
  is doing exactly its last-line-fence job here.
- Residual corner: claimant aborts, does **not** observe (violating the
  §2.3 post-abort gate), fresh-signs and begins; zombie lands between its
  observation and push; claimant's lease fails; recovery observes G_A ≠
  proposed ≠ expected → `projection-remote-state-ambiguous`, its second batch
  quarantined until the ref is fetched/reconciled. Fail-closed, recoverable,
  and the post-abort gate makes it rare. Acceptable.

The `projection-replay-required` guard's *reason* (a live push from the old
owner could still land, `docs/git-projection.md`) is thus not discarded — it
is downgraded from "abort is unsafe" to "abort has a bounded, receipt-anchored
degraded path", which is the best any design can do once exact replay is
physically impossible.

## 4. Wire/schema deltas, sized

Worker (all small; kebab-case codes on new 4xx per constraint):

1. `begin`: canonical-divergence query + 409 `canonical-commit-diverged` with
   `{ gitOid, pendingBatchId | null }` payload; index on
   `projection_states(canonical_commit_id)` in `src/schema.ts`. ~30 lines +
   Vitest.
2. `recover`: optional `abandon` boolean in `recoverProjectionBatchSchema`
   (`src/projection_protocol.ts:107-109`) + the one-branch relaxation at
   `projection_store.ts:809-815`. ~10 lines + Vitest.
3. No changes to state/receipt/cursor/batch shapes, `recordFetch`, remotes
   registry (`src/remote_protocol.ts`), budgets, or the Wasm validation
   kernel.

Machine crate:

4. `git_projection.rs`: export `SigningFn` plumbing (§1.1);
   `MissingMappedObject` stop behavior (§1.3.i). ~60 lines.
5. Sidecar fetch-back helper: bookmark-ref fetch reusing
   `git_subprocess/fetch.rs:78` + a fetch-by-SHA variant. ~80 lines.

CLI:

6. `push.rs`: repo-wide seeding (`:307-311`); fetch-back retry loop in
   `prepare_updates`; `rebuild_replay_heads` bytes-missing arm → fetch-back +
   import-equality verification (`:589-644`); claimant abandon protocol +
   post-abort observation gate in `recover_pending_batch` (`:516-564`);
   handle the two new error codes; `git.sign-exports` gate. ~150-200 lines.
7. `fetch.rs`: no changes (signed foreign commits already flow through
   import, signatures stripped into shadows — today's behavior is already
   (e)-correct on the fetch side).

Kernel, packs, pack manifests, `kernel-wasm`: **zero**.

Docs: `git-push.md` (signing gate, remove from unsupported list),
`git-projection.md` (determinism contract narrowed to import + trees;
remote-as-persistence recovery; claimed-batch abandon), `git-fetch.md`
(unchanged behavior, note E2).

## 5. Failure-mode table

| # | Situation | Detected by | Resolution |
|---|-----------|-------------|------------|
| 1 | Mapped OID, bytes missing locally, remote has them | `MissingMappedObject` (new) | fetch-back bookmark refs, then by-SHA; retry export (§2.1) |
| 2 | Mapped OID, bytes gone from remote, mapping pending | fetch-back exhausted | abort batch (abandon if claimed); fresh-sign is a new first-sign (§2.2) |
| 3 | Mapped OID, bytes gone from remote, mapping accepted | fetch-back exhausted | fail closed, named client error; = external rewrite, retirement verb deferred (§2.2, OQ2) |
| 4 | Same-bookmark begin race | 409 `push-in-progress` / `projection-cursor-stale` | claim+recover winner's batch, reseed, fetch-back, adopt (§3.1) |
| 5 | Cross-bookmark same-canonical race | 409 `canonical-commit-diverged` (new) | recover named pending batch or reseed+adopt; loser never pushed (§3.2) |
| 6 | Crash after push landed | claimant observes proposed | fetch-back, verify (§3.4), observe, recover → accepted (§2.3) |
| 7 | Crash before push landed, same machine, sidecar intact | replay path | exact lease-push replay, unchanged (§2.3) |
| 8 | Crash before push landed, claimant without bytes | remote at expected + no local objects | re-observe, `recover(abandon)`, post-abort gate, fresh-sign (§2.3) |
| 9 | Zombie push lands after abandon | `remote_moved` on next interaction | fetch reconciliation via persistent receipts; possible diverged canonical bookmark; convergent (§3.5) |
| 10 | Remote at third value during recovery | `projection-remote-state-ambiguous` | quarantine, unchanged |
| 11 | Adopted/fetched bytes wrong content | import-equality `ConflictingMapping` / hidden-scan `StaleMapping` | fail closed; corrupt or hostile remote (§3.4) |
| 12 | Keyless machine in a signing fleet | n/a | adopts and recovers fine (no key needed); its own exports unsigned → mixed history (OQ3) |
| 13 | Fetch overlapping a pending push | `fetch-overlaps-pending-push` | unchanged |
| 14 | Fresh machine, nothing local | `ObjectNotFound` + `MissingMappedObject` | `download_all_packs` (canonical/shadows) + fetch-back (Git bytes) (§2.1) |

## 6. Memo claims vs code — flags

Checked every cite the design leans on; all hold (`git_projection.rs:270,
288, 633-635`, `push.rs:136-142, 307-311, 346-354, 538-548, 589-644, 621-628,
699-721`, `projection_store.ts` receipt/fence/claim behavior, jj-lib 0.42
`git_backend.rs:1255, 1350-1361`, `backend.rs:182`). Corrections worth noting:

- **Addendum: "the bookmark lease picks a winner."** Imprecise: the journal
  (`push-in-progress` / cursor check at `begin`) picks the winner between
  devspace machines; the Git lease fences non-devspace writers and zombie
  pushes (§3.1, §3.5).
- **Addendum's implied "no Worker changes."** Option (e) needs no *schema*
  change and no kernel change, but it is not zero-Worker: the
  `projection-replay-required` rule (`projection_store.ts:809-815`) is built
  on the determinism premise and must gain the `abandon` escape, and closing
  cross-bookmark divergence needs the `begin`-time check. Both are small (§4)
  but real.
- **Memo §3 "Recovery replay fails closed, permanently … bricks the bookmark
  for the whole fleet."** True only when the sidecar bytes are also gone;
  same-machine recovery with an intact sidecar replays fine even today under
  signing (§2.3, seeds at `push.rs:609-612` short-circuit re-derivation).
  Doesn't change the conclusion, but the wedge is narrower than stated.
- **Memo §5's proposed fixes** (repo-wide seeding, pack-download fallback in
  the normal export path) carry over to (e) nearly verbatim, with the pack
  fallback generalised to "packs for canonical objects, remote fetch-back for
  Git bytes".

## 7. Open questions

1. **Scope of the one-OID-per-canonical rule: repo-wide vs per-remote.**
   Recommended: repo-wide (matches the determinism status quo where the same
   canonical history has one public SHA across mirrored remotes, and reads
   "sign once per public commit object" literally — one object, ever). Cost:
   pushing shared history to a second remote requires fetch-back *from a
   different remote* than the push target; if that remote is unreachable the
   push fails. Per-remote scoping removes the coupling but forks public SHAs
   across remotes. One-bit decision for Tom; the check's SQL is scoped or not
   by a `remote = ?` predicate.
2. **Retirement of accepted mappings** whose remote bytes are permanently
   gone (row 3). Needs an observation-gated journal verb (delete the lineage's
   state rows, permit a new binding) and belongs to the rewritten-history
   feature, which is unsupported today anyway. Until then row 3 fails closed.
3. **Mixed signed/unsigned fleets** (carried from memo §8): a keyless
   machine's exports interleave unsigned commits into signed public history.
   If unacceptable, a repository-level `require-signed-exports` policy flag
   belongs in the remote registry / repository record, not the machine —
   journal would need a way to *know* an export is signed, which it currently
   cannot verify (it never sees Git bytes); likely enforced client-side off a
   registry flag. Defer.
4. **Abandon UX**: automatic (recommended, with the §2.3 observation gates)
   vs gated behind an explicit `ds git push --abandon-pending`. Automatic
   matches the addendum's intent; the flag variant is safer against the §3.5
   residual corner at the cost of wedging unattended fleets.
5. **Fetch-by-SHA support variance**: the bookmark-ref-first fallback covers
   the common case; the residual gap is an OID reachable only from a
   *deleted* bookmark on a server without reachable-SHA-in-want — degrades to
   row 2/3. Not worth designing around now.
