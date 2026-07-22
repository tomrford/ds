# Devspace

Devspace synchronizes stock jj repositories through Cloudflare. Local work stays
fast and offline-first; a Cloudflare Durable Object stores the durable replicated
state. A rebuildable Git sidecar projects public history without making Git the
native repository authority.

## Development

Enter the pinned toolchain and run the full gate:

```sh
nix develop
pnpm install
pnpm check
pnpm test
```

The repository gate is also available without entering a shell:

```sh
nix develop -c pnpm check
nix develop -c pnpm test
```

## System model

Devspace has 3 storage boundaries:

- **Machine:** stock jj simple stores hold local repository and operation state.
  A machine-local catalog maps a repository name to its opaque cloud ID and
  incarnation. Checkouts share this repository but keep independent working-copy
  state.
- **Cloud:** one control-plane Durable Object owns the repository directory. One
  repository Durable Object per opaque ID owns immutable canonical objects,
  authoritative operation heads, synchronization receipts, registered Git
  remotes and the projection journal.
- **Git:** a rebuildable bare-Git sidecar translates native history for explicit
  fetch and push commands. Git remotes contain only projected public history.

The Rust validation kernel is independent of `jj-lib`. It validates jj 0.42
simple-store bytes and IDs in native code and as Wasm inside the repository
Durable Object. The accepted cloud schema is exactly jj's simple backend and
simple operation-store schema.

## Daily use

```sh
# Create an empty repository and its first checkout
ds init [<directory>]

# Import a Git remote and create the first checkout
ds init <git-url> [<directory>]

# Create another checkout at an explicit revision
ds add <repository> -r <revision> <path>

# Inspect local repositories and workspaces
ds repo list
ds list
ds -R <repository> log

# Remove a checkout but retain its repository
ds remove <path>

# Inspect local synchronization state
ds sync status

# Use the explicit public Git boundary
ds git remote add <name> <url>
ds git remote list
ds git fetch [--remote <name>] [-b <bookmark>]
ds git push -b <bookmark> [--remote <name>]
```

Ordinary work in an existing checkout is local. `ds repo new`, `ds repo add`,
`ds repo rename`, `ds repo remove`, `ds repo list`, first-use `ds add`, and
`ds init` require the Worker. Git fetch and push also contact their registered
Git remote.

`ds status` adds a local synchronization line to jj's output. It compares local
operation heads with the last accepted cloud heads and any durable local outbox;
it does not contact the Worker. `ds sync status` does the same for every local
catalog repository and pings only the local daemon. Use online repository
commands when current cloud directory or authorization state matters.

## Repositories and checkouts

`MachineStore` owns the protected local configuration, repository catalog,
creation journal, native repositories and synchronization sidecars. Native paths
use the opaque repository ID and incarnation, never the tenant-visible name.
The default root follows the platform data directory; `DEVSPACE_MACHINE_STORE_DIR`
is a bring-up and test override.

Repository creation records an idempotency intent before claiming a cloud name.
A lost response can therefore replay the same request. Git import uses the same
creation path, registers `origin`, imports remote heads and rejects Git links
before the simple backend can encode them.

`ds add` creates a deterministic workspace whose identity combines the machine
ID and canonical destination path. The checkout contains an ownership marker
and a stock repository pointer to the shared native repository. Publication is
staged and atomic; foreign or mismatched destinations are never overwritten.
A later `ds add` uses the local repository unless its first machine clone is
still incomplete.

`ds remove` accepts only an owned checkout. It snapshots pending edits, deletes
the checkout, forgets its workspace and removes its path record while retaining
the native repository. Interrupted and already-missing checkout states are
handled idempotently; moved or unowned directories are left untouched.

`ds -R <repository> log` resolves a local catalog name and opens the bare native
repository without a checkout or cloud request. The bare surface is read-only,
and `@` is unavailable because no working copy is selected.

The optional `devspace.git-shim` jj setting maintains a read-only Git index for
consumers that require one. It is off by default and does not make Git writes a
supported surface.

## Synchronization

After a successful repository command, `ds` sends a bounded local notification
and does not wait for cloud work. The daemon drains complete repositories on
startup, reacts to command notifications, polls every 15 seconds for remote work
and exits after an idle timeout. If daemon startup or notification fails, the
CLI starts a detached one-shot sync.

One locked sync pass:

1. replays any exact pending head transaction;
2. downloads and validates new cloud packs, then lets jj reconcile operation
   heads;
3. discovers local object closures, negotiates inventory and uploads only
   missing canonical objects; and
4. durably records each head request before advancing the cloud head set.

The sidecar stores accepted cloud heads and cursor, the installed pack frontier,
and an exact outbox for ambiguous responses. It does not replace jj's operation
head store. A later pass rediscovers native operations even when no outbox was
written, and a fresh machine can rebuild the exact repository from cloud packs
and heads.

Set `DEVSPACE_DAEMON=0` to use detached one-shots. Set
`DEVSPACE_BOUNDARY_SYNC=0` to disable command-boundary wake-ups entirely. Local
work remains durable and is discovered when synchronization is enabled again.

## Git and private content

`.dsprivate` uses gitignore syntax to select content that remains in native jj
history and cloud replication but is excluded from every Git projection. Policy
is per commit and can be nested. `.dsprivate` files are themselves always hidden.
This is projection privacy, not encryption: the machine and cloud authority can
read the canonical content.

`ds git fetch` imports public Git history and lifts it onto private native
lineage. `ds git push` synchronizes native state first, projects a hidden-safe
Git graph, uploads both native and public commit closures, records an exact
lease-protected journal batch, and reports success only after observing and
finalizing the remote ref result.

## Current authentication

Dogfooding uses one Worker secret and caller-configured machine IDs. The Worker
maps valid requests to one fixed development user, while the control plane and
repository objects still enforce typed user, machine, repository and incarnation
boundaries. First-class device enrolment replaces this development adapter
later; it is not part of the current implementation.

## Reference

- [`docs/sync.md`](docs/sync.md) — native storage, cloud replication,
  convergence and command-boundary recovery
- [`docs/kernel.md`](docs/kernel.md) — canonical jj object validation
- [`docs/hidden.md`](docs/hidden.md) — private-path policy and fetch pollution
- [`docs/git-projection.md`](docs/git-projection.md) — projection state and
  recovery journal
- [`docs/git-push.md`](docs/git-push.md) — public Git publication
- [`docs/git-fetch.md`](docs/git-fetch.md) — Git import, lifting and native view
  updates
