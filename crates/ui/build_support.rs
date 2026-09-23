//! The Node check the build script runs before npm, apart from `build.rs` so the crate's unit tests reach it:
//! a build script is not a test target.

/// The range the lockfile's Vite declares in its own `engines` field.
pub(crate) const NODE_RANGES: &str = "^20.19.0 || >=22.12.0";

/// Reads the `vMAJOR.MINOR.PATCH` that `node --version` answers.
pub(crate) fn parse_node_version(answer: &str) -> Option<(u32, u32, u32)> {
    let mut parts = answer.trim().strip_prefix('v')?.split('.');
    let mut next = || parts.next()?.parse::<u32>().ok();
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// Whether a version falls inside [`NODE_RANGES`].
pub(crate) fn node_supported((major, minor, _): (u32, u32, u32)) -> bool {
    (major == 20 && minor >= 19) || (major == 22 && minor >= 12) || major > 22
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
    fn node_20_is_supported_from_20_19_on() {
        assert!(!node_supported((20, 18, 3)));
        assert!(node_supported((20, 19, 0)));
        assert!(node_supported((20, 20, 1)));
    }

    #[test]
    fn node_22_is_supported_from_22_12_on_and_every_later_major_too() {
        assert!(!node_supported((22, 11, 9)));
        assert!(node_supported((22, 12, 0)));
        assert!(node_supported((23, 0, 0)));
        assert!(node_supported((24, 4, 1)));
    }

    #[test]
    fn node_before_20_19_and_node_21_and_22_before_22_12_are_refused() {
        for version in [(18, 20, 4), (19, 9, 0), (20, 0, 0), (21, 7, 3), (22, 0, 0)] {
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
        let version = parse_node_version(&format!("v{}.0", nvmrc.trim()));
        assert!(version.is_some_and(node_supported), "{nvmrc:?}");
    }
}
