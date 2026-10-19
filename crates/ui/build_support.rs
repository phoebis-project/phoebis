//! The Node check the build script runs before npm, apart from `build.rs` so the crate's unit tests reach it:
//! a build script is not a test target.

/// The floor the console's `engines` field declares.
pub(crate) const NODE_RANGES: &str = ">=26.0.0";

/// Reads the `vMAJOR.MINOR.PATCH` that `node --version` answers.
pub(crate) fn parse_node_version(answer: &str) -> Option<(u32, u32, u32)> {
    let mut parts = answer.trim().strip_prefix('v')?.split('.');
    let mut next = || parts.next()?.parse::<u32>().ok();
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// Whether a version falls inside [`NODE_RANGES`].
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
    fn the_ranges_match_the_web_packages_engines_field() {
        let field = format!(r#""node": "{NODE_RANGES}""#);
        assert!(web_file("package.json").contains(&field), "{field}");
    }

    #[test]
    fn the_nvmrc_names_a_supported_node() {
        let nvmrc = web_file(".nvmrc");
        let version = parse_node_version(&format!("v{}", nvmrc.trim()));
        assert!(version.is_some_and(node_supported), "{nvmrc:?}");
    }
}
