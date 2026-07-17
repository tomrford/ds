use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use devspace_machine::{CatalogEntry, MachineStore};

pub(crate) const BOUNDARY_SYNC_ENV: &str = "DEVSPACE_BOUNDARY_SYNC";

#[derive(Default)]
struct BoundarySyncState {
    suppressed: bool,
    repositories: BTreeMap<String, PathBuf>,
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
        .insert(
            entry.name.as_str().to_owned(),
            repository_directory.to_owned(),
        );
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
    if std::env::var_os(BOUNDARY_SYNC_ENV).is_some_and(|value| value == "0") {
        return;
    }
    let repositories = {
        let mut state = state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.suppressed {
            return;
        }
        std::mem::take(&mut state.repositories)
    };
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    for (name, repository_directory) in repositories {
        let Ok(log) = File::create(repository_directory.join("sync.log")) else {
            continue;
        };
        let Ok(error_log) = log.try_clone() else {
            continue;
        };
        let mut command = Command::new(&executable);
        command
            .args(["sync", "run", "--repository", &name])
            // The parent's cwd may no longer exist (`ds remove .` deletes it);
            // give the child a directory that outlives the command.
            .current_dir(&repository_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(error_log));
        detach(&mut command);
        let _ = command.spawn();
    }
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
