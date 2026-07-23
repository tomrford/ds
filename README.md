# Devspace

Devspace is a Cloudflare-native store for Jujutsu repositories. `ds` embeds
Jujutsu, keeps the canonical repository on each machine in a bare Git object
database, and replicates Git objects plus Jujutsu operation history through a
Worker.

The cloud is a durable authority, not a hosted working copy. Checkouts stay
local and disposable. The repository can be rebuilt exactly on a fresh machine
from cloud packs and operation objects.

## Development

Enter the pinned toolchain for every command:

```sh
nix develop -c pnpm check
nix develop -c pnpm test
```

`pnpm check` regenerates Worker types, builds the validation WebAssembly
module, type-checks the Worker, runs Clippy for every Rust target, and checks
Rust formatting. `pnpm test` runs the complete Rust and Worker test suites.

Build the Worker without deploying it:

```sh
nix develop -c pnpm build
```

## System model

One logical repository has three parts:

- a machine catalog entry with the cloud repository identity and local bare
  repository path;
- one bare Git object database shared by every checkout on that machine;
- one cloud `RepositoryGit` Durable Object containing validated Git packs,
  operation objects, operation heads, and the public-Git projection journal.

The Worker `ControlPlaneV2` Durable Object owns repository names, machine
enrollment, authorization, retirement, and repository creation. A repository
Durable Object accepts requests only while the control plane confirms its
incarnation.

Canonical commits, trees, and blobs are ordinary Git objects. Jujutsu
operations and views use jj's simple operation-store protobuf encoding and
Blake2b object IDs. The kernel validates both formats before cloud storage.

`store/extra` is a rebuildable local GitBackend cache. It is never replicated
and is reconstructed from canonical Git object bytes when required.

## Daily use

```sh
# Create an empty cloud repository and its first checkout.
ds repo new project
ds add project ./project --revision 'root()'

# Import a Git remote and create the first checkout.
ds init https://example.com/project.git ./project --name project

# Create another checkout at an explicit revision.
ds add project ./project-feature --revision main

# Create another checkout that edits an existing mutable revision.
ds add project ./project-edit --edit @

# Inspect local repositories and workspaces.
ds repo list
ds list
ds status
ds log

# Remove a disposable checkout but retain its repository.
ds remove ./project-worktree

# Inspect machine synchronization state.
ds sync status

# Configure and use the explicit public Git boundary.
ds git remote add origin git@example.com:owner/project.git
ds git fetch --remote origin
ds git push --remote origin --bookmark main
```

Run `ds --help` for the Devspace command surface and `ds help jj` for the
embedded Jujutsu command reference.

## Repositories and checkouts

`ds repo new` creates a cloud repository and materializes its bare local store.
`ds repo add` imports a Git remote without creating a checkout. `ds init`
imports and creates the first checkout. `ds add` creates further disposable
checkouts against the same machine repository.

The machine catalog is authoritative for local repository names. The cloud
control plane is authoritative for the repository ID, incarnation, and active
name. Creation uses a durable intent so an interrupted command can resume
without creating a second repository.

Every mutating checkout command uses the shared bare repository. Devspace
holds a repository mutation lock around command-boundary recovery and writes
the Jujutsu operation before asynchronous synchronization begins. A checkout
can be deleted without deleting either the machine repository or the cloud
repository.

`ds repo remove` is the destructive repository boundary. It retires the cloud
incarnation before deleting the machine catalog entry and local store.

## Synchronization

Synchronization transfers two content-addressed graphs:

- Git blobs, trees, and commits, addressed by 20-byte SHA-1 object IDs;
- Jujutsu operations and views, addressed by 64-byte Blake2b IDs.

Git objects travel in deterministic `DSPK` v2 packs. Operation objects use
their own closure and pack routes. The machine installs and validates every
download before it changes local operation heads.

A sync run:

1. acquires the repository sync lock and replays its durable outbox;
2. downloads missing Git packs and operation closures from the cloud;
3. reconciles the cloud operation heads into a local Jujutsu operation;
4. uploads newly reachable Git and operation objects;
5. records the intended operation-head transaction in the outbox;
6. advances the cloud heads and clears the acknowledged outbox entry.

Retries are idempotent. Objects are immutable, pack installation is
no-clobber, and operation-head updates compare the expected head set before
commit. A fresh machine can rebuild the bare repository from cloud bytes
alone.

See [Synchronization and convergence](docs/sync.md) for the complete contract.

## Git and private content

Normal Devspace synchronization retains canonical history, including paths
matched by `.dsprivate`. Public Git remotes are a separate boundary implemented
by `ds git push` and `ds git fetch`.

Push projects canonical Git history into the same object database. A commit
whose tree and parent cone are unchanged is an identity projection: its
canonical and public OIDs are equal, no mirror object is written, and existing
Git signatures remain intact. A commit affected by hidden paths or a rewritten
parent gets a minimal public tree and commit rewrite.

Fetch applies the inverse overlay operation. Foreign public commits are
replayed over the canonical hidden state recorded by their projected parents.
Hidden-free history remains byte-identical. If a foreign commit contains a
path that the inherited policy marks hidden, Devspace emits a data-disclosure
warning and represents the collision in canonical history instead of silently
accepting it.

The cloud journal binds each active remote bookmark to a
`canonicalOid`/`publicOid` pair and a nullable hidden-set identity. Leases and
durable batches make Git subprocess success recoverable across process or
network failure.

See [Hidden files](docs/hidden.md), [Git projection](docs/git-projection.md),
[Git push](docs/git-push.md), and [Git fetch](docs/git-fetch.md).

## Current authentication

Machines authenticate with an enrolled machine ID and shared secret. The
Worker hashes the secret before lookup and authorizes every repository request
through the control plane. Repository incarnations prevent a retired Durable
Object from becoming active again.

Authentication is intentionally machine-scoped. User accounts, interactive
login, and delegated repository sharing are outside the current contract.

## Reference

- [Validation kernel](docs/kernel.md)
- [Synchronization and convergence](docs/sync.md)
- [Git projection](docs/git-projection.md)
- [Git push](docs/git-push.md)
- [Git fetch](docs/git-fetch.md)
- [Hidden files](docs/hidden.md)
