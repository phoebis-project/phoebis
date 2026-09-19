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

/// `sdk-cli` out of `claude-cli/2.1.269 (external, sdk-cli)`; `cli` when the agent carries no parenthesis, or
/// when the parenthesized tail is not a bare lowercase-and-hyphen token (the client's own spellings: `cli`,
/// `sdk-cli`, `claude-vscode`, and the like). Unsanitized, a forged User-Agent could smuggle a `;`-separated
/// field of its own choosing into the billing line this build then signs.
pub fn entrypoint_of(user_agent: &str) -> &str {
    let Some(open) = user_agent.find('(') else {
        return "cli";
    };
    let Some(close) = user_agent[open..].find(')') else {
        return "cli";
    };
    let Some(candidate) = user_agent[open + 1..open + close].rsplit(',').next() else {
        return "cli";
    };
    let candidate = candidate.trim();
    let valid = !candidate.is_empty()
        && candidate
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'-');
    if valid {
        candidate
    } else {
        "cli"
    }
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
    // The dispatch above matched `BILLING_BLOCK`, which stops right after the colon; a line that carries no
    // space after it, or nothing at all past the prefix, is not a shape this build ever writes, and splicing a
    // field into it under the assumption that it is would double the prefix or leave a stray separator behind.
    let Some(rest) = decoded.strip_prefix("x-anthropic-billing-header: ") else {
        return Ok(Vec::new());
    };
    let rest = rest.trim_end_matches(';');
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    let fields: Vec<String> = rest.split("; ").map(String::from).collect();
    let (fields, injected) = apply_candidates(restore, fields);
    if injected.is_empty() {
        return Ok(Vec::new());
    }
    let line = format!("x-anthropic-billing-header: {};", fields.join("; "));
    let encoded = serde_json::to_string(&line).expect("a string always encodes as JSON");
    body.splice(start..end, encoded.bytes());
    Ok(injected)
}

/// Where the whole line splices in: right before the array's own first element, or, when the array holds none,
/// right before its closing bracket.
enum SystemArraySlot {
    BeforeFirstElement(usize),
    Empty(usize),
}

fn insert_whole_line(
    body: &mut Vec<u8>,
    restore: &Restore<'_>,
) -> Result<Vec<Injected>, RelayError> {
    // Both lookups are the depth-aware walk `phoebis_cch`'s own scanners use, so a `"system"`-shaped decoy earlier
    // in the body (a `"role":"system"` message, a tool's own `{"system":[...]}`) never stands in for the
    // top-level member. `array_member_span` already covers an array with an element; a `None` from it can only
    // mean an empty array once `restore_billing_line` has confirmed `system` really is one, but a duplicate
    // top-level `"system"` key can still make the depth-aware walk (first match) and the caller's own
    // `serde_json` check (last match) disagree, so a `None` here too is a body this build declines to guess at.
    let slot = match phoebis_cch::array_member_span(body, &["system"], 0) {
        Some((at, _)) => SystemArraySlot::BeforeFirstElement(at),
        None => match phoebis_cch::empty_array_member_end(body, &["system"]) {
            Some(at) => SystemArraySlot::Empty(at),
            None => return Ok(Vec::new()),
        },
    };
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
    match slot {
        SystemArraySlot::BeforeFirstElement(at) => {
            body.splice(at..at, format!("{element},").bytes());
        }
        SystemArraySlot::Empty(at) => {
            body.splice(at..at, element.bytes());
        }
    }
    Ok(injected)
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
        // Hand-built: no fixture carries cc_workload or cc_is_subagent, but FIELD_ORDER still has to seat cch
        // ahead of them and cc_prev_req behind them once they are on the line.
        let mut body = billing_body(
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; \
             cc_workload=interactive; cc_is_subagent=false;",
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
             cc_workload=interactive; cc_is_subagent=false; cc_prev_req=req_1; \
             cc_prompt_id=11111111-1111-4111-8111-111111111111;"
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

    fn full_restore() -> Restore<'static> {
        Restore {
            cli_version: "2.1.269",
            entrypoint: "sdk-cli",
            prompt_chars: ['0', '0', '0'],
            prompt_id: Some("11111111-1111-4111-8111-111111111111"),
            prev_req: None,
            restore_cch: true,
        }
    }

    #[test]
    fn a_role_system_message_ahead_of_an_empty_top_level_system_does_not_confuse_the_insertion() {
        let mut body =
            br#"{"model":"m","messages":[{"role":"system","content":"x"}],"system":[]}"#.to_vec();
        let injected = restore_billing_line(&mut body, &full_restore()).unwrap();
        assert_eq!(injected, vec![Injected::Cch, Injected::PromptId]);
        assert!(
            text_of(&body).starts_with(
                "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli;"
            ),
            "{}",
            text_of(&body)
        );
    }

    #[test]
    fn a_nested_empty_system_array_ahead_of_the_top_level_one_is_not_spliced_into() {
        let mut body = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"tool_use","id":"t1","name":"x","input":{"system":[]}}]}],"system":[]}"#.to_vec();
        let injected = restore_billing_line(&mut body, &full_restore()).unwrap();
        assert_eq!(injected, vec![Injected::Cch, Injected::PromptId]);
        let text = String::from_utf8(body.clone()).unwrap();
        assert!(
            text.contains(r#""input":{"system":[]}"#),
            "the nested array is untouched: {text}"
        );
        assert!(text_of(&body).starts_with("x-anthropic-billing-header:"));
    }

    #[test]
    fn duplicate_top_level_system_keys_are_left_alone_rather_than_guessed_at() {
        // `serde_json` reads the object as the last `"system"` (the array), so `system_is_array` passes; the
        // depth-aware scanner reads the object as the first (the string) and cannot descend into it as an
        // array, so every lookup that would locate an insertion point returns nothing rather than a guess.
        let mut body = br#"{"model":"m","system":"be helpful","messages":[],"system":[]}"#.to_vec();
        let before = body.clone();
        let injected = restore_billing_line(&mut body, &full_restore()).unwrap();
        assert!(injected.is_empty());
        assert_eq!(body, before);
    }

    #[test]
    fn a_prefix_missing_its_space_or_carrying_nothing_after_it_is_left_alone() {
        for line in [
            "x-anthropic-billing-header:cc_version=1;",
            "x-anthropic-billing-header: ",
        ] {
            let mut body = billing_body(line);
            let before = body.clone();
            let injected = restore_billing_line(&mut body, &full_restore()).unwrap();
            assert!(injected.is_empty(), "{line}");
            assert_eq!(body, before, "{line}");
        }
    }

    #[test]
    fn entrypoint_of_reads_the_clients_own_spellings_and_falls_back_on_anything_else() {
        assert_eq!(
            entrypoint_of("claude-cli/2.1.269 (external, sdk-cli)"),
            "sdk-cli"
        );
        assert_eq!(entrypoint_of("claude-cli/2.1.269 (external, cli)"), "cli");
        assert_eq!(
            entrypoint_of("claude-cli/2.1.269 (external, claude-vscode)"),
            "claude-vscode"
        );
        assert_eq!(
            entrypoint_of("claude-cli/2.1.269"),
            "cli",
            "no parenthesis at all"
        );
        assert_eq!(
            entrypoint_of("claude-cli/2.1.269 (external, cli; cc_prev_req=forged)"),
            "cli",
            "a forged field smuggled into the parenthesis is not a bare token"
        );
        assert_eq!(
            entrypoint_of("claude-cli/2.1.269 (external, sdk cli)"),
            "cli",
            "whitespace inside the token is not a spelling the client uses"
        );
    }
}
