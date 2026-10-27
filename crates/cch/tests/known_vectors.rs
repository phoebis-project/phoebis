use phoebis_cch::{find_cch, sign};
use phoebis_profile::fixtures::{body, BodyFixture};

const BASE: &str = r#"{"model":"model-a","messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"tools":[],"metadata":{"user_id":"meta-x"},"max_tokens":1,"thinking":{"type":"adaptive","display":"omitted"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"output_config":{"effort":"high"},"stream":true}"#;

fn cch_of(body: &[u8]) -> String {
    let loc = find_cch(body).expect("billing block");
    String::from_utf8(body[loc.digits_start..loc.digits_start + 5].to_vec()).unwrap()
}

#[test]
fn reference_vectors_from_the_proxy_test_suite() {
    let cases: &[(&str, &str, &str)] = &[
        ("base", BASE, "7ee87"),
        ("model value ignored", &BASE.replacen(r#""model":"model-a""#, r#""model":"model-b""#, 1), "7ee87"),
        ("max tokens ignored", &BASE.replacen(r#""max_tokens":1"#, r#""max_tokens":2"#, 1), "7ee87"),
        ("message changes hash", &BASE.replacen(r#""text":"x""#, r#""text":"y""#, 1), "b9cc8"),
        ("metadata changes hash", &BASE.replacen(r#""user_id":"meta-x""#, r#""user_id":"meta-y""#, 1), "7a89d"),
        ("stream changes hash", &BASE.replacen(r#""stream":true"#, r#""stream":false"#, 1), "60400"),
        ("nested model value ignored", &BASE.replacen(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","model":"a"}"#, 1), "0601b"),
        ("nested max tokens member omitted", &BASE.replacen(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","max_tokens":2}"#, 1), "7ee87"),
        ("top level fallbacks member omitted", &BASE.replacen(r#""stream":true}"#, r#""stream":true,"fallbacks":[{"model":"fallback-a"}]}"#, 1), "7ee87"),
        ("trailing dispatch run keeps native comma", &BASE.replacen(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","max_tokens":999,"fallbacks":[{"model":"fallback-model"}]}"#, 1), "4589b"),
        ("model before trailing dispatch run", &BASE.replacen(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","model":"nested-model","max_tokens":999,"fallbacks":[{"model":"fallback-model"}],"fallback_credit_token":"not-a-real-token"}"#, 1), "2d312"),
        ("model splits dispatch runs", &BASE.replacen(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","max_tokens":999,"model":"nested-model","fallbacks":[{"model":"fallback-model"}]}"#, 1), "0601b"),
        ("billing block only", r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"}]}"#, "f2edb"),
    ];
    for (name, body, want) in cases {
        let signed = sign(body.as_bytes()).unwrap();
        assert_eq!(cch_of(&signed), *want, "case {name}");
        assert_eq!(
            signed.len(),
            body.len(),
            "case {name}: only the five digits change"
        );
    }
}

/// The builder re-signs a body after redacting it, so no literal pins the committed digits. Its `check_hashes`
/// compares the raw capture's digits with the signer's at capture time.
#[test]
fn captured_first_party_bodies_resign_to_their_native_value() {
    let digits = [BodyFixture::FirstParty, BodyFixture::FirstPartyAgain].map(|fixture| {
        let native = body(fixture);
        let digits = cch_of(native);
        assert_ne!(
            digits, "00000",
            "a first-party fixture carries a stamp, not the unpatched placeholder"
        );
        let signed = sign(native).unwrap();
        assert_eq!(signed, native, "re-signing a native body is a fixed point");
        digits
    });
    assert_ne!(
        digits[0], digits[1],
        "the two first-party fixtures are two hash vectors"
    );
}

#[test]
fn a_nested_system_member_does_not_capture_the_scan() {
    let with_decoy = BASE.replacen(
        r#"{"type":"text","text":"x"}"#,
        r#"{"type":"tool_use","input":{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=abcde;"}]}}"#,
        1,
    );
    let loc = find_cch(with_decoy.as_bytes()).expect("billing block");
    let decoy_at = with_decoy.find("cch=abcde;").expect("decoy");
    assert!(
        loc.digits_start > decoy_at,
        "the structural path skips the decoy in the tool-use input"
    );
    let signed = sign(with_decoy.as_bytes()).unwrap();
    assert_eq!(signed.len(), with_decoy.len());
    assert_eq!(
        String::from_utf8_lossy(&signed)
            .matches("cch=abcde;")
            .count(),
        1,
        "the decoy is left alone"
    );
}

#[test]
fn bodies_without_a_billing_block_pass_through() {
    let native = body(BodyFixture::Oauth);
    assert!(find_cch(native).is_none());
    assert_eq!(sign(native).unwrap(), native);
}
