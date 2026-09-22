//! Quota samples from two sources: every response head of an OAuth account, and the hourly poll of the usage
//! endpoint. Heads cost nothing and cover active accounts; the poll fills idle gaps and is the only source of
//! extra-usage credit totals.

use std::sync::Arc;
use std::time::Duration;

use phoebis_oauth::{Clock, UsageService, UsageSnapshot};
use phoebis_relay::UnifiedHeaders;
use phoebis_store::{AccountKind, AccountState, Store};
use phoebis_transport::response::ResponseHead;

use crate::assemble::Record;
use crate::record::{ExtraUsageSampleRecord, QuotaSampleRecord};
use crate::rollup::bucket_of;
use crate::task::Ledger;
use crate::LedgerDb;

pub fn samples_from_head(
    account_id: &str,
    headers: &[(String, String)],
    observed_at: i64,
) -> (Vec<QuotaSampleRecord>, Option<ExtraUsageSampleRecord>) {
    let head = ResponseHead {
        status: 200,
        headers: headers.to_vec(),
    };
    match UnifiedHeaders::parse(&head) {
        Some(unified) => samples_from_unified(account_id, &unified, &head, observed_at),
        None => (Vec::new(), None),
    }
}

/// The assembler's entry: it parsed the head once for the attempt row and hands that parse here rather than
/// paying for a second clone and parse on every response head.
pub(crate) fn samples_from_unified(
    account_id: &str,
    unified: &UnifiedHeaders,
    head: &ResponseHead,
    observed_at: i64,
) -> (Vec<QuotaSampleRecord>, Option<ExtraUsageSampleRecord>) {
    let bucket = bucket_of(observed_at);
    let float = |name: &str| {
        head.get(name)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|f| f.is_finite())
    };
    let mut quota = Vec::new();
    let windows = [
        ("5h", unified.five_hour_utilization, unified.five_hour_reset),
        ("7d", unified.seven_day_utilization, unified.seven_day_reset),
        (
            "7d_oi",
            float("anthropic-ratelimit-unified-7d_oi-utilization"),
            unified.seven_day_oi_reset,
        ),
    ];
    for (window, utilization, reset_at) in windows {
        if utilization.is_some() || reset_at.is_some() {
            quota.push(QuotaSampleRecord {
                account_id: account_id.into(),
                bucket,
                window: window.into(),
                utilization,
                reset_at,
                source: "header".into(),
            });
        }
    }
    let extra = unified.overage_in_use.map(|in_use| ExtraUsageSampleRecord {
        account_id: account_id.into(),
        bucket,
        is_enabled: true,
        in_use: Some(in_use),
        monthly_limit: None,
        used_credits: None,
        utilization: float("anthropic-ratelimit-unified-overage-utilization"),
        currency: None,
        disabled_reason: unified.overage_disabled_reason.clone(),
        overage_reset: unified.overage_reset,
        source: "header".into(),
    });
    (quota, extra)
}

pub fn samples_from_poll(
    snapshot: &UsageSnapshot,
) -> (Vec<QuotaSampleRecord>, Option<ExtraUsageSampleRecord>) {
    let bucket = bucket_of(snapshot.fetched_at);
    // The endpoint reports percent; the headers report a fraction. One scale in the table.
    let row = |key: &str, utilization: Option<f64>, reset_at: Option<i64>| QuotaSampleRecord {
        account_id: snapshot.account_id.clone(),
        bucket,
        window: key.into(),
        utilization: utilization.map(|u| u / 100.0),
        reset_at,
        source: "poll".into(),
    };
    let mut quota: Vec<QuotaSampleRecord> = snapshot
        .windows
        .iter()
        .map(|w| row(&w.key, w.utilization, w.resets_at))
        .collect();
    if let Some(fable) = &snapshot.fable_window {
        quota.push(row("fable", fable.utilization, fable.resets_at));
    }
    let extra = snapshot
        .extra_usage
        .as_ref()
        .map(|e| ExtraUsageSampleRecord {
            account_id: snapshot.account_id.clone(),
            bucket,
            is_enabled: e.is_enabled,
            in_use: None,
            monthly_limit: e.monthly_limit,
            used_credits: e.used_credits,
            utilization: e.utilization,
            currency: e.currency.clone(),
            disabled_reason: e.disabled_reason.clone(),
            overage_reset: None,
            source: "poll".into(),
        });
    (quota, extra)
}

/// Whether the poll asks about this account: every OAuth account that holds a credential, paused or not,
/// because a paused account's quota is still the operator's to watch.
fn polled(kind: AccountKind, state: AccountState) -> bool {
    kind == AccountKind::Oauth
        && !matches!(state, AccountState::Disconnected | AccountState::Unpaired)
}

pub struct QuotaPoll;

impl QuotaPoll {
    pub fn spawn(
        _db: Arc<LedgerDb>,
        store: Arc<Store>,
        usage: Arc<UsageService>,
        ledger: Ledger,
        _clock: Clock,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let jitter = 0.9 + rand::random::<f64>() * 0.2;
                tokio::time::sleep(interval.mul_f64(jitter)).await;
                let Ok(accounts) = store.accounts() else {
                    continue;
                };
                for account in accounts.into_iter().filter(|a| polled(a.kind, a.state)) {
                    // The service's own cache, cap, and Retry-After apply; a snapshot with an error is a
                    // stale one, and a stale sample must not be written under a fresh bucket.
                    match usage.snapshot(&account.id).await {
                        Ok(snapshot) if snapshot.error.is_none() => {
                            let (quota, extra) = samples_from_poll(&snapshot);
                            ledger.record(Record::Samples { quota, extra });
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::debug!(account = %account.id, %error, "the quota poll got no snapshot")
                        }
                    }
                    // The profile row is the poll's to refresh: nothing else reads the endpoint after pairing,
                    // and the admin views show what it says.
                    match usage.profile(&account.id).await {
                        Ok(profile) => {
                            if let Err(error) =
                                store.set_profile_json(&account.id, &profile.raw.to_string())
                            {
                                tracing::warn!(account = %account.id, %error, "the profile could not be stored");
                            }
                        }
                        Err(error) => {
                            tracing::debug!(account = %account.id, %error, "the quota poll got no profile")
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_oauth::{ExtraUsage, UsageSnapshot, UsageWindow};

    #[test]
    fn a_head_yields_one_row_per_window_present_and_the_overage_row() {
        let headers: Vec<(String, String)> = [
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-5h-reset", "2000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.13"),
            ("anthropic-ratelimit-unified-overage-in-use", "true"),
            ("anthropic-ratelimit-unified-overage-utilization", "0.5"),
            ("anthropic-ratelimit-unified-overage-reset", "3000"),
        ]
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
        let (quota, extra) = samples_from_head("a", &headers, 1_234_567);
        assert_eq!(
            quota
                .iter()
                .map(|q| (
                    q.window.as_str(),
                    q.utilization,
                    q.reset_at,
                    q.bucket,
                    q.source.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("5h", Some(0.42), Some(2_000_000), 1_200, "header"),
                ("7d", Some(0.13), None, 1_200, "header")
            ]
        );
        let extra = extra.unwrap();
        assert_eq!(
            (
                extra.is_enabled,
                extra.in_use,
                extra.utilization,
                extra.overage_reset,
                extra.used_credits,
                extra.source.as_str()
            ),
            (true, Some(true), Some(0.5), Some(3_000_000), None, "header")
        );
        assert_eq!(samples_from_head("a", &[], 1), (vec![], None));
    }

    #[test]
    fn a_poll_yields_every_window_under_its_key_and_the_fable_window_and_credits() {
        let window = |key: &str, u: f64| UsageWindow {
            key: key.into(),
            utilization: Some(u),
            resets_at: Some(9_000_000),
        };
        let snapshot = UsageSnapshot {
            account_id: "a".into(),
            identity_generation: 1,
            fetched_at: 600_500,
            windows: vec![window("five_hour", 12.5), window("seven_day_opus", 3.0)],
            fable_window: Some(window("fable", 42.0)),
            extra_usage: Some(ExtraUsage {
                is_enabled: true,
                monthly_limit: Some(5_000),
                used_credits: Some(1_234),
                utilization: Some(0.2468),
                currency: Some("USD".into()),
                disabled_reason: None,
            }),
            raw_keys: vec![],
            error: None,
        };
        let (quota, extra) = samples_from_poll(&snapshot);
        assert_eq!(
            quota
                .iter()
                .map(|q| (
                    q.window.as_str(),
                    q.utilization,
                    q.bucket,
                    q.source.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("five_hour", Some(0.125), 600, "poll"),
                ("seven_day_opus", Some(0.03), 600, "poll"),
                ("fable", Some(0.42), 600, "poll")
            ]
        );
        let extra = extra.unwrap();
        assert_eq!(
            (
                extra.monthly_limit,
                extra.used_credits,
                extra.utilization,
                extra.currency.as_deref(),
                extra.in_use
            ),
            (Some(5_000), Some(1_234), Some(0.2468), Some("USD"), None)
        );
    }
}
