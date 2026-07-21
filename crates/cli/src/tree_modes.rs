use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

pub(crate) fn rewrite(root: &Path, rewrite: impl Fn(bool, u32) -> Option<u32>) -> io::Result<()> {
    let mut stack = vec![root.to_owned()];
    while let Some(path) = stack.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }

        let mut permissions = metadata.permissions();
        if let Some(next_mode) = rewrite(file_type.is_dir(), permissions.mode())
            && next_mode != permissions.mode()
        {
            permissions.set_mode(next_mode);
            fs::set_permissions(&path, permissions)?;
        }

        if file_type.is_dir() {
            for entry in fs::read_dir(path)? {
                stack.push(entry?.path());
            }
        }
    }
    Ok(())
}
