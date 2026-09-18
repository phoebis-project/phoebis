//! Alert rules, the engine that opens and resolves their events, and the webhook deliverer.

mod deliver;
mod engine;
mod inputs;
mod question;
mod rule;

pub use deliver::{body, deliver, signature, ReqwestWebhook, Sleeper, Webhook, BACKOFF};
pub use engine::{AlertParts, AlertStats, Alerts, DeliveryResult, Transition};
pub use inputs::{question_subject, spawn_inputs, AlertInputs, BusArm, Poller, SpendSource};
pub use question::{
    question_in_document, question_in_event, question_text, SseCutter, SSE_BUFFER_MAX,
};
pub use rule::{allowed_scopes, Kind, Rule, Scope, ScopeType, Signal};
