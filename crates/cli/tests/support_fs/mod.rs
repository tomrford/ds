use std::fs;
use std::path::Path;

pub fn remove_dir_all(path: &Path) {
    make_directories_writable(path);
    fs::remove_dir_all(path).unwrap();
}

#[cfg(unix)]
fn make_directories_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            make_directories_writable(&entry.path());
        }
    }
    let mut permissions = fs::symlink_metadata(path).unwrap().permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    fs::set_permissions(path, permissions).unwrap();
}

#[cfg(not(unix))]
fn make_directories_writable(_path: &Path) {}
