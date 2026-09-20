//! What an alert rule names and what a signal carries, and when the one matches the other.

use phoebis_store::AlertRuleRow;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    AskUserQuestion,
    ClientDrift,
    FundingRequired,
    KeepaliveCostCap,
    PoolExhausted,
    ReauthRequired,
    RefreshFailed,
    SessionActive,
    SessionIdle,
    SessionParked,
    SubagentActive,
    UtilizationAbove,
}

impl Kind {
    pub const ALL: [Kind; 12] = [
        Kind::AskUserQuestion,
        Kind::ClientDrift,
        Kind::FundingRequired,
        Kind::KeepaliveCostCap,
        Kind::PoolExhausted,
        Kind::ReauthRequired,
        Kind::RefreshFailed,
        Kind::SessionActive,
        Kind::SessionIdle,
        Kind::SessionParked,
        Kind::SubagentActive,
        Kind::UtilizationAbove,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::AskUserQuestion => "ask_user_question",
            Kind::ClientDrift => "client_drift",
            Kind::FundingRequired => "funding_required",
            Kind::KeepaliveCostCap => "keepalive_cost_cap",
            Kind::PoolExhausted => "pool_exhausted",
            Kind::ReauthRequired => "reauth_required",
            Kind::RefreshFailed => "refresh_failed",
            Kind::SessionActive => "session_active",
            Kind::SessionIdle => "session_idle",
            Kind::SessionParked => "session_parked",
            Kind::SubagentActive => "subagent_active",
            Kind::UtilizationAbove => "utilization_above",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeType {
    Account,
    All,
    AnyAccount,
    Key,
    Pool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    pub kind: ScopeType,
    pub id: Option<String>,
}

/// Which scopes a kind accepts, as the spec's table has them; the routes refuse the rest.
pub fn allowed_scopes(kind: Kind) -> &'static [ScopeType] {
    use ScopeType::*;
    match kind {
        Kind::AskUserQuestion
        | Kind::SessionActive
        | Kind::SessionIdle
        | Kind::SessionParked
        | Kind::SubagentActive => &[All, Key, Pool],
        Kind::ClientDrift => &[All],
        Kind::FundingRequired | Kind::ReauthRequired | Kind::RefreshFailed => {
            &[Account, AnyAccount]
        }
        Kind::KeepaliveCostCap | Kind::PoolExhausted => &[All, Pool],
        Kind::UtilizationAbove => &[Account, All, AnyAccount, Key, Pool],
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub id: String,
    pub kind: Kind,
    pub scope: Scope,
    pub threshold: Option<f64>,
    pub window: Option<String>,
    pub webhook_url: String,
    pub has_secret: bool,
    pub enabled: bool,
}

/// What an input names: the subject an event is keyed by, plus what the rule's scope is matched against.
#[derive(Clone, Debug, PartialEq)]
pub struct Signal {
    pub kind: Kind,
    pub account_id: Option<String>,
    pub key_id: Option<String>,
    pub pool_id: Option<String>,
    pub subject: String,
    pub detail: serde_json::Value,
}

impl Rule {
    /// `None` for a row whose kind or scope this build does not know, so a rule written by a later version is
    /// skipped rather than matched by halves.
    pub fn from_row(row: &AlertRuleRow) -> Option<Rule> {
        let kind = Kind::parse(&row.kind)?;
        let scope = Scope {
            kind: serde_json::from_value(serde_json::Value::String(row.scope_type.clone())).ok()?,
            id: row.scope_id.clone(),
        };
        Some(Rule {
            id: row.id.clone(),
            kind,
            scope,
            threshold: row.threshold,
            window: row.window.clone(),
            webhook_url: row.webhook_url.clone(),
            has_secret: row.has_secret,
            enabled: row.enabled,
        })
    }

    pub fn matches(&self, signal: &Signal) -> bool {
        if self.kind != signal.kind {
            return false;
        }
        match self.scope.kind {
            ScopeType::All => true,
            ScopeType::AnyAccount => signal.account_id.is_some(),
            ScopeType::Account => self.scope.id.is_some() && self.scope.id == signal.account_id,
            ScopeType::Key => self.scope.id.is_some() && self.scope.id == signal.key_id,
            ScopeType::Pool => self.scope.id.is_some() && self.scope.id == signal.pool_id,
        }
    }
}
