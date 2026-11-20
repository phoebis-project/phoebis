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

/// The OAuth scope each claude.ai feature needs, as this client build asks for it.
pub struct FeatureScopes {
    pub artifacts: &'static str,
    pub connectors: &'static str,
}

/// How a row's path matches: each path under a prefix that ends in a slash, or one whole path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRule {
    Under(&'static str),
    Whole(&'static str),
}

/// One call of a claude.ai feature as this client build sends it.
#[derive(Clone, Copy, Debug)]
pub struct FeatureRow {
    /// A method and a whole path the row leaves out. The path holds visible ASCII characters alone and no percent
    /// escape, since the listener compares it with the visible ASCII characters of the last decoded form of a request's
    /// path, which holds no escape, up to that form's first `?` or `#`. It holds no empty segment and no closing slash,
    /// since the compare drops them from the request's side alone. A path with an escape, a space or a character
    /// outside ASCII would match no request, including one that sends it as written, and the call it names would go
    /// upstream under the feature account's token.
    pub except: &'static [(&'static str, &'static str)],
    pub host: &'static str,
    pub methods: &'static [&'static str],
    pub path: PathRule,
    pub plane: Plane,
}

/// A call the feature listener serves without a key, matched by its whole shape.
#[derive(Clone, Copy, Debug)]
pub struct KeylessRow {
    pub host: &'static str,
    pub methods: &'static [&'static str],
    pub path: &'static str,
}

/// One socket of a claude.ai feature: a `GET` that upgrades to a WebSocket on a whole path.
#[derive(Clone, Copy, Debug)]
pub struct SocketRow {
    /// The feature's word, as `Feature::as_str` spells it.
    pub feature: &'static str,
    pub host: &'static str,
    pub path: &'static str,
    pub plane: Plane,
}

/// The calls of each claude.ai feature the router forwards, as this client build sends them, and the two it serves
/// without a key.
pub struct FeatureTable {
    pub artifacts: &'static [FeatureRow],
    pub connectors: &'static [FeatureRow],
    /// The client's check of a domain ahead of a web fetch, which names no account and leaves without a credential.
    pub domain_check: KeylessRow,
    /// The first-start check of an interactive client, which the router answers itself.
    pub hello: KeylessRow,
    /// The sockets the router carries, each admitted by its feature's own rule rather than a router key.
    pub sockets: &'static [SocketRow],
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
    pub feature_scopes: FeatureScopes,
    pub features: FeatureTable,
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
    // The client names no scope past inference for Artifacts, and opens the connectors under the MCP servers' scope.
    feature_scopes: FeatureScopes {
        artifacts: "user:inference",
        connectors: "user:mcp_servers",
    },
    // The methods are the ones this build sends on each row, and the `DELETE` of the two MCP session rows is the
    // session end the build defines and no site of it calls. `POST /api/frame/track` is the client's event report,
    // which the router forwards under no account. The MCP sessions leave on the inference plane, since the client opens
    // them through its fetch path, whose hello the inference connector shows each of the two hosts.
    features: FeatureTable {
        artifacts: &[
            FeatureRow {
                except: &[],
                host: "api.anthropic.com",
                methods: &["DELETE", "GET", "POST"],
                path: PathRule::Whole("/api/artifacts/mcp"),
                plane: Plane::Inference,
            },
            FeatureRow {
                except: &[("POST", "/api/frame/track")],
                host: "api.anthropic.com",
                methods: &["DELETE", "GET", "PATCH", "POST"],
                path: PathRule::Under("/api/frame/"),
                plane: Plane::Control,
            },
        ],
        connectors: &[
            FeatureRow {
                except: &[],
                host: "api.anthropic.com",
                methods: &["GET"],
                path: PathRule::Whole("/v1/mcp_servers"),
                plane: Plane::Control,
            },
            FeatureRow {
                except: &[],
                host: "mcp-proxy.anthropic.com",
                methods: &["DELETE", "GET", "POST"],
                path: PathRule::Under("/v1/mcp/"),
                plane: Plane::Inference,
            },
        ],
        domain_check: KeylessRow {
            host: "api.anthropic.com",
            methods: &["GET"],
            path: "/api/web/domain_info",
        },
        hello: KeylessRow {
            host: "api.anthropic.com",
            methods: &["GET", "HEAD"],
            path: "/api/hello",
        },
        // A capture of the client's socket hello equals the inference connector's, so the socket leaves on that plane,
        // whatever the plane of the page reads under `/api/frame/`.
        sockets: &[SocketRow {
            feature: "artifacts",
            host: "api.anthropic.com",
            path: "/api/frame/sync",
            plane: Plane::Inference,
        }],
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
