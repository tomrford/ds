# Signing rewritten public commits

## Decision

Do not implement signing yet.

The colocated architecture removes the old replay problem. A rewritten public
commit is an ordinary Git object in the canonical object database, and both its
OID and exact bytes become cloud-durable before a projection batch begins.
Recovery downloads and pushes that stored object. It does not re-project or
re-sign it.

The complete hypothesis is nevertheless false today. The journal selects one
public OID for each canonical OID, but a racing or later machine cannot always
discover and adopt that selection. In particular, an aborted batch deletes its
pair states but deliberately retains its immutable receipt. A later
nondeterministic signature produces another public OID, receives
`canonical-oid-diverged`, and has no receipt data with which to retry. Signing
must wait until receipt adoption is part of the machine protocol.

After that gap is closed, sign each rewritten public commit once when it is
first minted, store the signed commit through the existing pack path, and reuse
its exact bytes thereafter.

## Verified hypothesis chain

### Rewritten public commits are first-class stored objects

Projection rewrites the canonical commit payload and writes the resulting
public commit directly into the same Git object database
(`crates/machine/src/projection.rs:265-285`). Push then builds and uploads the
closure of every canonical and public head before it calls `begin_push`
(`crates/machine/src/journal_flow.rs:194-203`,
`crates/machine/src/journal_flow.rs:279-310`).

The Worker checks that both commits in every proposed pair are already present
as durable commit objects (`src/projection_store.ts:387-399`,
`src/projection_store.ts:1001-1020`). Git object identity covers the exact
payload bytes and validation does not normalize an accepted object
(`docs/kernel.md:26-29`). A `gpgsig` header therefore rides in the same durable,
content-addressed object as every other commit header.

Verdict: verified.

### Replay and fresh-machine recovery use stored bytes

The Worker replays the exact OIDs and hidden-set identity stored in the pending
batch (`src/projection_store.ts:289-342`). The claimant downloads cloud packs,
reads the recorded canonical and public commits, scans the recorded public
tree, and pushes the recorded public OID
(`crates/machine/src/journal_flow.rs:528-551`). It never calls
`project_hidden_paths` in recovery.

The live native proof creates a fresh machine after a process stops following
the Git push. The new machine recovers from packs, and the canonical and public
commit bytes match the first machine byte-for-byte
(`crates/machine/tests/journal.rs:248-317`). The lower-level pack proof also
installs a rewritten public commit into an empty repository and compares its
exact bytes (`crates/machine/tests/projection.rs:206-224`).

Verdict: verified. Signature nondeterminism does not affect recovery.

### Accepted mappings stop later projection

Normal push seeds projection from the journal snapshot
(`crates/machine/src/journal_flow.rs:153-160`). A seeded canonical/public pair
is a traversal stop: projection requires the recorded public commit to exist,
records that it reached the pair, and does not derive another commit
(`crates/machine/src/projection.rs:194-209`). Missing recorded bytes fail as an
object-read error; they do not cause re-projection.

The only production call to `project_hidden_paths` is the initial push
preparation at `crates/machine/src/journal_flow.rs:158-160`. Fetch uses
overlay-lift, while pending-batch recovery uses the recorded public OID.

Verdict: verified. There is no re-derivation comparison that signing would
break.

### The journal serializes different public OIDs, but the loser does not adopt

`begin` validates durability and stores repository-wide canonical/public
receipts before it creates the pending batch
(`src/projection_store.ts:379-425`). A receipt cannot be rebound to another
public OID; the Worker returns `canonical-oid-diverged`
(`src/projection_store.ts:1023-1065`). This closes same-bookmark and
cross-bookmark double-mint races at the authority.

The machine handles only `push-in-progress` by refreshing, recovering, and
retrying (`crates/machine/src/journal_flow.rs:279-309`).
`canonical-oid-diverged` is returned to the caller. The error response contains
only a code and message (`crates/machine/src/wire.rs:6-19`), so it does not tell
the loser which public OID won.

A snapshot does not close the gap:

- active state rows are exposed as mappings, but pending rows are excluded
  (`src/projection_store.ts:251-280`);
- a pending ref exposes its proposed public OID but not the corresponding
  canonical OID (`crates/machine/src/http_transport.rs:163-183`);
- abort deletes the pending state while retaining the receipt
  (`src/projection_store.ts:850-938`).

The Worker test deliberately proves that a receipt remains immutable after its
batch aborts (`test/projection.test.ts:551-584`). After that abort, no active or
pending state exposes the selected public OID. A new GPG signature, or any
other nondeterministic signature, can only mint another OID and be rejected.

Verdict: refuted. The authority chooses a winner, but the client does not always
adopt it. This is the implementation stop condition.

## Required adoption change

Keep the current one-public-per-canonical receipt and its survival across
abort. The public object is already cloud-durable, so retaining the first
selection gives `sign once` a clear meaning even when the first remote update
aborts.

Expose repository-wide receipt pairs to machines, including receipts whose
states are pending or were aborted. The preferred shape is a paginated
`receipts` collection in the projection snapshot:

```text
canonicalOid  publicOid
```

Push should seed `ProjectionMappings` from receipts as well as active mapping
rows. On `canonical-oid-diverged`, it should:

1. refresh the receipt snapshot;
2. download the cloud pack catalog;
3. retry projection from the recorded pair;
4. continue through the existing `push-in-progress` recovery path if another
   batch still owns the same bookmark.

This also handles cross-bookmark races. The second machine adopts the same
public commit before either a new batch or a Git subprocess can publish its
discarded local candidate.

Returning the winner only in the conflict response is a smaller wire change,
but it makes an error response the only source of durable projection state and
does not fix later pushes after an aborted batch. Exposing receipts is the more
complete contract.

The required test is a native two-machine race with a deliberately
nondeterministic test signer:

1. machine A and machine B mint different signed public OIDs for one canonical
   commit;
2. A's `begin` wins and A stops after the Git push;
3. B receives the divergence result, refreshes receipts, installs packs, and
   adopts A's public OID without signing again;
4. B recovers the pending batch;
5. an empty machine C installs cloud packs and confirms the signed commit bytes,
   journal cursor, and remote ref all use A's OID.

Also cover the same sequence with A's batch aborted before B retries. B must
still adopt the retained receipt and publish its stored public object.

## Signing mechanism after adoption exists

The current rewrite removes `gpgsig`, `gpgsig-sha256`, and `mergetag` while
preserving other headers (`crates/machine/src/projection.rs:451-485`). Keep that
header surgery: the original signatures cover different tree or parent bytes
and are no longer valid.

For a commit whose tree or parent list changed:

1. produce the final unsigned rewritten payload;
2. call jj-lib's `SigningFn` with that exact payload;
3. encode the returned signature as the new `gpgsig` header;
4. write the final commit through gix and use its resulting OID as the public
   mapping.

jj-lib 0.42 defines `SigningFn` as a callback from raw commit bytes to signature
bytes (`backend.rs:180-182`) and its GitBackend inserts the result as `gpgsig`
before writing the object (`git_backend.rs:1350-1367`). Devspace should reuse
that callback and header encoding at the raw rewrite write. It should not route
the rewritten payload through `GitBackend::write_commit`, because the current
raw rewrite intentionally preserves header order, encoding, and unknown
well-formed headers.

`MachineGitRepository` already initializes jj's `Signer` from `UserSettings`
(`crates/machine/src/store.rs:31-52`). The callback should be equivalent to
jj's normal commit builder:

```rust
|data| store.signer().sign(data, sign_settings.key.as_deref())
```

Only the non-identity branch at `crates/machine/src/projection.rs:273-285`
receives the callback. The identity fast path at lines 273-276 must not invoke
it.

## Configuration

Add one jj configuration gate:

```toml
[git]
sign-rewritten = false
```

Declare the default in Devspace's built-in jj configuration
(`crates/cli/src/lib.rs:88-97`). Reuse jj's `signing.backend`, `signing.key`,
and `signing.backends.*` settings for backend construction and key selection.
Do not reuse `git.sign-on-push`: its jj meaning is to rewrite canonical
history, while this feature signs only public projection objects.

`signing.behavior` must not decide whether projection signs. The explicit
`git.sign-rewritten` gate owns that decision. Projection has already discarded
an invalid source signature and is signing a different object, so jj's
keep/own/force policy for canonical commits does not describe this operation.

When `git.sign-rewritten = true` and no usable signing backend or key is
available, fail before writing any rewritten commits. Do not silently fall back
to unsigned output.

## Identity history and signature meaning

Identity projection remains byte-for-byte:

- a signed canonical commit keeps the user's original signature;
- an unsigned canonical commit stays unsigned even when
  `git.sign-rewritten = true`;
- a commit with a hidden-free tree but a rewritten parent is still a rewritten
  public commit and receives a projection signature.

A projection signature attests to the exact filtered public commit. It does not
claim that the original author approved removing hidden paths, and it does not
replace the user's signature on canonical private history. Author, committer,
message, and other preserved headers remain those of the canonical commit.

GitHub therefore sees:

| Public object | GitHub display |
| --- | --- |
| Identity, valid user signature | The existing signature and its existing verification result |
| Identity, unsigned | No verification status by default; `Unverified` can appear under vigilant mode |
| Rewritten, signing disabled | No status by default; `Unverified` can appear under vigilant mode |
| Rewritten, projection signature valid to GitHub | `Verified`, or `Partially verified` under the vigilant-mode author/committer rules |
| Rewritten, projection signature not recognized | `Unverified` |

GitHub verifies GPG, SSH, or S/MIME signatures found in the commit object; an
unsigned commit has no status by default. Its vigilant mode can instead show
unsigned commits as `Unverified` and can show a valid signature as
`Partially verified` when the author and committer differ. See GitHub's
[commit signature verification](https://docs.github.com/en/authentication/managing-commit-signature-verification/about-commit-signature-verification)
documentation.

Because rewritten commits retain the original committer identity, a projection
key does not automatically produce a GitHub `Verified` badge. In particular,
GitHub requires a GPG key identity and verified account email that match the
committer email. Prefer an SSH signing key registered to the account intended
to attest Devspace projections, and validate the exact GitHub display before
relying on branch rules.

## Mixed fleets and key rotation

Receipt replay and recovery are keyless. Once any machine records a signed
public OID, another machine needs only the journal pair and cloud pack bytes.
It must not re-sign the commit or require the old key.

A keyless machine has two distinct modes:

- with `git.sign-rewritten = true`, it can replay existing pairs but must fail
  when it needs to mint a new rewritten commit;
- with the gate off, it can mint new unsigned rewritten commits, so signed and
  unsigned public history can interleave.

The machine-local gate therefore does not mean that a repository is
all-signed. A repository-wide `require signed rewritten commits` policy would
need separate cloud policy and signature evidence. It is outside this design.
GitHub branch rules that require signed commits also require identity-path
canonical commits to arrive already signed.

Key rotation changes only future first mints. Existing receipt pairs continue
to select commits signed by the old key; descendants first projected after the
rotation use the new key. Old commits are never rewritten merely to update a
signature. GitHub also records successful verification persistently within a
repository network, so a later key rotation or revocation does not normally
change that stored result.

## Owner choices and recommendation

The remaining choices are:

1. Whether a receipt should remain the permanent selection after an aborted
   first batch. Recommendation: yes. The bytes are already cloud-durable, this
   preserves `sign once`, and exposing receipts makes the selection usable.
2. Whether signed and unsigned rewritten commits may mix. Recommendation:
   accept mixed history for the first machine-local, default-off feature. Add a
   repository policy only if `all rewritten commits are signed` becomes a
   product guarantee.
3. Which identity the projection signature represents. Recommendation: treat
   it explicitly as a projection attestation, preserve canonical
   author/committer headers, and use an SSH signing key whose GitHub ownership
   communicates that role.
4. Whether to sign rewritten public commits at all. Recommendation: yes, but
   only after receipt exposure, divergence retry, and the race/abort/fresh-pack
   native proof land together.

No signing prototype belongs in this change because choice 1 has an
unimplemented protocol dependency and the current machine cannot adopt the
journal's selected bytes.
