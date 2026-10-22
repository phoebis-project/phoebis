//! The Node check the build script runs before npm and its judgement of the bundle, apart from `build.rs` so the
//! crate's unit tests reach them: a build script is not a test target.

use std::time::SystemTime;

/// Whether the bundle needs a build: a placeholder, no bundle, a source newer than it, or a commit stamp (the file
/// the script writes in `OUT_DIR` after a build) absent or naming another commit than this build's.
pub(crate) fn bundle_stale(
    placeholder: bool,
    built: Option<SystemTime>,
    sources: Option<SystemTime>,
    stamped: Option<&str>,
    commit: &str,
) -> bool {
    placeholder
        || stamped != Some(commit)
        || match (built, sources) {
            (None, _) => true,
            (Some(b), Some(s)) => s > b,
            (Some(_), None) => false,
        }
}

/// The floor the console's `engines` field declares, the major alone.
pub(crate) const NODE_FLOOR: &str = ">=26.0.0";

/// Reads the `vMAJOR.MINOR.PATCH` that `node --version` answers.
pub(crate) fn parse_node_version(answer: &str) -> Option<(u32, u32, u32)> {
    let mut parts = answer.trim().strip_prefix('v')?.split('.');
    let mut next = || parts.next()?.parse::<u32>().ok();
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// Whether a version stands at or above [`NODE_FLOOR`].
pub(crate) fn node_supported((major, _, _): (u32, u32, u32)) -> bool {
    major >= 26
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web_file(name: &str) -> String {
        let path = format!("{}/../../web/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn a_node_version_answer_parses_as_major_minor_patch() {
        assert_eq!(parse_node_version("v20.19.2\n"), Some((20, 19, 2)));
        assert_eq!(parse_node_version("v22.12.0"), Some((22, 12, 0)));
    }

    #[test]
    fn an_answer_that_is_not_v_major_minor_patch_parses_to_nothing() {
        for answer in [
            "",
            "20.19.2",
            "v20.19",
            "v20.19.2.1",
            "v22.12.0-nightly20260101",
            "vx.y.z",
        ] {
            assert_eq!(parse_node_version(answer), None, "{answer:?}");
        }
    }

    #[test]
    fn node_is_supported_from_26_0_0_on() {
        assert!(!node_supported((25, 9, 3)));
        assert!(node_supported((26, 0, 0)));
        assert!(node_supported((26, 11, 1)));
    }

    #[test]
    fn every_major_after_26_is_supported_too() {
        assert!(node_supported((27, 0, 0)));
        assert!(node_supported((28, 4, 1)));
    }

    #[test]
    fn node_24_and_25_are_refused() {
        for version in [(24, 21, 0), (25, 0, 0), (25, 6, 1)] {
            assert!(!node_supported(version), "{version:?}");
        }
    }

    #[test]
    fn the_floor_matches_the_web_packages_engines_field() {
        let field = format!(r#""node": "{NODE_FLOOR}""#);
        assert!(web_file("package.json").contains(&field), "{field}");
    }

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    /// Sources a minute older than the bundle, so the time test alone reads the bundle fresh.
    fn newer_bundle() -> (Option<SystemTime>, Option<SystemTime>) {
        let sources = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        (
            Some(sources + std::time::Duration::from_secs(60)),
            Some(sources),
        )
    }

    #[test]
    fn a_bundle_newer_than_its_sources_is_stale_where_its_stamp_names_another_commit_or_none() {
        let (built, sources) = newer_bundle();
        let other = "fedcba9876543210fedcba9876543210fedcba98";
        assert!(bundle_stale(false, built, sources, Some(other), COMMIT));
        assert!(bundle_stale(false, built, sources, None, COMMIT));
    }

    #[test]
    fn a_bundle_newer_than_its_sources_and_stamped_with_the_commit_at_hand_is_fresh() {
        let (built, sources) = newer_bundle();
        assert!(!bundle_stale(false, built, sources, Some(COMMIT), COMMIT));
        assert!(!bundle_stale(false, built, None, Some(COMMIT), COMMIT));
    }

    #[test]
    fn a_placeholder_a_missing_bundle_or_a_newer_source_is_stale_whatever_the_stamp() {
        let (built, sources) = newer_bundle();
        assert!(bundle_stale(true, built, sources, Some(COMMIT), COMMIT));
        assert!(bundle_stale(false, None, sources, Some(COMMIT), COMMIT));
        assert!(bundle_stale(false, sources, built, Some(COMMIT), COMMIT));
    }

    #[test]
    fn the_nvmrc_names_a_supported_node() {
        let nvmrc = web_file(".nvmrc");
        let version = parse_node_version(&format!("v{}", nvmrc.trim()));
        assert!(version.is_some_and(node_supported), "{nvmrc:?}");
    }
}
