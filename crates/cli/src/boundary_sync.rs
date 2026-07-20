use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use devspace_machine::{CatalogEntry, MachineStore, RepositoryName};

pub(crate) const BOUNDARY_SYNC_ENV: &str = "DEVSPACE_BOUNDARY_SYNC";
pub(crate) const DAEMON_ENV: &str = "DEVSPACE_DAEMON";

const DAEMON_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(5),
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(40),
];

#[derive(Default)]
struct BoundarySyncState {
    suppressed: bool,
    repositories: BTreeMap<RepositoryName, PathBuf>,
    checkouts: BTreeSet<PathBuf>,
}

pub(crate) fn record_checkout(path: &Path) {
    state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .checkouts
        .insert(path.to_owned());
}

fn state() -> &'static Mutex<BoundarySyncState> {
    static STATE: OnceLock<Mutex<BoundarySyncState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(BoundarySyncState::default()))
}

pub(crate) fn record(entry: &CatalogEntry) {
    let Some(repository_directory) = entry.native_repository_path.parent() else {
        return;
    };
    state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .repositories
        .insert(entry.name.clone(), repository_directory.to_owned());
}

pub(crate) fn record_repository_path(path: &Path) {
    let Ok(store) = MachineStore::platform_default() else {
        return;
    };
    let Ok(entries) = store.entries() else {
        return;
    };
    let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    if let Some(entry) = entries.into_iter().find(|entry| {
        dunce::canonicalize(&entry.native_repository_path)
            .unwrap_or_else(|_| entry.native_repository_path.clone())
            == path
    }) {
        record(&entry);
    }
}

pub(crate) fn suppress() {
    let mut state = state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.suppressed = true;
    state.repositories.clear();
}

pub(crate) fn spawn_recorded() {
    let (suppressed, repositories, checkouts) = {
        let mut state = state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (
            state.suppressed,
            std::mem::take(&mut state.repositories),
            std::mem::take(&mut state.checkouts),
        )
    };
    for checkout in checkouts {
        crate::git_shim::ensure(&checkout);
    }
    if suppressed || std::env::var_os(BOUNDARY_SYNC_ENV).is_some_and(|value| value == "0") {
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let daemon_enabled =
        crate::daemon::SUPPORTED && !std::env::var_os(DAEMON_ENV).is_some_and(|value| value == "0");
    let store = daemon_enabled
        .then(MachineStore::platform_default)
        .and_then(Result::ok);
    for (name, repository_directory) in repositories {
        if let Some(store) = &store {
            if crate::daemon::notify_sync(store, &name) {
                continue;
            }
            spawn_daemon(&executable, store.root());
            if DAEMON_RETRY_DELAYS.iter().any(|delay| {
                thread::sleep(*delay);
                crate::daemon::notify_sync(store, &name)
            }) {
                continue;
            }
        }
        spawn_one_shot(&executable, &name, &repository_directory);
    }
}

fn spawn_daemon(executable: &Path, machine_store_root: &Path) {
    let mut command = Command::new(executable);
    command
        .args(["daemon", "run"])
        .current_dir(machine_store_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    let _ = command.spawn();
}

fn spawn_one_shot(executable: &Path, name: &RepositoryName, repository_directory: &Path) {
    let Ok(log) = File::create(repository_directory.join("sync.log")) else {
        return;
    };
    let Ok(error_log) = log.try_clone() else {
        return;
    };
    let mut command = Command::new(executable);
    command
        .args(["sync", "run", "--repository", name.as_str()])
        // The parent's cwd may no longer exist (`ds remove .` deletes it);
        // give the child a directory that outlives the command.
        .current_dir(repository_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log));
    detach(&mut command);
    let _ = command.spawn();
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) {}
