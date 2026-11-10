//! The usage body's limit reset block in a typed form, and the state of each of its grants at an instant.

use serde_json::{Map, Value};

use crate::exchange::{instant_ms, OauthError};

/// The router's name for the Fable week in a grant's windows. It has to equal `phoebis_routing::SEVEN_DAY_FABLE`,
/// which this crate cannot name because the routing crate depends on it.
pub(crate) const SEVEN_DAY_FABLE: &str = "seven_day_fable";

/// A window name as the router spells it: the upstream's name for the Fable week maps, every other reads as sent.
pub(crate) fn window_name(sent: &str) -> String {
    match sent {
        "seven_day_overage_included" => SEVEN_DAY_FABLE,
        other => other,
    }
    .to_string()
}

/// The block an account's usage body carries under `cedar_ember`, every instant in epoch milliseconds.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct LimitResets {
    pub cooldown_until: Option<i64>,
    pub eligible: bool,
    pub grants: Vec<Grant>,
    pub ineligible_reason: Option<String>,
}

/// One grant of the block. `blocking` and `clears` carry the router's window names.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Grant {
    pub blocking: Vec<String>,
    pub clears: Vec<String>,
    pub ends_at: i64,
    pub gives_back_weekly: bool,
    pub id: String,
    /// The client's kind: one grant per kind is offered at a time, and grants of two kinds can both be used.
    pub kind: String,
    pub label: String,
    pub paused: bool,
    pub resets_left: u32,
    pub resets_total: u32,
    pub starts_at: i64,
    pub unavailable_reason: Option<String>,
    pub usable_now: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantStatus {
    Blocked,
    Cooldown,
    Ended,
    Paused,
    Usable,
    Used,
    Waiting,
}

impl GrantStatus {
    /// `blocked`, `cooldown`, `ended`, `paused`, `usable`, `used` or `waiting`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Cooldown => "cooldown",
            Self::Ended => "ended",
            Self::Paused => "paused",
            Self::Usable => "usable",
            Self::Used => "used",
            Self::Waiting => "waiting",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GrantState {
    /// The upstream's `unavailable_reason` as sent on a `Blocked` grant, `None` on every other state.
    pub reason: Option<String>,
    pub state: GrantStatus,
}

/// Reads the `cedar_ember` member of a usage body alone, `None` where the body carries it null or not at all. A
/// member the parse needs that is absent or of another type fails the whole block, so a block read in part never
/// hides a grant.
pub fn parse_limit_resets(body: &[u8]) -> Result<Option<LimitResets>, OauthError> {
    let root: Value =
        serde_json::from_slice(body).map_err(|error| OauthError::UsageJson(error.to_string()))?;
    let map = root
        .as_object()
        .ok_or_else(|| OauthError::Malformed("usage response is not an object".into()))?;
    let block = match map.get("cedar_ember") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(block)) => block,
        Some(_) => return Err(malformed("cedar_ember")),
    };
    let grants = required(block, "grants", Value::as_array)?
        .iter()
        .map(parse_grant)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(LimitResets {
        cooldown_until: optional(block, "cooldown_until", instant_ms)?,
        eligible: required(block, "eligible", Value::as_bool)?,
        grants,
        ineligible_reason: optional(block, "ineligible_reason", as_string)?,
    }))
}

/// The grants of October carried no `type`, `gives_back_weekly` or `unavailable_reason`, so a grant without them
/// still parses, each read as its default.
fn parse_grant(value: &Value) -> Result<Grant, OauthError> {
    let grant = value.as_object().ok_or_else(|| malformed("grants"))?;
    let clears = required(grant, "clears", window_names)?;
    let gives_back_weekly = optional(grant, "gives_back_weekly", Value::as_bool)?.unwrap_or(false);
    let kind = kind_of(
        gives_back_weekly,
        optional(grant, "type", Value::as_str)?,
        &clears,
    );
    Ok(Grant {
        blocking: optional(grant, "blocking", window_names)?.unwrap_or_default(),
        clears,
        ends_at: required(grant, "ends_at", instant_ms)?,
        gives_back_weekly,
        id: required(grant, "id", as_string)?,
        kind,
        label: required(grant, "label", as_string)?,
        paused: optional(grant, "paused", Value::as_bool)?.unwrap_or(false),
        resets_left: required(grant, "resets_left", count)?,
        resets_total: required(grant, "resets_total", count)?,
        starts_at: required(grant, "starts_at", instant_ms)?,
        unavailable_reason: optional(grant, "unavailable_reason", as_string)?,
        usable_now: required(grant, "usable_now", Value::as_bool)?,
    })
}

/// The client's kind of a grant: `fable` for a grant that gives the Fable week back, else the upstream's `type`,
/// else one derived from the windows it clears.
fn kind_of(gives_back_weekly: bool, sent: Option<&str>, clears: &[String]) -> String {
    if gives_back_weekly {
        return "fable".to_string();
    }
    if let Some(sent) = sent {
        return sent.to_string();
    }
    let five_hour = clears.iter().any(|name| name == "five_hour");
    match (five_hour, clears.len()) {
        (true, 1) => "session",
        (false, _) => "weekly",
        (true, _) => "full",
    }
    .to_string()
}

fn required<'a, T>(
    object: &'a Map<String, Value>,
    member: &str,
    read: impl FnOnce(&'a Value) -> Option<T>,
) -> Result<T, OauthError> {
    object
        .get(member)
        .and_then(read)
        .ok_or_else(|| malformed(member))
}

/// A null member reads as an absent one, and any other value has to read as the member's type.
fn optional<'a, T>(
    object: &'a Map<String, Value>,
    member: &str,
    read: impl FnOnce(&'a Value) -> Option<T>,
) -> Result<Option<T>, OauthError> {
    match object.get(member) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => read(value).map(Some).ok_or_else(|| malformed(member)),
    }
}

fn malformed(member: &str) -> OauthError {
    OauthError::Malformed(format!(
        "limit reset block: {member} is absent or of another type"
    ))
}

fn as_string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

fn count(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|count| u32::try_from(count).ok())
}

fn window_names(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|name| name.as_str().map(window_name))
        .collect()
}

/// Each grant's state at `now`, in the block's order, by the client's rule: per kind, the first open grant is the
/// one a click can claim, and `next_grant_id` plays no part.
pub fn grant_states(block: &LimitResets, now: i64) -> Vec<GrantState> {
    let cooldown_ahead = block.cooldown_until.is_some_and(|until| until > now);
    let cooldown_passed = block.cooldown_until.is_some_and(|until| until <= now);
    let mut open_kinds: Vec<&str> = Vec::new();
    let plain = |state| GrantState {
        reason: None,
        state,
    };
    block
        .grants
        .iter()
        .map(|grant| {
            if grant.resets_left == 0 {
                return plain(GrantStatus::Used);
            }
            if grant.ends_at <= now {
                return plain(GrantStatus::Ended);
            }
            if grant.paused {
                return plain(GrantStatus::Paused);
            }
            // The block's cooldown names no grant, and a claim's answer writes it before the next read.
            if cooldown_ahead {
                return plain(GrantStatus::Cooldown);
            }
            let reason = grant.unavailable_reason.as_deref();
            // The clock can tell that a cooldown ended and nothing about a limit or a block.
            let freed_by_clock = cooldown_passed
                && (reason == Some("cooldown") || (reason.is_none() && grant.blocking.is_empty()));
            if grant.usable_now || freed_by_clock {
                if open_kinds.contains(&grant.kind.as_str()) {
                    return plain(GrantStatus::Waiting);
                }
                open_kinds.push(&grant.kind);
                return plain(GrantStatus::Usable);
            }
            if reason == Some("cooldown") {
                return plain(GrantStatus::Cooldown);
            }
            GrantState {
                reason: grant.unavailable_reason.clone(),
                state: GrantStatus::Blocked,
            }
        })
        .collect()
}

/// The resets an account holds at `now`, the count every reader shows. An ended grant's resets are lost, and
/// every other grant counts whatever its state, a waiting, cooled, blocked or paused one included.
pub fn resets_left(block: &LimitResets, now: i64) -> u32 {
    block
        .grants
        .iter()
        .filter(|grant| grant.ends_at > now)
        .fold(0, |sum, grant| sum.saturating_add(grant.resets_left))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(body: &str) -> LimitResets {
        parse_limit_resets(body.as_bytes())
            .expect("the body parses")
            .expect("the body carries a block")
    }

    fn states(block: &LimitResets, now: i64) -> Vec<GrantStatus> {
        grant_states(block, now)
            .into_iter()
            .map(|grant| grant.state)
            .collect()
    }

    #[test]
    fn the_second_usable_grant_of_a_kind_waits() {
        let block = parsed(
            r#"{"cedar_ember":{"eligible":true,"grants":[
                {"id":"rg-a-1","label":"Launch reset","gives_back_weekly":true,
                 "clears":["seven_day_overage_included"],"resets_left":1,"resets_total":1,"usable_now":true,
                 "starts_at":"1970-01-01T00:00:00Z","ends_at":"1970-01-02T00:00:00Z"},
                {"id":"rg-a-2","label":"Launch reset","gives_back_weekly":true,
                 "clears":["seven_day_overage_included"],"resets_left":1,"resets_total":1,"usable_now":true,
                 "starts_at":"1970-01-01T00:00:00Z","ends_at":"1970-01-02T00:00:00Z"}
            ]}}"#,
        );
        assert_eq!(
            states(&block, 1_000),
            vec![GrantStatus::Usable, GrantStatus::Waiting],
            "both grants are of the kind fable, so the second waits behind the first"
        );
    }

    #[test]
    fn grants_of_two_kinds_are_both_usable() {
        let block = parsed(
            r#"{"cedar_ember":{"eligible":true,"grants":[
                {"id":"rg-a-1","label":"Launch reset","gives_back_weekly":true,"type":"full",
                 "clears":["five_hour"],"resets_left":1,"resets_total":1,"usable_now":true,
                 "starts_at":"1970-01-01T00:00:00Z","ends_at":"1970-01-02T00:00:00Z"},
                {"id":"rg-a-2","label":"Launch reset","gives_back_weekly":false,"type":"full",
                 "clears":["five_hour"],"resets_left":1,"resets_total":1,"usable_now":true,
                 "starts_at":"1970-01-01T00:00:00Z","ends_at":"1970-01-02T00:00:00Z"}
            ]}}"#,
        );
        assert_eq!(
            states(&block, 1_000),
            vec![GrantStatus::Usable, GrantStatus::Usable],
            "the grant that gives the Fable week back is of the kind fable, the other of the kind full"
        );
    }

    #[test]
    fn a_grant_without_a_type_takes_its_kind_from_what_it_clears() {
        let block = parsed(
            r#"{"cedar_ember":{"eligible":true,"grants":[
                {"id":"rg-a-1","label":"Launch reset","clears":["five_hour"],"resets_left":1,"resets_total":1,
                 "usable_now":true,"starts_at":"1970-01-01T00:00:00Z","ends_at":"1970-01-02T00:00:00Z"},
                {"id":"rg-a-2","label":"Launch reset","clears":["seven_day"],"resets_left":1,"resets_total":1,
                 "usable_now":true,"starts_at":"1970-01-01T00:00:00Z","ends_at":"1970-01-02T00:00:00Z"}
            ]}}"#,
        );
        assert_eq!(
            states(&block, 1_000),
            vec![GrantStatus::Usable, GrantStatus::Usable],
            "the first clears five_hour alone and is of the kind session, the second of the kind weekly"
        );
    }

    #[test]
    fn a_used_grant_frees_the_next_of_its_kind() {
        let block = parsed(
            r#"{"cedar_ember":{"eligible":true,"grants":[
                {"id":"rg-a-1","label":"Launch reset","gives_back_weekly":true,
                 "clears":["seven_day_overage_included"],"resets_left":0,"resets_total":1,"usable_now":true,
                 "starts_at":"1970-01-01T00:00:00Z","ends_at":"1970-01-02T00:00:00Z"},
                {"id":"rg-a-2","label":"Launch reset","gives_back_weekly":true,
                 "clears":["seven_day_overage_included"],"resets_left":1,"resets_total":1,"usable_now":true,
                 "starts_at":"1970-01-01T00:00:00Z","ends_at":"1970-01-02T00:00:00Z"}
            ]}}"#,
        );
        assert_eq!(
            states(&block, 1_000),
            vec![GrantStatus::Used, GrantStatus::Usable],
            "a grant with no reset left is used whatever usable_now reads"
        );
    }

    #[test]
    fn a_cooldown_ends_by_the_clock() {
        // 86_400_000 is 1970-01-02T00:00:00Z, the cooldown's instant.
        let block = parsed(
            r#"{"cedar_ember":{"eligible":true,"cooldown_until":"1970-01-02T00:00:00Z","grants":[
                {"id":"rg-a-1","label":"Launch reset","clears":["five_hour"],"resets_left":1,"resets_total":1,
                 "usable_now":false,"unavailable_reason":"cooldown",
                 "starts_at":"1970-01-01T00:00:00Z","ends_at":"1970-01-03T00:00:00Z"}
            ]}}"#,
        );
        assert_eq!(
            (states(&block, 86_399_999), states(&block, 86_400_000)),
            (vec![GrantStatus::Cooldown], vec![GrantStatus::Usable]),
            "the cooldown holds the grant up to its instant and no longer"
        );
    }

    #[test]
    fn another_reason_outlives_a_passed_cooldown() {
        // The cooldown's instant, 1970-01-01T00:00:01Z, is 1_000, before the clock of 2_000.
        let block = parsed(
            r#"{"cedar_ember":{"eligible":true,"cooldown_until":"1970-01-01T00:00:01Z","grants":[
                {"id":"rg-a-1","label":"Launch reset","clears":["five_hour"],"resets_left":1,"resets_total":1,
                 "usable_now":false,"unavailable_reason":"needs_limit",
                 "starts_at":"1970-01-01T00:00:00Z","ends_at":"1970-01-02T00:00:00Z"}
            ]}}"#,
        );
        let answer: Vec<(GrantStatus, Option<String>)> = grant_states(&block, 2_000)
            .into_iter()
            .map(|grant| (grant.state, grant.reason))
            .collect();
        assert_eq!(
            answer,
            vec![(GrantStatus::Blocked, Some("needs_limit".to_string()))],
            "the clock tells that the cooldown ended and nothing about the limit the grant needs"
        );
    }

    #[test]
    fn a_grant_ends_at_its_end() {
        // 86_400_000 is 1970-01-02T00:00:00Z, the grant's end.
        let block = parsed(
            r#"{"cedar_ember":{"eligible":true,"grants":[
                {"id":"rg-a-1","label":"Launch reset","clears":["five_hour"],"resets_left":1,"resets_total":1,
                 "usable_now":true,"starts_at":"1970-01-01T00:00:00Z","ends_at":"1970-01-02T00:00:00Z"}
            ]}}"#,
        );
        assert_eq!(
            (states(&block, 86_399_999), states(&block, 86_400_000)),
            (vec![GrantStatus::Usable], vec![GrantStatus::Ended]),
            "the grant is offered up to its end and not at it"
        );
    }

    #[test]
    fn a_body_without_the_block_is_no_block_without_a_grant() {
        assert!(
            parse_limit_resets(br#"{"cedar_ember":null}"#)
                .expect("the body parses")
                .is_none(),
            "a null member is no block"
        );
        let block =
            parsed(r#"{"cedar_ember":{"eligible":false,"ineligible_reason":"seat","grants":[]}}"#);
        assert_eq!(
            (
                block.eligible,
                block.ineligible_reason.as_deref(),
                block.grants.len()
            ),
            (false, Some("seat"), 0),
            "a seat's block lists no grant and says why"
        );
    }
}
