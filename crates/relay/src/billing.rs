//! The billing-line editor: filling in the first-party fields a reduced-shape request lacks, without disturbing
//! a field the caller already sent.

use crate::body::BILLING_BLOCK;
use crate::memory::Injected;
use crate::RelayError;

/// The fields of the attribution line in the client's own order. A restored field lands where this order puts it,
/// and `cc_turn_origin` and the five after it are never restored.
pub const FIELD_ORDER: [&str; 13] = [
    "cc_version",
    "cc_entrypoint",
    "cch",
    "cc_workload",
    "cc_is_subagent",
    "cc_prev_req",
    "cc_prompt_id",
    "cc_turn_origin",
    "cc_prompt_index",
    "cc_turn_index",
    "cc_gateway",
    "cc_project",
    "cc_project_src",
];

pub struct Restore<'a> {
    pub prompt_id: Option<&'a str>,
    pub prev_req: Option<&'a str>,
    /// Items the account has refused on a previous attempt; each is left out of the line even when the field
    /// it names is otherwise absent and restorable.
    pub skip: Vec<Injected>,
}

/// Fills every absent field the restoration names on the attribution line at `system[0]`. A body whose first
/// system block is not that line, or whose `system` is not an array, is left as sent: the relay never writes a
/// line the client did not, so every restoration carries the client's own stamp. A present field is never
/// rewritten.
pub fn restore_billing_line(
    body: &mut Vec<u8>,
    restore: &Restore<'_>,
) -> Result<Vec<Injected>, RelayError> {
    match phoebis_cch::string_member_span(body, &["system", "0", "text"]) {
        Some((start, end)) if body[start..end].starts_with(BILLING_BLOCK) => {
            patch_line(body, start, end, restore)
        }
        _ => Ok(Vec::new()),
    }
}

/// `<cli_version>.<3 hex>` where the hex is `sha256("59cf53e54c78" + u4 + u7 + u20 + cli_version)[0..3]` and
/// `u4`, `u7`, `u20` are the sampled UTF-16 code units decoded lossily as one sequence, exactly as the client's
/// string concatenation and UTF-8 encoding produce them: two sampled surrogates that happen to pair become one
/// four-byte character, and a lone surrogate becomes U+FFFD.
pub fn cc_version(cli_version: &str, prompt_units: [u16; 3]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update("59cf53e54c78");
    hasher.update(String::from_utf16_lossy(&prompt_units));
    hasher.update(cli_version);
    let digest = hasher.finalize();
    // The digest's `LowerHex` reads the precision as the digit count, so this is the first three hex digits.
    format!("{cli_version}.{digest:.3x}")
}

/// How a stamped line's `cc_version` relates to the version the user agent names: the client's own version
/// bare, the client's version with a three-hex suffix, or another version altogether.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CarriedVersion<'a> {
    Bare,
    Suffixed(&'a str),
    Other,
}

/// The `cc_version` value of a stamped line, read against `client_version`; `None` for a line with no
/// `cc_version` field. No free-form splitting: a value is bare when it equals the client's version, suffixed
/// when it is the client's version, a dot, and three hex characters, and `Other` otherwise.
pub(crate) fn carried_version<'a>(
    text: &'a str,
    client_version: &str,
) -> Option<CarriedVersion<'a>> {
    let value = text
        .strip_prefix("x-anthropic-billing-header: ")?
        .split("; ")
        .find_map(|field| field.strip_prefix("cc_version="))?
        .trim_end_matches(';');
    if value == client_version {
        return Some(CarriedVersion::Bare);
    }
    let suffix = value
        .strip_prefix(client_version)
        .and_then(|rest| rest.strip_prefix('.'))
        .filter(|tail| tail.len() == 3 && tail.bytes().all(|b| b.is_ascii_hexdigit()));
    Some(suffix.map_or(CarriedVersion::Other, CarriedVersion::Suffixed))
}

/// The version half of a `claude-cli/` user agent, accepted only as the client's own spelling of one: digits
/// and dots, then at most one `-` and a prerelease suffix of ASCII letters, digits, dots, and hyphens. A header
/// value admits `;` and `=`, so an unconstrained version could smuggle a `;`-separated field of its own
/// choosing into the billing line this build then signs. Anything else is refused under the contract's version
/// item rather than restored from a guessed version.
pub fn restorable_version(cli_version: &str) -> Option<&str> {
    let (release, prerelease) = match cli_version.split_once('-') {
        Some((release, prerelease)) => (release, Some(prerelease)),
        None => (cli_version, None),
    };
    let spelled = !release.is_empty()
        && release
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    let suffixed = prerelease.is_none_or(|suffix| {
        !suffix.is_empty()
            && suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
    });
    (spelled && suffixed).then_some(cli_version)
}

/// The token in the entrypoint's position of a `claude-cli/` user agent, the second comma-separated token of the
/// parenthesis, after `external`: `sdk-cli` out of `claude-cli/2.1.310 (external, sdk-cli)`, `claude-desktop-3p` out
/// of `(external, claude-desktop-3p, agent-sdk/0.3.295)`, `cli` out of `(external, cli, workload/cron)`. `cli` when
/// the agent carries no parenthesis, no second token, or one outside the entrypoint token alphabet:
/// `claude-cli/<version>` alone is the client's own spelling of the interactive entrypoint. The alphabet admits a
/// digit for `claude-desktop-3p` and an underscore for `claude_in_slack`, `local_agent` and the `remote_*` family,
/// which would otherwise read as `cli` whatever their position. Unsanitized, a forged User-Agent could smuggle a
/// `;`-separated field of its own choosing into the billing line this build signs.
pub fn entrypoint_of(user_agent: &str) -> &str {
    let Some(open) = user_agent.find('(') else {
        return "cli";
    };
    let Some(close) = user_agent[open..].find(')') else {
        return "cli";
    };
    let Some(candidate) = user_agent[open + 1..open + close].split(',').nth(1) else {
        return "cli";
    };
    let candidate = candidate.trim();
    if is_entrypoint_token(candidate) {
        candidate
    } else {
        "cli"
    }
}

/// The entrypoint token alphabet: ASCII lowercase letters, ASCII digits, hyphens and underscores, 1 to `MAX_ID_LEN`
/// bytes, so `claude-desktop-3p` passes on its digit and `claude_in_slack`, `local_agent` and the `remote_*` family on
/// their underscore, while a separator (`;`, `=`, whitespace, `/`, `.`) fails. Shared with the cross-check's read of
/// `cc_entrypoint` and with the cost model's loader, which checks a regime's name and its entrypoints against it.
pub fn is_entrypoint_token(candidate: &str) -> bool {
    // Bounded the same as `header_id`'s identifiers: a token running past this length reaches a log line and a
    // ledger key, so it reads as unknown rather than widening every row that names its class.
    (1..=crate::identity::MAX_ID_LEN).contains(&candidate.len())
        && candidate
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Absent-field candidates in `FIELD_ORDER`'s relative order: `cch` before `cc_prev_req` before `cc_prompt_id`.
/// An item the account has refused (`restore.skip`) never becomes a candidate, whether or not its field is
/// otherwise absent and restorable.
fn candidates(restore: &Restore<'_>) -> Vec<(&'static str, String, Injected)> {
    let mut out = Vec::new();
    if !restore.skip.contains(&Injected::Cch) {
        out.push(("cch", "cch=00000".to_string(), Injected::Cch));
    }
    if let Some(id) = restore.prev_req {
        if !restore.skip.contains(&Injected::PrevReq) {
            out.push((
                "cc_prev_req",
                format!("cc_prev_req={id}"),
                Injected::PrevReq,
            ));
        }
    }
    if let Some(id) = restore.prompt_id {
        if !restore.skip.contains(&Injected::PromptId) {
            out.push((
                "cc_prompt_id",
                format!("cc_prompt_id={id}"),
                Injected::PromptId,
            ));
        }
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

    /// The formula as the 2.1.278 binary computes it (read 2026-09-21): UTF-16 code units 4, 7, and 20 of the
    /// first user message's text, `0` for a missing one, the salt, the version, sha256, three hex characters.
    /// The prompt below is longer than 20 units and pure ASCII, so scalar-value and code-unit indexing agree.
    #[test]
    fn the_suffix_of_a_long_prompt_reproduces_the_clients_formula() {
        let prompt = "Explain how the relay restores the billing line for a reduced-shape request.";
        let units: Vec<u16> = prompt.encode_utf16().collect();
        assert_eq!(
            (units[4], units[7], units[20]),
            (b'a' as u16, b' ' as u16, b'y' as u16)
        );
        assert_eq!(
            cc_version("2.1.278", [units[4], units[7], units[20]]),
            "2.1.278.233"
        );
        assert_eq!(
            cc_version("2.1.269", [units[4], units[7], units[20]]),
            "2.1.269.3ef"
        );
    }

    /// The seat's example: a supplementary-plane character before position 4 shifts the client's UTF-16 sample
    /// away from what a Unicode-scalar-value sample would read (`dgt`, suffix `9ad`), so a suffix computed the
    /// old way would refuse a legitimately signed request. Verified against the client's own formula: sha256
    /// of `"59cf53e54c78" + "cfs" + "2.1.278"`, first three hex characters, is `143`.
    #[test]
    fn a_supplementary_character_before_position_4_reproduces_the_clients_suffix() {
        let prompt = "a\u{1D11E}bcdefghijklmnopqrstuvwxyz";
        let units: Vec<u16> = prompt.encode_utf16().collect();
        assert_eq!(
            (units[4], units[7], units[20]),
            (b'c' as u16, b'f' as u16, b's' as u16)
        );
        assert_eq!(
            cc_version("2.1.278", [units[4], units[7], units[20]]),
            "2.1.278.143"
        );
    }

    /// A supplementary character straddling position 4 leaves only its low surrogate there, unpaired in the
    /// three-unit sample; `from_utf16_lossy` reads it as U+FFFD, the same replacement the client's own decode
    /// produces for a lone surrogate. Verified against sha256 of `"59cf53e54c78" + "\u{FFFD}gt" + "2.1.278"`.
    #[test]
    fn a_supplementary_character_straddling_position_4_decodes_its_lone_surrogate_as_replacement() {
        let prompt = "abc\u{10000}efghijklmnopqrst";
        let units: Vec<u16> = prompt.encode_utf16().collect();
        assert_eq!(
            (units[4], units[7], units[20]),
            (0xDC00, b'g' as u16, b't' as u16)
        );
        assert_eq!(
            cc_version("2.1.278", [units[4], units[7], units[20]]),
            "2.1.278.373"
        );
    }

    /// Positions 4 and 7 land on two different characters' surrogate halves that happen to pair once sampled
    /// together; `from_utf16_lossy` combines them into one four-byte character, matching what the client's own
    /// string concatenation and UTF-8 encoding produce. Verified against sha256 of
    /// `"59cf53e54c78" + "\u{10037}t" + "2.1.278"`.
    #[test]
    fn two_unrelated_sampled_surrogates_that_happen_to_pair_decode_as_one_character() {
        let prompt = "abcd\u{10000}\u{10437}hijklmnopqrst";
        let units: Vec<u16> = prompt.encode_utf16().collect();
        assert_eq!(
            (units[4], units[7], units[20]),
            (0xD800, 0xDC37, b't' as u16)
        );
        assert_eq!(
            cc_version("2.1.278", [units[4], units[7], units[20]]),
            "2.1.278.355"
        );
    }

    #[test]
    fn a_carried_version_is_read_against_the_clients_own_version() {
        let line = |v: &str| {
            format!("x-anthropic-billing-header: cc_version={v}; cc_entrypoint=cli; cch=9b244;")
        };
        assert_eq!(
            carried_version(&line("2.1.269"), "2.1.269"),
            Some(CarriedVersion::Bare),
            "a three-digit patch is not a suffix"
        );
        assert_eq!(
            carried_version(&line("2.1.269.517"), "2.1.269"),
            Some(CarriedVersion::Suffixed("517"))
        );
        assert_eq!(
            carried_version(&line("2.1.269-rc.1.abc"), "2.1.269-rc.1"),
            Some(CarriedVersion::Suffixed("abc")),
            "a prerelease client's suffix follows its whole version"
        );
        assert_eq!(
            carried_version(&line("2.1.300.517"), "2.1.269"),
            Some(CarriedVersion::Other),
            "another version is the hash check's to judge"
        );
        assert_eq!(
            carried_version(&line("2.1.269.51"), "2.1.269"),
            Some(CarriedVersion::Other),
            "two characters is not a suffix"
        );
        assert_eq!(
            carried_version(&line("2.1.269.xyz"), "2.1.269"),
            Some(CarriedVersion::Other),
            "and neither is a non-hex tail"
        );
        assert_eq!(
            carried_version("x-anthropic-billing-header: cc_entrypoint=cli;", "2.1.269"),
            None
        );
    }

    #[test]
    fn each_absent_field_is_inserted_after_the_last_present_field_that_precedes_it() {
        // Hand-built: no fixture carries cc_workload, cc_is_subagent or the five fields after cc_turn_origin, but
        // FIELD_ORDER still has to seat cch ahead of the first two, cc_prev_req behind them and both ids ahead of
        // the five once they are on the line.
        let mut body = billing_body(
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; \
             cc_workload=interactive; cc_is_subagent=false; cc_prompt_index=0; cc_turn_index=3; \
             cc_gateway=1; cc_project=p; cc_project_src=git;",
        );
        let restore = Restore {
            prompt_id: Some("11111111-1111-4111-8111-111111111111"),
            prev_req: Some("req_1"),
            skip: Vec::new(),
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
             cc_prompt_id=11111111-1111-4111-8111-111111111111; cc_prompt_index=0; cc_turn_index=3; \
             cc_gateway=1; cc_project=p; cc_project_src=git;"
        );
    }

    #[test]
    fn a_present_cc_prev_req_is_kept() {
        let mut body = billing_body(
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli; cc_prev_req=req_kept;",
        );
        let restore = Restore {
            prompt_id: None,
            prev_req: Some("req_other"),
            skip: vec![Injected::Cch],
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
        let injected = restore_billing_line(&mut body, &full_restore()).unwrap();
        assert!(injected.is_empty());
        assert_eq!(body, before);
    }

    fn full_restore() -> Restore<'static> {
        Restore {
            prompt_id: Some("11111111-1111-4111-8111-111111111111"),
            prev_req: None,
            skip: Vec::new(),
        }
    }

    #[test]
    fn a_skipped_item_is_left_out_even_when_its_field_is_absent_and_otherwise_restorable() {
        let mut body = billing_body(
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli;",
        );
        let restore = Restore {
            skip: vec![Injected::Cch, Injected::PromptId],
            ..full_restore()
        };
        let injected = restore_billing_line(&mut body, &restore).unwrap();
        assert_eq!(
            injected,
            Vec::<Injected>::new(),
            "cch and cc_prompt_id are the only candidates full_restore offers, and both are skipped"
        );
        assert_eq!(
            text_of(&body),
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli;"
        );
    }

    #[test]
    fn a_system_array_whose_first_block_is_not_the_line_is_left_alone() {
        let empty = br#"{"model":"m","system":[],"messages":[]}"#.to_vec();
        let plain_first_block =
            br#"{"model":"m","system":[{"type":"text","text":"be helpful"}],"messages":[]}"#
                .to_vec();
        // The scanner reads the first `system`, the string, and cannot descend into it as an array, so every
        // lookup that would locate a field returns nothing rather than a guess.
        let duplicate_keys =
            br#"{"model":"m","system":"be helpful","messages":[],"system":[]}"#.to_vec();
        for body in [empty, plain_first_block, duplicate_keys] {
            let mut body = body;
            let before = body.clone();
            let injected = restore_billing_line(&mut body, &full_restore()).unwrap();
            assert!(injected.is_empty());
            assert_eq!(body, before);
        }
    }

    /// `insert_if_absent` already seats a field by `FIELD_ORDER` position alone, so a trailing field the
    /// restoration never fills stays trailing once another field lands ahead of it.
    #[test]
    fn a_field_filled_on_a_first_party_line_lands_ahead_of_cc_turn_origin() {
        let mut body = billing_body(
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=cli; cch=9b244; \
             cc_prompt_id=11111111-1111-4111-8111-111111111111; cc_turn_origin=human;",
        );
        let restore = Restore {
            prompt_id: None,
            prev_req: Some("req_1"),
            skip: vec![Injected::Cch],
        };
        let injected = restore_billing_line(&mut body, &restore).unwrap();
        assert_eq!(injected, vec![Injected::PrevReq]);
        assert_eq!(
            text_of(&body),
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=cli; cch=9b244; \
             cc_prev_req=req_1; cc_prompt_id=11111111-1111-4111-8111-111111111111; cc_turn_origin=human;"
        );
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
    fn a_version_carrying_a_separator_of_its_own_restores_nothing() {
        assert_eq!(restorable_version("2.1.269"), Some("2.1.269"));
        assert_eq!(restorable_version("2"), Some("2"));
        assert_eq!(restorable_version("2.1.269;cc_prev_req=forged"), None);
        assert_eq!(restorable_version("2.1.269=x"), None);
        assert_eq!(restorable_version("2..1"), None);
        assert_eq!(restorable_version(""), None);
    }

    #[test]
    fn a_prerelease_suffix_of_letters_digits_dots_and_hyphens_is_the_clients_own_spelling() {
        assert_eq!(restorable_version("2.1.269-beta"), Some("2.1.269-beta"));
        assert_eq!(restorable_version("2.1.269-beta.1"), Some("2.1.269-beta.1"));
        assert_eq!(restorable_version("2.1.269-rc-2"), Some("2.1.269-rc-2"));
        assert_eq!(restorable_version("2.1.269-"), None);
        assert_eq!(restorable_version("-beta"), None);
        assert_eq!(restorable_version("2.1.269-beta;cc_prev_req=forged"), None);
        assert_eq!(restorable_version("2.1.269-beta=x"), None);
        assert_eq!(restorable_version("2.1.269-be ta"), None);
        assert_eq!(restorable_version("2.1.269-b\u{e9}ta"), None);
        assert_eq!(restorable_version("2.1.269-beta-1;x"), None);
    }

    #[test]
    fn entrypoint_of_reads_the_clients_own_spellings_and_falls_back_on_anything_else() {
        assert_eq!(
            entrypoint_of("claude-cli/2.1.295 (external, claude-desktop-3p, agent-sdk/0.3.295)"),
            "claude-desktop-3p",
            "the token in the entrypoint's position, whatever follows it"
        );
        assert_eq!(
            entrypoint_of("claude-cli/2.1.300 (external, cli, workload/cron)"),
            "cli",
            "read by its position, the third token never consulted"
        );
        assert_eq!(
            entrypoint_of("claude-cli/2.1.295 (external, claude-desktop-3p)"),
            "claude-desktop-3p",
            "a digit is in the alphabet"
        );
        assert_eq!(
            entrypoint_of("claude-cli/2.1.300 (external, local_agent)"),
            "local_agent",
            "an underscore is in the alphabet"
        );
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
            entrypoint_of("claude-cli/2.1.269 (external)"),
            "cli",
            "no token in the entrypoint's position"
        );
        assert_eq!(
            entrypoint_of("claude-cli/2.1.269 (external, cli=1)"),
            "cli",
            "an equals sign is a separator"
        );
        assert_eq!(
            entrypoint_of("claude-cli/2.1.269 (external, cli.1)"),
            "cli",
            "a dot is a separator"
        );
        assert_eq!(
            entrypoint_of("claude-cli/2.1.269 (external, sdk cli)"),
            "cli",
            "whitespace inside the token is not a spelling the client uses"
        );
        assert_eq!(
            entrypoint_of("claude-cli/2.1.269 (external,  sdk-cli )"),
            "sdk-cli",
            "whitespace around the token is trimmed"
        );
        let over_bound = format!(
            "claude-cli/2.1.269 (external, {})",
            "a".repeat(crate::identity::MAX_ID_LEN + 1)
        );
        assert_eq!(
            entrypoint_of(&over_bound),
            "cli",
            "a token past the bound reads as unknown"
        );
    }
}
