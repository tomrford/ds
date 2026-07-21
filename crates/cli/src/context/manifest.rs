//! The `.repos/.lock` lockfile, in Grepo's TOML format.
//!
//! `ds context` only materializes entries created from raw Git URLs. Entries
//! with Grepo package-source metadata or other backends are carried as
//! `Foreign`: their raw fields round-trip through rewrites untouched so
//! running `ds context` never destroys state it does not understand.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use anyhow::{Context as _, Result, bail};

use super::{is_valid_alias, write_atomic_str};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LockMode {
    /// `update` advances to the remote default branch head.
    Default,
    /// `update` advances to the named branch or tag.
    Ref { ref_name: String },
    /// Pinned; `update` leaves it alone.
    Exact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitLockEntry {
    pub alias: String,
    pub url: String,
    pub subdir: Option<String>,
    pub mode: LockMode,
    pub commit: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForeignEntry {
    pub alias: String,
    raw: StoredLockEntry,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LockEntry {
    Git(GitLockEntry),
    Foreign(ForeignEntry),
}

impl LockEntry {
    pub fn alias(&self) -> &str {
        match self {
            Self::Git(entry) => &entry.alias,
            Self::Foreign(entry) => &entry.alias,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lockfile {
    repos: BTreeMap<String, LockEntry>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
struct StoredLockfile {
    #[serde(default)]
    repos: BTreeMap<String, StoredLockEntry>,
}

/// Known fields for Devspace-owned Git entries plus an opaque remainder for
/// unsupported entries. The remainder prevents a lockfile rewrite from
/// deleting data merely because Devspace does not understand it.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
struct StoredLockEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    ref_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default, flatten)]
    extra: BTreeMap<String, toml::Value>,
}

impl Lockfile {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
        };
        Self::parse(&contents).with_context(|| format!("parse {}", path.display()))
    }

    pub fn parse(contents: &str) -> Result<Self> {
        let stored: StoredLockfile = toml::from_str(contents).context("invalid lockfile TOML")?;
        let mut repos = BTreeMap::new();
        for (alias, entry) in stored.repos {
            if !is_valid_alias(&alias) {
                bail!("invalid alias in lockfile: {alias}");
            }
            repos.insert(alias.clone(), decode_entry(alias, entry)?);
        }
        Ok(Self { repos })
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        write_atomic_str(path, &self.render()?)
    }

    pub fn render(&self) -> Result<String> {
        let stored = StoredLockfile {
            repos: self
                .repos
                .iter()
                .map(|(alias, entry)| (alias.clone(), encode_entry(entry)))
                .collect(),
        };
        toml::to_string_pretty(&stored).context("render lockfile")
    }

    pub fn upsert(&mut self, entry: LockEntry) {
        self.repos.insert(entry.alias().to_string(), entry);
    }

    pub fn remove(&mut self, alias: &str) -> bool {
        self.repos.remove(alias).is_some()
    }

    pub fn get(&self, alias: &str) -> Option<&LockEntry> {
        self.repos.get(alias)
    }

    pub fn aliases(&self) -> Vec<String> {
        self.repos.keys().cloned().collect()
    }

    pub fn entries(&self) -> impl Iterator<Item = &LockEntry> {
        self.repos.values()
    }

    /// All aliases when `aliases` is empty, otherwise the given ones,
    /// erroring on unknown names.
    pub fn select(&self, aliases: &[String]) -> Result<Vec<String>> {
        if aliases.is_empty() {
            return Ok(self.aliases());
        }
        for alias in aliases {
            if !self.repos.contains_key(alias) {
                bail!("alias not found: {alias}");
            }
        }
        Ok(aliases.to_vec())
    }
}

fn decode_entry(alias: String, stored: StoredLockEntry) -> Result<LockEntry> {
    if stored.source.is_some() {
        return Ok(LockEntry::Foreign(ForeignEntry { alias, raw: stored }));
    }

    match stored.backend.as_deref().unwrap_or("git") {
        "git" => {
            let url = stored
                .url
                .with_context(|| format!("{alias}: missing url"))?;
            let mode = match stored.mode.as_deref() {
                Some("default") => LockMode::Default,
                Some("ref") => LockMode::Ref {
                    ref_name: stored
                        .ref_name
                        .with_context(|| format!("{alias}: mode=ref requires a ref value"))?,
                },
                Some("exact") => LockMode::Exact,
                Some(other) => bail!("{alias}: unknown mode {other:?}"),
                None => bail!("{alias}: missing mode"),
            };
            Ok(LockEntry::Git(GitLockEntry {
                alias,
                url,
                subdir: stored.subdir,
                mode,
                commit: stored.commit,
            }))
        }
        _ => Ok(LockEntry::Foreign(ForeignEntry { alias, raw: stored })),
    }
}

fn encode_entry(entry: &LockEntry) -> StoredLockEntry {
    match entry {
        LockEntry::Git(entry) => {
            let (mode, ref_name) = match &entry.mode {
                LockMode::Default => ("default".to_string(), None),
                LockMode::Ref { ref_name } => ("ref".to_string(), Some(ref_name.clone())),
                LockMode::Exact => ("exact".to_string(), None),
            };
            StoredLockEntry {
                backend: None,
                source: None,
                url: Some(entry.url.clone()),
                subdir: entry.subdir.clone(),
                mode: Some(mode),
                ref_name,
                commit: entry.commit.clone(),
                sha256: None,
                extra: BTreeMap::new(),
            }
        }
        LockEntry::Foreign(entry) => entry.raw.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_rerenders_git_entries_canonically() {
        let input = r#"[repos.named_ref]
url = "git@github.com:tomrford/mint.git"
mode = "ref"
ref = "main"
commit = "def"

[repos.pinned]
url = "git@github.com:tomrford/grepo.git"
mode = "exact"
commit = "123"
"#;
        let lockfile = Lockfile::parse(input).unwrap();
        assert_eq!(
            lockfile.get("named_ref"),
            Some(&LockEntry::Git(GitLockEntry {
                alias: "named_ref".into(),
                url: "git@github.com:tomrford/mint.git".into(),
                subdir: None,
                mode: LockMode::Ref {
                    ref_name: "main".into()
                },
                commit: Some("def".into()),
            }))
        );
        assert_eq!(lockfile.render().unwrap(), input);
    }

    #[test]
    fn foreign_entries_round_trip_untouched() {
        let input = r#"[repos.serde]
backend = "tarball"
source = "cargo:serde@1.0.197"
url = "https://crates.io/api/v1/crates/serde/1.0.197/download"
sha256 = "3fb1c873e1b9b056a4dc4c0c198b24c3ffa059243875552b2bd0933b1aee4ce2"
integrity = "sha512-opaque"
"#;
        let lockfile = Lockfile::parse(input).unwrap();
        assert!(matches!(lockfile.get("serde"), Some(LockEntry::Foreign(_))));
        assert_eq!(lockfile.render().unwrap(), input);
    }

    #[test]
    fn package_sourced_git_entries_are_foreign_and_round_trip_untouched() {
        let input = r#"[repos.react]
source = "npm:react@18.2.0"
url = "https://github.com/facebook/react.git"
subdir = "packages/react"
mode = "exact"
commit = "123"
"#;
        let lockfile = Lockfile::parse(input).unwrap();
        assert!(matches!(lockfile.get("react"), Some(LockEntry::Foreign(_))));
        assert_eq!(lockfile.render().unwrap(), input);
    }

    #[test]
    fn rewrites_preserve_foreign_entries_alongside_git_edits() {
        let mut lockfile = Lockfile::parse(
            r#"[repos.serde]
backend = "tarball"
source = "cargo:serde@1.0.197"
url = "https://crates.io/api/v1/crates/serde/1.0.197/download"
sha256 = "3fb1c873e1b9b056a4dc4c0c198b24c3ffa059243875552b2bd0933b1aee4ce2"
"#,
        )
        .unwrap();
        lockfile.upsert(LockEntry::Git(GitLockEntry {
            alias: "mint".into(),
            url: "git@github.com:tomrford/mint.git".into(),
            subdir: None,
            mode: LockMode::Default,
            commit: Some("abc".into()),
        }));

        let rendered = lockfile.render().unwrap();
        assert!(rendered.contains("backend = \"tarball\""));
        assert!(rendered.contains("[repos.mint]"));
    }

    #[test]
    fn rejects_invalid_aliases() {
        let error = Lockfile::parse(
            r#"[repos.".bad"]
url = "git@github.com:tomrford/grepo.git"
mode = "default"
"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid alias"));
    }
}
