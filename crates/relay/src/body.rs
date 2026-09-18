//! The in-place body edits: the bound account's identity, the model the upstream is asked for, and the billing
//! hash that has to follow both.

use crate::identity::is_valid_id;
use crate::RelayError;

#[derive(Debug)]
pub struct BodyEdits {
    pub account_uuid_replaced: bool,
    pub attribution_stripped: bool,
    pub model_replaced: bool,
    pub cch_restamped: bool,
}

/// A member an edit needs to find. A body toward the measured host that lacks one is refused rather than sent
/// on: the account it would be billed against, or the model it would be served by, is not the one the router
/// resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyMember {
    MetadataUserId,
    Model,
}

impl BodyMember {
    pub fn path(self) -> &'static str {
        match self {
            BodyMember::MetadataUserId => "metadata.user_id",
            BodyMember::Model => "model",
        }
    }
}

/// What a splice found where it expected a member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Spliced {
    /// The member is not there, or not in the shape the scanner reads.
    Absent,
    Replaced,
    /// The member is already what the relay would have written.
    Unchanged,
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
        // The client hashes by rules this build no longer shares. Neither a restamp by the old rules nor the
        // client's own hash over an edited body is known to satisfy the upstream, so nothing is sent until the
        // router is re-pinned; the caller still learns which kind of drift it was.
        return Err(RelayError::ClientDrift { kind: client_drift });
    }
    // An API-key account has no identity to splice: the measured client never carried one for it.
    let account_uuid_replaced = match account_uuid {
        None => false,
        Some(uuid) => {
            // The uuid is spliced into an escaped JSON string, so a value needing escaping would corrupt the
            // body rather than identify the account.
            if !is_valid_id(uuid) {
                return Err(RelayError::AccountUuid);
            }
            match replace_account_uuid(body, uuid) {
                // Silently sending a body the account identity never reached would bill the request against
                // whatever the caller wrote, so the caller is told which member the relay could not find.
                Spliced::Absent => return Err(RelayError::BodyShape(BodyMember::MetadataUserId)),
                Spliced::Replaced => true,
                Spliced::Unchanged => false,
            }
        }
    };
    let model_replaced = match replace_model(body, upstream_model) {
        Spliced::Absent => return Err(RelayError::BodyShape(BodyMember::Model)),
        Spliced::Replaced => true,
        Spliced::Unchanged => false,
    };
    let cch_restamped = phoebis_cch::restamp_if_present(body)?;
    Ok(BodyEdits {
        account_uuid_replaced,
        attribution_stripped: false,
        model_replaced,
        cch_restamped,
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

fn replace_account_uuid(body: &mut Vec<u8>, uuid: &str) -> Spliced {
    let Some((start, end)) = phoebis_cch::string_member_span(body, &["metadata", "user_id"]) else {
        return Spliced::Absent;
    };
    let Some(key_at) = find(&body[start..end], ESCAPED_UUID_KEY) else {
        return Spliced::Absent;
    };
    let value_start = start + key_at + ESCAPED_UUID_KEY.len();
    let Some(value_len) = find(&body[value_start..end], ESCAPED_QUOTE) else {
        return Spliced::Absent;
    };
    let value_end = value_start + value_len;
    if body[value_start..value_end] == *uuid.as_bytes() {
        return Spliced::Unchanged;
    }
    body.splice(value_start..value_end, uuid.bytes());
    Spliced::Replaced
}

fn replace_model(body: &mut Vec<u8>, upstream_model: &str) -> Spliced {
    let Some((start, end)) = phoebis_cch::string_member_span(body, &["model"]) else {
        return Spliced::Absent;
    };
    if serde_json::from_slice::<String>(&body[start..end])
        .ok()
        .as_deref()
        == Some(upstream_model)
    {
        return Spliced::Unchanged;
    }
    let encoded = serde_json::to_string(upstream_model).expect("a string always encodes as JSON");
    body.splice(start..end, encoded.bytes());
    Spliced::Replaced
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "00000000-0000-4000-8000-000000000001";

    fn native(uuid: &str, model: &str) -> Vec<u8> {
        format!(
            r#"{{"model":"{model}","metadata":{{"user_id":"{{\"account_uuid\":\"{uuid}\",\"session_id\":\"s-1\"}}"}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn a_body_that_already_names_the_account_and_the_model_keeps_its_bytes() {
        let mut body = native(UUID, "claude-sonnet-4-5");
        let before = body.clone();
        let edits = edit_body(&mut body, Some(UUID), "claude-sonnet-4-5")
            .expect("a JSON object is editable");
        assert!(!edits.account_uuid_replaced);
        assert!(!edits.model_replaced);
        assert!(!edits.cch_restamped);
        assert_eq!(body, before);
    }

    #[test]
    fn an_oauth_body_with_no_account_uuid_target_is_refused() {
        let mut body = br#"{"model":"claude-sonnet-4-5","messages":[]}"#.to_vec();
        assert!(matches!(
            edit_body(&mut body, Some(UUID), "claude-sonnet-4-5"),
            Err(RelayError::BodyShape(BodyMember::MetadataUserId))
        ));
    }

    #[test]
    fn a_body_with_no_top_level_model_is_refused_for_either_account_kind() {
        let mut body = br#"{"messages":[]}"#.to_vec();
        assert!(matches!(
            edit_body(&mut body, None, "claude-sonnet-4-5"),
            Err(RelayError::BodyShape(BodyMember::Model))
        ));
        let mut oauth = String::from_utf8(native(UUID, "claude-sonnet-4-5"))
            .unwrap()
            .replacen(r#""model""#, r#""mdl""#, 1)
            .into_bytes();
        assert!(matches!(
            edit_body(&mut oauth, Some(UUID), "claude-sonnet-4-5"),
            Err(RelayError::BodyShape(BodyMember::Model))
        ));
    }

    #[test]
    fn an_api_key_body_needs_no_account_uuid() {
        let mut body = br#"{"model":"claude-sonnet-4-5","messages":[]}"#.to_vec();
        let edits = edit_body(&mut body, None, "claude-sonnet-4-5")
            .expect("an api-key account never carried an identity");
        assert!(!edits.account_uuid_replaced);
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
