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

/// Parse a GitHub release tag into a [`semver::Version`].
///
/// GitHub tags may use 2, 3 or 4 numeric components:
///   - `v0.1`       → `0.1.0`
///   - `v0.1.7`     → `0.1.7`
///   - `v0.1.7.2`   → `0.1.70002`  (patch = major_patch * 10000 + sub_patch)
///
/// The 4-component encoding preserves ordering so that
/// `0.1.7.2 > 0.1.7.1 > 0.1.7 (= 0.1.70000)`.
///
/// Pre-release suffixes (e.g. `-beta.1`) are forwarded to `semver` as-is
/// after the numeric normalisation.
pub fn parse_release_tag(tag: &str) -> Option<Version> {
    let tag = tag.strip_prefix('v').unwrap_or(tag);

    // Split off an optional pre-release / build-metadata suffix that
    // starts at the first '-' or '+' after the version numbers.
    let (numeric, suffix) = split_suffix(tag);
    let parts: Vec<&str> = numeric.split('.').collect();

    match parts.len() {
        // "x.x" → "x.x.0"
        2 => {
            let major: u64 = parts[0].parse().ok()?;
            let minor: u64 = parts[1].parse().ok()?;
            let mut v = Version::new(major, minor, 0);
            apply_suffix(&mut v, suffix);
            Some(v)
        }
        // "x.x.y" → "x.x.(y*10000)" so it compares correctly with
        // four-component tags like "x.x.y.z".
        3 => {
            let major: u64 = parts[0].parse().ok()?;
            let minor: u64 = parts[1].parse().ok()?;
            let patch: u64 = parts[2].parse().ok()?;
            let mut v = Version::new(major, minor, patch * 10000);
            apply_suffix(&mut v, suffix);
            Some(v)
        }
        // "x.x.y.z" → "x.x.(y*10000+z)"
        4 => {
            let major: u64 = parts[0].parse().ok()?;
            let minor: u64 = parts[1].parse().ok()?;
            let patch_hi: u64 = parts[2].parse().ok()?;
            let patch_lo: u64 = parts[3].parse().ok()?;
            let mut v = Version::new(major, minor, patch_hi * 10000 + patch_lo);
            apply_suffix(&mut v, suffix);
            Some(v)
        }
        _ => None,
    }
}

/// Also encode the local CARGO_PKG_VERSION the same way so that
/// comparisons against 4-component release tags are correct.
///
/// `current_version()` already returns a valid 3-component semver
/// (e.g. `0.1.7`).  We re-encode it as `0.1.70000` so that
/// `0.1.7.2` (→ `0.1.70002`) is correctly detected as newer.
pub fn normalize_current_version() -> Version {
    let v = current_version();
    Version::new(v.major, v.minor, v.patch * 10000)
}

pub fn is_newer(candidate: &Version, current: &Version) -> bool {
    candidate > current
}

/// Split `"1.2.3.4-beta.1"` into `("1.2.3.4", Some("beta.1"))`.
fn split_suffix(s: &str) -> (&str, Option<&str>) {
    // Walk past all digits-and-dots to find the first '-' or '+'.
    if let Some(pos) = s.find(|c: char| c == '-' || c == '+') {
        (&s[..pos], Some(&s[pos + 1..]))
    } else {
        (s, None)
    }
}

fn apply_suffix(v: &mut Version, suffix: Option<&str>) {
    if let Some(s) = suffix {
        if let Ok(pre) = semver::Prerelease::new(s) {
            v.pre = pre;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_component_tag() {
        let parsed = parse_release_tag("v0.1.3").unwrap();
        assert_eq!(parsed, Version::new(0, 1, 30000));
    }

    #[test]
    fn four_component_tag() {
        let parsed = parse_release_tag("v0.1.7.2").unwrap();
        // 7*10000 + 2 = 70002
        assert_eq!(parsed, Version::new(0, 1, 70002));
    }

    #[test]
    fn two_component_tag() {
        let parsed = parse_release_tag("v0.2").unwrap();
        assert_eq!(parsed, Version::new(0, 2, 0));
    }

    #[test]
    fn four_component_ordering() {
        let v71 = parse_release_tag("v0.1.7.1").unwrap();
        let v72 = parse_release_tag("v0.1.7.2").unwrap();
        let v7 = parse_release_tag("v0.1.7").unwrap();
        // Normalise current 0.1.7 the same way the updater does:
        let current = Version::new(0, 1, 7 * 10000);
        assert!(v72 > v71);
        assert!(v71 > current);
        assert_eq!(v7, current);
        assert!(v7 < v71);
        assert!(parse_release_tag("v0.1.8").unwrap() > v72);
    }

    #[test]
    fn prerelease_suffix_preserved() {
        let parsed = parse_release_tag("v0.2.0-beta.1").unwrap();
        assert!(!parsed.pre.is_empty());
        assert_eq!(parsed.major, 0);
        assert_eq!(parsed.minor, 2);
        assert_eq!(parsed.patch, 0);
    }

    #[test]
    fn normalize_current_encodes_patch() {
        // CARGO_PKG_VERSION is "0.1.7" → normalized to 0.1.70000
        let v = normalize_current_version();
        assert_eq!(v.patch, current_version().patch * 10000);
    }

    #[test]
    fn older_or_equal_versions_are_not_newer() {
        let current = Version::new(0, 1, 70000); // normalized 0.1.7
        assert!(!is_newer(&Version::new(0, 1, 70000), &current));
        assert!(!is_newer(&Version::new(0, 1, 60000), &current));
        assert!(is_newer(&Version::new(0, 1, 70001), &current));
    }
}
