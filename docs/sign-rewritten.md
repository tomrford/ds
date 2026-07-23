# Signing rewritten public commits

## Status

Signing rewritten public commits is parked. Devspace does not expose a
machine-side signing gate. Identity-projected commits still retain their exact
existing bytes and signatures; commits whose tree or parents change are
rewritten deterministically without a new signature.

## Why machine-side signing is deferred

Without signing, the projection algorithm is deterministic:

```text
canonical commit + versioned hidden policy -> public commit bytes -> public OID
```

Every honest machine therefore produces the same canonical/public pair. The
existing journal batches, pair states, cursors, leases, and idempotent recovery
handle concurrent pushes of that same pair.

Machine-specific signing keys make the mint nondeterministic. Two machines can
produce different public OIDs for the same canonical commit. Making that safe
requires a permanent winner record, a paginated way to discover winners,
cloud-pack adoption by losing machines, descendant reparenting, new signatures,
and a retry protocol for pending, aborted, and accepted races. That protocol
cost is not justified for a feature that is not on the planned push path.

## Plan of record

The planned server-side push makes the Worker the single actor that projects
and sends commits to the Git remote. If rewritten commits need signatures, sign
them there. One minting actor removes the cross-machine race by construction.

A later GitHub App design could instead use GitHub APIs that create commits
under GitHub's signing authority. That is an optional follow-up, not a current
contract.

## Compatibility constraint

Determinism now pins public OIDs across machines. The projection rewrite
algorithm must not change silently. Any change that can alter public commit
bytes needs a compatibility story for mixed clients, stored journal state, and
the Worker rollout before it ships.
