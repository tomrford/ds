#![cfg(unix)]

use std::env;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use blake2::{Blake2b512, Digest as _};
use rustix::net::SocketAddrUnix;
use rustix::process::getuid;

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

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
