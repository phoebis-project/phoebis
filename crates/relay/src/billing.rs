//! The billing-line editor: filling in the first-party fields a reduced-shape request lacks, without disturbing
//! a field the caller already sent.

use crate::body::BILLING_BLOCK;
use crate::memory::Injected;
use crate::RelayError;

/// The fields of the attribution line in the client's own order; a restored field lands where this order puts it.
pub const FIELD_ORDER: [&str; 7] = [
    "cc_version",
    "cc_entrypoint",
    "cch",
    "cc_workload",
    "cc_is_subagent",
    "cc_prev_req",
    "cc_prompt_id",
];

pub struct Restore<'a> {
    pub cli_version: &'a str,
    pub entrypoint: &'a str,
    pub prompt_chars: [char; 3],
    pub prompt_id: Option<&'a str>,
    pub prev_req: Option<&'a str>,
    pub restore_cch: bool,
}

/// Fills every absent field the restoration names, or inserts the whole line as `system[0]` when the body has a
/// `system` array whose first block is not the attribution line. Returns what was inserted. A present field is
/// never rewritten.
pub fn restore_billing_line(
    body: &mut Vec<u8>,
    restore: &Restore<'_>,
) -> Result<Vec<Injected>, RelayError> {
    let system_is_array = matches!(
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("system").cloned()),
        Some(serde_json::Value::Array(_))
    );
    if !system_is_array {
        return Ok(Vec::new());
    }
    match phoebis_cch::string_member_span(body, &["system", "0", "text"]) {
        Some((start, end)) if body[start..end].starts_with(BILLING_BLOCK) => {
            patch_line(body, start, end, restore)
        }
        _ => insert_whole_line(body, restore),
    }
}

/// `<cli_version>.<3 hex>` where the hex is `sha256("59cf53e54c78" + c4 + c7 + c20 + cli_version)[0..3]`.
pub fn cc_version(cli_version: &str, prompt_chars: [char; 3]) -> String {
    use sha2::{Digest, Sha256};
    let mut input = String::from("59cf53e54c78");
    input.extend(prompt_chars);
    input.push_str(cli_version);
    let digest = Sha256::digest(input.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{cli_version}.{}", &hex[0..3])
}

/// `sdk-cli` out of `claude-cli/2.1.269 (external, sdk-cli)`; `cli` when the agent carries no parenthesis.
pub fn entrypoint_of(user_agent: &str) -> &str {
    let Some(open) = user_agent.find('(') else {
        return "cli";
    };
    let Some(close) = user_agent[open..].find(')') else {
        return "cli";
    };
    user_agent[open + 1..open + close]
        .rsplit(',')
        .next()
        .map(str::trim)
        .unwrap_or("cli")
}

/// Absent-field candidates in `FIELD_ORDER`'s relative order: `cch` before `cc_prev_req` before `cc_prompt_id`.
fn candidates(restore: &Restore<'_>) -> Vec<(&'static str, String, Injected)> {
    let mut out = Vec::new();
    if restore.restore_cch {
        out.push(("cch", "cch=00000".to_string(), Injected::Cch));
    }
    if let Some(id) = restore.prev_req {
        out.push((
            "cc_prev_req",
            format!("cc_prev_req={id}"),
            Injected::PrevReq,
        ));
    }
    if let Some(id) = restore.prompt_id {
        out.push((
            "cc_prompt_id",
            format!("cc_prompt_id={id}"),
            Injected::PromptId,
        ));
    }
    out
}

/// Inserts `value` after the last field in `fields` that precedes `name` in `FIELD_ORDER`, or at the front when
/// none does. A field already present is left untouched and not counted as injected.
fn insert_if_absent(
    fields: &mut Vec<String>,
    injected: &mut Vec<Injected>,
    name: &str,
    value: String,
    item: Injected,
) {
    if fields.iter().any(|f| f.starts_with(&format!("{name}="))) {
        return;
    }
    let order = FIELD_ORDER
        .iter()
        .position(|f| *f == name)
        .expect("restorable field is in FIELD_ORDER");
    let at = fields
        .iter()
        .rposition(|f| {
            let fname = f.split('=').next().unwrap_or(f);
            FIELD_ORDER
                .iter()
                .position(|o| *o == fname)
                .is_some_and(|p| p < order)
        })
        .map_or(0, |i| i + 1);
    fields.insert(at, value);
    injected.push(item);
}

fn apply_candidates(
    restore: &Restore<'_>,
    mut fields: Vec<String>,
) -> (Vec<String>, Vec<Injected>) {
    let mut injected = Vec::new();
    for (name, value, item) in candidates(restore) {
        insert_if_absent(&mut fields, &mut injected, name, value, item);
    }
    (fields, injected)
}

fn patch_line(
    body: &mut Vec<u8>,
    start: usize,
    end: usize,
    restore: &Restore<'_>,
) -> Result<Vec<Injected>, RelayError> {
    let decoded: String = serde_json::from_slice(&body[start..end])
        .expect("string_member_span validated a JSON string");
    let rest = decoded
        .strip_prefix("x-anthropic-billing-header: ")
        .unwrap_or(&decoded);
    let fields: Vec<String> = rest
        .trim_end_matches(';')
        .split("; ")
        .map(String::from)
        .collect();
    let (fields, injected) = apply_candidates(restore, fields);
    if injected.is_empty() {
        return Ok(Vec::new());
    }
    let line = format!("x-anthropic-billing-header: {};", fields.join("; "));
    let encoded = serde_json::to_string(&line).expect("a string always encodes as JSON");
    body.splice(start..end, encoded.bytes());
    Ok(injected)
}

fn insert_whole_line(
    body: &mut Vec<u8>,
    restore: &Restore<'_>,
) -> Result<Vec<Injected>, RelayError> {
    let fields = vec![
        format!(
            "cc_version={}",
            cc_version(restore.cli_version, restore.prompt_chars)
        ),
        format!("cc_entrypoint={}", restore.entrypoint),
    ];
    let (fields, injected) = apply_candidates(restore, fields);
    let line = format!("x-anthropic-billing-header: {};", fields.join("; "));
    let element = format!(
        r#"{{"type":"text","text":{}}}"#,
        serde_json::to_string(&line).expect("a string always encodes as JSON")
    );
    match phoebis_cch::array_member_span(body, &["system"], 0) {
        Some((at, _)) => {
            body.splice(at..at, format!("{element},").bytes());
        }
        None => {
            let at = empty_system_array_end(body).expect("caller confirmed system is an array");
            body.splice(at..at, element.bytes());
        }
    }
    Ok(injected)
}

/// The empty-array shape (`"system":[]`) has no element for `array_member_span` to return; there is nothing
/// between the brackets to walk past, so the scan for `]` needs no depth tracking.
fn empty_system_array_end(body: &[u8]) -> Option<usize> {
    let key = br#""system""#;
    let at = body.windows(key.len()).position(|w| w == key)?;
    let mut i = at + key.len();
    let ws = |body: &[u8], i: &mut usize| {
        while matches!(body.get(*i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            *i += 1;
        }
    };
    ws(body, &mut i);
    if body.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    ws(body, &mut i);
    if body.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    ws(body, &mut i);
    (body.get(i) == Some(&b']')).then_some(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn billing_body(line: &str) -> Vec<u8> {
        format!(r#"{{"model":"m","system":[{{"type":"text","text":"{line}"}}],"messages":[]}}"#)
            .into_bytes()
    }

    fn text_of(body: &[u8]) -> String {
        let (start, end) = phoebis_cch::string_member_span(body, &["system", "0", "text"]).unwrap();
        serde_json::from_slice(&body[start..end]).unwrap()
    }

    #[test]
    fn cc_version_hashes_the_salt_the_prompt_characters_and_the_client_version() {
        assert_eq!(cc_version("2.1.269", ['0', '0', '0']), "2.1.269.517");
    }

    #[test]
    fn each_absent_field_is_inserted_after_the_last_present_field_that_precedes_it() {
        let mut body = billing_body(
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli;",
        );
        let restore = Restore {
            cli_version: "2.1.269",
            entrypoint: "sdk-cli",
            prompt_chars: ['0', '0', '0'],
            prompt_id: Some("11111111-1111-4111-8111-111111111111"),
            prev_req: Some("req_1"),
            restore_cch: true,
        };
        let injected = restore_billing_line(&mut body, &restore).unwrap();
        assert_eq!(
            injected,
            vec![Injected::Cch, Injected::PrevReq, Injected::PromptId]
        );
        assert_eq!(
            text_of(&body),
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; cch=00000; \
             cc_prev_req=req_1; cc_prompt_id=11111111-1111-4111-8111-111111111111;"
        );
    }

    #[test]
    fn a_present_cc_prev_req_is_kept() {
        let mut body = billing_body(
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; cc_prev_req=req_kept;",
        );
        let restore = Restore {
            cli_version: "2.1.269",
            entrypoint: "sdk-cli",
            prompt_chars: ['0', '0', '0'],
            prompt_id: None,
            prev_req: Some("req_other"),
            restore_cch: false,
        };
        restore_billing_line(&mut body, &restore).unwrap();
        assert_eq!(
            text_of(&body),
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; cc_prev_req=req_kept;"
        );
    }

    #[test]
    fn a_body_whose_system_is_a_string_is_left_alone() {
        let mut body = br#"{"model":"m","system":"be helpful","messages":[]}"#.to_vec();
        let before = body.clone();
        let restore = Restore {
            cli_version: "2.1.269",
            entrypoint: "sdk-cli",
            prompt_chars: ['0', '0', '0'],
            prompt_id: Some("11111111-1111-4111-8111-111111111111"),
            prev_req: None,
            restore_cch: true,
        };
        let injected = restore_billing_line(&mut body, &restore).unwrap();
        assert!(injected.is_empty());
        assert_eq!(body, before);
    }
}
