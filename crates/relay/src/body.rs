//! The in-place body edits: the bound account's identity, the model the upstream is asked for, and the billing
//! hash that has to follow both.

use crate::identity::is_valid_id;
use crate::RelayError;

#[derive(Debug)]
pub struct BodyEdits {
    pub account_uuid_replaced: bool,
    pub model_replaced: bool,
    pub cch_restamped: bool,
}

/// `"account_uuid":"` as it appears escaped inside the `metadata.user_id` string.
const ESCAPED_UUID_KEY: &[u8] = br#"\"account_uuid\":\""#;
const ESCAPED_QUOTE: &[u8] = br#"\""#;

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
