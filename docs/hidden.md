# Hidden files

Devspace versions hidden content in canonical Jujutsu history while excluding
it from every public Git projection. Hidden means hidden from Git remotes and
plain Git consumers, not from the machine owner or the Devspace cloud
authority.

## Policy model

The hidden set is per-commit content. A `.dsprivate` file in any directory
contains gitignore patterns anchored at that directory. Files are chained from
the repository root down: deeper files override shallower files through
ordinary last-match-wins gitignore semantics. The files are part of each
commit's tree, so the hidden set branches, merges, and conflicts through
ordinary Jujutsu machinery. A descendant inherits its parent's set until a
commit changes it.

Every `.dsprivate` file is always hidden at every depth. This rule is fixed and
is not expressed inside the file.

Patterns use jj-lib's `GitIgnoreFile`, backed by gix-ignore. Anchoring, `*`,
`**`, directory patterns, negation, comments, blank lines, and escaped leading
`#` or `!` therefore match Jujutsu's `.gitignore` behavior. An excluded
directory is pruned without descent, so a later negation cannot re-include a
child beneath it. Gitignore syntax has no parse-error state.

The hidden-set identity is absent when the commit contains no `.dsprivate`
files. Otherwise it is Blake2b-512 over `devspace-hidden-set-v1` followed by
every `.dsprivate` in repository-path byte order. Each entry contains the path
byte length as an unsigned 64-bit little-endian integer, its UTF-8 path bytes,
and its 64-byte Jujutsu blob ID.

A conflicted `.dsprivate`, or one that is not a regular file, fails projection
closed for that commit. The executable bit is allowed. No public commit is
created until the policy entry is repaired.

There is no repository-level policy registry or policy epoch. Changing the
hidden set is an ordinary canonical commit, replicated like any other.

## User contract

Edit the applicable `.dsprivate` file to change the hidden set. Every snapshot
discovers these files top-down, chains each file at its directory, and
force-tracks every discovered policy file and matched working-copy path,
including files beneath a matched directory.

Discovery does not descend into a directory already hidden by the chained
private matcher. It also does not descend into a gitignored directory unless
the private matcher selects that directory. The base ignore chain matches
jj-cli's GitBackend path: the backend repository's configured global excludes
plus `.git/info/exclude`. Root `.jj` and `.git` directories are skipped.

An existing ignored secret therefore becomes canonical as soon as an
applicable private pattern is present for the next ordinary command or
explicit snapshot.

`.dsprivate` is not an ignore file: matching a path versions it privately and
hides it from public Git. Keep private paths covered by `.gitignore` as well so
plain-Git collaborators do not commit local copies. Devspace does not infer
private policy from `.gitignore`.

Removing a pattern does not untrack content or delete it from canonical
history. Gitignore the path, then run `ds file untrack <path>` to stop tracking
it. A path that remains tracked after its pattern is removed is eligible for
the next public projection.

## Checkout Git shim

Some local tools require a `.git` directory even though Jujutsu owns the
checkout. Devspace maintains a guarded Git index shim whose object alternate
points at the canonical bare Git object database.

The shim excludes `.dsprivate`, private paths, base-ignored paths, and
fail-closed policy roots before it runs `git add -A`. It then restores
canonical public files that a base ignore would otherwise omit. The `.git`
directory is made read-only outside a guarded refresh.

The shim is a compatibility view, not a writable repository boundary. It does
not define canonical history and cannot be used to bypass projected
`ds git push`.

## Projection

Projection resolves every `.dsprivate` in each canonical commit. As the tree
walk enters a directory, it chains that directory's policy before filtering
entries.

Policy blob reads are cached by Jujutsu blob ID. Rewritten subtrees are cached
by source tree, path, and effective policy-chain identity. Every `.dsprivate`
is excluded. Matching files and symlinks are removed before their objects are
read. Matching directories are pruned without descent. Empty filtered
directories are omitted.

Before push, Devspace walks the complete public tree under the canonical
commit's prefix-aware matcher. Any matching path or `.dsprivate` is a hard
leak error.

Projection is Git-to-Git in one object database. Hidden-free commits and
unchanged parent cones keep their canonical OID. A hidden path or rewritten
parent creates only the affected public trees and commit. Existing public
history is immutable: hiding an already published path makes the next public
commit delete it, but older public commits retain the published bytes.

Canonical conflicted commits fail public projection. Publishing around an
unresolved private conflict could silently choose a public deletion, so the
boundary waits for explicit resolution.

## Journal binding

A journal state binds:

```text
canonicalOid  publicOid  hiddenSetId?
```

An identity commit uses one OID on both sides and needs no mapping receipt.
A rewritten commit stores the pair and effective hidden-set identity. One
canonical OID cannot be bound to two public OIDs.

Fetch uses these pairs as overlay-lift seeds. The public OID anchors immutable
remote history; the canonical OID and hidden-set lineage anchor the private
overlay.

## Fetch, inheritance, and disclosure

Fetch replays every new public delta over its canonical parent state. Private
policy and private values therefore flow structurally into canonical mirrored
commits. Hidden-free history remains identity history.

A collaborator can publish bytes at a path the inherited policy hides.
Devspace does not rewrite the remote:

- the public Git commit remains unchanged;
- fetch prints a `WARNING: DATA DISCLOSURE` diagnostic;
- the canonical mirror carries a normal Jujutsu conflict between the public
  value and the private side;
- repeated public edits update the public side, and a public deletion leaves
  the private value clean;
- if the private value wins, the next push publishes a deletion, never the
  private bytes.

A published `.dsprivate` file is the same case because `.dsprivate` is always
hidden.

The guarantee is precise: Devspace never adds hidden bytes to a public Git
object. Bytes that a collaborator already published remain on the remote until
its history is rewritten outside Devspace.

A public addition at a hidden path with no private seed becomes a
materializable modify-delete conflict against a synthetic tombstone. The
tombstone is an internal negative merge term selected from two fixed canonical
file objects. Tombstone A is used unless the public bytes equal tombstone A, in
which case tombstone B prevents conflict simplification.

Tombstone A has these exact UTF-8 bytes, including LF line endings and the
trailing newline:

```text
This conflict placeholder was inserted by Devspace.
A collaborator published this file at a path this repository keeps
private. The other side of this conflict is their published content;
no private value existed here. Keep the content this file should have
privately; deleting the file publishes a deletion on the next push.
devspace-tombstone-v1-a
```

Tombstone B is byte-for-byte identical except for its final line:

```text
devspace-tombstone-v1-b
```

Files, executable files, and symlinks use the same native conflict. A public
directory at a hidden path resolves per file. Git submodules are unsupported
because jj's GitBackend commit schema cannot store them.

## Encryption boundary

Devspace does not encrypt hidden files before cloud replication. A
machine-only key would stop other machines from reading, merging, and
materializing the file; giving the key to the cloud returns to the current
trust model.

Server-blind end-to-end encryption is a separate product with different merge
semantics. The boundary is direct: canonical machine stores and the cloud
authority see hidden content; public Git objects and Git remotes do not.
