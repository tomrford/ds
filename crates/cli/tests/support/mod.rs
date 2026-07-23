#![allow(dead_code)]

use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use blake2::{Blake2b512, Digest as _};
use devspace_machine::MachineGitRepository as MachineRepository;
use devspace_machine::{
    MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineStore, SharedSecret,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::object_id::ObjectId as _;
use jj_lib::settings::UserSettings;
#[cfg(unix)]
use rustix::net::SocketAddrUnix;
#[cfg(unix)]
use rustix::process::getuid;

pub mod fake_worker;

pub const TEST_MACHINE_ID: &str = "12121212121212121212121212121212";
pub const TEST_SHARED_SECRET: &str = "cli-development-secret";

pub fn settings() -> UserSettings {
    let mut config = StackedConfig::with_defaults();
    config.add_layer(
        ConfigLayer::parse(
            ConfigSource::User,
            r#"
                [user]
                name = "Devspace Test"
                email = "devspace@example.invalid"
            "#,
        )
        .unwrap(),
    );
    UserSettings::from_config(config).unwrap()
}

pub fn write_cli_config(root: &Path) -> PathBuf {
    let path = root.join("jj-config.toml");
    fs::write(
        &path,
        r#"
            [user]
            name = "Devspace Test"
            email = "devspace@example.invalid"

            [ui]
            color = "never"

            [snapshot]
            auto-update-stale = true
        "#,
    )
    .unwrap();
    path
}

pub fn ds(cwd: &Path, config: &Path, args: &[&str]) -> Output {
    ds_command(cwd, config).args(args).output().unwrap()
}

pub fn ds_with_home(cwd: &Path, home: &Path, config: &Path, args: &[&str]) -> Output {
    ds_command_with_home(cwd, home, config)
        .args(args)
        .output()
        .unwrap()
}

pub fn ds_with_env(
    cwd: &Path,
    home: &Path,
    config: &Path,
    args: &[&str],
    environment: &[(&str, &str)],
) -> Output {
    let mut command = ds_command_with_home(cwd, home, config);
    command.args(args).envs(environment.iter().copied());
    command.output().unwrap()
}

pub fn ds_command(cwd: &Path, config: &Path) -> Command {
    ds_command_with_home(cwd, config.parent().unwrap(), config)
}

pub fn ds_command_with_home(cwd: &Path, home: &Path, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ds"));
    command
        .current_dir(cwd)
        .env(MACHINE_STORE_OVERRIDE, home.join("machine-store"))
        .env("JJ_CONFIG", config)
        .env("DEVSPACE_BOUNDARY_SYNC", "0")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat");
    command
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn machine_store(root: &Path) -> MachineStore {
    MachineStore::new(root.join("machine-store"))
}

pub fn configure_machine(root: &Path, base_url: &str) {
    configure_machine_as(root, base_url, TEST_MACHINE_ID, TEST_SHARED_SECRET);
}

pub fn configure_machine_as(root: &Path, base_url: &str, machine_id: &str, secret: &str) {
    write_machine_config(root, base_url, machine_id, secret, None);
}

pub fn configure_machine_from_env(root: &Path, machine_id: &str) {
    let base_url = env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
    let shared_secret = env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
    configure_machine_as(root, &base_url, machine_id, &shared_secret);
}

pub fn configure_machine_with_name(root: &Path, base_url: &str, machine_name: Option<&str>) {
    write_machine_config(
        root,
        base_url,
        TEST_MACHINE_ID,
        TEST_SHARED_SECRET,
        machine_name,
    );
}

pub fn set_machine_git_shim(root: &Path, enabled: bool) {
    let store = machine_store(root);
    let config = store.load_config().unwrap().with_git_shim(enabled);
    store.write_config(&config).unwrap();
}

fn write_machine_config(
    root: &Path,
    base_url: &str,
    machine_id: &str,
    secret: &str,
    machine_name: Option<&str>,
) {
    let config = MachineConfig::new(
        base_url,
        MachineId::parse(machine_id).unwrap(),
        SharedSecret::new(secret).unwrap(),
    )
    .unwrap();
    let config = match machine_name {
        Some(name) => config.with_machine_name(name).unwrap(),
        None => config,
    };
    machine_store(root).write_config(&config).unwrap();
}

pub fn seal_commit(cwd: &Path, home: &Path, config: &Path, description: &str) {
    let described = ds_with_home(cwd, home, config, &["describe", "-m", description]);
    assert!(described.status.success(), "{}", stderr(&described));
    let sealed = ds_with_home(cwd, home, config, &["new"]);
    assert!(sealed.status.success(), "{}", stderr(&sealed));
}

pub fn commit_id(cwd: &Path, config: &Path, revision: &str) -> String {
    commit_id_with_home(cwd, config.parent().unwrap(), config, revision)
}

pub fn commit_id_with_home(cwd: &Path, home: &Path, config: &Path, revision: &str) -> String {
    let output = ds_with_home(
        cwd,
        home,
        config,
        &[
            "log",
            "-r",
            revision,
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output).trim().to_owned()
}

pub fn repository_commit_ids(home: &Path, config: &Path, name: &str) -> Vec<String> {
    let output = ds_with_home(
        home,
        home,
        config,
        &[
            "-R",
            name,
            "log",
            "-r",
            "all()",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output).lines().map(str::to_owned).collect()
}

pub async fn operation_heads(repository_path: &Path) -> Vec<String> {
    let repository = MachineRepository::open(repository_path, &settings())
        .await
        .unwrap();
    let mut heads = repository
        .repo()
        .op_heads_store()
        .get_op_heads()
        .await
        .unwrap()
        .into_iter()
        .map(|head| head.hex())
        .collect::<Vec<_>>();
    heads.sort();
    heads
}

pub fn poll_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    condition()
}

#[cfg(unix)]
pub fn daemon_socket_path(store_root: &Path) -> PathBuf {
    let canonical_root = dunce::canonicalize(store_root).unwrap();
    let encoded = format!("unix:{}", hex_bytes(canonical_root.as_os_str().as_bytes()));
    let digest = Blake2b512::digest(encoded.as_bytes());
    let socket_name = format!("{}.sock", hex_bytes(&digest[..12]));

    if let Some(temp_root) = env::var_os("TMPDIR").map(PathBuf::from)
        && temp_root.is_absolute()
    {
        let candidate = temp_root.join("devspace-daemon").join(&socket_name);
        if SocketAddrUnix::new(&candidate).is_ok() {
            return candidate;
        }
    }

    PathBuf::from(format!("/tmp/devspace-daemon-{}", getuid().as_raw())).join(socket_name)
}

#[cfg(unix)]
fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
