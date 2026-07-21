# devspace

Cloudflare-native devspace. The current contract is in `README.md` and
`docs/kernel.md`, `docs/sync.md`, `docs/hidden.md`,
`docs/git-projection.md`, `docs/git-push.md` and `docs/git-fetch.md`.
The legacy `tomrford/devspace-legacy` repository is a historical behavioural
reference only; nothing canonical depends on it.

This is a ds checkout — use `ds`, never git or jj directly.

Gate: `nix develop -c pnpm check` and `nix develop -c pnpm test`.

## jj format parity

- `crates/kernel` is a maintained mini-fork of jj's storage format, pinned to
  jj-lib 0.42.0 semantics. It must never depend on jj-lib, even as a dev
  dependency.
- On any jj version bump, audit the kernel against the jj-lib source itself
  (cargo registry copy), surface by surface: `content_hash.rs` impls;
  `ContentHash` struct field orders and enum ordinals in `backend.rs`,
  `op_store.rs`, `merge.rs`; `conflict_labels.rs`; conversion and legacy
  decode branches and object path layouts in `simple_backend.rs` and
  `simple_op_store.rs`. Mirror what changed, regenerate
  `crates/kernel/tests/jj_golden.txt` from the new jj version, and run the full
  gate.
- Gitignore matching through jj-lib's gix-ignore wrapper is canonical projection
  behavior; audit it against the machine matcher golden tests on every jj bump.
- Golden vectors are canonical bytes with jj ContentHash IDs, originally
  emitted by the old server as a jj-lib 0.42.0 oracle. The kernel rejects
  non-canonical encodings rather than normalizing; normalization is
  machine-side work.
- The accepted schema is exactly jj's simple backend and simple operation-store
  schema. Do not add Devspace-only fields. Values the simple backend cannot
  store, including Git submodules, must fail before encoding.
- The simple-store on-disk layout knowledge in
  `crates/machine/src/object_closure.rs::object_path` is fork surface living
  outside the kernel; audit it on every bump alongside the encodings.

## jj bump rollout (encoding changes)

Cloud replication is byte-exact and objects are content-addressed by jj's
semantic hash, so a bump only matters for rollout when the audit above finds
that canonical bytes changed for existing object shapes (proto3 field
additions usually don't). When they did:

1. Bump `devspace_kernel::ENCODING_VERSION`. Clients advertise it via the
   `x-devspace-client` header (set in `hardened_http_client`).
2. Deploy the Worker FIRST, with the kernel accepting both the old and new
   canonical forms (an accept-set decode branch, never a data migration —
   stored bytes are immutable and are never rewritten). The Worker may gate
   stale clients on the advertised encoding with an "upgrade ds" error;
   never let them hit a canonicality failure.
3. Upgrade machines after. Mixed-epoch machines writing the same logical
   object as different bytes trip the no-clobber check by design.
4. Accept-set branches are deletable at any time by re-incarnating the
   affected repositories: the cloud store is fully derivable from any
   up-to-date machine (re-upload under the new encoding). No migration code,
   ever.
