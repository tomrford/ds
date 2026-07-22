use std::io::Write as _;

use jj_cli::command_error::CommandError;
use jj_cli::ui::Ui;

const CORE: &str = r#"# Devspace

Devspace keeps native jj repositories on your machine and synchronizes them
through a cloud authority. Git is a projection boundary for sharing selected
history, not Devspace's source of truth. Git writes and all `jj` commands fail;
use `ds` for changes.

## Daily work

- inspect work with `ds status`, `ds log`, and `ds diff`
- seal a change with `ds describe -m '<message>'`, then `ds new`
- publish a sealed change with `ds bookmark set <name> -r @-`, then
  `ds git push -b <name>`
- fetch collaborator work with `ds git fetch [--remote <name>] [-b <bookmark>]`
- choose exactly one checkout mode; create a new child change with
  `ds add <repo> -r <revision> <path>`
- or edit an existing mutable change in another checkout with
  `ds add <repo> --edit <revision> <path>`
- list every workspace for this repository with `ds list`
- remove a checkout, but retain its repository, with `ds remove <path>`
- `ds add --json` and `ds remove --json` emit `root`, `repo`, `workspace_id`
  and the full canonical `change_id`; removal emits null after abandoning a
  disposable head
- `ds list --json` emits
  `[{current, workspace_id, change_id, commit_id, description}]`
- `ds repo list --json` emits `[{name, availability, checkouts}]`; availability
  is `available-locally`, `cloud-only`, or `missing-from-cloud`
- `ds sync status --json` emits
  `{daemon_running, repositories: [{repo, complete, has_sync_state, pending}]}`
- `ds git remote list --json` emits `[{name, url}]`
- JSON modes write only one JSON document and its trailing newline to stdout
- import a Git remote without a checkout with `ds repo add <git-url>`
- create a repository and its first checkout with `ds init [<git-url>] [<path>]`
- create an empty cloud repository without a checkout with `ds repo new [<name>]`
- inspect machine, cloud, daemon, Git and checkout health with `ds doctor`

## Experimental Git compatibility

The best-effort Git index shim is off by default. Enable it for this machine
with `ds config set git-shim true`. The shim does not make Git a supported
write surface.

## Private paths

`.dsprivate` uses gitignore syntax. It is not an ignore file: a matched path
is versioned by Devspace and hidden from every Git projection, and matched
paths are tracked automatically. Run `ds skill private` for details.

## Pinned context

A checkout containing `.repos/` holds read-only reference repositories
pinned by `.repos/.lock`. Run `ds skill context` for details.

## Synchronization

Work is offline-first: commands operate on the local repository and schedule
a background sync afterwards; a machine daemon drains the queue. `ds status`
shows the current checkout's sync line; `ds sync status` covers every local
repository. Neither contacts the cloud.

More detail: `ds skill private`, `ds skill context`, or `ds skill jj`.
"#;

const PRIVATE: &str = r#"# Private paths with `.dsprivate`

Devspace versions private content in native jj history and removes it from
every Git projection. The machine owner and cloud authority can still read
it; `.dsprivate` does not encrypt content.

## Write policy

A `.dsprivate` file contains gitignore patterns anchored at its directory.
Devspace chains policy files from the repository root down, with ordinary
last-match-wins behavior. It supports anchoring, `*`, `**`, directory
patterns, negation, comments, blank lines, and escaped leading `#` or `!`.

Every `.dsprivate` file is itself private. A matched directory hides and
tracks everything below it; a later negation cannot re-include a child
because Devspace does not descend into the hidden directory.

## Track private content

`.dsprivate` is not an ignore file. On the next snapshot, Devspace
automatically tracks each policy file and matching working-copy path,
including files that `.gitignore` excludes. Keep the same paths in
`.gitignore` so plain Git users do not commit local copies.

Removing a pattern does not remove existing content from native history.
Add the path to `.gitignore`, remove the private pattern, then run
`ds file untrack <path>` if the path should stop being versioned.

## Git boundary

Every export excludes `.dsprivate` files and matching paths. Export fails
when a relevant policy file or exported commit is conflicted, because
silently deleting the public side would be unsafe.

Hiding a path after Git published it makes the next public commit delete
the path. Older Git commits still contain the published bytes. If a Git
collaborator publishes content at a private path, fetch preserves the
private value as a jj conflict and warns that the public bytes remain
reachable until someone rewrites the remote history outside Devspace.
"#;

const CONTEXT: &str = r#"# Pinned repository context

Some checkouts include `.repos/` with a `.repos/.lock` file. This is a
grepo-compatible convention for making exact external source snapshots
available beside the project.

Each `.repos/<alias>` entry is a generated link to a shared cached
snapshot: a plain read-only tree with Git metadata stripped. Treat it as
reference material, not project code. Use it to inspect upstream
implementations, APIs, formats, or tests, but do not patch it or depend on
unrecorded changes inside it.

`.repos/.lock` is the tracked source of truth for pinned sources and
revisions. Review it when you need to establish which upstream version the
checkout uses. If `.repos/` or its lock is absent, the checkout does not
provide pinned repository context.
"#;

const JJ: &str = r#"# Devspace and jj

`ds` embeds jj: every jj command, template, revset, and your jj user
config (name, email, aliases) work unchanged — `ds log`, `ds describe`,
`ds rebase`, `ds op undo`, and so on. A user alias whose name matches a
Devspace command is shadowed by the Devspace command. Run `ds jj config` for
jj configuration and `ds help jj` for the full jj command reference.

## Conventions

Seal work as named changes: `ds describe -m '<message>'` then `ds new`.
Publish by pointing a bookmark and pushing it: `ds bookmark set <name>
-r @-`, `ds git push -b <name>`.

## The Git boundary is Devspace's

jj's own Git commands are fenced in a checkout: `ds git clone`, `ds git
init`, `ds git export`, `ds git import`, and colocation refuse to run
because Devspace owns Git projection. Use `ds git push`, `ds git fetch`,
and `ds git remote add/list` instead.

Fetch imports public commits and lifts them onto private lineage, so
private files survive collaborator changes; a force-pushed remote fails
closed. Push projects hidden-safe history and reports success only after
the cloud journal accepts the observed remote refs. Tags, push options,
signing, Git submodules, and SHA-256 remotes are unsupported on this
boundary.
"#;

#[derive(Clone, clap::ValueEnum)]
enum SkillTopic {
    Core,
    Private,
    Context,
    Jj,
}

#[derive(clap::Args)]
pub(crate) struct SkillArgs {
    /// Show detailed guidance for a topic.
    #[arg(value_enum)]
    topic: Option<SkillTopic>,
}

pub(crate) fn print_skill(ui: &mut Ui, args: SkillArgs) -> Result<(), CommandError> {
    let page = match args.topic {
        None | Some(SkillTopic::Core) => CORE,
        Some(SkillTopic::Private) => PRIVATE,
        Some(SkillTopic::Context) => CONTEXT,
        Some(SkillTopic::Jj) => JJ,
    };
    write!(ui.stdout(), "{page}")?;
    Ok(())
}
