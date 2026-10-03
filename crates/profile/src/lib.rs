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
/// The beta lists the measured client sends in each of its three shapes. The relay emits none of them either;
/// it adjusts the caller's own list by one entry, the one the credential decides.
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

    #[test]
    fn pinned_profile_matches_measured_constants() {
        let p = claude_code_2_1_269_linux_x64();
        assert_eq!(p.cli_version, "2.1.269");
        assert_eq!(
            p.user_agent_sdk_cli,
            "claude-cli/2.1.269 (external, sdk-cli)"
        );
        assert_eq!(p.stainless.runtime_version, "v26.3.0");
        assert_eq!(p.betas.oauth_non_first_party, "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,extended-cache-ttl-2025-04-11");
        assert_eq!(p.control.client_id, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(
            by_name("claude-code-2.1.269-linux-x64").map(|x| x.name),
            Some(p.name)
        );
        assert!(by_name("claude-code-9.9.9").is_none());
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
            fixtures::body(fixtures::BodyFixture::FirstParty9b244).len(),
            80404
        );
    }

    #[test]
    fn the_safeguards_splice_lands_before_stream_and_the_beta_joins_the_list() {
        use fixtures::{BodyFixture, FixtureKind, SAFEGUARDS_MEMBER};
        let member = SAFEGUARDS_MEMBER.as_bytes();
        for fixture in [
            BodyFixture::ApiKey,
            BodyFixture::FirstParty9b244,
            BodyFixture::FirstPartyE142e,
            BodyFixture::Oauth,
        ] {
            let native = fixtures::body(fixture);
            let spliced = fixtures::with_safeguards(native);
            assert!(spliced.ends_with(&[b",", member, br#","stream":true}"#].concat()));
            assert_eq!(spliced.len(), native.len() + member.len() + 1);
        }
        for fixture in [
            BodyFixture::CountApiKey,
            BodyFixture::CountOauth,
            BodyFixture::CountOauthFirstParty,
        ] {
            let native = fixtures::body(fixture);
            let spliced = fixtures::with_safeguards(native);
            assert!(spliced.ends_with(&[b",", member, b"}"].concat()));
            assert_eq!(spliced.len(), native.len() + member.len() + 1);
        }
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
