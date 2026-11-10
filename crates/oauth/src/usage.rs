//! Parses the OAuth usage payload and caches it per account, coalescing concurrent fetches.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, Shared};
use phoebis_profile::Profile;
use phoebis_store::{AccountRow, Store};
use phoebis_transport::control::ControlClient;
use phoebis_transport::TransportError;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::exchange::{
    claim_request, control_request, instant_ms, parse_profile, ControlKind, OauthError,
    ProfileResponse,
};
use crate::limit_resets::{
    fold_claim_answer, grant_states, parse_claim_answer, parse_limit_resets, ClaimAnswer,
    GrantStatus, LimitResets, SEVEN_DAY_FABLE,
};
use crate::manager::{retry_after_ms, AcquireError, Clock, CredentialManager, LandedMark};
use crate::random;

// A poisoned mutex means a prior panic happened while the cache or in-flight map was being mutated, so the
// bookkeeping can no longer be trusted.
const LOCK_POISONED: &str = "usage service mutex poisoned by a prior panic";

/// A usage response is a small JSON document, so 5 s covers a slow upstream without holding a concurrency
/// slot for long.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The bound the client puts on a claim's request. Nothing sends a claim twice, so its one answer gets a longer wait
/// than a read's.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(25);

/// How long an unsettled claim's request id goes out again for its grant, counted from the instant its outcome
/// arrived, as the client keeps it.
const REQUEST_ID_REUSE: Duration = Duration::from_secs(600);

/// The longest an account waits out the usage endpoint, whether a `Retry-After` asked for more or the ladder
/// reached it: fifteen minutes, the token refresh's own cap. A 429 that keeps coming keeps the account here
/// rather than parking its readings for a day behind one answer.
pub const BACKOFF_CAP_MS: i64 = 15 * 60 * 1_000;

/// The wait a 429 without `Retry-After` earns, by how many 429s the account has answered in a row; past the
/// ladder every further 429 waits `BACKOFF_CAP_MS`. The ladder starts at two minutes: the endpoint's own limit
/// is per account and per minute, so a one-minute retry met the same answer.
const BACKOFF_LADDER_MS: [i64; 3] = [120_000, 240_000, 480_000];

/// The name a passing fault of the limit resets read carries. Its 429 counts on the hold under `USAGE_ENDPOINT`, so
/// the held answer names the endpoint the hold belongs to.
const LIMIT_RESETS_ENDPOINT: &str = "limit resets";

/// The endpoint names a 429's warning, the held answer and a passing fault carry.
const PROFILE_ENDPOINT: &str = "profile";
const USAGE_ENDPOINT: &str = "usage";

/// The error code of the usage endpoint's 403 to an organization that holds no plan.
const PLAN_REFUSED_CODE: &str = "oauth_not_allowed_for_organization";

/// The standing's last error while the poll sends an account whose plan ended no usage request.
const PLAN_ENDED: &str = "plan ended";

/// Top-level keys the 2.1.270 binary and the Management Center source use for the Fable weekly window,
/// read only when `limits[]` is absent from the response.
const FABLE_KEY_ALIASES: &[&str] = &["cinder_cove", "iguana_necktie"];

/// The key a snapshot gives the Fable week, so an answer built from the router's windows reads like one the
/// endpoint gave.
pub const FABLE_WINDOW_KEY: &str = "fable";

/// How an attempt at the usage or the profile endpoint ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PollOutcome {
    Clean,
    Failed,
    RateLimited,
}

/// One account's standing with the endpoint after its last attempt, kept beside the cached snapshot because
/// every caller (the poll, the startup step, the profile refresh, an admin refresh) passes through the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollStatus {
    /// The first 429 of the current run of 429s; `None` outside a run.
    pub backoff_since: Option<i64>,
    pub last_attempt_at: i64,
    /// When the windows of the last clean answer were read; `None` until one lands.
    pub last_clean_at: Option<i64>,
    /// The message of the last attempt that was not clean; `None` after a clean one.
    pub last_error: Option<String>,
    pub last_outcome: PollOutcome,
    /// When the account may be asked again; `None` when the next slot may ask it.
    pub next_attempt_at: Option<i64>,
    /// 429s answered in a row, the ladder's step, reset to 0 by a clean answer or the plan-ended pause.
    pub rate_limited_in_a_row: u32,
}

/// One edge of an account's usage backoff, fired where the service moves `CachedEntry.backoff_since`: a run starts
/// on the first 429 of either endpoint (`endpoint` names it) and ends on the next clean usage answer or at the
/// account's plan-ended pause. Called with the cache lock released, after the entry landed, so a hook may take any lock
/// of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackoffEdge {
    Started {
        account_id: String,
        at: i64,
        endpoint: &'static str,
        /// The status or the error class that opened the run.
        reason: String,
        attempts: u32,
        next_attempt_at: i64,
        /// When the last clean usage answer before the run was read, or `None` when this process read no clean answer.
        last_clean_at: Option<i64>,
    },
    Ended {
        account_id: String,
        at: i64,
        since: i64,
        attempts: u32,
        resumed_at: i64,
        cause: BackoffEndCause,
    },
}

/// What closed a backoff run: the word the `poll_backoff_ended` event's detail carries as `cause`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackoffEndCause {
    /// A clean usage answer, so the poll resumed at `resumed_at`.
    CleanPoll,
    /// The plan-ended pause, so the poll stays paused and `resumed_at` is the pause's instant.
    PlanEnded,
}

impl BackoffEndCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CleanPoll => "clean_poll",
            Self::PlanEnded => "plan_ended",
        }
    }
}

pub type BackoffHook = Arc<dyn Fn(BackoffEdge) + Send + Sync>;

#[derive(Clone, Debug)]
pub struct UsageWindow {
    pub key: String,
    pub utilization: Option<f64>,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct ExtraUsage {
    pub is_enabled: bool,
    /// Minor units. The endpoint writes the two amounts with a fraction part at times (`102500.0`), a rounding
    /// artifact of the figure the body's `spend` object carries as an integer, so the parse rounds each to the
    /// nearest minor unit for the readers that store an integer.
    #[serde(default, deserialize_with = "minor_units")]
    pub monthly_limit: Option<i64>,
    #[serde(default, deserialize_with = "minor_units")]
    pub used_credits: Option<i64>,
    pub utilization: Option<f64>,
    pub currency: Option<String>,
    /// The currency's minor-unit exponent, null where the body carries none.
    #[serde(default)]
    pub decimal_places: Option<u8>,
    pub disabled_reason: Option<String>,
}

/// A number with an optional fraction part, rounded to the nearest minor unit. `null` and an absent member read
/// as `None`.
fn minor_units<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let value: Option<f64> = serde::Deserialize::deserialize(d)?;
    Ok(value.map(|amount| amount.round() as i64))
}

/// The body's `spend` object, the seat's spend against its cap in minor units, as the account view shows it. A
/// `spend` object whose `used` lacks `amount_minor` or `currency`, or whose `exponent` is absent or past `u8`, reads
/// as no `spend` object: the fallback to the extra-usage amounts applies and nothing fails.
#[derive(Clone, Debug, PartialEq)]
pub struct SpendReading {
    pub used_minor: i64,
    /// Null where the body sets no limit: the spend alone, no bar.
    pub limit_minor: Option<i64>,
    pub currency: String,
    pub exponent: u8,
    /// The body's `percent`, or `extra_usage.utilization` for a reading the amounts built, on the percent scale.
    pub percent: Option<f64>,
    /// True where the body's `spend` object gave the reading, false where the extra-usage amounts built it: the
    /// poll's row stores the `spend` columns from the object alone.
    pub from_spend: bool,
}

#[derive(Clone, Debug)]
pub struct UsageSnapshot {
    pub account_id: String,
    pub identity_generation: u64,
    pub fetched_at: i64,
    pub windows: Vec<UsageWindow>,
    pub fable_window: Option<UsageWindow>,
    pub extra_usage: Option<ExtraUsage>,
    pub raw_keys: Vec<String>,
    pub error: Option<String>,
    pub spend: Option<SpendReading>,
    pub fable_week_share: Option<f64>,
}

impl UsageSnapshot {
    /// Whether the snapshot holds a window, the Fable week's included.
    pub fn has_windows(&self) -> bool {
        !self.windows.is_empty() || self.fable_window.is_some()
    }

    /// The windows that read at their limit, under the router's names.
    pub fn windows_at_limit(&self) -> BTreeSet<String> {
        let at_limit =
            |window: &&UsageWindow| window.utilization.is_some_and(|percent| percent >= 100.0);
        let fable = self
            .fable_window
            .iter()
            .filter(at_limit)
            .map(|_| SEVEN_DAY_FABLE.to_string());
        self.windows
            .iter()
            .filter(at_limit)
            .map(|window| window.key.clone())
            .chain(fable)
            .collect()
    }
}

/// What an account's usage standing reads off its latest snapshot, answered without a copy of the snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct LatestStanding {
    pub error: Option<String>,
    pub fetched_at: i64,
    pub has_windows: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error("{0}")]
    Unavailable(String),
    /// The passing fault of a profile read or a limit resets read, displayed as the text its caller logs.
    #[error("{}", .0.text)]
    Fault(PollFault),
}

/// A passing fault of the usage read, the profile read or the limit resets read: any status but 200 and 429 (the
/// usage read's 401 apart), a transport failure, or a limit resets block that does not parse.
#[derive(Clone, Debug, PartialEq)]
pub struct PollFault {
    /// `limit resets`, `profile` or `usage`.
    pub endpoint: &'static str,
    /// The status the endpoint answered, `None` on a transport failure.
    pub status: Option<u16>,
    /// The error text the snapshot records for a usage fault, the profile refresh's for a profile one, and the text
    /// the limit resets read's warning logs for a limit resets one.
    pub text: String,
}

impl PollFault {
    /// Whether the request failed in transport, the fault that carries no status.
    pub fn transport(&self) -> bool {
        self.status.is_none()
    }
}

/// Where a bundle stands in the registry: the holder's incarnation and lifecycle generation beside the bundle's own
/// generation, since a recreated id and a reconnected slot both restart the generation at 1 and a figure read alone
/// would match the old holder's bundle. A usage 401 answers it as the credential it refused, the fence the re-pair
/// mark's conditional write compares: the account's lineage at the fetch's start and the acquired token's generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    pub incarnation: u64,
    pub lifecycle_generation: u64,
    pub credential_generation: u64,
}

impl Refused {
    /// The row's holder and slot at the bundle generation `credential_generation`.
    pub(crate) fn of(account: &AccountRow, credential_generation: u64) -> Refused {
        Refused {
            incarnation: account.incarnation,
            lifecycle_generation: account.lifecycle_generation,
            credential_generation,
        }
    }

    /// Whether the lineage is the row's holder and slot, at whatever generation.
    pub(crate) fn of_row(&self, account: &AccountRow) -> bool {
        self.incarnation == account.incarnation
            && self.lifecycle_generation == account.lifecycle_generation
    }
}

/// What a poll's fetch found beyond its snapshot, handed to the caller that started the fetch alone, so one fetch
/// reports once however many callers joined it.
#[derive(Clone, Debug, PartialEq)]
pub enum PollIssue {
    Fault(PollFault),
    /// A usage 403 whose `error.details.error_code` is `code`, the refusal an organization without a plan answers: the
    /// profile decides whether the plan ended, and the fault stands for a profile that reports one.
    PlanRefused {
        fault: PollFault,
        code: String,
    },
    Unauthorized(Refused),
}

impl PollIssue {
    /// The error text the snapshot records for the issue.
    fn text(&self) -> String {
        match self {
            PollIssue::Fault(fault) | PollIssue::PlanRefused { fault, .. } => fault.text.clone(),
            PollIssue::Unauthorized(_) => status_fault(USAGE_ENDPOINT, 401).text,
        }
    }
}

/// Reads every top-level object with a `utilization` member as a window, keeping its key. `extra_usage`
/// carries a `utilization` of its own but describes purchased credit rather than a quota window, so it is
/// parsed separately. The Fable weekly window comes from `limits[]` when present. A top-level key is read
/// only as a fallback for the wire shapes captured before that field existed. The spend reading comes from
/// the body's `spend` object where it carries the spent amount with its currency and exponent, else from the
/// extra-usage amounts where they carry the used credits, a currency and `decimal_places`, and is none
/// otherwise.
pub fn parse_usage(body: &[u8]) -> Result<ParsedUsage, OauthError> {
    let root: Value = serde_json::from_slice(body).map_err(usage_json)?;
    let map = root
        .as_object()
        .ok_or_else(|| OauthError::Malformed("usage response is not an object".into()))?;

    let raw_keys: Vec<String> = map.keys().cloned().collect();

    let mut windows: Vec<UsageWindow> = map
        .iter()
        .filter(|(key, _)| key.as_str() != "extra_usage" && key.as_str() != "limits")
        .filter_map(|(key, value)| window_from_value(key, value))
        .collect();

    let extra_usage = match map.get("extra_usage") {
        Some(Value::Null) | None => None,
        Some(value) => Some(serde_json::from_value(value.clone()).map_err(usage_json)?),
    };
    let spend = spend_reading(map.get("spend"), extra_usage.as_ref());

    let (fable_window, fable_week_share) = match map.get("limits") {
        Some(limits) => match fable_from_limits(limits) {
            Some((index, window)) => {
                // The alias is the same quota under the name an older response used, so leaving it among the
                // windows would show the operator one limit twice.
                windows.retain(|window| !FABLE_KEY_ALIASES.contains(&window.key.as_str()));
                (Some(window), fable_week_share(map, index))
            }
            None => (None, None),
        },
        None => {
            let alias = windows
                .iter()
                .position(|w| FABLE_KEY_ALIASES.contains(&w.key.as_str()));
            (alias.map(|index| windows.remove(index)), None)
        }
    };

    Ok(ParsedUsage {
        windows,
        fable_window,
        extra_usage,
        spend,
        raw_keys,
        fable_week_share,
    })
}

fn usage_json(error: serde_json::Error) -> OauthError {
    OauthError::UsageJson(error.to_string())
}

fn spend_reading(spend: Option<&Value>, extra: Option<&ExtraUsage>) -> Option<SpendReading> {
    let from_object = spend.and_then(|spend| {
        let used = spend.get("used")?;
        Some(SpendReading {
            used_minor: used.get("amount_minor").and_then(Value::as_i64)?,
            limit_minor: spend
                .get("limit")
                .and_then(|limit| limit.get("amount_minor"))
                .and_then(Value::as_i64),
            currency: used.get("currency").and_then(Value::as_str)?.to_string(),
            exponent: used
                .get("exponent")
                .and_then(Value::as_u64)
                .and_then(|exponent| u8::try_from(exponent).ok())?,
            percent: spend.get("percent").and_then(Value::as_f64),
            from_spend: true,
        })
    });
    from_object.or_else(|| {
        let extra = extra?;
        Some(SpendReading {
            used_minor: extra.used_credits?,
            limit_minor: extra.monthly_limit,
            currency: extra.currency.clone()?,
            exponent: extra.decimal_places?,
            // The endpoint gives `utilization` on the percent scale, as the `spend` object gives `percent`.
            percent: extra.utilization,
            from_spend: false,
        })
    })
}

fn window_from_value(key: &str, value: &Value) -> Option<UsageWindow> {
    let object = value.as_object()?;
    object.get("utilization")?;
    Some(UsageWindow {
        key: key.to_string(),
        utilization: object.get("utilization").and_then(Value::as_f64),
        resets_at: object.get("resets_at").and_then(instant_ms),
    })
}

fn fable_from_limits(limits: &Value) -> Option<(usize, UsageWindow)> {
    let (index, entry) = limits.as_array()?.iter().enumerate().find(|(_, item)| {
        item.get("kind").and_then(Value::as_str) == Some("weekly_scoped")
            && item
                .get("scope")
                .and_then(|scope| scope.get("model"))
                .and_then(|model| model.get("display_name"))
                .and_then(Value::as_str)
                .is_some_and(|name| name.starts_with("Fable"))
    })?;
    Some((
        index,
        UsageWindow {
            key: FABLE_WINDOW_KEY.to_string(),
            utilization: entry.get("percent").and_then(Value::as_f64),
            resets_at: entry.get("resets_at").and_then(instant_ms),
        },
    ))
}

/// The Fable week's used share of the 7-day window as a fraction of 1, from the share entry that names the
/// Fable week's place in `limits`.
fn fable_week_share(map: &serde_json::Map<String, Value>, limit_index: usize) -> Option<f64> {
    let entry = map
        .get("weekly_scoped_shares")?
        .as_array()?
        .iter()
        .find(|share| {
            share.get("limit_index").and_then(Value::as_u64) == u64::try_from(limit_index).ok()
        })?;
    Some(entry.get("used_percent_of_weekly")?.as_f64()? / 100.0)
}

#[derive(Debug)]
pub struct ParsedUsage {
    pub windows: Vec<UsageWindow>,
    pub fable_window: Option<UsageWindow>,
    pub extra_usage: Option<ExtraUsage>,
    pub spend: Option<SpendReading>,
    pub raw_keys: Vec<String>,
    pub fable_week_share: Option<f64>,
}

/// Why a fetch did not produce a usable payload. Kept apart from `UsageError`, which surfaces only when the
/// fetch task itself panicked; an ordinary upstream failure always yields a snapshot instead, empty on a
/// first-ever fetch, so the not-before that gates the next attempt is never lost for want of one to attach to.
enum FetchFailure {
    Message(String),
    /// The instant the 429 arrived, which the hold runs from, and the `Retry-After` it asked for.
    RateLimited {
        received_at: i64,
        retry_after: Option<i64>,
    },
    Issue(PollIssue),
}

/// Whose reading a fetch or a cache entry is. A deleted id created again draws a new incarnation but starts
/// over at lifecycle generation 1, so the generation alone would take the old holder's answer for the new one's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Identity {
    incarnation: u64,
    lifecycle_generation: u64,
}

impl Identity {
    fn of(account: &AccountRow) -> Identity {
        Identity {
            incarnation: account.incarnation,
            lifecycle_generation: account.lifecycle_generation,
        }
    }

    fn at(self, credential_generation: u64) -> Refused {
        Refused {
            incarnation: self.incarnation,
            lifecycle_generation: self.lifecycle_generation,
            credential_generation,
        }
    }
}

/// The snapshot cached for a first-ever fetch that failed: no windows to fall back on, but a real snapshot
/// all the same, so its `not_before` survives in the same cache entry instead of being discarded for lack
/// of a prior one to update.
fn empty_snapshot(
    account_id: &str,
    generation: Option<u64>,
    fetched_at: i64,
    error: String,
) -> UsageSnapshot {
    UsageSnapshot {
        account_id: account_id.to_string(),
        identity_generation: generation.unwrap_or_default(),
        fetched_at,
        windows: Vec::new(),
        fable_window: None,
        extra_usage: None,
        raw_keys: Vec::new(),
        error: Some(error),
        spend: None,
        fable_week_share: None,
    }
}

struct CachedEntry {
    fetched_at: i64,
    identity: Identity,
    /// Set past `fetched_at` only by a 429, to the ladder's step or the capped `Retry-After`. Any other answer
    /// leaves it equal to `fetched_at`, so the next read may go out at once.
    not_before: i64,
    snapshot: UsageSnapshot,
    /// The endpoint whose 429 armed the hold, named in the held answer.
    held_by: &'static str,
    backoff_since: Option<i64>,
    last_clean_at: Option<i64>,
    /// The message of the last attempt that was not clean; `None` after a clean one.
    last_error: Option<String>,
    last_outcome: PollOutcome,
    rate_limited_in_a_row: u32,
}

impl CachedEntry {
    /// Counts a 429 against the account: the ladder steps, a run starts unless one is under way, and the hold
    /// becomes the `Retry-After` asked for or else the ladder's step, never shorter than one already armed.
    /// Answers the run's start when this 429 opened it.
    fn rate_limited(
        &mut self,
        account_id: &str,
        endpoint: &'static str,
        at: i64,
        retry_after: Option<i64>,
    ) -> Option<BackoffEdge> {
        let opens_run = self.backoff_since.is_none();
        self.rate_limited_in_a_row = self.rate_limited_in_a_row.saturating_add(1);
        self.backoff_since.get_or_insert(at);
        self.last_outcome = PollOutcome::RateLimited;
        self.held_by = endpoint;
        let wait = retry_after.unwrap_or_else(|| ladder_step(self.rate_limited_in_a_row));
        self.not_before = self.not_before.max(at.saturating_add(wait));
        self.last_error = Some(self.hold_message());
        tracing::warn!(
            account = %account_id,
            next_attempt_at = %phoebis_store::utc_stamp(self.not_before),
            retry_after_ms = ?retry_after,
            "the {endpoint} endpoint rate-limited the account"
        );
        opens_run.then(|| BackoffEdge::Started {
            account_id: account_id.to_string(),
            at,
            endpoint,
            // A run opens on a 429 alone, so the status is its reason. The hold's sentence is the snapshot's.
            reason: "429".into(),
            attempts: self.rate_limited_in_a_row,
            next_attempt_at: self.not_before,
            last_clean_at: self.last_clean_at,
        })
    }

    /// The cached snapshot marked as no new reading, for a caller asking while the account waits out its hold:
    /// the poll and a manual refresh must not record the cached windows as freshly read.
    fn held_answer(&self) -> UsageSnapshot {
        UsageSnapshot {
            error: Some(self.hold_message()),
            ..self.snapshot.clone()
        }
    }

    /// The one sentence the held answer and the poll's last error both read. It names the endpoint alone: the
    /// deadline is `not_before`, which the status answers as `next_attempt_at` for the console to print in the
    /// viewer's zone.
    fn hold_message(&self) -> String {
        format!("the {} endpoint rate-limited the account", self.held_by)
    }

    fn status(&self, now: i64) -> PollStatus {
        PollStatus {
            backoff_since: self.backoff_since,
            last_attempt_at: self.fetched_at,
            last_clean_at: self.last_clean_at,
            last_error: self.last_error.clone(),
            last_outcome: self.last_outcome,
            next_attempt_at: (now < self.not_before).then_some(self.not_before),
            rate_limited_in_a_row: self.rate_limited_in_a_row,
        }
    }
}

/// A transport failure of any of the three reads, under the text the snapshot, the profile refresh and the limit
/// resets read's warning record for one.
fn transport_fault(endpoint: &'static str, error: &str) -> PollFault {
    PollFault {
        endpoint,
        status: None,
        text: format!("transport: {error}"),
    }
}

/// A status any of the three reads is refused with, under the one sentence the snapshot, the profile read and the
/// limit resets read's warning record for it.
fn status_fault(endpoint: &'static str, status: u16) -> PollFault {
    PollFault {
        endpoint,
        status: Some(status),
        text: format!("{endpoint} request rejected with status {status}"),
    }
}

/// A usage status other than 200 and 429: the 401 that sends the account to the re-pair under `fence`, the 403 that
/// names an organization's refusal, and every other status a passing fault.
fn usage_refusal(status: u16, body: &[u8], fence: Refused) -> FetchFailure {
    if status == 401 {
        return FetchFailure::Issue(PollIssue::Unauthorized(fence));
    }
    let fault = status_fault(USAGE_ENDPOINT, status);
    FetchFailure::Issue(match error_code(body) {
        Some(code) if status == 403 && code == PLAN_REFUSED_CODE => {
            PollIssue::PlanRefused { fault, code }
        }
        _ => PollIssue::Fault(fault),
    })
}

/// An error body's `error.details.error_code`, `None` for a body that is no JSON or names none.
fn error_code(body: &[u8]) -> Option<String> {
    let body: Value = serde_json::from_slice(body).ok()?;
    Some(body["error"]["details"]["error_code"].as_str()?.to_string())
}

/// Set by the first block that names the router's pinned client as its ineligible reason, so the warning logs once
/// per process.
static PINNED_CLIENT_WARNED: AtomicBool = AtomicBool::new(false);

/// One warning per process where a block's ineligible reason is the router's own pinned client: every card then
/// loses its resets for a cause the console does not show.
fn warn_pinned_client(account_id: &str, block: Option<&LimitResets>) {
    let Some(reason) = block.and_then(|block| block.ineligible_reason.as_deref()) else {
        return;
    };
    if matches!(reason, "cli_version" | "surface")
        && !PINNED_CLIENT_WARNED.swap(true, Ordering::Relaxed)
    {
        tracing::warn!(
            account = %account_id,
            reason = %reason,
            "the upstream offers no limit reset to the router's pinned client"
        );
    }
}

/// The ladder's wait for the `in_a_row`-th 429 in a row, counted from 1.
fn ladder_step(in_a_row: u32) -> i64 {
    (in_a_row as usize)
        .checked_sub(1)
        .and_then(|index| BACKOFF_LADDER_MS.get(index))
        .copied()
        .unwrap_or(BACKOFF_CAP_MS)
}

type FetchOutcome = Result<(UsageSnapshot, Option<PollIssue>), String>;
type SharedFetch = Shared<BoxFuture<'static, FetchOutcome>>;

/// Why a limit resets read failed. `Clone`, since every caller joined to the read takes a copy.
#[derive(Clone, Debug)]
enum ResetsFailure {
    Unavailable(String),
    Fault(PollFault),
}

type SharedResetsRead = Shared<BoxFuture<'static, Result<Option<LimitResets>, ResetsFailure>>>;

struct Inner {
    cache: Mutex<HashMap<String, CachedEntry>>,
    inflight: Mutex<HashMap<String, SharedFetch>>,
    resets_inflight: Mutex<HashMap<String, SharedResetsRead>>,
    /// The reauth mark an account's last failed acquisition landed, until a caller takes it: a fetch answers a
    /// snapshot or a profile, neither of which carries the mark, and the routing hears of it only from a report.
    landed_marks: Mutex<HashMap<String, LandedMark>>,
    /// The limit resets read each account's poll slot decides by, beside the holder it was written for.
    resets_memory: Mutex<HashMap<String, (Identity, LimitResetsMemory)>>,
    /// One lock over the kept request ids and the claims on their way, so `invalidate` and a claim's settling never
    /// interleave.
    claim_memory: Mutex<ClaimMemory>,
    on_backoff: Mutex<Option<BackoffHook>>,
    semaphore: Semaphore,
    manager: Arc<CredentialManager>,
    control: Arc<ControlClient>,
    profile: &'static Profile,
    store: Arc<Store>,
    clock: Clock,
}

/// Removes an account's in-flight entry once its fetch task ends, however it ends, so a panic never leaves
/// every later caller joined to a future that has already resolved.
struct InflightGuard {
    account_id: String,
    inner: Arc<Inner>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut inflight) = self.inner.inflight.lock() {
            inflight.remove(&self.account_id);
        }
    }
}

/// `InflightGuard` for a limit resets read's entry.
struct ResetsReadGuard {
    account_id: String,
    inner: Arc<Inner>,
}

impl Drop for ResetsReadGuard {
    fn drop(&mut self) {
        if let Ok(mut inflight) = self.inner.resets_inflight.lock() {
            inflight.remove(&self.account_id);
        }
    }
}

pub struct UsageService {
    inner: Arc<Inner>,
}

/// What the poll's slot decides a limit resets read by, as the service remembers it for the row's holder. A restart
/// forgets it, and `invalidate` drops it. An entry is keyed by its holder, so one written for an earlier holder
/// never answers for a later one.
#[derive(Clone, Debug, Default)]
pub struct LimitResetsMemory {
    /// The windows that read at their limit in the cached snapshot when the last read was sent, empty where the
    /// service remembers none.
    pub at_limit: BTreeSet<String>,
    /// Whether a read is due whatever the hour says, until the next read's request goes out.
    pub due: bool,
    /// The instant the last read that sent a request went out, clean or failed.
    pub last_sent_at: Option<i64>,
}

/// The router's class of a claim that left.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimOutcome {
    Refused,
    Unsettled,
    Used,
}

impl ClaimOutcome {
    /// `refused`, `unsettled` or `used`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::Unsettled => "unsettled",
            Self::Used => "used",
        }
    }
}

/// One claim that left, as the service answers it.
#[derive(Clone, Debug)]
pub struct Claim {
    /// The answer where the upstream sent one that parsed and names a `result`.
    pub answer: Option<ClaimAnswer>,
    /// The instant the answer arrived or the request failed, on the service's clock.
    pub at: i64,
    /// The transport error's text for a claim with no answer.
    pub error: Option<String>,
    /// The grant's label in the stored block the claim was checked on.
    pub label: String,
    pub outcome: ClaimOutcome,
    /// The request id the claim sent.
    pub request_id: String,
    /// The HTTP status, `None` where no head arrived.
    pub status: Option<u16>,
}

/// Why a claim sent nothing.
#[derive(Debug)]
pub enum ClaimRefusal {
    /// The account's row reads another holder than the one the caller confirmed.
    AccountChanged,
    /// A claim of the account is on its way.
    InFlight,
    /// The stored identity names no organization id of the form a request's path may carry.
    NoOrganization,
    /// The token could not be acquired, or the request cannot have reached the upstream, with the sentence.
    NotSent(String),
    /// The stored block does not read the grant usable at the service's clock: another state, a grant the block does
    /// not list, or no stored block.
    NotUsable,
}

/// A version 4 UUID in its lowercase form, as the client's `randomUUID` makes a claim's request id.
fn new_request_id() -> String {
    let mut bytes = [0u8; 16];
    random::fill(&mut bytes);
    // The version and the variant bits, as RFC 9562 sets them for a random UUID.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// One holder's kept request ids by grant id, each with the instant its unsettled outcome arrived.
type KeptIds = HashMap<String, (String, i64)>;

#[derive(Default)]
struct ClaimMemory {
    /// The kept request ids by account id and grant id, with the holder they were kept under and the instant
    /// the unsettled outcome arrived.
    kept: HashMap<String, (Identity, KeptIds)>,
    /// The accounts with a claim on its way, each with whether an `invalidate` crossed it.
    on_their_way: HashMap<String, bool>,
}

/// Ends an account's claim on its way when the claim ends, however it ends.
struct ClaimGuard<'a> {
    account_id: String,
    inner: &'a Inner,
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut claims) = self.inner.claim_memory.lock() {
            claims.on_their_way.remove(&self.account_id);
        }
    }
}

/// A claim whose head arrived, classed as the client classes it: a 401, 403 or 429 refuses with no answer, any other
/// status but 200 and a 200 whose body does not parse leave the claim unsettled, and a parsed answer takes its class
/// from its word.
fn answered(
    status: u16,
    body: &[u8],
    repeated: bool,
    at: i64,
    label: String,
    request_id: String,
) -> Claim {
    let (outcome, answer) = match status {
        401 | 403 | 429 => (ClaimOutcome::Refused, None),
        200 => match parse_claim_answer(body) {
            Ok(answer) => (class_of(&answer.result, repeated), Some(answer)),
            Err(_) => (ClaimOutcome::Unsettled, None),
        },
        _ => (ClaimOutcome::Unsettled, None),
    };
    Claim {
        answer,
        at,
        error: None,
        label,
        outcome,
        request_id,
        status: Some(status),
    }
}

/// The class of a parsed answer's `result`. A word the router does not know leaves the claim unsettled, since it
/// says nothing about whether the reset was spent.
fn class_of(result: &str, repeated: bool) -> ClaimOutcome {
    match result {
        "reset" => ClaimOutcome::Used,
        "already_used" | "ineligible" | "not_limited" => ClaimOutcome::Refused,
        // A cooldown that answers a repeat can be the first attempt's own, which may have spent the reset.
        "cooldown" if !repeated => ClaimOutcome::Refused,
        _ => ClaimOutcome::Unsettled,
    }
}

impl UsageService {
    pub fn new(
        manager: Arc<CredentialManager>,
        control: Arc<ControlClient>,
        profile: &'static Profile,
        concurrency: usize,
        clock: Clock,
    ) -> Self {
        let store = manager.store().clone();
        UsageService {
            inner: Arc::new(Inner {
                cache: Mutex::new(HashMap::new()),
                inflight: Mutex::new(HashMap::new()),
                resets_inflight: Mutex::new(HashMap::new()),
                landed_marks: Mutex::new(HashMap::new()),
                resets_memory: Mutex::new(HashMap::new()),
                claim_memory: Mutex::new(ClaimMemory::default()),
                on_backoff: Mutex::new(None),
                semaphore: Semaphore::new(concurrency),
                manager,
                control,
                profile,
                store,
                clock,
            }),
        }
    }

    /// The scheduled poll's and the admin refresh's read: the endpoint under the account's hold, joined to a fetch
    /// already out and written to the cache `latest` reads, so a poll beside a manual refresh makes one request and
    /// a held-down refresh button cannot turn into a stream of requests against an endpoint that rate-limits per
    /// account. A held account answers its last snapshot with the error set.
    pub async fn poll(&self, account_id: &str) -> Result<UsageSnapshot, UsageError> {
        self.poll_reporting(account_id)
            .await
            .map(|(snapshot, _)| snapshot)
    }

    /// `poll` with the typed issue of the fetch this call started: `None` on a clean read, a held answer and a caller
    /// joined to another's fetch.
    pub async fn poll_reporting(
        &self,
        account_id: &str,
    ) -> Result<(UsageSnapshot, Option<PollIssue>), UsageError> {
        let now = (self.inner.clock)();
        if let Some(held) = self.inner.held(account_id, now) {
            return Ok((held, None));
        }
        self.join_or_start(account_id).await
    }

    /// The re-pair mark for a usage 401 under `StateCause::AuthFailed`, through the manager's conditional write.
    pub fn mark_auth_failed(&self, account_id: &str, refused: Refused) -> Option<LandedMark> {
        self.inner.manager.mark_auth_failed(account_id, refused)
    }

    /// The service's clock, which stamps its attempts, for a caller that records what an attempt found.
    pub fn now(&self) -> i64 {
        (self.inner.clock)()
    }

    /// Records that the account's profile reports no plan, so the poll sends it no usage request: the standing's last
    /// error names the pause, and the snapshot keeps the windows it last read. A backoff run open at the pause ends
    /// with it, its `Ended` edge handed to the hook as a clean answer's is.
    pub fn plan_ended(&self, account_id: &str) {
        let at = (self.inner.clock)();
        let edge = {
            let mut cache = self.inner.cache.lock().expect(LOCK_POISONED);
            match cache.get_mut(account_id) {
                Some(entry) => {
                    entry.last_error = Some(PLAN_ENDED.into());
                    // The paused poll sends no usage request, so the clean answer that ends a run never comes: a
                    // run left open would hold `poll_backoff` and keep the ledger's end row unwritten until a restart.
                    let edge = entry.backoff_since.take().map(|since| BackoffEdge::Ended {
                        account_id: account_id.to_string(),
                        at,
                        since,
                        attempts: std::mem::take(&mut entry.rate_limited_in_a_row),
                        resumed_at: at,
                        cause: BackoffEndCause::PlanEnded,
                    });
                    self.inner.backoff_hook(edge)
                }
                None => {
                    // A start reads the profile before any usage request, so the pause has no entry to name it on yet.
                    let identity = self.inner.identity(account_id).unwrap_or_default();
                    cache.insert(
                        account_id.to_string(),
                        CachedEntry {
                            fetched_at: at,
                            identity,
                            not_before: at,
                            snapshot: empty_snapshot(
                                account_id,
                                Some(identity.lifecycle_generation),
                                at,
                                PLAN_ENDED.into(),
                            ),
                            held_by: USAGE_ENDPOINT,
                            backoff_since: None,
                            last_clean_at: None,
                            last_error: Some(PLAN_ENDED.into()),
                            last_outcome: PollOutcome::Failed,
                            rate_limited_in_a_row: 0,
                        },
                    );
                    None
                }
            }
        };
        announce(edge);
    }

    /// Ends the pause `plan_ended` recorded, for a profile that reports a plan again, by clearing its sentence alone.
    /// The ladder stands: a backoff run still open closes at the next clean usage poll, whose end counts the run's
    /// 429s and resets the ladder as every clean poll does, and a 429 before it continues the run's ladder.
    pub fn plan_restored(&self, account_id: &str) {
        let mut cache = self.inner.cache.lock().expect(LOCK_POISONED);
        let Some(entry) = cache.get_mut(account_id) else {
            return;
        };
        if entry.last_error.as_deref() == Some(PLAN_ENDED) {
            entry.last_error = None;
        }
    }

    /// How much longer the account waits out its backoff at the service's clock; `None` when it may be asked now.
    pub fn held_for(&self, account_id: &str) -> Option<Duration> {
        let now = (self.inner.clock)();
        let not_before = self.inner.next_attempt_at(account_id, now)?;
        Some(Duration::from_millis((not_before - now) as u64))
    }

    /// The last snapshot held for the account's row, clean or carrying its last attempt's error, with no fetch.
    /// `None` for an entry another holder of the id left behind (a re-pairing or a delete and a new account
    /// under the id that no route invalidated), so no caller shows the old holder's windows as the new one's. The
    /// holder is the row the caller holds, so the answer is for the account its response shows.
    pub fn latest_for(&self, account: &AccountRow) -> Option<UsageSnapshot> {
        let identity = Identity::of(account);
        self.inner
            .cache
            .lock()
            .expect(LOCK_POISONED)
            .get(&account.id)
            .filter(|entry| entry.identity == identity)
            .map(|entry| entry.snapshot.clone())
    }

    /// The account's standing after its last attempt; `None` for an account not asked since the process
    /// started or since `invalidate` cleared it.
    pub fn poll_status(&self, account_id: &str) -> Option<PollStatus> {
        let now = (self.inner.clock)();
        let cache = self.inner.cache.lock().expect(LOCK_POISONED);
        cache.get(account_id).map(|entry| entry.status(now))
    }

    /// `poll_status` of the account's id and the standing's reads of the snapshot `latest_for` answers for its row,
    /// under one take of the cache and with no copy of the snapshot, for a caller that lists every account's standing.
    pub fn standing_for(
        &self,
        account: &AccountRow,
    ) -> (Option<PollStatus>, Option<LatestStanding>) {
        let now = (self.inner.clock)();
        let identity = Identity::of(account);
        let cache = self.inner.cache.lock().expect(LOCK_POISONED);
        let Some(entry) = cache.get(&account.id) else {
            return (None, None);
        };
        let latest = (entry.identity == identity).then(|| LatestStanding {
            error: entry.snapshot.error.clone(),
            fetched_at: entry.snapshot.fetched_at,
            has_windows: entry.snapshot.has_windows(),
        });
        (Some(entry.status(now)), latest)
    }

    /// The reauth mark the account's last failed acquisition landed, once: the poll and the profile refresh take it
    /// after their call to report it to the routing.
    pub fn take_landed_mark(&self, account_id: &str) -> Option<LandedMark> {
        self.inner
            .landed_marks
            .lock()
            .expect(LOCK_POISONED)
            .remove(account_id)
    }

    /// Replaces the hook every backoff edge is handed to. No edge reaches a hook before one is set.
    pub fn set_on_backoff(&self, hook: BackoffHook) {
        *self.inner.on_backoff.lock().expect(LOCK_POISONED) = Some(hook);
    }

    /// Drops what the service holds for the account: the cached snapshot, the remembered limit resets read with its
    /// limit set and its due mark, and the kept request ids. A pairing keeps the holder those are keyed by, so the
    /// remembered hour would hold back the read that fills the block the pairing cleared, and a kept id would go out
    /// for the next pairing. A claim on its way across the call keeps no id.
    pub fn invalidate(&self, account_id: &str) {
        self.inner
            .cache
            .lock()
            .expect(LOCK_POISONED)
            .remove(account_id);
        self.inner
            .resets_memory
            .lock()
            .expect(LOCK_POISONED)
            .remove(account_id);
        let mut claims = self.inner.claim_memory.lock().expect(LOCK_POISONED);
        claims.kept.remove(account_id);
        // A claim on its way went out under the pairing before this call, so its id is not kept for the next one.
        if let Some(crossed) = claims.on_their_way.get_mut(account_id) {
            *crossed = true;
        }
    }

    async fn join_or_start(
        &self,
        account_id: &str,
    ) -> Result<(UsageSnapshot, Option<PollIssue>), UsageError> {
        let mut started = false;
        let shared = {
            let mut inflight = self.inner.inflight.lock().expect(LOCK_POISONED);
            match inflight.get(account_id) {
                Some(existing) => existing.clone(),
                None => {
                    started = true;
                    let inner = self.inner.clone();
                    let id = account_id.to_string();
                    // Spawned so a caller that goes away mid-fetch does not stop the fetch other callers are
                    // joined to; the guard's map lock is held across the spawn so a fetch finishing at once
                    // cannot remove the entry before it is inserted.
                    let handle = tokio::spawn(async move {
                        let guard = InflightGuard {
                            account_id: id,
                            inner,
                        };
                        guard.inner.fetch_and_cache(&guard.account_id).await
                    });
                    let shared = async move {
                        match handle.await {
                            Ok(outcome) => outcome,
                            Err(_) => Err("usage fetch panicked".to_string()),
                        }
                    }
                    .boxed()
                    .shared();
                    inflight.insert(account_id.to_string(), shared.clone());
                    shared
                }
            }
        };
        shared
            .await
            .map(|(snapshot, issue)| (snapshot, issue.filter(|_| started)))
            .map_err(UsageError::Unavailable)
    }

    /// The profile document, fetched now: the usage cache's `not_before` gates it and a 429 here counts against
    /// that gate, so the two endpoints share one backoff per account.
    pub async fn profile(&self, account_id: &str) -> Result<ProfileResponse, UsageError> {
        let now = (self.inner.clock)();
        if self.inner.next_attempt_at(account_id, now).is_some() {
            return Err(UsageError::Unavailable(
                "the account is waiting out a rate limit".into(),
            ));
        }
        let token = self
            .inner
            .manager
            .acquire(account_id)
            .await
            .map_err(|error| {
                self.inner.keep_landed_mark(account_id, &error);
                UsageError::Unavailable(format!("acquire: {error}"))
            })?;
        let permit = self
            .inner
            .semaphore
            .acquire()
            .await
            .expect("usage semaphore is never closed");
        let req = control_request(
            self.inner.profile,
            ControlKind::Profile,
            Some(token.secret.expose()),
            None,
        );
        let host = ControlKind::Profile.host(self.inner.profile);
        let sent = self.inner.control.send(req, host, FETCH_TIMEOUT).await;
        drop(permit);
        let (head, body) = sent.map_err(|error| {
            UsageError::Fault(transport_fault(PROFILE_ENDPOINT, &error.to_string()))
        })?;
        if head.status == 429 {
            // The hold runs from the answer's arrival, as a usage 429's does; `now` predates the credential and
            // permit waits.
            let received_at = (self.inner.clock)();
            self.inner.rate_limited(
                account_id,
                PROFILE_ENDPOINT,
                received_at,
                retry_after_ms(&head, received_at, BACKOFF_CAP_MS),
            );
            return Err(UsageError::Unavailable(
                "profile request rate limited".into(),
            ));
        }
        if head.status != 200 {
            return Err(UsageError::Fault(status_fault(
                PROFILE_ENDPOINT,
                head.status,
            )));
        }
        parse_profile(&body).map_err(|error| UsageError::Unavailable(format!("parse: {error}")))
    }

    /// The account's limit resets, read now in the client's shape and stored on its row: the block as stored, `None`
    /// where the body carried none. An answer that lands after a claim's fold or a pairing writes nothing, the read
    /// stays due for the account's next poll slot, and the caller reads what the row holds. Refused while the account
    /// waits out its hold, and a 429 counts on that hold.
    pub async fn limit_resets(&self, account_id: &str) -> Result<Option<LimitResets>, UsageError> {
        let now = (self.inner.clock)();
        if self.inner.next_attempt_at(account_id, now).is_some() {
            return Err(UsageError::Unavailable(
                "the account is waiting out a rate limit".into(),
            ));
        }
        let shared = {
            let mut inflight = self.inner.resets_inflight.lock().expect(LOCK_POISONED);
            match inflight.get(account_id) {
                Some(existing) => existing.clone(),
                None => {
                    let inner = self.inner.clone();
                    let id = account_id.to_string();
                    // Spawned so a caller that goes away cuts neither the request nor the store's write, with the
                    // map's lock held across the spawn as `join_or_start` holds it.
                    let handle = tokio::spawn(async move {
                        let guard = ResetsReadGuard {
                            account_id: id,
                            inner,
                        };
                        let outcome = guard.inner.read_limit_resets(&guard.account_id).await;
                        // Logged here, once per read, however many callers joined it.
                        if let Err(ResetsFailure::Fault(fault)) = &outcome {
                            let status = fault.status.map_or_else(
                                || "transport".to_string(),
                                |status| status.to_string(),
                            );
                            tracing::warn!(
                                account = %guard.account_id,
                                status = %status,
                                error = %fault.text,
                                "a read of the limit resets failed"
                            );
                        }
                        outcome
                    });
                    let shared = async move {
                        match handle.await {
                            Ok(outcome) => outcome,
                            Err(_) => Err(ResetsFailure::Unavailable(
                                "limit resets read panicked".into(),
                            )),
                        }
                    }
                    .boxed()
                    .shared();
                    inflight.insert(account_id.to_string(), shared.clone());
                    shared
                }
            }
        };
        shared.await.map_err(|failure| match failure {
            ResetsFailure::Unavailable(message) => UsageError::Unavailable(message),
            ResetsFailure::Fault(fault) => UsageError::Fault(fault),
        })
    }

    /// The limit resets read the service remembers for the row's holder, the default where it remembers none.
    pub fn limit_resets_memory(&self, account: &AccountRow) -> LimitResetsMemory {
        let holder = Identity::of(account);
        self.inner
            .resets_memory
            .lock()
            .expect(LOCK_POISONED)
            .get(&account.id)
            .filter(|(written_for, _)| *written_for == holder)
            .map(|(_, memory)| memory.clone())
            .unwrap_or_default()
    }

    /// Marks the account's limit resets read due for its next poll slot.
    pub fn mark_limit_resets_due(&self, account: &AccountRow) {
        self.inner
            .mark_resets_due(&account.id, Identity::of(account));
    }

    /// Claims `grant_id` for the holder of `account`, the row the caller read and confirmed the reset on. A grant id
    /// repeats across accounts, so the claim never follows the id to another holder.
    pub async fn claim_limit_reset(
        &self,
        account: &AccountRow,
        grant_id: &str,
    ) -> Result<Claim, ClaimRefusal> {
        let inner = &self.inner;
        let account_id = account.id.as_str();
        // The caller confirmed this holder's reset. A grant id names a promotion and repeats across accounts, so
        // an account created again under the id must not have its own spent.
        let holder = Identity::of(account);
        let row = inner
            .store
            .account(account_id)
            .ok()
            .flatten()
            .filter(|row| Identity::of(row) == holder)
            .ok_or(ClaimRefusal::AccountChanged)?;
        let organization = row
            .identity
            .as_ref()
            .and_then(|identity| identity.organization_uuid.as_deref())
            // The id becomes a part of the request's path, so nothing but a UUID's characters reaches the target.
            .filter(|id| id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
            .ok_or(ClaimRefusal::NoOrganization)?;
        // Two requests at once could spend two resets on one intent.
        let _on_its_way = inner
            .begin_claim(account_id)
            .ok_or(ClaimRefusal::InFlight)?;
        // Checked under the reservation, so a second click finds the count the first one's fold left, and matched
        // whole against the stored block, so an id that reaches the upstream is one the upstream sent. The epoch
        // comes from the same read: a pairing that lands before the answer moves it, and the fold writes nothing.
        let (label, epoch) = inner
            .usable_grant(account_id, grant_id)
            .ok_or(ClaimRefusal::NotUsable)?;
        let token = inner.manager.acquire(account_id).await.map_err(|error| {
            inner.keep_landed_mark(account_id, &error);
            ClaimRefusal::NotSent(format!("acquire: {error}"))
        })?;
        // The acquisition can wait on a refresh, long enough for the id to pass to another holder.
        if inner.identity(account_id) != Some(holder) {
            return Err(ClaimRefusal::AccountChanged);
        }
        let (request_id, repeated) = inner.request_id_for(account_id, holder, grant_id);
        let request = claim_request(
            inner.profile,
            organization,
            token.secret.expose(),
            grant_id,
            &request_id,
        );
        let sent = inner
            .control
            .send(request, inner.profile.control.usage.host, CLAIM_TIMEOUT)
            .await;
        let at = (inner.clock)();
        let claim = match sent {
            Err(TransportError::Unreachable(detail)) => return Err(ClaimRefusal::NotSent(detail)),
            Err(error) => Claim {
                answer: None,
                at,
                error: Some(error.to_string()),
                label,
                outcome: ClaimOutcome::Unsettled,
                request_id,
                status: None,
            },
            Ok((head, body)) => answered(head.status, &body, repeated, at, label, request_id),
        };
        inner.settle_request_id(account_id, holder, grant_id, &claim, repeated);
        if let Some(answer) = &claim.answer {
            inner.fold_claim(account_id, holder, epoch, grant_id, answer);
        }
        if claim.outcome == ClaimOutcome::Refused {
            inner.mark_resets_due(account_id, holder);
        }
        Ok(claim)
    }
}

impl Inner {
    async fn read_limit_resets(
        &self,
        account_id: &str,
    ) -> Result<Option<LimitResets>, ResetsFailure> {
        // Taken together before anything is sent, and before the token, so a token acquired before a pairing never
        // writes its answer under the pairing's epoch: the store refuses the write of an answer that lands for
        // another holder, or after a claim's fold or a pairing moved the epoch.
        let holder = self
            .identity(account_id)
            .ok_or_else(|| ResetsFailure::Unavailable("account not found".into()))?;
        let epoch = match self.store.account_extras_of(account_id) {
            Ok(Some(extras)) => extras.limit_resets_epoch,
            Ok(None) => return Err(ResetsFailure::Unavailable("account not found".into())),
            Err(error) => return Err(ResetsFailure::Unavailable(format!("store: {error}"))),
        };
        let token = self.manager.acquire(account_id).await.map_err(|error| {
            self.keep_landed_mark(account_id, &error);
            ResetsFailure::Unavailable(format!("acquire: {error}"))
        })?;
        let permit = self
            .semaphore
            .acquire()
            .await
            .expect("usage semaphore is never closed");
        let sent_at = (self.clock)();
        // The due mark ends here, where the request goes out. No line below ends it, so a mark set while this
        // read is out stands for the next slot.
        self.remember_resets_read(account_id, holder, sent_at);
        let request = control_request(
            self.profile,
            ControlKind::LimitResets,
            Some(token.secret.expose()),
            None,
        );
        let host = ControlKind::LimitResets.host(self.profile);
        let sent = self.control.send(request, host, FETCH_TIMEOUT).await;
        drop(permit);
        let (head, body) = sent.map_err(|error| {
            ResetsFailure::Fault(transport_fault(LIMIT_RESETS_ENDPOINT, &error.to_string()))
        })?;
        if head.status == 429 {
            // The two reads share the endpoint's limit, so the hold is the usage read's own.
            let received_at = (self.clock)();
            let retry_after = retry_after_ms(&head, received_at, BACKOFF_CAP_MS);
            self.rate_limited(account_id, USAGE_ENDPOINT, received_at, retry_after);
            return Err(ResetsFailure::Unavailable(
                "limit resets request rate limited".into(),
            ));
        }
        if head.status != 200 {
            return Err(ResetsFailure::Fault(status_fault(
                LIMIT_RESETS_ENDPOINT,
                head.status,
            )));
        }
        let block = parse_limit_resets(&body).map_err(|error| {
            ResetsFailure::Fault(PollFault {
                endpoint: LIMIT_RESETS_ENDPOINT,
                status: Some(200),
                text: format!("parse: {error}"),
            })
        })?;
        warn_pinned_client(account_id, block.as_ref());
        let text = block
            .as_ref()
            .map(|block| serde_json::to_string(block).expect("a block serializes"));
        let (incarnation, lifecycle) = (holder.incarnation, holder.lifecycle_generation);
        match self.store.set_limit_resets(
            account_id,
            incarnation,
            lifecycle,
            epoch,
            text.as_deref(),
            sent_at,
        ) {
            Ok(true) => {
                self.drop_settled_request_ids(account_id, holder, block.as_ref(), sent_at);
                Ok(block)
            }
            // A fold, a pairing or another holder came between the start and the answer. The read landed nothing,
            // and the instant remembered at its send would hold the next read back for an hour, so the read stays
            // due under the holder the row has now. The row holds what the caller should read.
            Ok(false) => {
                if let Some(stored) = self.identity(account_id) {
                    self.mark_resets_due(account_id, stored);
                }
                Ok(self.stored_limit_resets(account_id))
            }
            Err(error) => Err(ResetsFailure::Unavailable(format!("store: {error}"))),
        }
    }

    /// The block on the account's row, `None` without one and for a row that is gone.
    fn stored_limit_resets(&self, account_id: &str) -> Option<LimitResets> {
        let extras = self.store.account_extras_of(account_id).ok().flatten()?;
        serde_json::from_value(extras.limit_resets?).ok()
    }

    /// Writes the holder's entry of the memory as a read's request goes out: its instant, the windows the cached
    /// snapshot of the same holder reads at their limit, and the due mark's end.
    fn remember_resets_read(&self, account_id: &str, holder: Identity, sent_at: i64) {
        let at_limit = self
            .cache
            .lock()
            .expect(LOCK_POISONED)
            .get(account_id)
            .filter(|entry| entry.identity == holder)
            .map(|entry| entry.snapshot.windows_at_limit())
            .unwrap_or_default();
        self.resets_memory.lock().expect(LOCK_POISONED).insert(
            account_id.to_string(),
            (
                holder,
                LimitResetsMemory {
                    at_limit,
                    due: false,
                    last_sent_at: Some(sent_at),
                },
            ),
        );
    }

    /// Sets the due mark in the holder's entry of the memory.
    fn mark_resets_due(&self, account_id: &str, holder: Identity) {
        let mut memory = self.resets_memory.lock().expect(LOCK_POISONED);
        let entry = memory
            .entry(account_id.to_string())
            .or_insert_with(|| (holder, LimitResetsMemory::default()));
        if entry.0 != holder {
            *entry = (holder, LimitResetsMemory::default());
        }
        entry.1.due = true;
    }

    /// Reserves the account for one claim, `None` while another is on its way.
    fn begin_claim(&self, account_id: &str) -> Option<ClaimGuard<'_>> {
        let mut claims = self.claim_memory.lock().expect(LOCK_POISONED);
        if claims.on_their_way.contains_key(account_id) {
            return None;
        }
        claims.on_their_way.insert(account_id.to_string(), false);
        Some(ClaimGuard {
            account_id: account_id.to_string(),
            inner: self,
        })
    }

    /// The grant's label and the stored block's epoch where the block reads the grant usable at the service's
    /// clock, `None` for another state, for a grant the block does not list and for a row with no block.
    fn usable_grant(&self, account_id: &str, grant_id: &str) -> Option<(String, u64)> {
        let extras = self.store.account_extras_of(account_id).ok().flatten()?;
        let block: LimitResets = serde_json::from_value(extras.limit_resets?).ok()?;
        let at = block.grants.iter().position(|grant| grant.id == grant_id)?;
        let usable = grant_states(&block, (self.clock)())[at].state == GrantStatus::Usable;
        usable.then(|| (block.grants[at].label.clone(), extras.limit_resets_epoch))
    }

    /// The holder's kept request id for the grant and `true` while it is younger than `REQUEST_ID_REUSE`, else a new
    /// id and `false`.
    fn request_id_for(&self, account_id: &str, holder: Identity, grant_id: &str) -> (String, bool) {
        let now = (self.clock)();
        let reuse = REQUEST_ID_REUSE.as_millis() as i64;
        let claims = self.claim_memory.lock().expect(LOCK_POISONED);
        let kept = claims
            .kept
            .get(account_id)
            .filter(|(kept_under, _)| *kept_under == holder)
            .and_then(|(_, ids)| ids.get(grant_id))
            .filter(|(_, kept_at)| now - kept_at < reuse);
        match kept {
            Some((id, _)) => (id.clone(), true),
            None => (new_request_id(), false),
        }
    }

    /// Keeps the id of an unsettled claim that sent a new one, leaves a kept id as it is after an unsettled repeat, and
    /// drops it once the claim settles.
    fn settle_request_id(
        &self,
        account_id: &str,
        holder: Identity,
        grant_id: &str,
        claim: &Claim,
        repeated: bool,
    ) {
        let mut claims = self.claim_memory.lock().expect(LOCK_POISONED);
        // An `invalidate` crossed the claim, so its id went out under the pairing before and is kept for no other.
        if claims.on_their_way.get(account_id) == Some(&true) {
            return;
        }
        match claim.outcome {
            ClaimOutcome::Unsettled if repeated => {}
            ClaimOutcome::Unsettled => {
                let entry = claims
                    .kept
                    .entry(account_id.to_string())
                    .or_insert_with(|| (holder, HashMap::new()));
                if entry.0 != holder {
                    *entry = (holder, HashMap::new());
                }
                entry
                    .1
                    .insert(grant_id.to_string(), (claim.request_id.clone(), claim.at));
            }
            ClaimOutcome::Refused | ClaimOutcome::Used => {
                if let Some((kept_under, ids)) = claims.kept.get_mut(account_id) {
                    if *kept_under == holder {
                        ids.remove(grant_id);
                    }
                }
            }
        }
    }

    /// Drops the holder's kept request ids that this block read settles: a grant the block no longer lists, or one it
    /// does not read usable while no cooldown lies ahead. An id kept at or after `sent_at`, the instant the read's
    /// request went out, stays, since that read says nothing about the attempt.
    fn drop_settled_request_ids(
        &self,
        account_id: &str,
        holder: Identity,
        block: Option<&LimitResets>,
        sent_at: i64,
    ) {
        let mut claims = self.claim_memory.lock().expect(LOCK_POISONED);
        let Some((kept_under, ids)) = claims.kept.get_mut(account_id) else {
            return;
        };
        if *kept_under != holder {
            return;
        }
        let cooling = block
            .and_then(|block| block.cooldown_until)
            .is_some_and(|until| until > sent_at);
        let states = block
            .map(|block| grant_states(block, sent_at))
            .unwrap_or_default();
        ids.retain(|grant_id, (_, kept_at)| {
            // A read sent at the instant the id was kept is not known to follow the attempt.
            if *kept_at >= sent_at {
                return true;
            }
            let listed =
                block.and_then(|block| block.grants.iter().position(|grant| &grant.id == grant_id));
            listed.is_some_and(|at| cooling || states[at].state == GrantStatus::Usable)
        });
    }

    /// Folds a claim's answer into the stored block under the holder and the epoch the claim was checked on.
    fn fold_claim(
        &self,
        account_id: &str,
        holder: Identity,
        epoch: u64,
        grant_id: &str,
        answer: &ClaimAnswer,
    ) {
        let folded = self.store.fold_limit_resets(
            account_id,
            holder.incarnation,
            holder.lifecycle_generation,
            epoch,
            |stored| {
                let mut block: LimitResets = serde_json::from_str(stored).ok()?;
                fold_claim_answer(&mut block, grant_id, answer);
                Some(serde_json::to_string(&block).expect("a block serializes"))
            },
        );
        // The claim left, so its outcome stands whatever the store does with its answer.
        match folded {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                account = %account_id,
                grant = %grant_id,
                "the stored block took no fold of a limit reset claim's answer"
            ),
            Err(error) => tracing::warn!(
                account = %account_id,
                grant = %grant_id,
                error = %error,
                "the stored block took no fold of a limit reset claim's answer"
            ),
        }
    }

    fn identity(&self, account_id: &str) -> Option<Identity> {
        self.store
            .account(account_id)
            .ok()
            .flatten()
            .map(|account| Identity::of(&account))
    }

    /// The held answer while the account waits out its hold at `now`.
    fn held(&self, account_id: &str, now: i64) -> Option<UsageSnapshot> {
        let cache = self.cache.lock().expect(LOCK_POISONED);
        let entry = cache.get(account_id)?;
        (now < entry.not_before).then(|| entry.held_answer())
    }

    fn keep_landed_mark(&self, account_id: &str, error: &AcquireError) {
        if let Some(mark) = error.landed_mark() {
            self.landed_marks
                .lock()
                .expect(LOCK_POISONED)
                .insert(account_id.to_string(), mark.clone());
        }
    }

    fn next_attempt_at(&self, account_id: &str, now: i64) -> Option<i64> {
        let cache = self.cache.lock().expect(LOCK_POISONED);
        let entry = cache.get(account_id)?;
        (now < entry.not_before).then_some(entry.not_before)
    }

    async fn fetch_and_cache(&self, account_id: &str) -> FetchOutcome {
        let now = (self.clock)();
        if let Some(held) = self.held(account_id, now) {
            return Ok((held, None));
        }
        let started = self.identity(account_id);
        let (read_at, result) = self.fetch(account_id, started.unwrap_or_default()).await;
        let ended = self.identity(account_id);
        self.commit(account_id, read_at, started != ended, ended, result)
    }

    /// The attempt's instant with its result: the send's, read once the credential and the concurrency permit
    /// are held, since a request that completes during those waits is already in what the endpoint answers and
    /// the router drops a charge only up to the reading's instant, and the failure's own when no request went out. A
    /// 401 is fenced on `lineage`, the account's at the fetch's start, and the acquired token's generation.
    async fn fetch(
        &self,
        account_id: &str,
        lineage: Identity,
    ) -> (i64, Result<ParsedUsage, FetchFailure>) {
        let token = match self.manager.acquire(account_id).await {
            Ok(token) => token,
            Err(error) => {
                self.keep_landed_mark(account_id, &error);
                return (
                    (self.clock)(),
                    Err(FetchFailure::Message(format!("acquire: {error}"))),
                );
            }
        };
        let permit = self
            .semaphore
            .acquire()
            .await
            .expect("usage semaphore is never closed");
        let sent_at = (self.clock)();
        let fence = lineage.at(token.generation);
        let result = self
            .request_usage(token.secret.expose(), fence, permit)
            .await;
        (sent_at, result)
    }

    async fn request_usage(
        &self,
        bearer: &str,
        fence: Refused,
        permit: tokio::sync::SemaphorePermit<'_>,
    ) -> Result<ParsedUsage, FetchFailure> {
        let req = control_request(self.profile, ControlKind::Usage, Some(bearer), None);
        let host = ControlKind::Usage.host(self.profile);
        let sent = self.control.send(req, host, FETCH_TIMEOUT).await;
        drop(permit);
        let (head, body) = sent.map_err(|error| {
            FetchFailure::Issue(PollIssue::Fault(transport_fault(
                USAGE_ENDPOINT,
                &error.to_string(),
            )))
        })?;
        if head.status == 429 {
            let received_at = (self.clock)();
            return Err(FetchFailure::RateLimited {
                received_at,
                retry_after: retry_after_ms(&head, received_at, BACKOFF_CAP_MS),
            });
        }
        if head.status != 200 {
            return Err(usage_refusal(head.status, &body, fence));
        }
        parse_usage(&body).map_err(|error| FetchFailure::Message(format!("parse: {error}")))
    }

    /// A result whose identity moved between the fetch's start and its completion belongs to a holder that has
    /// already been replaced (a disconnect, a new pairing, or a delete and a new account under the id), so it
    /// is discarded instead of cached; the caller gets whatever was cached before, unchanged.
    fn commit(
        &self,
        account_id: &str,
        fetched_at: i64,
        identity_changed: bool,
        ended: Option<Identity>,
        result: Result<ParsedUsage, FetchFailure>,
    ) -> FetchOutcome {
        let (outcome, edge) = {
            let mut cache = self.cache.lock().expect(LOCK_POISONED);
            let (outcome, edge) = self.commit_in(
                &mut cache,
                account_id,
                fetched_at,
                identity_changed,
                ended,
                result,
            );
            (outcome, self.backoff_hook(edge))
        };
        announce(edge);
        outcome
    }

    fn backoff_hook(&self, edge: Option<BackoffEdge>) -> Option<(BackoffHook, BackoffEdge)> {
        let edge = edge?;
        let hook = self.on_backoff.lock().expect(LOCK_POISONED).clone()?;
        Some((hook, edge))
    }

    fn commit_in(
        &self,
        cache: &mut HashMap<String, CachedEntry>,
        account_id: &str,
        fetched_at: i64,
        identity_changed: bool,
        ended: Option<Identity>,
        result: Result<ParsedUsage, FetchFailure>,
    ) -> (FetchOutcome, Option<BackoffEdge>) {
        if identity_changed {
            // A hold armed while this fetch was out still marks the answer, as on every other path; an entry an
            // earlier holder of the id left behind is nobody's answer.
            let now = (self.clock)();
            let outcome = match cache.get(account_id) {
                Some(entry) if Some(entry.identity) != ended => {
                    Err("account identity changed while fetching usage".to_string())
                }
                Some(entry) if now < entry.not_before => Ok(entry.held_answer()),
                Some(entry) => Ok(entry.snapshot.clone()),
                None => Err("account identity changed while fetching usage".to_string()),
            };
            // What the fetch found is about the holder it started under, which no longer stands.
            return (outcome.map(|snapshot| (snapshot, None)), None);
        }
        // A hold armed while this fetch was out belongs to the account rather than to the snapshot it is
        // stored beside, so replacing the entry never shortens it.
        let (held, held_by) = cache
            .get(account_id)
            .map(|entry| (entry.not_before, entry.held_by))
            .unwrap_or((i64::MIN, USAGE_ENDPOINT));
        match result {
            Ok(parsed) => {
                let snapshot = UsageSnapshot {
                    account_id: account_id.to_string(),
                    identity_generation: ended.unwrap_or_default().lifecycle_generation,
                    fetched_at,
                    windows: parsed.windows,
                    fable_window: parsed.fable_window,
                    extra_usage: parsed.extra_usage,
                    raw_keys: parsed.raw_keys,
                    error: None,
                    spend: parsed.spend,
                    fable_week_share: parsed.fable_week_share,
                };
                let edge = cache.get(account_id).and_then(|entry| {
                    entry.backoff_since.map(|since| BackoffEdge::Ended {
                        account_id: account_id.to_string(),
                        at: fetched_at,
                        since,
                        attempts: entry.rate_limited_in_a_row,
                        resumed_at: fetched_at,
                        cause: BackoffEndCause::CleanPoll,
                    })
                });
                cache.insert(
                    account_id.to_string(),
                    CachedEntry {
                        fetched_at,
                        identity: ended.unwrap_or_default(),
                        not_before: fetched_at.max(held),
                        snapshot: snapshot.clone(),
                        held_by,
                        backoff_since: None,
                        last_clean_at: Some(fetched_at),
                        last_error: None,
                        last_outcome: PollOutcome::Clean,
                        rate_limited_in_a_row: 0,
                    },
                );
                (Ok((snapshot, None)), edge)
            }
            Err(failure) => {
                let previous = cache.get(account_id);
                let (backoff_since, last_clean_at, rate_limited_in_a_row, identity) = previous
                    .map(|entry| {
                        (
                            entry.backoff_since,
                            entry.last_clean_at,
                            entry.rate_limited_in_a_row,
                            entry.identity,
                        )
                    })
                    .unwrap_or((None, None, 0, ended.unwrap_or_default()));
                let previous = previous.map(|entry| entry.snapshot.clone());
                let (message, rate_limited, issue) = match failure {
                    FetchFailure::Message(message) => (message, None, None),
                    FetchFailure::RateLimited {
                        received_at,
                        retry_after,
                    } => (
                        "usage request rate-limited".to_string(),
                        Some((received_at, retry_after)),
                        None,
                    ),
                    FetchFailure::Issue(issue) => (issue.text(), None, Some(issue)),
                };
                let last_error = Some(message.clone());
                let snapshot = match previous {
                    // The snapshot's timestamp says when its windows were read, and a failed attempt read
                    // none, so it keeps the one from the last success rather than passing stale quota off as
                    // freshly fetched. The cache entry's own timestamp is this attempt's, which the poll's
                    // standing answers as its last attempt.
                    Some(previous) => UsageSnapshot {
                        error: Some(message),
                        ..previous
                    },
                    None => empty_snapshot(
                        account_id,
                        ended.map(|identity| identity.lifecycle_generation),
                        fetched_at,
                        message,
                    ),
                };
                let mut entry = CachedEntry {
                    fetched_at,
                    identity,
                    not_before: fetched_at.max(held),
                    snapshot: snapshot.clone(),
                    held_by,
                    backoff_since,
                    last_clean_at,
                    last_error,
                    last_outcome: PollOutcome::Failed,
                    rate_limited_in_a_row,
                };
                let edge = rate_limited.and_then(|(received_at, retry_after)| {
                    entry.rate_limited(account_id, USAGE_ENDPOINT, received_at, retry_after)
                });
                cache.insert(account_id.to_string(), entry);
                (Ok((snapshot, issue)), edge)
            }
        }
    }

    /// Counts a 429 of the profile read or the limit resets read against the account without touching the snapshot,
    /// so a usage fetch after this waits as a usage 429 would have made it wait.
    fn rate_limited(
        &self,
        account_id: &str,
        endpoint: &'static str,
        at: i64,
        retry_after: Option<i64>,
    ) {
        let edge = {
            let mut cache = self.cache.lock().expect(LOCK_POISONED);
            let edge = self.rate_limited_in(&mut cache, account_id, endpoint, at, retry_after);
            self.backoff_hook(edge)
        };
        announce(edge);
    }

    fn rate_limited_in(
        &self,
        cache: &mut HashMap<String, CachedEntry>,
        account_id: &str,
        endpoint: &'static str,
        at: i64,
        retry_after: Option<i64>,
    ) -> Option<BackoffEdge> {
        match cache.get_mut(account_id) {
            Some(entry) => entry.rate_limited(account_id, endpoint, at, retry_after),
            None => {
                let identity = self.identity(account_id).unwrap_or_default();
                let mut entry = CachedEntry {
                    fetched_at: at,
                    identity,
                    not_before: at,
                    snapshot: empty_snapshot(
                        account_id,
                        Some(identity.lifecycle_generation),
                        at,
                        "no usage fetched yet".into(),
                    ),
                    held_by: endpoint,
                    backoff_since: None,
                    last_clean_at: None,
                    last_error: None,
                    last_outcome: PollOutcome::RateLimited,
                    rate_limited_in_a_row: 0,
                };
                let edge = entry.rate_limited(account_id, endpoint, at, retry_after);
                cache.insert(account_id.to_string(), entry);
                edge
            }
        }
    }
}

/// Hands an edge to its hook. The caller has released the cache lock.
fn announce(edge: Option<(BackoffHook, BackoffEdge)>) {
    if let Some((hook, edge)) = edge {
        hook(edge);
    }
}
