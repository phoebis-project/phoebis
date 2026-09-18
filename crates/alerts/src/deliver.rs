//! One webhook delivery: the spec's body, the signature when the rule has a secret, three attempts with the
//! backoff, and a row per attempt.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use phoebis_store::{AlertDeliveryRow, AlertEventRow, Secret};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::engine::{AlertParts, DeliveryResult};
use crate::rule::Rule;

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
    pub fn new() -> ReqwestWebhook {
        // No redirects: a signature is over this body toward this URL, and a redirect would resend it elsewhere.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("phoebis/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("a reqwest client");
        ReqwestWebhook { client }
    }
}

impl Default for ReqwestWebhook {
    fn default() -> Self {
        Self::new()
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
                request = request.header("x-router-signature", signature);
            }
            // The error's Display names the URL, which may carry a token in its path; only the kind is kept.
            request
                .send()
                .await
                .map(|r| r.status().as_u16())
                .map_err(|e| {
                    if e.is_timeout() {
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

/// The spec's body: rule, kind, subject, state, opened_at, resolved_at, detail, router.
pub fn body(rule: &Rule, event: &AlertEventRow, state: &str, router: &str) -> Vec<u8> {
    let detail: serde_json::Value = event
        .detail_json
        .as_deref()
        .and_then(|d| serde_json::from_str(d).ok())
        .unwrap_or(serde_json::Value::Null);
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

/// Three attempts at most; the first is immediate and the wait before attempt n + 1 is BACKOFF[n - 1]. Every
/// attempt lands as a row before the next starts, so an operator reading mid-retry sees the truth so far.
pub async fn deliver(
    parts: &AlertParts,
    rule: &Rule,
    event: &AlertEventRow,
    state: &str,
    sleeper: &Sleeper,
) -> DeliveryResult {
    let raw = body(rule, event, state, &parts.router_name);
    let secret = parts.store.alert_rule_secret(&rule.id).ok().flatten();
    let signature = secret.as_ref().map(|s| signature(s, &raw));
    let mut last = DeliveryResult {
        status: None,
        error: Some("not attempted".into()),
    };
    for attempt in 1..=BACKOFF.len() as u32 {
        if attempt > 1 {
            sleeper(BACKOFF[attempt as usize - 2]).await;
        }
        let answer = parts
            .webhook
            .post(rule.webhook_url.clone(), raw.clone(), signature.clone())
            .await;
        let (status, error) = match &answer {
            Ok(s) => (Some(*s), None),
            Err(e) => (None, Some(e.clone())),
        };
        let delivered = status.is_some_and(|s| (200..300).contains(&s));
        let row = AlertDeliveryRow {
            id: 0,
            event_id: event.id.clone(),
            state: state.into(),
            attempt,
            at: (parts.clock)(),
            status,
            error: error.clone(),
        };
        if let Err(e) = parts.store.insert_alert_delivery(&row) {
            tracing::warn!(%e, rule = %rule.id, "an alert delivery row could not be written");
        }
        parts
            .metrics
            .observe_alert_delivery(&rule.id, if delivered { "delivered" } else { "failed" });
        last = DeliveryResult { status, error };
        if delivered {
            return last;
        }
    }
    last
}
