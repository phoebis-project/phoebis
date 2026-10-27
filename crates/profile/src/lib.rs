//! Pinned wire constants of Claude Code 2.1.269 on Linux x64 and the fixture files measured from it. The
//! inference and control fixtures are that build's; the `count_*` fixtures were measured from 2.1.278, the build
//! installed when the client's own token count was recorded.

pub mod catalog;
pub mod fixtures;

/// The measured client's own SDK headers. The relay emits none of them: a caller's headers travel as it sent
/// them, so these are the record of the capture the fixture suites are compared against.
pub struct Stainless {
    pub arch: &'static str,
    pub lang: &'static str,
    pub os: &'static str,
    pub package_version: &'static str,
    pub runtime: &'static str,
    pub runtime_version: &'static str,
    pub timeout: &'static str,
}
/// The beta lists the measured client sends in each of its three shapes. The relay drops three betas where the
/// request calls for it (`DANGEROUS_TOOL_USE_BETA` under an off auto-mode setting, `EXTENDED_TTL_BETA` for a
/// five-minute body, `OAUTH_BETA` on an API-key account), injects three (`CACHE_DIAGNOSIS_BETA` under the pool's
/// cache diagnostics switch, `EXTENDED_TTL_BETA` for a one-hour body, `OAUTH_BETA` on an OAuth account), and passes
/// every other beta the client sends through: the pass-through set is whatever the client sends less the dropped
/// ones, and it changes with the client's version.
pub struct Betas {
    pub oauth_non_first_party: &'static str,
    pub oauth_first_party: &'static str,
    pub api_key_non_first_party: &'static str,
}
pub struct Control {
    pub authorize_claude_ai: &'static str,
    pub authorize_console: &'static str,
    pub client_id: &'static str,
    pub manual_redirect: &'static str,
    pub profile_url: &'static str,
    pub roles_url: &'static str,
    pub token_url: &'static str,
    pub usage_url: &'static str,
    pub axios_user_agent: &'static str,
    pub default_login_scopes: &'static str,
    pub refresh_fallback_scopes: &'static str,
}
pub struct TlsProfiles {
    pub curves: &'static str,
    pub control_cipher_list: &'static str,
}

pub struct Profile {
    pub name: &'static str,
    pub cli_version: &'static str,
    pub build_tag: &'static str,
    pub user_agent_sdk_cli: &'static str,
    pub user_agent_cli: &'static str,
    pub stainless: Stainless,
    pub betas: Betas,
    pub control: Control,
    pub tls: TlsProfiles,
}

static PROFILE_2_1_269: Profile = Profile {
    name: "claude-code-2.1.269-linux-x64",
    cli_version: "2.1.269",
    build_tag: "2.1.269.517",
    user_agent_sdk_cli: "claude-cli/2.1.269 (external, sdk-cli)",
    user_agent_cli: "claude-cli/2.1.269 (external, cli)",
    stainless: Stainless { arch: "x64", lang: "js", os: "Linux", package_version: "0.112.1", runtime: "node", runtime_version: "v26.3.0", timeout: "600" },
    betas: Betas {
        oauth_non_first_party: "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,extended-cache-ttl-2025-04-11",
        oauth_first_party: "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advanced-tool-use-2025-11-20,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
        api_key_non_first_party: "claude-code-20250219,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05",
    },
    control: Control {
        authorize_claude_ai: "https://claude.com/cai/oauth/authorize",
        authorize_console: "https://platform.claude.com/oauth/authorize",
        client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        manual_redirect: "https://platform.claude.com/oauth/code/callback",
        profile_url: "https://api.anthropic.com/api/oauth/profile",
        roles_url: "https://api.anthropic.com/api/oauth/claude_cli/roles",
        token_url: "https://platform.claude.com/v1/oauth/token",
        usage_url: "https://api.anthropic.com/api/oauth/usage",
        axios_user_agent: "axios/1.15.2",
        default_login_scopes: "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
        refresh_fallback_scopes: "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
    },
    tls: TlsProfiles {
        curves: "X25519MLKEM768:X25519:P-256:P-384",
        control_cipher_list: "ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES128-SHA256:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA",
    },
};

/// The one profile this build carries: every reader takes it from here, and a re-pin changes it with the rebuild.
pub fn claude_code_2_1_269_linux_x64() -> &'static Profile {
    &PROFILE_2_1_269
}

pub fn by_name(name: &str) -> Option<&'static Profile> {
    [&PROFILE_2_1_269].into_iter().find(|p| p.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::path::Path;

    #[test]
    fn by_name_finds_the_pinned_profile_and_nothing_else() {
        let p = claude_code_2_1_269_linux_x64();
        assert_eq!(
            by_name("claude-code-2.1.269-linux-x64").map(|x| x.name),
            Some(p.name)
        );
        assert!(by_name("claude-code-9.9.9").is_none());
    }

    /// Every file under `dir`, keyed by its path below `root`, to the lower-hex SHA-256 of its bytes.
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, files);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();
                let digest = format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap()));
                files.insert(relative, digest);
            }
        }
    }

    /// The fixtures are redacted client captures that are never edited by hand; a changed byte, a missing row or an
    /// extra file fails here rather than at the next capture comparison.
    #[test]
    fn every_fixture_matches_the_development_page_manifest() {
        let manifest = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/site/development.md"
        ))
        .unwrap();
        let rows: BTreeMap<String, String> = manifest
            .lines()
            .filter_map(|line| {
                let cells: Vec<&str> = line.split('|').map(str::trim).collect();
                (cells.len() == 4 && cells[1].starts_with('`') && cells[2].len() == 66).then(|| {
                    (
                        cells[1].trim_matches('`').to_string(),
                        cells[2].trim_matches('`').to_string(),
                    )
                })
            })
            .collect();
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures"));
        let mut files = BTreeMap::new();
        walk(dir, dir, &mut files);
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            rows.keys().collect::<Vec<_>>(),
            "the manifest names every fixture and nothing else"
        );
        for (file, digest) in &files {
            assert_eq!(digest, &rows[file], "{file} differs from the manifest");
        }
    }

    #[test]
    fn fixtures_load_and_have_measured_sizes() {
        assert_eq!(
            fixtures::client_hello(fixtures::Plane::Inference).len(),
            1499 + 5
        );
        assert_eq!(
            fixtures::client_hello(fixtures::Plane::Control).len(),
            1473 + 5
        );
        assert!(
            fixtures::inference_head(fixtures::FixtureKind::OauthFirstParty)
                .starts_with(b"POST /v1/messages?beta=true HTTP/1.1\r\n")
        );
        assert_eq!(
            fixtures::body(fixtures::BodyFixture::FirstParty).len(),
            80404
        );
    }

    #[test]
    fn the_safeguards_splice_lands_before_stream_and_the_beta_joins_the_list() {
        use fixtures::{BodyFixture, FixtureKind, SAFEGUARDS_MEMBER};
        let member = SAFEGUARDS_MEMBER.as_bytes();
        for fixture in [
            BodyFixture::ApiKey,
            BodyFixture::FirstParty,
            BodyFixture::FirstPartyAgain,
            BodyFixture::Oauth,
        ] {
            let native = fixtures::body(fixture);
            let spliced = fixtures::with_safeguards(native);
            assert!(spliced.ends_with(&[b",", member, br#","stream":true}"#].concat()));
            assert_eq!(spliced.len(), native.len() + member.len() + 1);
        }
        let native = fixtures::body(BodyFixture::Count);
        let spliced = fixtures::with_safeguards(native);
        assert!(spliced.ends_with(&[b",", member, b"}"].concat()));
        assert_eq!(spliced.len(), native.len() + member.len() + 1);
        for kind in [
            FixtureKind::ApiKey,
            FixtureKind::CountApiKey,
            FixtureKind::CountOauthFirstParty,
            FixtureKind::CountOauthNonFirstParty,
            FixtureKind::OauthFirstParty,
            FixtureKind::OauthNonFirstParty,
        ] {
            let head = String::from_utf8(fixtures::with_dangerous_tool_use_beta(
                fixtures::inference_head(kind),
            ))
            .unwrap();
            let line = head
                .split("\r\n")
                .find(|line| line.to_ascii_lowercase().starts_with("anthropic-beta:"))
                .expect("every request head carries the beta line");
            assert!(line.ends_with(",dangerous-tool-use-2026-09-03"), "{line}");
        }
    }
}
