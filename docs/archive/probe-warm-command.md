> Archived probe report (2026-07-15). Conclusions are folded into the product
> specs and subsystem docs; this file is evidence, not current guidance.

# Warm command latency probe

## Verdict

The warm-command gate holds. Across 3 pair-only runs, the worst measured
`ds-probe / jj-probe` ratio was 1.0135x. This is 0.9865 ratio points below the
2x limit and uses 50.7% of the allowed ratio.

Both fixture sizes produced the same result within process-timing noise. The
Devspace path added 0.029 ms on the 64-operation fixture and 0.035 ms on the
1,000-operation fixture.

| Fixture | Contents | `ds-probe` median | `ds-probe` p10 to p90 | `jj-probe` median | `jj-probe` p10 to p90 | Ratio | Margin to 2x |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Small | 64 operations, 64 commits, 64 files, 64 bookmarks | 3.072 ms | 3.017 to 3.186 ms | 3.043 ms | 2.986 to 3.148 ms | 1.0093x | 0.9907x |
| Large | 1,000 operations, 400 commits, 400 files, 1,000 bookmarks | 3.616 ms | 3.547 to 3.817 ms | 3.581 ms | 3.516 to 3.801 ms | 1.0098x | 0.9902x |

Each median contains 101 complete process runs after 10 warm-up pairs. The
runner reversed the first binary in each pair. It sent stdout to `/dev/null`,
but each child still formatted and wrote 52 lines. Before timing, the runner
confirmed that both probes produced byte-for-byte identical output.

The table reports the final pair-only run. Across all 3 pair-only runs, the
small-fixture ratio ranged from 1.0093x to 1.0135x. The large-fixture ratio
ranged from 1.0078x to 1.0134x.

## Command path

`ds-probe` calls `MachineRepository::open()`. `jj-probe` calls
`RepoLoader::init_from_file_system()` followed by `load_at_head()`. Both then
call the same shared function, which:

- reads the loaded operation ID and view
- starts from the visible heads
- walks parents in deterministic order
- loads and renders up to 50 commits
- writes each commit ID, change ID and first description line to stdout

Neither probe creates or snapshots a working copy. Process creation, async
runtime setup, repository loading, commit walking and output are all inside the
timed interval.

The fixtures extend the existing warm-open fixture shape. Every operation adds
a remote bookmark, which grows the stored view. The first 64 small-fixture
operations and first 400 large-fixture operations also add a commit and file.

The probes and fixture builder are Cargo examples so they can use the existing
Tokio development dependency. The probe added no dependency or lockfile change.
All measurements used release binaries with the repository's `opt-level = "s"`,
single codegen unit, LTO, stripping and abort-on-panic settings. `hyperfine` was
not available, so the std-only `probe-runner` measured `Command::status()` with
`Instant`.

## Where the overhead lives

The only Devspace-specific work on the measured path is in
`MachineRepository::open()`. It reads and validates the 5 stock jj store-type
markers, constructs the same `RepoLoader`, calls the same `load_at_head()`, and
stores the path and loaded repository in a wrapper.

The existing release-only open probe reproduced its earlier result on this
machine:

| Path | Median |
| --- | ---: |
| stock jj repository open | 133.641 microseconds |
| `MachineRepository::open()` | 175.460 microseconds |
| ratio | 1.313x |
| absolute difference | 41.819 microseconds |

The complete command ratio is lower because process startup and the shared jj
work dominate the fixed validation cost. The complete-command differences of
29 to 35 microseconds are below the open probe's 41.819 microseconds and within
the expected noise of separate process measurements. There is no evidence of a
second source of Devspace overhead.

The large fixture added 0.544 ms to `ds-probe` and 0.538 ms to `jj-probe`. The
6 microsecond difference is not meaningful at this resolution. The added cost
therefore sits in shared jj work, such as loading the larger view and repository
objects. This probe does not separate operation depth from view size because it
increases both. It shows no Devspace-specific cost that grows with repository
size.

## Zero-cloud proof

The measured entry point imports only `MachineRepository` from the Devspace
crate. `MachineRepository::open()` reads local type files and enters jj's local
`RepoLoader`; it does not construct `HttpTransport`, `SyncEngine` or any other
cloud component.

A release `ds-probe` run completed successfully under macOS `sandbox-exec` with
`network*` denied. A release-binary string scan found no `HttpTransport`,
`reqwest` or `hyper-rustls` identifiers. The workspace enables jj-lib's Git
feature, so both probes contain generic jj Git documentation strings. This is
not a Devspace cloud path and both probes have the same dynamic library set.

The zero-cloud requirement therefore holds by the source call graph and by the
network-denied execution.

## Stock jj CLI sanity check

The installed CLI is jj 0.41.0, while the probes use pinned jj-lib 0.42.0. A
`MachineRepository` is a bare jj repository store, not a jj workspace, so the
CLI could not open it directly. A temporary `.jj` workspace wrapper made the
same store readable with ignore-working-copy semantics.

The fixture's remote bookmarks have `New` state. The CLI's default log revset
therefore showed only the root commit, so the sanity run used `-r 'all()'` to
render the same 50 visible commits. In a separate 101-sample rotating run, the
CLI medians were 10.740 ms for the small fixture and 11.827 ms for the large
fixture. These figures include the full CLI parser, config, revset and template
stack. They are a sanity reference only and are not the gate denominator.

The real repo-targeted command runner should open the bare machine repository
directly, as the paired probes do. It should not construct temporary workspace
state or put CLI parsing work into the comparison against the jj-lib baseline.

## Machine context

- machine: Tom's Mac mini, Apple M4, 16 GB memory, AC power
- operating system: macOS 26.5.2, build 25F84, arm64
- toolchain: Rust 1.97.0 and Cargo 1.97.0 through `nix develop`
- libraries: jj-lib 0.42.0 and Tokio 1.52.3
- measurement time: 15 July 2026 at 8.18pm CEST
- uptime and load at capture: 2 days 18 hours; 5.43, 4.19, 2.78
