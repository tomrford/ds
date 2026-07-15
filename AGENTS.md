# devspace

Cloudflare-native devspace. Plan:
`../devspace-v2-ref-and-docs/docs/specs/cloudflare-v3.md` (the old server is a
loose behavioural reference only; nothing canonical may depend on it).

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
- Golden vectors are canonical bytes with jj ContentHash IDs, originally
  emitted by the old server as a jj-lib 0.42.0 oracle. The kernel rejects
  non-canonical encodings rather than normalizing; normalization is
  machine-side work.
- The accepted schema is exactly jj's simple backend and simple operation-store
  schema. Do not add Devspace-only fields. Values the simple backend cannot
  store, including Git submodules, must fail before encoding.
