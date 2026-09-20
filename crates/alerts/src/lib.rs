//! Alert rules, the engine that opens and resolves their events, and the webhook deliverer.

mod deliver;
mod destination;
mod engine;
mod inputs;
mod question;
mod rule;

pub use deliver::{body, deliver, signature, ReqwestWebhook, Sleeper, Webhook, BACKOFF};
pub use destination::{admitted, ipv4_refusal, ipv6_refusal, LINK_LOCAL, LOOPBACK, UNSPECIFIED};
pub use engine::{AlertParts, AlertStats, Alerts, DeliveryResult, Transition};
pub use inputs::{
    agent_subject, question_subject, spawn_inputs, sweep_expired, AlertInputs, BusArm,
    LifecycleArm, Poller, SpendSource, Tracked, IDLE_MINUTES, LIFECYCLE_KINDS,
};
pub use question::{
    question_in_document, question_in_event, question_text, SseCutter, SSE_BUFFER_MAX,
};
pub use rule::{allowed_scopes, Kind, Rule, Scope, ScopeType, Signal};
