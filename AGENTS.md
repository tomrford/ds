# devspace

Cloudflare-native devspace. The current contract is in `README.md` and
`docs/kernel.md`, `docs/sync.md`, `docs/hidden.md`,
`docs/git-projection.md`, `docs/git-push.md` and `docs/git-fetch.md`.
The legacy `tomrford/devspace-legacy` repository is a historical behavioural
reference only; nothing canonical depends on it.

Detect the VCS surface before use. In a ds checkout, use `ds`, never git or jj
directly. A plain Git checkout uses `git`; this tree becomes a ds checkout again
after re-initialization.

Gate: `nix develop -c pnpm check` and `nix develop -c pnpm test`.

## jj format parity

- `crates/kernel` is a maintained mini-fork of jj's GitBackend commit format
  and simple operation-store format, pinned to jj-lib 0.42.0 semantics. It must
  never depend on jj-lib, even as a dev dependency.
- On any jj version bump, audit the kernel against the jj-lib source itself
  (cargo registry copy). For Git objects, inspect `git_backend.rs`: commit
  header parsing and encoding, derived and stored change IDs, `jj:trees`,
  conflict-label headers, unknown-header handling, and reconstruction of the
  rebuildable `store/extra` cache. Mirror changes in `commit.rs`, `tree.rs`,
  `hash.rs`, and object-reference extraction.
- Use `crates/kernel-oracle`, which may depend on jj-lib, to generate and compare
  oracle results during the audit. Keep jj-lib out of `crates/kernel` itself.
- Audit operation and view encoding in `simple_op_store.rs` plus the
  `ContentHash` implementations, struct field orders, merge encodings, and enum
  ordinals in `backend.rs`, `op_store.rs`, `merge.rs`, and
  `conflict_labels.rs`. Mirror changes in `crates/kernel/src/ops/`.
- Regenerate `git_golden.txt`, `git_golden_oracle.txt`, and `ops_golden.txt`
  from the new jj version. Golden Git vectors are exact object payloads with
  standard Git OIDs. Operation vectors are canonical protobuf bytes with jj
  semantic Blake2b IDs. The kernel rejects non-canonical operation encodings
  rather than normalizing them.
- The accepted schemas are exactly Git blob/tree/commit objects as used by
  GitBackend and jj's simple operation store. Do not add Devspace-only object
  fields. Gitlink rejection is Devspace projection policy, not a GitBackend
  storage limitation; reject Gitlinks before a canonical commit crosses that
  boundary.
- Gitignore matching through jj-lib's gix-ignore wrapper is canonical private
  projection behavior. Audit it against the machine projection and working-copy
  tests on every jj bump.
- `crates/cli/src/working_copy.rs::base_ignores` mirrors jj-cli's
  GitBackend branch of `WorkspaceCommandHelper::base_ignores`: the backend Git
  config's global excludes plus `.git/info/exclude`. Audit it on every bump.
- `crates/machine/src/op_sync.rs::object_path` knows the simple operation
  store's on-disk layout outside the kernel. Audit it with operation encodings
  on every bump.

## jj bump rollout

Git object bytes and OIDs are standard, immutable Git data. Operation objects
are byte-exact and content-addressed. A jj bump needs a protocol rollout only
if the audit finds that an existing logical object shape now has different
canonical bytes or reference semantics.

When that happens:

1. define a new advertised transport capability after `git-pack/2`;
2. deploy the Worker first with an explicit accept set for every live canonical
   form and an upgrade error for stale clients;
3. upgrade machines after the Worker accepts the new capability;
4. reject mixed writers that propose different bytes for one object ID through
   the existing no-clobber checks.

Stored objects are immutable and are never migrated or normalized in place. A
repository can be re-incarnated and re-uploaded from an up-to-date machine when
an old accepted form must be retired.
