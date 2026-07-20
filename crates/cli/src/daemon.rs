#[cfg(unix)]
mod platform {
    use std::collections::{BTreeSet, VecDeque};
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, BufRead as _, Read as _, Write as _};
    use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _, PermissionsExt as _};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
    use std::thread;
    use std::time::{Duration, Instant};

    use devspace_machine::{CatalogEntry, MachineStore, RepositoryName};
    use jj_cli::cli_util::CommandHelper;
    use jj_cli::command_error::{CommandError, user_error};
    use jj_cli::ui::Ui;

    use crate::checkout::reject_unsupported_global_options;
    use crate::sync::{SyncRun, run_sync_entry};

    const DAEMON_LOCK_FILE: &str = "daemon.lock";
    const DAEMON_LOG_FILE: &str = "daemon.log";
    const DAEMON_SOCKET_FILE: &str = "daemon.sock";
    const POLL_INTERVAL: Duration = Duration::from_secs(15);
    const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
    const LOOP_INTERVAL: Duration = Duration::from_millis(25);
    const MAX_NOTIFICATION_BYTES: u64 = 512;

    // Integration tests opt into short process timers through these hooks. They
    // are deliberately test-prefixed and have no effect unless the marker is set.
    const TEST_HOOKS_ENV: &str = "DEVSPACE_DAEMON_TEST_HOOKS";
    const TEST_POLL_MS_ENV: &str = "DEVSPACE_DAEMON_TEST_POLL_MS";
    const TEST_IDLE_MS_ENV: &str = "DEVSPACE_DAEMON_TEST_IDLE_MS";

    #[derive(clap::Args)]
    pub(crate) struct DaemonArgs {
        #[command(subcommand)]
        command: DaemonCommand,
    }

    #[derive(clap::Subcommand)]
    enum DaemonCommand {
        /// Run the machine-store synchronization daemon in the foreground.
        Run,
    }

    pub(crate) async fn run_daemon(
        ui: &mut Ui,
        command: &CommandHelper,
        args: DaemonArgs,
    ) -> Result<(), CommandError> {
        match args.command {
            DaemonCommand::Run => run(ui, command).await,
        }
    }

    async fn run(ui: &mut Ui, command: &CommandHelper) -> Result<(), CommandError> {
        reject_unsupported_global_options(command, "daemon run")?;
        let store =
            MachineStore::platform_default().map_err(|error| user_error(error.to_string()))?;
        protect_store_root(store.root()).map_err(user_error)?;
        let Some(_lock) = try_lock_daemon(store.root()).map_err(user_error)? else {
            writeln!(
                ui.status(),
                "The devspace daemon is already running; exiting."
            )?;
            return Ok(());
        };

        let mut log = open_log(store.root()).map_err(user_error)?;
        let socket_path = store.root().join(DAEMON_SOCKET_FILE);
        remove_stale_socket(&socket_path).map_err(user_error)?;
        let listener = UnixListener::bind(&socket_path).map_err(|error| {
            user_error(format!(
                "failed to bind daemon socket at {}: {error}",
                socket_path.display()
            ))
        })?;
        let _socket = SocketCleanup::new(socket_path.clone());
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            user_error(format!(
                "failed to protect daemon socket at {}: {error}",
                socket_path.display()
            ))
        })?;
        let events = spawn_socket_listener(listener).map_err(user_error)?;

        let (poll_interval, idle_timeout) = daemon_intervals();
        let mut queue = VecDeque::new();
        let mut queued = BTreeSet::new();
        enqueue_complete_catalog(&store, &mut queue, &mut queued).map_err(user_error)?;
        let mut last_notification = Instant::now();
        let mut next_poll = Instant::now() + poll_interval;
        writeln!(log, "daemon started")?;

        loop {
            drain_socket_events(
                &events,
                &store,
                &mut queue,
                &mut queued,
                &mut log,
                &mut last_notification,
            )
            .map_err(user_error)?;

            let now = Instant::now();
            if now >= next_poll {
                if let Err(error) = enqueue_complete_catalog(&store, &mut queue, &mut queued) {
                    writeln!(log, "poll failed: {error}")?;
                }
                next_poll = now + poll_interval;
            }
            if now.duration_since(last_notification) >= idle_timeout {
                writeln!(log, "daemon exiting after idle timeout")?;
                return Ok(());
            }

            if let Some(name) = queue.pop_front() {
                queued.remove(&name);
                sync_repository(&store, &name, command, &mut log).await?;
                continue;
            }

            let until_poll = next_poll.saturating_duration_since(Instant::now());
            let until_idle =
                idle_timeout.saturating_sub(Instant::now().duration_since(last_notification));
            thread::sleep(LOOP_INTERVAL.min(until_poll).min(until_idle));
        }
    }

    async fn sync_repository(
        store: &MachineStore,
        name: &RepositoryName,
        command: &CommandHelper,
        log: &mut File,
    ) -> Result<(), CommandError> {
        let Some(entry) = store
            .resolve(name)
            .map_err(|error| user_error(error.to_string()))?
        else {
            writeln!(
                log,
                "repository `{name}` disappeared before synchronization"
            )?;
            return Ok(());
        };
        if !is_complete(&entry) {
            writeln!(log, "repository `{name}` is incomplete; skipping")?;
            return Ok(());
        }

        writeln!(log, "sync `{name}` started")?;
        match run_sync_entry(store, &entry, command.settings()).await {
            Ok(SyncRun::Completed) => writeln!(log, "sync `{name}` completed")?,
            Ok(SyncRun::AlreadyLocked) => writeln!(log, "sync `{name}` skipped: lock held")?,
            Err(error) => writeln!(log, "sync `{name}` failed: {error}")?,
        }
        Ok(())
    }

    fn drain_socket_events(
        events: &Receiver<SocketEvent>,
        store: &MachineStore,
        queue: &mut VecDeque<RepositoryName>,
        queued: &mut BTreeSet<RepositoryName>,
        log: &mut File,
        last_notification: &mut Instant,
    ) -> Result<(), String> {
        loop {
            let event = match events.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    return Err("daemon socket listener stopped unexpectedly".to_owned());
                }
            };
            match event {
                SocketEvent::Ping => *last_notification = Instant::now(),
                SocketEvent::Sync(name) => {
                    *last_notification = Instant::now();
                    match store.resolve(&name).map_err(|error| error.to_string())? {
                        Some(entry) if is_complete(&entry) => enqueue(name, queue, queued),
                        Some(_) => {
                            writeln!(
                                log,
                                "repository `{name}` is incomplete; notification ignored"
                            )
                            .map_err(|error| error.to_string())?;
                        }
                        None => {
                            writeln!(log, "unknown repository `{name}` in notification")
                                .map_err(|error| error.to_string())?;
                        }
                    }
                }
                SocketEvent::UnknownInput(input) => {
                    *last_notification = Instant::now();
                    writeln!(log, "ignored unknown daemon input: {input:?}")
                        .map_err(|error| error.to_string())?;
                }
                SocketEvent::MalformedInput => {
                    *last_notification = Instant::now();
                    writeln!(log, "ignored malformed daemon input")
                        .map_err(|error| error.to_string())?;
                }
                SocketEvent::EmptyInput => {
                    writeln!(log, "ignored empty daemon input")
                        .map_err(|error| error.to_string())?;
                }
                SocketEvent::ConnectionFailed(error) => {
                    writeln!(log, "daemon connection failed: {error}")
                        .map_err(|error| error.to_string())?;
                }
            }
        }
    }

    fn spawn_socket_listener(listener: UnixListener) -> Result<Receiver<SocketEvent>, String> {
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("devspace-daemon-socket".to_owned())
            .spawn(move || listen(listener, &sender))
            .map_err(|error| format!("failed to start daemon socket listener: {error}"))?;
        Ok(receiver)
    }

    fn listen(listener: UnixListener, events: &Sender<SocketEvent>) {
        loop {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) => {
                    send(
                        events,
                        SocketEvent::ConnectionFailed(format!(
                            "failed to accept daemon notification: {error}"
                        )),
                    );
                    return;
                }
            };
            if let Err(error) = stream.set_read_timeout(Some(Duration::from_secs(1)))
                && error.kind() != io::ErrorKind::InvalidInput
            {
                send(
                    events,
                    SocketEvent::ConnectionFailed(format!(
                        "failed to configure daemon connection: {error}"
                    )),
                );
                continue;
            }
            if let Err(error) = handle_notification(&mut stream, events) {
                send(events, SocketEvent::ConnectionFailed(error));
            }
        }
    }

    fn handle_notification(
        stream: &mut UnixStream,
        events: &Sender<SocketEvent>,
    ) -> Result<(), String> {
        let mut line = String::new();
        let bytes = {
            let mut reader = io::BufReader::new(&mut *stream).take(MAX_NOTIFICATION_BYTES + 1);
            reader
                .read_line(&mut line)
                .map_err(|error| format!("failed to read daemon notification: {error}"))?
        };
        if bytes == 0 {
            send(events, SocketEvent::EmptyInput);
            return Ok(());
        }
        if bytes as u64 > MAX_NOTIFICATION_BYTES || !line.ends_with('\n') {
            send(events, SocketEvent::MalformedInput);
            return Ok(());
        }
        let input = line.strip_suffix('\n').expect("newline checked above");
        if input == "ping" {
            send(events, SocketEvent::Ping);
            stream
                .write_all(b"pong\n")
                .map_err(|error| format!("failed to answer daemon ping: {error}"))?;
            return Ok(());
        }
        if let Some(raw_name) = input.strip_prefix("sync ")
            && let Ok(name) = RepositoryName::parse(raw_name)
        {
            send(events, SocketEvent::Sync(name));
            return Ok(());
        }
        send(events, SocketEvent::UnknownInput(input.to_owned()));
        Ok(())
    }

    fn send(events: &Sender<SocketEvent>, event: SocketEvent) {
        let _ = events.send(event);
    }

    fn enqueue_complete_catalog(
        store: &MachineStore,
        queue: &mut VecDeque<RepositoryName>,
        queued: &mut BTreeSet<RepositoryName>,
    ) -> Result<(), String> {
        for entry in store.entries().map_err(|error| error.to_string())? {
            if is_complete(&entry) {
                enqueue(entry.name, queue, queued);
            }
        }
        Ok(())
    }

    fn enqueue(
        name: RepositoryName,
        queue: &mut VecDeque<RepositoryName>,
        queued: &mut BTreeSet<RepositoryName>,
    ) {
        if queued.insert(name.clone()) {
            queue.push_back(name);
        }
    }

    fn is_complete(entry: &CatalogEntry) -> bool {
        entry.native_repository_path.is_dir()
    }

    fn daemon_intervals() -> (Duration, Duration) {
        if std::env::var_os(TEST_HOOKS_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
            return (POLL_INTERVAL, IDLE_TIMEOUT);
        }
        (
            test_duration(TEST_POLL_MS_ENV).unwrap_or(POLL_INTERVAL),
            test_duration(TEST_IDLE_MS_ENV).unwrap_or(IDLE_TIMEOUT),
        )
    }

    fn test_duration(name: &str) -> Option<Duration> {
        let milliseconds = std::env::var(name).ok()?.parse::<u64>().ok()?;
        (milliseconds > 0).then(|| Duration::from_millis(milliseconds))
    }

    fn protect_store_root(root: &Path) -> Result<(), String> {
        fs::create_dir_all(root).map_err(|error| {
            format!(
                "failed to create machine store at {}: {error}",
                root.display()
            )
        })?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).map_err(|error| {
            format!(
                "failed to protect machine store at {}: {error}",
                root.display()
            )
        })
    }

    fn try_lock_daemon(root: &Path) -> Result<Option<DaemonLock>, String> {
        let path = root.join(DAEMON_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|error| {
                format!("failed to open daemon lock at {}: {error}", path.display())
            })?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            format!(
                "failed to protect daemon lock at {}: {error}",
                path.display()
            )
        })?;
        match file.try_lock() {
            Ok(()) => Ok(Some(DaemonLock { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(error)) => Err(format!(
                "failed to lock daemon lock at {}: {error}",
                path.display()
            )),
        }
    }

    fn open_log(root: &Path) -> Result<File, String> {
        let path = root.join(DAEMON_LOG_FILE);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("failed to open daemon log at {}: {error}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            format!(
                "failed to protect daemon log at {}: {error}",
                path.display()
            )
        })?;
        Ok(file)
    }

    fn remove_stale_socket(path: &Path) -> Result<(), String> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() || metadata.file_type().is_file() => {
                fs::remove_file(path).map_err(|error| {
                    format!(
                        "failed to remove stale daemon socket at {}: {error}",
                        path.display()
                    )
                })
            }
            Ok(_) => Err(format!(
                "cannot replace stale daemon socket at {} because it is not a file",
                path.display()
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "failed to inspect daemon socket at {}: {error}",
                path.display()
            )),
        }
    }

    struct DaemonLock {
        _file: File,
    }

    enum SocketEvent {
        Ping,
        Sync(RepositoryName),
        UnknownInput(String),
        MalformedInput,
        EmptyInput,
        ConnectionFailed(String),
    }

    struct SocketCleanup {
        path: PathBuf,
    }

    impl SocketCleanup {
        fn new(path: PathBuf) -> Self {
            Self { path }
        }
    }

    impl Drop for SocketCleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(not(unix))]
mod platform {
    use jj_cli::cli_util::CommandHelper;
    use jj_cli::command_error::{CommandError, user_error};
    use jj_cli::ui::Ui;

    #[derive(clap::Args)]
    pub(crate) struct DaemonArgs {
        #[command(subcommand)]
        command: DaemonCommand,
    }

    #[derive(clap::Subcommand)]
    enum DaemonCommand {
        /// Run the machine-store synchronization daemon in the foreground.
        Run,
    }

    pub(crate) async fn run_daemon(
        _ui: &mut Ui,
        _command: &CommandHelper,
        args: DaemonArgs,
    ) -> Result<(), CommandError> {
        match args.command {
            DaemonCommand::Run => Err(user_error(
                "The devspace daemon is not supported on Windows; command-boundary sync remains active.",
            )),
        }
    }
}

pub(crate) use platform::{DaemonArgs, run_daemon};
