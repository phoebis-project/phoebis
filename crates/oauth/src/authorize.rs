//! Builds the OAuth authorization URL, preserving Claude Code's measured parameter order.

use phoebis_profile::Profile;

/// The redirect Claude Code registers with the provider: a loopback URL on a port the OS hands out at login.
/// The router listens on no such port; the browser's navigation there fails to load and the operator copies
/// the URL from the address bar. Drawing the port from the same dynamic range the OS would keeps the
/// authorization request inside what the provider has seen from this client.
pub fn loopback_redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}/callback")
}

/// A port from the IANA dynamic range, which is where `listen(0)` lands on every platform the client runs on.
pub fn ephemeral_port() -> u16 {
    rand::random_range(49_152..=65_535)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginKind {
    ClaudeAi,
    Console,
}

/// The parameter order and encoding are measured from Claude Code; a map-based query serializer
/// would alphabetize the keys and lose it, so the query is built by hand instead.
pub fn authorization_url(
    profile: &Profile,
    kind: LoginKind,
    redirect_uri: &str,
    scopes: &str,
    challenge: &str,
    state: &str,
) -> String {
    let base = match kind {
        LoginKind::ClaudeAi => profile.control.authorize_claude_ai,
        LoginKind::Console => profile.control.authorize_console,
    };
    let pairs = [
        ("code", "true"),
        ("client_id", profile.control.client_id),
        ("response_type", "code"),
        ("redirect_uri", redirect_uri),
        ("scope", scopes),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];
    let query = pairs
        .into_iter()
        .map(|(key, value)| format!("{key}={}", percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{query}")
}

/// Percent-encodes everything outside RFC 3986's unreserved set, so a space becomes `%20` rather
/// than the `+` an `application/x-www-form-urlencoded` encoder would produce.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
