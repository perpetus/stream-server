use semver::Version;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum UpdateChannel {
    Stable,
    Prerelease,
}

impl Default for UpdateChannel {
    fn default() -> Self {
        Self::Stable
    }
}

pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0))
}

pub fn parse_release_tag(tag: &str) -> Option<Version> {
    let normalized = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(normalized).ok()
}

pub fn is_newer(candidate: &Version, current: &Version) -> bool {
    candidate > current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_comparison_handles_v_prefix() {
        let parsed = parse_release_tag("v0.1.3").unwrap();
        assert_eq!(parsed, Version::parse("0.1.3").unwrap());
    }

    #[test]
    fn older_or_equal_versions_are_not_newer() {
        let current = Version::parse("0.1.2").unwrap();
        assert!(!is_newer(&Version::parse("0.1.2").unwrap(), &current));
        assert!(!is_newer(&Version::parse("0.1.1").unwrap(), &current));
        assert!(is_newer(&Version::parse("0.1.3").unwrap(), &current));
    }
}
