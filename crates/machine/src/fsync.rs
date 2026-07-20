use std::fs::File;
use std::io;
use std::path::Path;

#[cfg(unix)]
pub fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
pub fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}
