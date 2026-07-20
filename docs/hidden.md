# Hidden files

Devspace versions hidden content in native jj history while excluding it from
every Git projection. Hidden means hidden from Git remotes and plain Git
consumers, not from the machine owner or the cloud authority.

## Policy model

The hidden set is per-commit content. A file named `.dshide` at the repository
root contains gitignore patterns. It is part of each commit's tree, so the
hidden set branches, merges and conflicts through ordinary jj machinery, and
every descendant inherits its parent's set until a commit changes it. Nested
`.dshide` files are ordinary content and may themselves match the root policy.

`.dshide` is itself always hidden. This rule is fixed and not expressed inside
the file.

Patterns use the semantics of jj-lib's `GitIgnoreFile`, backed by gix-ignore.
Anchoring, `*`, `**`, directory patterns, negation, comments, blank lines and
escaped leading `#` or `!` therefore behave like jj's own `.gitignore`
handling. Directories are hideable. An excluded directory is pruned without
descent, so a later negation cannot re-include a child beneath it. Gitignore
syntax has no parse-error state.

The hidden-set identity is the `FileId` of the root `.dshide` blob, or `None`
when the commit has no `.dshide`. A conflicted `.dshide`, or a root `.dshide`
entry that is not a regular file, fails export closed for that commit. The
executable bit is allowed. Nothing is projected until the conflict or entry
type is repaired.

There is no repository-level policy registry, no policy epoch and no
cloud-synchronous policy mutation. Changing the hidden set is an ordinary
commit, serialized and replicated like any other.

## User contract

Edit the root `.dshide` file to change the hidden set. Every snapshot in a
Devspace checkout force-tracks `.dshide` itself and all working-copy files its
current contents match, including files beneath matched directories. An
existing gitignored secret therefore becomes canonical as soon as its pattern
is present for the next ordinary command or explicit snapshot.

`.dshide` is not an ignore file: matching a path versions it privately and
hides it from Git. Keep hidden paths covered by gitignore as well so plain-Git
collaborators do not commit their local copies. Devspace does not edit or infer
`.dshide` policy from `.gitignore`.

Removing a pattern does not untrack matching content or delete it from native
history. Gitignore the path, then run `ds file untrack <path>` to stop tracking
it. A path still tracked after its pattern is removed is eligible for the next
Git publication from descendants of that commit.

## Projection

Export resolves each commit's matcher from that commit's root `.dshide` and
caches matchers by `FileId`. `.dshide` itself is always excluded. Matching
files and symlinks are removed before their objects are read into Git
(filter-before-read); matching directories are pruned without descent;
directories made empty by filtering are omitted. Before push, the projected
tree is walked in full under the canonical head's matcher and any matching
path or root `.dshide` is reported as a leak. Export fails closed on any
conflict in an exported commit. Exporting around an unresolved hidden conflict
would silently publish a deletion of the public side, a public effect nobody
chose, so publication always waits for an explicit resolution.

Changing the hidden set does not rewrite existing Git history. Hiding an
already-published path makes the next public commit delete it; older public
commits keep the bytes they published.

## Journal binding

Projection states bind each published Git object to one private canonical
commit and the identity of the `.dshide` blob under which it was exported.
Fetch interprets a seed under the hidden set recorded by its exact state; when
ancestry reaches a Git object recorded through several bookmarks, the newest
state per bookmark must agree on one private commit and hidden-set identity,
or the seed is ambiguous and fails closed.

## Fetch, inheritance and pollution

Fetch lifts imported public commits onto private lineage (see
`git-fetch.md`). Because lifting replays each public delta onto the merged
private parents, `.dshide` and all hidden values flow to lifted commits
structurally: fetched changes inherit the hidden rules of the lineage they
grew from.

A collaborator can publish bytes at a path the applicable patterns hide.
Devspace tolerates this and never rewrites remote history:

- the remote commit is imported unchanged as immutable public history
- the lifted commit carries a native, non-blocking jj conflict at the path
  (private value against public value); repeated public edits update the
  public side, and a public deletion leaves the private value clean
- conflict surfaces (`ds resolve --list`, log, status) label these conflicts
  explicitly as involving a hidden path, and fetch warns once, loudly, that
  the bytes are public until the remote's history is rewritten externally
- resolution is an ordinary child change; if the private value wins, the next
  push publishes a deletion of the path, never the private bytes

A `.dshide` file pushed by a plain-git collaborator is the same case:
`.dshide` is always hidden, so it produces the same labeled conflict and
warning rather than a special code path.

The guarantee is therefore: Devspace never adds hidden bytes to a Git object;
bytes a collaborator already published remain reachable on the remote until
its history is rewritten outside Devspace.

A public addition at a hidden path with no private seed value becomes a
materializable modify-delete conflict against a synthetic tombstone. The
tombstone is an internal negative merge term chosen from 2 fixed canonical
objects so one can always be selected without colliding with the public file
ID; hidden filtering prevents either object from reaching Git. The exact bytes
of both objects are canonical semantics and must be defined before machines
can lift independently. The proposed shape is 2 distinct self-describing
sentinel texts, so a materialized conflict explains itself in the working copy
without CLI mediation.

Non-file values arriving at a hidden path follow one rule: represent a native
conflict when the simple backend can encode both terms (files, executable
files, symlinks); fail closed otherwise (directories, Git links).

## Encryption boundary

Devspace does not obfuscate or encrypt hidden files before replicating them.
A machine-only key would stop other machines from reading, merging and
materializing the file; giving the key to the cloud returns to the current
trust model without adding secrecy. Server-blind end-to-end encryption is a
separate product with different merge semantics. The boundary is direct: the
canonical store and cloud authority see hidden content; Git storage and Git
remotes do not.

## Open items

- Canonical byte definitions and selection rule for the 2 tombstone objects.
- Hidden-path labeling in conflict surfaces needs a CLI design; the current
  embedded runner exposes only bare-repository `log`.
