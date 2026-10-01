//! The in-place body edits: the bound account's identity, the model the upstream is asked for, the billing hash
//! that has to follow both, and the first-party fields a reduced-shape request lacks.

use crate::billing::{restore_billing_line, CarriedVersion, Restore};
use crate::identity::is_valid_id;
use crate::memory::Injected;
use crate::{CacheTtl, RelayError};

#[derive(Debug)]
pub struct BodyEdits {
    pub account_uuid_replaced: bool,
    pub attribution_stripped: bool,
    pub model_replaced: bool,
    pub cch_restamped: bool,
    pub restored: Vec<Injected>,
    pub cache_ttl_edited: bool,
}

/// What a body carrying no `cache_control` marker of its own does under a cache TTL policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Markerless {
    /// The API's top-level member is added, so the policy's TTL reaches a message the client left unmarked.
    TakesTheMember,
    /// The body travels as sent: the client asked for no cache, and the operator's toggle leaves it that way.
    GoesAsSent,
}

/// The native edit's inputs: the account identity and model every request needs, the restoration a
/// `claude-cli/` inference request carries, the diagnostics member `compatibility.cache_diagnostics` forces, and
/// the pool's cache TTL policy.
pub struct NativeEdit<'a> {
    pub account_uuid: Option<&'a str>,
    pub upstream_model: &'a str,
    pub restore: Option<Restore<'a>>,
    /// Outer `None` leaves the body alone; the inner option is the previous `message_start` id on this account,
    /// `None` writing a JSON `null` for a first turn, a migration, or a failover.
    pub diagnostics: Option<Option<&'a str>>,
    /// `None` leaves every marker as sent; `Some` rewrites every marker the body carries to this TTL.
    pub cache_ttl: Option<CacheTtl>,
    /// What the TTL rewrite does to a body carrying no marker at all.
    pub markerless: Markerless,
    /// The `claude-cli/` version of the user agent and the prompt units `BodyFacts` read, for the
    /// version-suffix check; `None` for a caller the check does not apply to.
    pub version_check: Option<VersionCheck<'a>>,
    pub safeguards: SafeguardsEdit,
}

/// What the edit does to the client's `safeguards` member under the pool's or key's auto-mode setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SafeguardsEdit {
    /// The setting is on: the member travels as sent.
    Keep,
    /// The setting is off: the member is cut with one comma, and a member the raw walk cannot reach refuses the
    /// request rather than travelling.
    Drop,
}

#[derive(Clone, Copy, Debug)]
pub struct VersionCheck<'a> {
    pub client_version: &'a str,
    pub prompt_units: [u16; 3],
}

/// The beta the relay claims for itself under `compatibility.cache_diagnostics`; a client-sent one is never
/// reported as injected, since its 400 is the client's to handle.
pub const CACHE_DIAGNOSIS_BETA: &str = Injected::CacheDiagnosisBeta.wire_name();
/// The beta a one-hour cache TTL pairs with; dropped from the caller's list for a five-minute body.
pub const EXTENDED_TTL_BETA: &str = Injected::ExtendedTtlBeta.wire_name();
/// The beta that asks the upstream to review each tool use; dropped with the `safeguards` member under an off
/// auto-mode setting.
pub const DANGEROUS_TOOL_USE_BETA: &str = "dangerous-tool-use-2026-09-03";

/// A member an edit needs to find. A body toward the measured host that lacks one is refused rather than sent
/// on: the account it would be billed against, or the model it would be served by, is not the one the router
/// resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyMember {
    /// A billing stamp in `system[0].text` the decoder reads and the raw walk cannot reach, so the restamp
    /// after an edit would leave the client's hash on bytes it was not computed over.
    BillingStamp,
    /// A `cache_control` marker the decoder reads and the raw walk cannot reach, or one it reaches and cannot
    /// read.
    CacheControl,
    MetadataUserId,
    Model,
    /// A top-level member the caller's body carries more than once, raw or escaped; the name is one of the
    /// members admission and the body edit act on.
    Repeated(&'static str),
    /// A `safeguards` member the decoded top-level key count finds and the raw walk cannot reach.
    Safeguards,
}

impl BodyMember {
    pub fn path(self) -> &'static str {
        match self {
            BodyMember::BillingStamp => "system[0].text",
            BodyMember::CacheControl => "cache_control",
            BodyMember::MetadataUserId => "metadata.user_id",
            BodyMember::Model => "model",
            BodyMember::Repeated(name) => name,
            BodyMember::Safeguards => "safeguards",
        }
    }

    /// `RelayError::BodyShape`'s `Display` text: a billing stamp the restamp cannot reach, a `cache_control`
    /// marker the rewrite cannot place, a repeated member, or a `safeguards` member the cut cannot place, was found
    /// rather than absent, so none reads like the other two variants' "has no ... member to edit".
    pub(crate) fn shape_message(self) -> String {
        match self {
            BodyMember::BillingStamp => {
                "request body carries a billing stamp the router cannot restamp".into()
            }
            BodyMember::CacheControl => {
                "request body carries a cache_control marker the router cannot place".into()
            }
            BodyMember::Repeated(name) => {
                format!("request body carries the top-level member \"{name}\" more than once")
            }
            BodyMember::Safeguards => {
                "request body carries a safeguards member the router cannot place".into()
            }
            other => format!("request body has no {} member to edit", other.path()),
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
pub(crate) const BILLING_BLOCK: &[u8] = br#""x-anthropic-billing-header:"#;

/// Length of the hash after `cch=`, the only bytes `phoebis_cch::sign` rewrites.
const CCH_DIGITS: usize = 5;

pub fn edit_body(body: &mut Vec<u8>, edit: &NativeEdit<'_>) -> Result<BodyEdits, RelayError> {
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
    if let Some(check) = edit.version_check {
        if carries_a_stamp(body) {
            if let Some(kind) = version_drift(body, check) {
                return Err(RelayError::ClientDrift { kind });
            }
        }
    }
    // A stamp the raw walk cannot reach (a key on its path or the line spelled with an escape, or a second
    // spelling after the one the walk reads) is one the restamp below never rewrites, so an edited body would
    // carry the client's hash over bytes it was not computed for. Kept to compare after the edits: a body no edit
    // touched still carries the client's own stamp and travels.
    let unreachable_stamp = (phoebis_cch::decoded_stamp_count(body)
        > usize::from(carries_a_stamp(body)))
    .then(|| body.clone());
    if edit.safeguards == SafeguardsEdit::Drop {
        // A second spelling would reach the upstream after the first is cut and a last-wins parser would act
        // on it; an escaped spelling is one the raw walk never finds. Both refuse rather than send a body
        // the operator asked to have stripped. Presence is counted over decoded keys, not read from a parse of
        // the whole body: a body a strict decoder refuses (a lone surrogate escape in a string value) is one
        // the cut below still walks.
        let spellings = phoebis_cch::top_level_member_count(body, "safeguards")?;
        if spellings > 1 {
            return Err(RelayError::BodyShape(BodyMember::Repeated("safeguards")));
        }
        let cut = phoebis_cch::remove_top_level_member(body, "safeguards").is_some();
        if spellings == 1 && !cut {
            return Err(RelayError::BodyShape(BodyMember::Safeguards));
        }
    }
    // An API-key account has no identity to splice: the measured client never carried one for it.
    let account_uuid_replaced = match edit.account_uuid {
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
    let model_replaced = match replace_model(body, edit.upstream_model) {
        Spliced::Absent => return Err(RelayError::BodyShape(BodyMember::Model)),
        Spliced::Replaced => true,
        Spliced::Unchanged => false,
    };
    let mut restored = match &edit.restore {
        Some(restore) => restore_billing_line(body, restore)?,
        None => Vec::new(),
    };
    // A body with no restoration context still gets the member under the option: it names the operator's
    // choice and the host, not the client.
    let skip_diagnostics = edit
        .restore
        .as_ref()
        .is_some_and(|r| r.skip.contains(&Injected::Diagnostics));
    if let Some(previous) = edit.diagnostics {
        if !skip_diagnostics && insert_diagnostics(body, previous) {
            restored.push(Injected::Diagnostics);
        }
    }
    let cache_ttl_edited = match edit.cache_ttl {
        Some(ttl) => set_cache_ttl(body, ttl, edit.markerless)?,
        None => false,
    };
    if unreachable_stamp.is_some_and(|sent| *body != sent) {
        return Err(RelayError::BodyShape(BodyMember::BillingStamp));
    }
    let cch_restamped = phoebis_cch::restamp_if_present(body)?;
    Ok(BodyEdits {
        account_uuid_replaced,
        attribution_stripped: false,
        model_replaced,
        cch_restamped,
        restored,
        cache_ttl_edited,
    })
}

/// The rewrite with its fail-closed guard. The walk reads the client's own spelling of `cache_control` and a
/// decoder reads it through its escapes, so a body spelling the key any other way carries a marker the rewrite
/// never reaches: its hour would outlive a five-minute policy, unenforced and unreported. Where the two
/// readings disagree the request is refused. Answers whether any byte moved.
///
/// The decoder's count comes from a key-decoding walk over the same scanner, never a parse of the whole body: a
/// strict parser refuses a body for a lone surrogate escape (a client emits one for a string cut mid-pair) that
/// says nothing about its markers.
pub fn set_cache_ttl(
    body: &mut Vec<u8>,
    ttl: CacheTtl,
    markerless: Markerless,
) -> Result<bool, RelayError> {
    let changed = rewrite_cache_ttl(body, ttl, markerless)?;
    let enforced = match (
        phoebis_cch::decoded_cache_control_count(body),
        phoebis_cch::cache_control_spans(body),
    ) {
        (Ok(decoded), Ok(walked)) => decoded == walked.len(),
        _ => false,
    };
    if !enforced {
        return Err(RelayError::BodyShape(BodyMember::CacheControl));
    }
    Ok(changed)
}

/// Every `cache_control` member at the five positions the Messages API takes one from: the top level,
/// `system[]`, `tools[]`, `messages[].content[]`, and the blocks inside a content block's own `content`. One
/// object of that name anywhere else (a tool's input schema, a tool call's input) is the client's data.
pub(crate) fn schema_cache_controls(
    root: &serde_json::Map<String, serde_json::Value>,
) -> Vec<&serde_json::Value> {
    fn blocks(value: Option<&serde_json::Value>) -> impl Iterator<Item = &serde_json::Value> {
        value
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|block| block.get("cache_control"))
    }
    let mut found: Vec<&serde_json::Value> = root.get("cache_control").into_iter().collect();
    found.extend(blocks(root.get("system")));
    found.extend(blocks(root.get("tools")));
    let messages = root
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten();
    for message in messages {
        let content = message.get("content");
        found.extend(blocks(content));
        // The walk reads a `tool_result`'s own blocks one level down, so this reading does too, or the facts
        // would miss a marker the rewrite edits.
        let nested = content
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten();
        for block in nested {
            found.extend(blocks(block.get("content")));
        }
    }
    found
}

/// Rewrites every ephemeral marker to `ttl`: `1h` writes `"ttl":"1h"` right after `"type":"ephemeral"`, `5m`
/// removes the `ttl` member, and every other member of the marker keeps its place. A marker whose `type` is not
/// `ephemeral` is left as sent. A body with no marker anywhere gets the top-level `cache_control` member with
/// the TTL, before `stream`, where `markerless` allows it. Returns whether any byte changed.
///
/// Every marker is read before any is written: one the parser cannot read (a number out of range, a value that
/// is no object) would keep its own TTL beside rewritten ones, and the dispatch would report the policy's TTL for
/// the whole body, so the request is refused as a marker the rewrite cannot place.
fn rewrite_cache_ttl(
    body: &mut Vec<u8>,
    ttl: CacheTtl,
    markerless: Markerless,
) -> Result<bool, RelayError> {
    let spans = phoebis_cch::cache_control_spans(body)?;
    if spans.is_empty() {
        if markerless == Markerless::GoesAsSent {
            return Ok(false);
        }
        add_top_level_cache_control(body, ttl);
        return Ok(true);
    }
    let markers = spans
        .into_iter()
        .map(|(start, end)| {
            serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&body[start..end])
                .map(|marker| (start, end, marker))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| RelayError::BodyShape(BodyMember::CacheControl))?;
    let mut changed = false;
    // Last to first so an earlier span's offsets still hold after a later one is spliced.
    for (start, end, marker) in markers.into_iter().rev() {
        if marker.get("type").and_then(serde_json::Value::as_str) != Some("ephemeral") {
            continue;
        }
        let mut rebuilt = serde_json::Map::new();
        rebuilt.insert(
            "type".to_string(),
            serde_json::Value::String("ephemeral".into()),
        );
        if ttl == CacheTtl::OneHour {
            rebuilt.insert("ttl".to_string(), serde_json::Value::String("1h".into()));
        }
        for (key, value) in &marker {
            if key == "type" || key == "ttl" {
                continue;
            }
            rebuilt.insert(key.clone(), value.clone());
        }
        let rewritten = serde_json::to_string(&rebuilt).expect("a map serializes");
        if rewritten.as_bytes() != &body[start..end] {
            body.splice(start..end, rewritten.into_bytes());
            changed = true;
        }
    }
    Ok(changed)
}

/// The top-level `cache_control` member for a body with no marker anywhere: before `stream` when present, else
/// last, the same placement rule `insert_diagnostics` uses.
fn add_top_level_cache_control(body: &mut Vec<u8>, ttl: CacheTtl) {
    let member = match ttl {
        CacheTtl::OneHour => r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#.to_string(),
        CacheTtl::FiveMinutes => r#""cache_control":{"type":"ephemeral"}"#.to_string(),
    };
    match phoebis_cch::object_member_span(body, &["stream"]) {
        Some((start, _)) => {
            body.splice(start..start, format!("{member},").bytes());
        }
        None => {
            let end = body
                .iter()
                .rposition(|b| *b == b'}')
                .expect("checked to be an object");
            let empty = body[..end]
                .iter()
                .rev()
                .find(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                == Some(&b'{');
            body.splice(
                end..end,
                format!("{}{member}", if empty { "" } else { "," }).bytes(),
            );
        }
    }
}

/// The lifetime of the deepest marker on each tier of an edited body, `(static, message)` in ms: a marker inside
/// `tools[]` or `system[]` is the static tier's, one inside `messages[]` the message tier's, a top-level member
/// both; `None` for a tier without a marker.
pub fn tier_ttls(body: &[u8]) -> (Option<i64>, Option<i64>) {
    let Ok(spans) = phoebis_cch::cache_control_spans(body) else {
        return (None, None);
    };
    let tools = phoebis_cch::object_member_span(body, &["tools"]);
    let system = phoebis_cch::object_member_span(body, &["system"]);
    let messages = phoebis_cch::object_member_span(body, &["messages"]);
    let within = |span: Option<(usize, usize)>, at: usize| {
        span.is_some_and(|(start, end)| (start..end).contains(&at))
    };
    // The deepest marker per tier, the tail the pin's warmth reads. The static tier ranks by the cached prefix's
    // own order (tools, then system, then a top-level marker) before the offset within a tier, so a body that
    // serializes `system` ahead of `tools` reads the same; the message tier is one array, whose body order is
    // its prefix order. A marker inside no container is a top-level member and counts for both tiers.
    let mut static_marker: Option<((u8, usize), (usize, usize))> = None;
    let mut message_marker = None;
    for (start, end) in spans {
        let is_message = within(messages, start);
        let static_rank = if within(tools, start) {
            Some(0)
        } else if within(system, start) {
            Some(1)
        } else if is_message {
            None
        } else {
            Some(2)
        };
        let is_top_level = static_rank == Some(2);
        if let Some(rank) = static_rank {
            if static_marker.is_none_or(|(deepest, _)| (rank, start) > deepest) {
                static_marker = Some(((rank, start), (start, end)));
            }
        }
        if (is_message || is_top_level) && message_marker.is_none_or(|(s, _)| start > s) {
            message_marker = Some((start, end));
        }
    }
    let static_marker = static_marker.map(|(_, span)| span);
    (
        static_marker.map(|span| span_ttl_ms(body, span)),
        message_marker.map(|span| span_ttl_ms(body, span)),
    )
}

/// The TTL every `cache_control` marker of an edited body carries, read off every marker `cache_control_spans`
/// lists; `None` for a body without a marker or with markers at both TTLs.
pub fn uniform_ttl(body: &[u8]) -> Option<i64> {
    // Every marker rather than the deepest per tier: two tiers whose deepest markers agree can still hold a
    // marker at the other TTL further up, and the cache writes that prefix at that TTL.
    let spans = phoebis_cch::cache_control_spans(body).ok()?;
    let mut ttls = spans.into_iter().map(|span| span_ttl_ms(body, span));
    let first = ttls.next()?;
    ttls.all(|ttl| ttl == first).then_some(first)
}

fn span_ttl_ms(body: &[u8], (start, end): (usize, usize)) -> i64 {
    serde_json::from_slice::<serde_json::Value>(&body[start..end])
        .ok()
        .map_or(300_000, |marker| crate::facts::marker_ttl_ms(&marker))
}

/// Before `stream` when the body has one, so the member lands where the measured client puts it; a body
/// without `stream` gets it last. Never rewrites a member the client sent.
fn insert_diagnostics(body: &mut Vec<u8>, previous: Option<&str>) -> bool {
    if phoebis_cch::object_member_span(body, &["diagnostics"]).is_some() {
        return false;
    }
    let member = format!(
        r#""diagnostics":{{"previous_message_id":{}}}"#,
        previous.map_or("null".to_string(), |p| serde_json::to_string(p)
            .expect("a string encodes"))
    );
    match phoebis_cch::object_member_span(body, &["stream"]) {
        Some((start, _)) => {
            body.splice(start..start, format!("{member},").bytes());
            true
        }
        None => {
            // `starts_with_object` only checked the first byte and `edit_body` skips a full parse, so a body
            // that opens with `{` and never closes reaches here; the upstream would refuse it either way, so
            // nothing is inserted rather than guessing at a position that does not exist.
            let Some(end) = body.iter().rposition(|b| *b == b'}') else {
                return false;
            };
            let empty = body[..end]
                .iter()
                .rev()
                .find(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                == Some(&b'{');
            body.splice(
                end..end,
                format!("{}{member}", if empty { "" } else { "," }).bytes(),
            );
            true
        }
    }
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

/// Whether the version suffix the client stamped reproduces from the body's own prompt and the version its
/// user agent names. A line with no `cc_version`, a bare version with no suffix, or a version other than the
/// agent's is not this drift: the first two carry nothing to check, and the third is the hash check's to judge.
fn version_drift(body: &[u8], check: VersionCheck<'_>) -> Option<&'static str> {
    let (start, end) = phoebis_cch::string_member_span(body, &["system", "0", "text"])?;
    // The span is the JSON token with its quotes and escapes; the billing line is its decoded text.
    let text = serde_json::from_slice::<String>(&body[start..end]).ok()?;
    let CarriedVersion::Suffixed(suffix) =
        crate::billing::carried_version(&text, check.client_version)?
    else {
        return None;
    };
    let expected = crate::billing::cc_version(check.client_version, check.prompt_units);
    (expected.rsplit('.').next() != Some(suffix)).then_some("version_suffix")
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
    use phoebis_profile::fixtures::{body as fixture_body, with_safeguards, BodyFixture};

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
        let edits = edit_body(
            &mut body,
            &NativeEdit {
                account_uuid: Some(UUID),
                upstream_model: "claude-sonnet-4-5",
                restore: None,
                diagnostics: None,
                cache_ttl: None,
                markerless: Markerless::TakesTheMember,
                version_check: None,
                safeguards: SafeguardsEdit::Keep,
            },
        )
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
            edit_body(
                &mut body,
                &NativeEdit {
                    account_uuid: Some(UUID),
                    upstream_model: "claude-sonnet-4-5",
                    restore: None,
                    diagnostics: None,
                    cache_ttl: None,
                    markerless: Markerless::TakesTheMember,
                    version_check: None,
                    safeguards: SafeguardsEdit::Keep,
                },
            ),
            Err(RelayError::BodyShape(BodyMember::MetadataUserId))
        ));
    }

    #[test]
    fn a_body_with_no_top_level_model_is_refused_for_either_account_kind() {
        let mut body = br#"{"messages":[]}"#.to_vec();
        assert!(matches!(
            edit_body(
                &mut body,
                &NativeEdit {
                    account_uuid: None,
                    upstream_model: "claude-sonnet-4-5",
                    restore: None,
                    diagnostics: None,
                    cache_ttl: None,
                    markerless: Markerless::TakesTheMember,
                    version_check: None,
                    safeguards: SafeguardsEdit::Keep,
                },
            ),
            Err(RelayError::BodyShape(BodyMember::Model))
        ));
        let mut oauth = String::from_utf8(native(UUID, "claude-sonnet-4-5"))
            .unwrap()
            .replacen(r#""model""#, r#""mdl""#, 1)
            .into_bytes();
        assert!(matches!(
            edit_body(
                &mut oauth,
                &NativeEdit {
                    account_uuid: Some(UUID),
                    upstream_model: "claude-sonnet-4-5",
                    restore: None,
                    diagnostics: None,
                    cache_ttl: None,
                    markerless: Markerless::TakesTheMember,
                    version_check: None,
                    safeguards: SafeguardsEdit::Keep,
                },
            ),
            Err(RelayError::BodyShape(BodyMember::Model))
        ));
    }

    #[test]
    fn an_api_key_body_needs_no_account_uuid() {
        let mut body = br#"{"model":"claude-sonnet-4-5","messages":[]}"#.to_vec();
        let edits = edit_body(
            &mut body,
            &NativeEdit {
                account_uuid: None,
                upstream_model: "claude-sonnet-4-5",
                restore: None,
                diagnostics: None,
                cache_ttl: None,
                markerless: Markerless::TakesTheMember,
                version_check: None,
                safeguards: SafeguardsEdit::Keep,
            },
        )
        .expect("an api-key account never carried an identity");
        assert!(!edits.account_uuid_replaced);
    }

    #[test]
    fn the_uuid_and_the_model_are_spliced_in_place() {
        let mut body = br#"{"model":"claude-sonnet-4-5[1m]","metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"}}"#.to_vec();
        let edits = edit_body(
            &mut body,
            &NativeEdit {
                account_uuid: Some("acct-uuid"),
                upstream_model: "claude-sonnet-4-5",
                restore: None,
                diagnostics: None,
                cache_ttl: None,
                markerless: Markerless::TakesTheMember,
                version_check: None,
                safeguards: SafeguardsEdit::Keep,
            },
        )
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
            edit_body(
                &mut body,
                &NativeEdit {
                    account_uuid: None,
                    upstream_model: "claude-sonnet-4-5",
                    restore: None,
                    diagnostics: None,
                    cache_ttl: None,
                    markerless: Markerless::TakesTheMember,
                    version_check: None,
                    safeguards: SafeguardsEdit::Keep,
                },
            ),
            Err(RelayError::NotAnObject)
        ));
    }

    #[test]
    fn insert_diagnostics_places_the_member_last_with_correct_comma_handling() {
        let mut non_empty = br#"{"model":"m"}"#.to_vec();
        assert!(insert_diagnostics(&mut non_empty, None));
        assert_eq!(
            non_empty,
            br#"{"model":"m","diagnostics":{"previous_message_id":null}}"#
        );

        let mut empty = br#"{}"#.to_vec();
        assert!(insert_diagnostics(&mut empty, Some("msg_1")));
        assert_eq!(empty, br#"{"diagnostics":{"previous_message_id":"msg_1"}}"#);
    }

    #[test]
    fn insert_diagnostics_does_nothing_rather_than_panicking_on_an_unterminated_body() {
        let mut body = br#"{"model":"m""#.to_vec();
        assert!(!insert_diagnostics(&mut body, None));
        assert_eq!(
            body, br#"{"model":"m""#,
            "left untouched rather than guessed at"
        );
    }

    #[test]
    fn set_cache_ttl_rewrites_a_bare_marker_both_ways() {
        let mut to_hour = br#"{"cache_control":{"type":"ephemeral"}}"#.to_vec();
        assert!(
            set_cache_ttl(&mut to_hour, CacheTtl::OneHour, Markerless::TakesTheMember).unwrap()
        );
        assert_eq!(
            to_hour,
            br#"{"cache_control":{"type":"ephemeral","ttl":"1h"}}"#
        );

        let mut stays_five = br#"{"cache_control":{"type":"ephemeral"}}"#.to_vec();
        assert!(
            !set_cache_ttl(
                &mut stays_five,
                CacheTtl::FiveMinutes,
                Markerless::TakesTheMember
            )
            .unwrap(),
            "already the requested shape"
        );
        assert_eq!(stays_five, br#"{"cache_control":{"type":"ephemeral"}}"#);
    }

    #[test]
    fn set_cache_ttl_rewrites_an_hour_marker_both_ways_and_keeps_a_third_members_place() {
        let mut to_five =
            br#"{"cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}}"#.to_vec();
        assert!(set_cache_ttl(
            &mut to_five,
            CacheTtl::FiveMinutes,
            Markerless::TakesTheMember
        )
        .unwrap());
        assert_eq!(
            to_five, br#"{"cache_control":{"type":"ephemeral","scope":"global"}}"#,
            "ttl is dropped, scope keeps its place after type"
        );

        let mut to_hour =
            br#"{"cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}}"#.to_vec();
        assert!(
            !set_cache_ttl(&mut to_hour, CacheTtl::OneHour, Markerless::TakesTheMember).unwrap(),
            "already an hour"
        );
    }

    #[test]
    fn set_cache_ttl_leaves_a_marker_of_another_type_as_sent() {
        let mut body = br#"{"cache_control":{"type":"persistent"}}"#.to_vec();
        assert!(!set_cache_ttl(&mut body, CacheTtl::OneHour, Markerless::TakesTheMember).unwrap());
        assert_eq!(body, br#"{"cache_control":{"type":"persistent"}}"#);
    }

    #[test]
    fn set_cache_ttl_adds_the_top_level_member_before_stream_when_no_marker_is_present() {
        let mut with_stream = br#"{"model":"m","stream":true}"#.to_vec();
        assert!(set_cache_ttl(
            &mut with_stream,
            CacheTtl::OneHour,
            Markerless::TakesTheMember
        )
        .unwrap());
        assert_eq!(
            with_stream,
            br#"{"model":"m","cache_control":{"type":"ephemeral","ttl":"1h"},"stream":true}"#
        );

        let mut without_stream = br#"{"model":"m"}"#.to_vec();
        assert!(set_cache_ttl(
            &mut without_stream,
            CacheTtl::FiveMinutes,
            Markerless::TakesTheMember
        )
        .unwrap());
        assert_eq!(
            without_stream,
            br#"{"model":"m","cache_control":{"type":"ephemeral"}}"#
        );
    }

    #[test]
    fn set_cache_ttl_leaves_a_body_with_no_marker_as_sent_where_the_member_is_not_accepted() {
        let mut body = br#"{"model":"m","messages":[],"tools":[]}"#.to_vec();
        assert!(!set_cache_ttl(&mut body, CacheTtl::OneHour, Markerless::GoesAsSent).unwrap());
        assert_eq!(body, br#"{"model":"m","messages":[],"tools":[]}"#);
    }

    #[test]
    fn set_cache_ttl_rewrites_every_wire_position_and_keeps_the_marker_count() {
        // No fixture body carries a marker inside `tools[]`; this synthetic body covers all three container
        // positions in one pass.
        let mut body = br#"{"system":[{"type":"text","cache_control":{"type":"ephemeral"}}],"tools":[{"name":"t","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{"type":"text","cache_control":{"type":"ephemeral"}}]}]}"#.to_vec();
        assert!(set_cache_ttl(&mut body, CacheTtl::OneHour, Markerless::TakesTheMember).unwrap());
        let spans = phoebis_cch::cache_control_spans(&body).unwrap();
        assert_eq!(spans.len(), 3, "the rewrite adds and removes no marker");
        for (start, end) in spans {
            assert_eq!(&body[start..end], br#"{"type":"ephemeral","ttl":"1h"}"#);
        }
    }

    #[test]
    fn set_cache_ttl_refuses_a_marker_the_walk_cannot_reach() {
        let escaped = br#"{"system":[{"type":"text","text":"s","cache_contr\u006fl":{"type":"ephemeral","ttl":"1h"}}],"messages":[]}"#;
        assert!(
            escaped.windows(6).any(|w| w == br"\u006f"),
            "the escape is in the raw bytes"
        );
        assert_eq!(
            phoebis_cch::decoded_cache_control_count(escaped).unwrap(),
            1,
            "the decoder reads the marker"
        );
        assert!(
            phoebis_cch::cache_control_spans(escaped).unwrap().is_empty(),
            "the walk does not reach it"
        );
        let mut body = escaped.to_vec();
        assert!(matches!(
            set_cache_ttl(&mut body, CacheTtl::FiveMinutes, Markerless::TakesTheMember),
            Err(RelayError::BodyShape(BodyMember::CacheControl))
        ));

        let repeated = br#"{"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral"},"cache_contr\u006fl":{"type":"ephemeral","ttl":"1h"}}],"messages":[]}"#;
        let mut body = repeated.to_vec();
        assert!(matches!(
            set_cache_ttl(&mut body, CacheTtl::FiveMinutes, Markerless::TakesTheMember),
            Err(RelayError::BodyShape(BodyMember::CacheControl))
        ));
    }

    #[test]
    fn set_cache_ttl_places_every_marker_of_a_body_a_strict_decoder_refuses() {
        let surrogate = br#"{"system":[{"type":"text","text":"\udc00","cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
        assert!(
            serde_json::from_slice::<serde_json::Value>(surrogate).is_err(),
            "a lone surrogate escape is one the decoder refuses"
        );
        let mut body = surrogate.to_vec();
        assert!(
            set_cache_ttl(&mut body, CacheTtl::FiveMinutes, Markerless::TakesTheMember).unwrap()
        );
        let spans = phoebis_cch::cache_control_spans(&body).unwrap();
        assert_eq!(spans.len(), 2, "no marker added or refused");
        for (start, end) in spans {
            assert_eq!(&body[start..end], br#"{"type":"ephemeral"}"#);
        }
    }

    #[test]
    fn set_cache_ttl_refuses_a_marker_its_parser_cannot_read_beside_a_readable_one() {
        let unreadable = br#"{"system":[{"type":"text","text":"s","cache_control":{"type":1e400,"type":"ephemeral","ttl":"1h"}}],"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
        let spans = phoebis_cch::cache_control_spans(unreadable).unwrap();
        assert_eq!(
            (
                phoebis_cch::decoded_cache_control_count(unreadable).unwrap(),
                spans.len()
            ),
            (2, 2),
            "the walks agree, so the count guard alone passes the body"
        );
        let (start, end) = spans[0];
        assert!(
            serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(
                &unreadable[start..end]
            )
            .is_err(),
            "a number out of range is one the marker parser refuses"
        );
        for ttl in [CacheTtl::FiveMinutes, CacheTtl::OneHour] {
            let mut body = unreadable.to_vec();
            assert!(
                matches!(
                    set_cache_ttl(&mut body, ttl, Markerless::TakesTheMember),
                    Err(RelayError::BodyShape(BodyMember::CacheControl))
                ),
                "{ttl:?}"
            );
        }

        let not_an_object = br#"{"system":[{"type":"text","text":"s","cache_control":"ephemeral"}],"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
        let mut body = not_an_object.to_vec();
        assert!(matches!(
            set_cache_ttl(&mut body, CacheTtl::FiveMinutes, Markerless::TakesTheMember),
            Err(RelayError::BodyShape(BodyMember::CacheControl))
        ));
    }

    #[test]
    fn set_cache_ttl_places_both_plain_spellings_of_a_repeated_marker() {
        let mut body = br#"{"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","ttl":"1h"},"cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[]}"#.to_vec();
        assert!(
            set_cache_ttl(&mut body, CacheTtl::FiveMinutes, Markerless::TakesTheMember).unwrap()
        );
        assert_eq!(
            body,
            br#"{"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral"},"cache_control":{"type":"ephemeral"}}],"messages":[]}"#,
            "whichever spelling a parser keeps carries the policy's TTL"
        );
    }

    #[test]
    fn tier_ttls_reads_the_deepest_marker_per_tier_and_a_top_level_marker_counts_for_both() {
        let mixed = br#"{"tools":[{"name":"t","cache_control":{"type":"ephemeral"}}],"system":[{"type":"text","text":"a","cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]},{"role":"assistant","content":[{"type":"text","text":"y","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
        assert_eq!(
            tier_ttls(mixed),
            (Some(3_600_000), Some(3_600_000)),
            "the deepest marker of each tier wins: the last system block and the last message"
        );
        let tail_only = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}]}"#;
        assert_eq!(
            tier_ttls(tail_only),
            (None, Some(300_000)),
            "a marker on the tail alone leaves the static tier unrecorded"
        );
        let top_level = br#"{"cache_control":{"type":"ephemeral"}}"#;
        assert_eq!(tier_ttls(top_level), (Some(300_000), Some(300_000)));
        assert_eq!(tier_ttls(br#"{"messages":[]}"#), (None, None));
        assert_eq!(tier_ttls(b"not json"), (None, None));
    }

    #[test]
    fn uniform_ttl_reads_every_marker_not_the_deepest_per_tier() {
        let mixed = br#"{"tools":[{"name":"t","cache_control":{"type":"ephemeral","ttl":"1h"}}],"system":[{"type":"text","text":"a","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}]}"#;
        assert_eq!(tier_ttls(mixed), (Some(300_000), Some(300_000)));
        assert_eq!(
            uniform_ttl(mixed),
            None,
            "the hour marker in the tools is above the deepest static marker and still makes the request mixed"
        );
        let hour = br#"{"tools":[{"name":"t","cache_control":{"type":"ephemeral","ttl":"1h"}}],"system":[{"type":"text","text":"a","cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
        assert_eq!(uniform_ttl(hour), Some(3_600_000));
        assert_eq!(uniform_ttl(br#"{"messages":[]}"#), None);
    }

    #[test]
    fn the_static_tiers_deepest_marker_follows_the_prefix_order_whatever_the_member_order() {
        // The cached prefix runs tools, then system, so the system block's five minutes is the deeper marker
        // even where the body serializes `system` first.
        let system_first = br#"{"system":[{"type":"text","text":"a","cache_control":{"type":"ephemeral"}}],"tools":[{"name":"t","cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[]}"#;
        let tools_first = br#"{"tools":[{"name":"t","cache_control":{"type":"ephemeral","ttl":"1h"}}],"system":[{"type":"text","text":"a","cache_control":{"type":"ephemeral"}}],"messages":[]}"#;
        assert_eq!(tier_ttls(system_first).0, Some(300_000));
        assert_eq!(
            tier_ttls(tools_first).0,
            Some(300_000),
            "the usual member order reads the same"
        );
    }

    fn drop_edit(safeguards: SafeguardsEdit) -> NativeEdit<'static> {
        NativeEdit {
            account_uuid: None,
            upstream_model: "claude-sonnet-4-5",
            restore: None,
            diagnostics: None,
            cache_ttl: None,
            markerless: Markerless::TakesTheMember,
            version_check: None,
            safeguards,
        }
    }

    /// The recorded first-party body with the member spliced in and re-signed, as a client in auto mode sends it.
    fn stamped_with_safeguards() -> Vec<u8> {
        phoebis_cch::sign(&with_safeguards(fixture_body(BodyFixture::FirstParty9b244))).unwrap()
    }

    #[test]
    fn edit_body_cuts_the_safeguards_member_under_drop_and_restamps() {
        let mut body = stamped_with_safeguards();
        let edits =
            edit_body(&mut body, &drop_edit(SafeguardsEdit::Drop)).expect("the member is cut");
        assert!(edits.cch_restamped);
        let root: serde_json::Value = serde_json::from_slice(&body).expect("the cut body parses");
        assert!(root.get("safeguards").is_none());
        assert_eq!(phoebis_cch::sign(&body).unwrap(), body, "the stamp is current");
        assert!(
            body == fixture_body(BodyFixture::FirstParty9b244),
            "one comma went with the member, so the body is the recording again"
        );
    }

    #[test]
    fn edit_body_keeps_the_member_under_keep() {
        let stamped = stamped_with_safeguards();
        let mut body = stamped.clone();
        edit_body(&mut body, &drop_edit(SafeguardsEdit::Keep)).expect("editable");
        assert!(body == stamped, "no byte moved");
    }

    #[test]
    fn edit_body_refuses_a_present_member_the_walk_cannot_reach() {
        let spliced = String::from_utf8(with_safeguards(fixture_body(BodyFixture::Oauth))).unwrap();
        let escaped = spliced
            .replacen(r#""safeguards""#, r#""safeg\u0075ards""#, 1)
            .into_bytes();
        assert!(
            escaped.windows(6).any(|w| w == br"\u0075"),
            "the escape is in the raw bytes"
        );
        let root: serde_json::Value = serde_json::from_slice(&escaped).unwrap();
        assert!(
            root.get("safeguards").is_some(),
            "the decoder reads the escaped key as the plain one"
        );
        let mut body = escaped.clone();
        assert!(matches!(
            edit_body(&mut body, &drop_edit(SafeguardsEdit::Drop)),
            Err(RelayError::BodyShape(BodyMember::Safeguards))
        ));

        let twice = spliced
            .replacen(r#""stream":"#, r#""safeguards":[],"stream":"#, 1)
            .into_bytes();
        let mut body = twice;
        assert!(matches!(
            edit_body(&mut body, &drop_edit(SafeguardsEdit::Drop)),
            Err(RelayError::BodyShape(BodyMember::Repeated("safeguards")))
        ));

        let without = fixture_body(BodyFixture::Oauth);
        let mut body = without.to_vec();
        edit_body(&mut body, &drop_edit(SafeguardsEdit::Drop)).expect("nothing to cut");
        assert!(body == without);
    }

    #[test]
    fn a_member_in_a_body_the_decoder_refuses_is_cut_or_refused_never_passed_through() {
        let surrogate = String::from_utf8(fixture_body(BodyFixture::Oauth).to_vec())
            .unwrap()
            .replacen(r#""text":""#, r#""text":"\udc00"#, 1);
        assert!(
            serde_json::from_slice::<serde_json::Value>(surrogate.as_bytes()).is_err(),
            "a lone surrogate escape is one the decoder refuses"
        );
        let spliced = with_safeguards(surrogate.as_bytes());

        let mut body = spliced.clone();
        edit_body(&mut body, &drop_edit(SafeguardsEdit::Drop)).expect("the member is cut");
        assert!(
            body == surrogate.as_bytes(),
            "the member left with one comma"
        );

        let escaped = String::from_utf8(spliced)
            .unwrap()
            .replacen(r#""safeguards""#, r#""safeg\u0075ards""#, 1)
            .into_bytes();
        let mut body = escaped;
        assert!(matches!(
            edit_body(&mut body, &drop_edit(SafeguardsEdit::Drop)),
            Err(RelayError::BodyShape(BodyMember::Safeguards))
        ));
    }

    /// The digits a client writes after `cch=` over `body` as it serialized it, hashed from the bytes alone so a
    /// stamp `find_cch` cannot reach is hashed too.
    fn client_stamp(body: &[u8]) -> String {
        let at = find(body, b"cch=").expect("a stamped body") + 4;
        let mut zeroed = body.to_vec();
        zeroed[at..at + CCH_DIGITS].copy_from_slice(b"00000");
        let view = phoebis_cch::normalize(&zeroed).expect("the body parses");
        format!(
            "{:05x}",
            xxhash_rust::xxh64::xxh64(&view, phoebis_cch::SEED) & 0xF_FFFF
        )
    }

    fn carried_stamp(body: &[u8]) -> String {
        let at = find(body, b"cch=").expect("a stamped body") + 4;
        String::from_utf8(body[at..at + CCH_DIGITS].to_vec()).unwrap()
    }

    /// The recorded first-party body with the member, its billing line under a `text` key spelled with an
    /// escape, stamped as a client serializing that spelling stamps it.
    fn escaped_stamp_with_safeguards() -> Vec<u8> {
        let mut body =
            String::from_utf8(with_safeguards(fixture_body(BodyFixture::FirstParty9b244)))
                .unwrap()
                .replacen(
                    r#""text":"x-anthropic-billing-header"#,
                    r#""te\u0078t":"x-anthropic-billing-header"#,
                    1,
                )
                .into_bytes();
        let at = find(&body, b"cch=").unwrap() + 4;
        let stamp = client_stamp(&body);
        body[at..at + CCH_DIGITS].copy_from_slice(stamp.as_bytes());
        body
    }

    #[test]
    fn a_stamp_the_restamp_cannot_reach_refuses_an_edit_and_travels_as_sent_without_one() {
        let sent = escaped_stamp_with_safeguards();
        assert!(
            phoebis_cch::find_cch(&sent).is_none() && !carries_a_stamp(&sent),
            "the raw walk misses the stamp"
        );
        let root: serde_json::Value = serde_json::from_slice(&sent).unwrap();
        assert!(
            root["system"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("cch=")),
            "a decoder reads the billing line under the escaped key"
        );
        // Hashed without the relay: the client's digits hold for the bytes it sent, and the bytes the cut leaves
        // need others, which no restamp writes since none reaches the stamp.
        let mut cut = sent.clone();
        phoebis_cch::remove_top_level_member(&mut cut, "safeguards").expect("the member is there");
        assert_eq!(
            (
                carried_stamp(&sent),
                client_stamp(&sent),
                client_stamp(&cut)
            ),
            ("4e6eb".into(), "4e6eb".into(), "6d872".into())
        );

        let mut body = sent.clone();
        assert!(matches!(
            edit_body(&mut body, &drop_edit(SafeguardsEdit::Drop)),
            Err(RelayError::BodyShape(BodyMember::BillingStamp))
        ));

        let mut body = sent.clone();
        let edits = edit_body(&mut body, &drop_edit(SafeguardsEdit::Keep))
            .expect("nothing to edit, so nothing to restamp");
        assert!(!edits.cch_restamped);
        assert!(
            body == sent,
            "no byte moved, so the client's own stamp holds"
        );

        let mut body = sent.clone();
        assert!(
            matches!(
                edit_body(
                    &mut body,
                    &NativeEdit {
                        upstream_model: "claude-opus-4-1",
                        ..drop_edit(SafeguardsEdit::Keep)
                    }
                ),
                Err(RelayError::BodyShape(BodyMember::BillingStamp))
            ),
            "any edit needs the restamp, the member kept or not"
        );
    }

    #[test]
    fn body_shapes_display_names_a_repeated_member_rather_than_an_absent_one() {
        assert_eq!(
            RelayError::BodyShape(BodyMember::Repeated("model")).to_string(),
            "request body carries the top-level member \"model\" more than once"
        );
        assert_eq!(
            RelayError::BodyShape(BodyMember::Model).to_string(),
            "request body has no model member to edit"
        );
    }
}
