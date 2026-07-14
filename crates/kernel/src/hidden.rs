use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HiddenPath(String);

impl HiddenPath {
    pub fn parse(input: impl Into<String>) -> Result<Self, InvalidHiddenPath> {
        let input = input.into();
        let invalid = input.is_empty()
            || input.starts_with('/')
            || input.ends_with('/')
            || input
                .split('/')
                .any(|component| component.is_empty() || component == "." || component == "..");
        if invalid {
            return Err(InvalidHiddenPath);
        }
        Ok(Self(input))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for HiddenPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("HiddenPath").field(&self.0).finish()
    }
}

impl fmt::Display for HiddenPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidHiddenPath;

impl fmt::Display for InvalidHiddenPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("hidden paths must be exact, non-root, repository-relative file paths")
    }
}

impl Error for InvalidHiddenPath {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HiddenPathSet(BTreeSet<HiddenPath>);

impl HiddenPathSet {
    pub fn from_paths(paths: impl IntoIterator<Item = HiddenPath>) -> Self {
        Self(paths.into_iter().collect())
    }

    pub fn contains(&self, path: &HiddenPath) -> bool {
        self.0.contains(path)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &HiddenPath> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_exact_repository_relative_paths() {
        assert_eq!(
            HiddenPath::parse("secrets/token").unwrap().as_str(),
            "secrets/token"
        );
        for invalid in [
            "", "/secret", "secret/", ".", "..", "a/./b", "a/../b", "a//b",
        ] {
            assert!(HiddenPath::parse(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn set_is_sorted_and_duplicate_free() {
        let paths = HiddenPathSet::from_paths([
            HiddenPath::parse("z").unwrap(),
            HiddenPath::parse("a").unwrap(),
            HiddenPath::parse("z").unwrap(),
        ]);
        assert_eq!(
            paths.iter().map(HiddenPath::as_str).collect::<Vec<_>>(),
            ["a", "z"]
        );
    }
}
