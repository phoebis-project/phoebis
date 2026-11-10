//! Pinned wire constants of Claude Code 2.1.310 on Linux x64 and the fixture files measured from it. One capture
//! of that build recorded every fixture: the inference plane, the count path and the control plane.

pub mod catalog;
pub mod fixtures;

/// The beta that asks the upstream to review each tool use. It lives here, below the relay, so the relay's drop and
/// the fixture heads that carry it name one value.
pub const DANGEROUS_TOOL_USE_BETA: &str = "dangerous-tool-use-2026-09-03";

/// The two TLS planes the measured client speaks, each with its own recorded hello and connector settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plane {
    Inference,
    Control,
}

/// A control endpoint in the parts the client sends: the host it resolves, presents as SNI and names in `Host`, and
/// the request target. None carries a port, and the limit resets read alone carries a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlEndpoint {
    pub host: &'static str,
    pub target: &'static str,
}

pub struct Control {
    pub authorize_claude_ai: &'static str,
    pub authorize_console: &'static str,
    pub client_id: &'static str,
    pub limit_resets: ControlEndpoint,
    pub profile: ControlEndpoint,
    pub roles: ControlEndpoint,
    pub token: ControlEndpoint,
    pub usage: ControlEndpoint,
    pub axios_user_agent: &'static str,
    pub default_login_scopes: &'static str,
    /// The agent of the profile and roles reads. The token endpoint keeps the Axios agent and the usage read
    /// carries the CLI agent, so each control call's agent is measured on its own.
    pub read_user_agent: &'static str,
    pub refresh_fallback_scopes: &'static str,
}
pub struct TlsProfiles {
    pub curves: &'static str,
    pub control_cipher_list: &'static str,
    /// The captured hello's `signature_algorithms` in its order, in BoringSSL's names.
    pub signature_algorithms: &'static str,
}

pub struct Profile {
    pub name: &'static str,
    pub user_agent_cli: &'static str,
    pub control: Control,
    pub tls: TlsProfiles,
}

static PROFILE_2_1_310: Profile = Profile {
    name: "claude-code-2.1.310-linux-x64",
    user_agent_cli: "claude-cli/2.1.310 (external, cli)",
    control: Control {
        authorize_claude_ai: "https://claude.com/cai/oauth/authorize",
        authorize_console: "https://platform.claude.com/oauth/authorize",
        client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        // The client's own shape of the usage read: the upstream fills the limit reset block for it and leaves the
        // spend members null.
        limit_resets: ControlEndpoint {
            host: "api.anthropic.com",
            target: "/api/oauth/usage?cedar_ember=1&skip_spend=1",
        },
        profile: ControlEndpoint {
            host: "api.anthropic.com",
            target: "/api/oauth/profile",
        },
        roles: ControlEndpoint {
            host: "api.anthropic.com",
            target: "/api/oauth/claude_cli/roles",
        },
        token: ControlEndpoint {
            host: "platform.claude.com",
            target: "/v1/oauth/token",
        },
        usage: ControlEndpoint {
            host: "api.anthropic.com",
            target: "/api/oauth/usage",
        },
        axios_user_agent: "axios/1.15.2",
        default_login_scopes: "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload user:plugins",
        read_user_agent: "claude-code/2.1.310",
        refresh_fallback_scopes: "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload user:plugins",
    },
    tls: TlsProfiles {
        curves: "X25519MLKEM768:X25519:P-256:P-384",
        control_cipher_list: "ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES128-SHA256:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA",
        signature_algorithms: "ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:rsa_pkcs1_sha256:ecdsa_secp384r1_sha384:rsa_pss_rsae_sha384:rsa_pkcs1_sha384:rsa_pss_rsae_sha512:rsa_pkcs1_sha512:ed25519:ecdsa_secp521r1_sha512:rsa_pkcs1_sha1",
    },
};

/// The one profile this build carries: every reader takes it from here, and a re-pin changes it with the rebuild.
pub fn claude_code_2_1_310_linux_x64() -> &'static Profile {
    &PROFILE_2_1_310
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::path::Path;

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
