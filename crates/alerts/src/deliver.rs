//! One webhook delivery: the spec's body, the signature when the rule has a secret, three attempts with the
//! backoff, and a row per attempt.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use phoebis_store::{AlertDeliveryRow, AlertEventRow, Secret};
use sha2::Sha256;

use crate::destination::{refusal_of, GuardedResolver};
use crate::engine::{AlertParts, DeliveryResult};
use crate::rule::{LifecycleEvent, Rule};

/// A boxed future rather than an async trait, so the fake in the tests needs no extra crate.
pub type Sleeper = dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync;

pub trait Webhook: Send + Sync {
    fn post(
        &self,
        url: String,
        body: Vec<u8>,
        signature: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<u16, String>> + Send>>;
}

pub struct ReqwestWebhook {
    client: reqwest::Client,
}

impl ReqwestWebhook {
    /// `allow_local` is the operator's policy on destinations only this host can reach, the same one the
    /// admin routes read: off, the resolver drops those addresses out of every answer a name gives, so a
    /// host whose record moves after the rule was written is still refused.
    pub fn new(allow_local: bool) -> ReqwestWebhook {
        // No redirects: a signature is over this body toward this URL, and a redirect would resend it elsewhere.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(GuardedResolver::new(allow_local)))
            .user_agent(format!("phoebis/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("a reqwest client");
        ReqwestWebhook { client }
    }
}

impl Webhook for ReqwestWebhook {
    fn post(
        &self,
        url: String,
        body: Vec<u8>,
        signature: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<u16, String>> + Send>> {
        let client = self.client.clone();
        Box::pin(async move {
            let mut request = client
                .post(&url)
                .header("content-type", "application/json")
                .body(body);
            if let Some(signature) = signature {
                request = request.header("x-phoebis-signature", signature);
            }
            // The error's Display names the URL, which may carry a token in its path; only the kind is kept,
            // and the refusal of a destination the policy rules out, which says nothing about the URL.
            request
                .send()
                .await
                .map(|r| r.status().as_u16())
                .map_err(|e| {
                    if let Some(refusal) = refusal_of(&e) {
                        refusal
                    } else if e.is_timeout() {
                        "timeout".into()
                    } else if e.is_connect() {
                        "connect failed".into()
                    } else {
                        "request failed".into()
                    }
                })
        })
    }
}

pub const BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(4),
    Duration::from_secs(16),
];

/// The spec's body: rule, kind, subject, state, opened_at, resolved_at, detail, router. `now` is the moment of
/// this attempt, not the event's own clock read: a retry can cross the question's deadline between attempts.
pub fn body(rule: &Rule, event: &AlertEventRow, state: &str, router: &str, now: i64) -> Vec<u8> {
    let mut detail: serde_json::Value = event
        .detail_json
        .as_deref()
        .and_then(|d| serde_json::from_str(d).ok())
        .unwrap_or(serde_json::Value::Null);
    if let Some(map) = detail.as_object_mut() {
        // The text is a copy of a captured body and may not outlive it; the admin API applies this rule at
        // read time and the wire applies it here, per attempt, since a retry can cross the deadline.
        let expired = map
            .get("question_expires_at")
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|at| now >= at);
        if expired && map.contains_key("question") {
            map.insert("question".into(), serde_json::Value::Null);
        }
    }
    // Keys in name order, so a receiver that re-serializes to verify the signature sees the same bytes.
    serde_json::to_vec(&serde_json::json!({
        "detail": detail,
        "kind": rule.kind.as_str(),
        "opened_at": event.opened_at,
        "resolved_at": event.resolved_at,
        "router": router,
        "rule": rule.id,
        "state": state,
        "subject": event.subject,
    }))
    .expect("the body serializes")
}

pub fn signature(secret: &Secret, body: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.expose().as_bytes())
        .expect("HMAC takes any key length");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    format!(
        "sha256={}",
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// A 2xx is delivered; every other status, and every transport error, is an attempt that failed.
pub(crate) fn delivered(status: u16) -> bool {
    (200..300).contains(&status)
}

/// What an attempt reports when the rule's secret is expected but cannot be had.
pub(crate) const SECRET_UNAVAILABLE: &str = "secret unavailable";

/// The signature for this body, or the failure that stops the attempt. A rule the operator marked as signed
/// never posts unsigned: a receiver that authenticates by signature would drop the body anyway, and one that
/// accepts it unsigned would trust bytes nobody proved.
pub(crate) fn sign(parts: &AlertParts, rule: &Rule, raw: &[u8]) -> Result<Option<String>, String> {
    match parts.store.alert_rule_secret(&rule.id) {
        Ok(Some(secret)) => Ok(Some(signature(&secret, raw))),
        Ok(None) if !rule.has_secret => Ok(None),
        Ok(None) => {
            tracing::warn!(rule = %rule.id, "the alert rule is signed but its secret is gone");
            Err(SECRET_UNAVAILABLE.into())
        }
        Err(e) => {
            tracing::warn!(%e, rule = %rule.id, "the alert rule's secret could not be read");
            Err(SECRET_UNAVAILABLE.into())
        }
    }
}

/// Three attempts at most; the first is immediate and the wait before attempt n + 1 is BACKOFF[n - 1]. Every
/// attempt lands as a row before the next starts, so an operator reading mid-retry sees the truth so far. The
/// secret is resolved per attempt, so a rule whose secret is briefly unreadable signs on the retry.
pub async fn deliver(
    parts: &AlertParts,
    rule: &Rule,
    event: &AlertEventRow,
    state: &str,
    lifecycle: Option<LifecycleEvent>,
    sleeper: &Sleeper,
) -> DeliveryResult {
    let mut last = DeliveryResult {
        status: None,
        error: Some("not attempted".into()),
    };
    for attempt in 1..=BACKOFF.len() as u32 {
        if attempt > 1 {
            sleeper(BACKOFF[attempt as usize - 2]).await;
        }
        let now = (parts.clock)();
        let raw = body(rule, event, state, &parts.router_name, now);
        let (status, error) = match sign(parts, rule, &raw) {
            Ok(signature) => match parts
                .webhook
                .post(rule.webhook_url.clone(), raw, signature)
                .await
            {
                Ok(s) => (Some(s), None),
                Err(e) => (None, Some(e)),
            },
            Err(e) => (None, Some(e)),
        };
        let landed = status.is_some_and(delivered);
        let row = AlertDeliveryRow {
            id: 0,
            // A lifecycle delivery opens no event, so the row names what it posted itself.
            event_id: lifecycle.is_none().then(|| event.id.clone()),
            rule_id: rule.id.clone(),
            kind: rule.kind.as_str().into(),
            subject: event.subject.clone(),
            event: lifecycle.map(|e| e.as_str().into()),
            // Read only for a lifecycle row: a stateful event's detail can carry a captured question the
            // purge blanks in `alert_events` alone, and a copy here would outlive that redaction.
            detail_json: lifecycle.and_then(|_| event.detail_json.clone()),
            state: state.into(),
            attempt,
            at: now,
            status,
            error: error.clone(),
        };
        if let Err(e) = parts.store.insert_alert_delivery(&row) {
            tracing::warn!(%e, rule = %rule.id, "an alert delivery row could not be written");
        }
        parts
            .metrics
            .observe_alert_delivery(&rule.id, if landed { "delivered" } else { "failed" });
        last = DeliveryResult { status, error };
        if landed {
            return last;
        }
    }
    last
}
