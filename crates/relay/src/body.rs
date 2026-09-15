//! The in-place body edits: the bound account's identity, the model the upstream is asked for, and the billing
//! hash that has to follow both.

use crate::identity::is_valid_id;
use crate::RelayError;

#[derive(Debug)]
pub struct BodyEdits {
    pub account_uuid_replaced: bool,
    pub model_replaced: bool,
    pub cch_restamped: bool,
    /// Names the kind of disagreement when the client's own billing hash was not computed by this build's
    /// rules, in which case the edits that would force a restamp were skipped.
    pub client_drift: Option<&'static str>,
}

/// `"account_uuid":"` as it appears escaped inside the `metadata.user_id` string.
const ESCAPED_UUID_KEY: &[u8] = br#"\"account_uuid\":\""#;
const ESCAPED_QUOTE: &[u8] = br#"\""#;

/// Opening quote and prefix of the billing block as it appears in `system[0].text`.
const BILLING_BLOCK: &[u8] = br#""x-anthropic-billing-header:"#;

/// Length of the hash after `cch=`, the only bytes `phoebis_cch::sign` rewrites.
const CCH_DIGITS: usize = 5;

pub fn edit_body(
    body: &mut Vec<u8>,
    account_uuid: Option<&str>,
    upstream_model: &str,
) -> Result<BodyEdits, RelayError> {
    // A full parse would walk a body of up to the request limit a second time to learn what the upstream
    // validates anyway; the edits below fail closed on a body their scanner cannot walk.
    if !starts_with_object(body) {
        return Err(RelayError::NotAnObject);
    }
    if let Some(client_drift) = drift(body)? {
        // The client hashes by rules this build no longer shares, so its hash is the only one that can still
        // satisfy the upstream: every edit that would force a restamp is dropped and the body travels as it
        // arrived. The model edit stays because the hash view empties the model string anyway.
        return Ok(BodyEdits {
            account_uuid_replaced: false,
            model_replaced: replace_model(body, upstream_model),
            cch_restamped: false,
            client_drift: Some(client_drift),
        });
    }
    let account_uuid_replaced = match account_uuid {
        None => false,
        Some(uuid) => {
            // The uuid is spliced into an escaped JSON string, so a value needing escaping would corrupt the
            // body rather than identify the account.
            if !is_valid_id(uuid) {
                return Err(RelayError::AccountUuid);
            }
            replace_account_uuid(body, uuid)
        }
    };
    let model_replaced = replace_model(body, upstream_model);
    let cch_restamped = phoebis_cch::restamp_if_present(body)?;
    Ok(BodyEdits {
        account_uuid_replaced,
        model_replaced,
        cch_restamped,
        client_drift: None,
    })
}

/// Whether the hash the client stamped differs from the one this build computes over the body as received.
/// A body that stamped no hash has nothing to protect and never drifts.
///
/// `find_cch` reads lowercase hex only, so a client stamping uppercase digits leaves a stamp that scanner
/// cannot read at all; that counts as drift rather than as a body carrying no hash.
fn drift(body: &[u8]) -> Result<Option<&'static str>, RelayError> {
    if !carries_a_stamp(body) {
        return Ok(None);
    }
    let Some(location) = phoebis_cch::find_cch(body) else {
        return Ok(Some("hash"));
    };
    let digits = location.digits_start..location.digits_start + CCH_DIGITS;
    let signed = phoebis_cch::sign(body)?;
    Ok((signed[digits.clone()] != body[digits]).then_some("hash"))
}

/// Whether the billing block claims a hash, readable by `find_cch` or not. A first-party client stamps one; a
/// non-first-party billing block carries the entrypoint alone and leaves the body free to be edited.
fn carries_a_stamp(body: &[u8]) -> bool {
    phoebis_cch::string_member_span(body, &["system", "0", "text"]).is_some_and(|(start, end)| {
        let text = &body[start..end];
        text.starts_with(BILLING_BLOCK) && find(text, b"cch=").is_some()
    })
}

fn starts_with_object(body: &[u8]) -> bool {
    body.iter()
        .find(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        == Some(&b'{')
}

fn replace_account_uuid(body: &mut Vec<u8>, uuid: &str) -> bool {
    let Some((start, end)) = phoebis_cch::string_member_span(body, &["metadata", "user_id"]) else {
        return false;
    };
    let Some(key_at) = find(&body[start..end], ESCAPED_UUID_KEY) else {
        return false;
    };
    let value_start = start + key_at + ESCAPED_UUID_KEY.len();
    let Some(value_len) = find(&body[value_start..end], ESCAPED_QUOTE) else {
        return false;
    };
    let value_end = value_start + value_len;
    if body[value_start..value_end] == *uuid.as_bytes() {
        return false;
    }
    body.splice(value_start..value_end, uuid.bytes());
    true
}

fn replace_model(body: &mut Vec<u8>, upstream_model: &str) -> bool {
    let Some((start, end)) = phoebis_cch::string_member_span(body, &["model"]) else {
        return false;
    };
    if serde_json::from_slice::<String>(&body[start..end])
        .ok()
        .as_deref()
        == Some(upstream_model)
    {
        return false;
    }
    let encoded = serde_json::to_string(upstream_model).expect("a string always encodes as JSON");
    body.splice(start..end, encoded.bytes());
    true
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_without_the_native_metadata_keeps_its_bytes() {
        let mut body = br#"{"model":"claude-sonnet-4-5","messages":[]}"#.to_vec();
        let before = body.clone();
        let edits = edit_body(
            &mut body,
            Some("00000000-0000-4000-8000-000000000001"),
            "claude-sonnet-4-5",
        )
        .expect("a JSON object is editable");
        assert!(!edits.account_uuid_replaced);
        assert!(!edits.model_replaced);
        assert!(!edits.cch_restamped);
        assert_eq!(body, before);
    }

    #[test]
    fn the_uuid_and_the_model_are_spliced_in_place() {
        let mut body = br#"{"model":"claude-sonnet-4-5[1m]","metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"}}"#.to_vec();
        let edits = edit_body(&mut body, Some("acct-uuid"), "claude-sonnet-4-5")
            .expect("a JSON object is editable");
        assert!(edits.account_uuid_replaced && edits.model_replaced);
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"model":"claude-sonnet-4-5","metadata":{"user_id":"{\"account_uuid\":\"acct-uuid\",\"session_id\":\"s-1\"}"}}"#
        );
    }

    #[test]
    fn a_body_that_is_not_an_object_is_refused() {
        let mut body = b"[1,2,3]".to_vec();
        assert!(matches!(
            edit_body(&mut body, None, "claude-sonnet-4-5"),
            Err(RelayError::NotAnObject)
        ));
    }
}
