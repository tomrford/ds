# Hidden files

Devspace versions exact file paths in native jj history while excluding them
from every Git projection. Hidden means hidden from Git remotes and plain Git
consumers, not from the machine owner or the cloud authority.

This specification supersedes the v2 `hidden-files.md`. The projection
invariants are unchanged; the policy model is not.

## Policy model

The hidden set is per-commit content. A repository-root file named `.dshide`
lists one exact, repository-relative file path per line. The file is part of
each commit's tree, so the hidden set branches, merges and conflicts through
ordinary jj machinery, and every descendant inherits its parent's set until a
commit changes it.

`.dshide` is itself always hidden. This rule is fixed and not expressed inside
the file.

Paths are exact file paths. Directories, absolute paths, parent traversal,
repository-root aliases and duplicate entries are rejected. A malformed or
conflicted `.dshide` fails export closed for that commit; nothing is projected
until the file is repaired or the conflict resolved.

There is no repository-level policy registry, no policy epoch and no
cloud-synchronous policy mutation. Changing the hidden set is an ordinary
commit, serialized and replicated like any other.

## User contract

```sh
ds hidden add .env
ds hidden list
ds hidden remove .env
```

The verbs are sugar over editing `.dshide` in the working copy; editing the
file directly is equivalent. `ds hidden add` also force-tracks the path in the
same snapshot, so an existing gitignored file becomes canonical immediately.
Devspace does not edit or infer policy from `.gitignore`; hidden paths should
usually also be gitignored so plain-git collaborators never commit their local
copies, and the CLI warns when they are not.

Removing a path does not delete it from native history. It makes the content
eligible for the next Git publication from descendants of that commit, so the
CLI warns.

## Projection

Export filters each commit under that commit's own `.dshide` plus `.dshide`
itself. Hidden paths are removed before their file objects are read into Git
(filter-before-read), directories made empty by filtering are omitted, and
every projected head is scanned again before push. Export fails closed on any
conflict in an exported commit; a conflict at a hidden path is labeled as
hidden-involved in the error. Exporting around an unresolved hidden conflict
would silently publish a deletion of the public side, a public effect nobody
chose, so publication always waits for an explicit resolution.

Changing the hidden set does not rewrite existing Git history. Hiding an
already-published path makes the next public commit delete it; older public
commits keep the bytes they published.

## Journal binding

Projection states bind each published Git object to one private canonical
commit and the identity of the `.dshide` blob under which it was exported.
This replaces the v2 policy epoch. Fetch interprets a seed under the hidden
set recorded by its exact state; when ancestry reaches a Git object recorded
through several bookmarks, the newest state per bookmark must agree on one
private commit and hidden-set identity, or the seed is ambiguous and fails
closed.

## Fetch, inheritance and pollution

Fetch lifts imported public commits onto private lineage (see
`git-fetch.md`). Because lifting replays each public delta onto the merged
private parents, `.dshide` and all hidden values flow to lifted commits
structurally: fetched changes inherit the hidden rules of the lineage they
grew from.

A collaborator can publish bytes at a path the applicable set hides. Devspace
tolerates this and never rewrites remote history:

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

Non-file values arriving at an exact hidden path follow one rule: represent a
native conflict when the simple backend can encode both terms (files,
executable files, symlinks); fail closed otherwise (directories, Git links).

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
- The `.dshide` grammar (comments? ordering?) must be frozen before golden
  fixtures exist; entries are exact paths, so no glob semantics are planned.
- The spike-era repository-scoped policy routes and epoch tables in the cloud
  journal predate this model and are retired by the fetch/lift work.
- Hidden-path labeling in conflict surfaces needs a CLI design once the
  command runner exists.
