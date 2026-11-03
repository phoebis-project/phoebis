//! Typed access to the measured fixture files embedded at compile time.

use crate::Plane;

/// The request heads `inference_head` answers. The count heads come from the same capture as the inference heads,
/// so a difference between the two belongs to the count path. Every count head carries no `X-Stainless-Timeout`
/// and none of the `advanced-tool-use-2025-11-20`, `extended-cache-ttl-2025-04-11`,
/// `prompt-caching-scope-2026-01-05` and `thinking-token-count-2026-05-13` betas, the first-party one also none of
/// `cache-diagnosis-2026-04-07` and `thinking-binding-controls-2026-08-01`, and all three append
/// `token-counting-2024-11-01`.
pub enum FixtureKind {
    OauthNonFirstParty,
    OauthFirstParty,
    ApiKey,
    Hello,
    CountOauthNonFirstParty,
    CountOauthFirstParty,
    CountApiKey,
}
pub enum BodyFixture {
    FirstParty,
    FirstPartyAgain,
    Oauth,
    ApiKey,
    /// The one count body the three count heads share.
    Count,
}
pub enum ControlFixture {
    /// The refresh grant.
    Token,
    /// The authorization-code grant of a login. The capture's token endpoint answers both grants with one body.
    TokenCode,
    Profile,
    Roles,
    Usage,
}

pub fn inference_head(kind: FixtureKind) -> &'static [u8] {
    match kind {
        FixtureKind::OauthNonFirstParty => include_bytes!("../fixtures/inference_oauth.http"),
        FixtureKind::OauthFirstParty => include_bytes!("../fixtures/inference_oauth_fp.http"),
        FixtureKind::ApiKey => include_bytes!("../fixtures/inference_apikey.http"),
        FixtureKind::Hello => include_bytes!("../fixtures/hello.http"),
        FixtureKind::CountOauthNonFirstParty => include_bytes!("../fixtures/count_oauth.http"),
        FixtureKind::CountOauthFirstParty => include_bytes!("../fixtures/count_oauth_fp.http"),
        FixtureKind::CountApiKey => include_bytes!("../fixtures/count_apikey.http"),
    }
}

pub fn body(kind: BodyFixture) -> &'static [u8] {
    match kind {
        BodyFixture::FirstParty => include_bytes!("../fixtures/body_fp.json"),
        BodyFixture::FirstPartyAgain => include_bytes!("../fixtures/body_fp_again.json"),
        BodyFixture::Oauth => include_bytes!("../fixtures/body_oauth.json"),
        BodyFixture::ApiKey => include_bytes!("../fixtures/body_apikey.json"),
        BodyFixture::Count => include_bytes!("../fixtures/count.json"),
    }
}

/// The documented member with the redaction's placeholder for every machine-shaped value.
pub const SAFEGUARDS_MEMBER: &str = r#""safeguards":[{"type":"dangerous_tool_use","classifier_context":{"v":1,"permission_mode":"auto","platform":"linux","live_cwd":"<cwd>","home_dir":"<home>","git_state":{"origin":"<remote>"},"rules":{"allow":[{"rule":"<rule>","source":"<source>"}],"deny":[],"ask":[]}}}]"#;

/// The member spliced before the body's top-level `"stream":` (each recorded inference body carries it once, last),
/// else before the closing brace. Never signs: a stamped body keeps the client's `cch` until the test re-signs it
/// with `phoebis_cch::sign`, so this crate needs no dependency on the signer.
pub fn with_safeguards(body: &[u8]) -> Vec<u8> {
    const STREAM: &[u8] = br#""stream":"#;
    let (at, splice) = match body.windows(STREAM.len()).rposition(|w| w == STREAM) {
        Some(at) => (at, format!("{SAFEGUARDS_MEMBER},")),
        None => (
            body.iter()
                .rposition(|b| *b == b'}')
                .expect("a body fixture is a JSON object"),
            format!(",{SAFEGUARDS_MEMBER}"),
        ),
    };
    [&body[..at], splice.as_bytes(), &body[at..]].concat()
}

/// A raw fixture head (`inference_head` answers `&'static [u8]`) with the beta appended to its `anthropic-beta`
/// line; the relay suites hand it to `request_from_fixture`, the router suite to `parse_head`.
pub fn with_dangerous_tool_use_beta(head: &[u8]) -> Vec<u8> {
    const NAME: &[u8] = b"anthropic-beta:";
    let mut out = Vec::with_capacity(head.len() + 32);
    let mut found = false;
    for line in head.split_inclusive(|b| *b == b'\n') {
        if !found && line.len() >= NAME.len() && line[..NAME.len()].eq_ignore_ascii_case(NAME) {
            let end = line.len()
                - line
                    .iter()
                    .rev()
                    .take_while(|b| matches!(b, b'\r' | b'\n'))
                    .count();
            out.extend_from_slice(&line[..end]);
            out.push(b',');
            out.extend_from_slice(crate::DANGEROUS_TOOL_USE_BETA.as_bytes());
            out.extend_from_slice(&line[end..]);
            found = true;
        } else {
            out.extend_from_slice(line);
        }
    }
    assert!(found, "the head carries no anthropic-beta line to extend");
    out
}

pub fn client_hello(plane: Plane) -> Vec<u8> {
    let hex = match plane {
        Plane::Inference => include_str!("../fixtures/hello_inference.hex"),
        Plane::Control => include_str!("../fixtures/hello_control.hex"),
    };
    let hex = hex.trim();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("fixture hex"))
        .collect()
}

pub fn control_head(which: ControlFixture) -> &'static [u8] {
    match which {
        ControlFixture::Token => include_bytes!("../fixtures/control_token.http"),
        ControlFixture::TokenCode => include_bytes!("../fixtures/control_token_code.http"),
        ControlFixture::Profile => include_bytes!("../fixtures/control_profile.http"),
        ControlFixture::Roles => include_bytes!("../fixtures/control_roles.http"),
        ControlFixture::Usage => include_bytes!("../fixtures/control_usage.http"),
    }
}

pub fn control_response(which: ControlFixture) -> &'static [u8] {
    match which {
        ControlFixture::Token | ControlFixture::TokenCode => {
            include_bytes!("../fixtures/responses/token.json")
        }
        ControlFixture::Profile => include_bytes!("../fixtures/responses/profile.json"),
        ControlFixture::Roles => include_bytes!("../fixtures/responses/roles.json"),
        ControlFixture::Usage => include_bytes!("../fixtures/responses/usage.json"),
    }
}
