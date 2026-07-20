use std::fs;
use std::path::{Path, PathBuf};

use devspace_machine::GitProjection;
use jj_lib::settings::UserSettings;

const PROJECTION_STAGING_DIRECTORY: &str = ".projection-staging";

pub(super) fn open_or_create_projection(
    path: &Path,
    settings: &UserSettings,
) -> Result<(GitProjection, bool), String> {
    let staging = projection_staging_path(path)?;
    remove_disposable_projection(&staging, "stale Git projection staging")?;

    let rebuilt = path.exists();
    if rebuilt {
        match GitProjection::open(path, settings) {
            Ok(projection) => return Ok((projection, false)),
            Err(_) => remove_disposable_projection(path, "invalid Git projection sidecar")?,
        }
    }

    initialize_projection(path, &staging, settings).map(|projection| (projection, rebuilt))
}

fn projection_staging_path(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Git projection sidecar has no parent directory".to_owned())?;
    Ok(parent.join(PROJECTION_STAGING_DIRECTORY))
}

fn initialize_projection(
    path: &Path,
    staging: &Path,
    settings: &UserSettings,
) -> Result<GitProjection, String> {
    let staged = GitProjection::init(staging, settings).map_err(|error| error.to_string())?;
    drop(staged);
    fs::rename(staging, path).map_err(|error| {
        format!(
            "failed to publish Git projection sidecar at {}: {error}",
            path.display()
        )
    })?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                format!(
                    "failed to sync Git projection sidecar parent at {}: {error}",
                    parent.display()
                )
            })?;
    }
    GitProjection::open(path, settings).map_err(|error| error.to_string())
}

fn remove_disposable_projection(path: &Path, label: &str) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "failed to inspect {label} at {}: {error}",
                path.display()
            ));
        }
    };
    let result = if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|error| format!("failed to remove {label} at {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};

    fn projection_settings() -> UserSettings {
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

    #[test]
    fn corrupt_projection_sidecar_is_rebuilt_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("projection");
        fs::create_dir_all(path.join("store")).unwrap();

        let (projection, rebuilt) =
            open_or_create_projection(&path, &projection_settings()).unwrap();

        assert!(rebuilt);
        assert_eq!(projection.root(), path);
        assert!(!temp.path().join(PROJECTION_STAGING_DIRECTORY).exists());
        drop(projection);
        GitProjection::open(&path, &projection_settings()).unwrap();
    }

    #[test]
    fn stale_projection_staging_is_removed_before_initialization() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("projection");
        let staging = temp.path().join(PROJECTION_STAGING_DIRECTORY);
        fs::create_dir_all(staging.join("store")).unwrap();
        fs::write(staging.join("interrupted"), b"partial initialization").unwrap();

        let (projection, rebuilt) =
            open_or_create_projection(&path, &projection_settings()).unwrap();

        assert!(!rebuilt);
        assert_eq!(projection.root(), path);
        assert!(!staging.exists());
        drop(projection);
        GitProjection::open(&path, &projection_settings()).unwrap();
    }
}
