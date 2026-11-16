//! The sessions route's spend members in the wire's words: a session's spend apart from its probes', its agents'
//! entries and its last day, from the figures `phoebis_ledger::session_figures` prices at the read off the ledger's
//! per-session totals.

use std::collections::BTreeMap;

use phoebis_ledger::{AgentFigures, DayFigures, SpendFigures, SpendMoney, NO_CLASS};
use utoipa::ToSchema;

/// The session's spend over its client requests, and the same figures over its probes, apart.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionSpend {
    /// The client requests priced, probes and refused requests out.
    pub requests: u64,
    /// Their dollars at the list price, the sum of what the request log shows as `cost_usd`, where a client
    /// classifier call the plan paid adds zero.
    pub usd: f64,
    /// The dollars billed by funding, `api_key`, `fast`, `overage` or `usage_based`, a zero entry left out.
    pub billed_usd: BTreeMap<String, f64>,
    /// The billed dollars by the account that served them: an object of account id to dollars, a zero entry left out,
    /// empty where no request billed. Its sum is the sum of `billed_usd`.
    pub billed_usd_by_account: BTreeMap<String, f64>,
    /// The requests' Pro-week figures summed, the share of the Pro 7-day window each serving attempt's usage spends
    /// whatever account tier served it, null where a request priced nothing and 0 with no plan-funded request.
    pub equivalent: Option<f64>,
    /// The shares of each account's own windows under the tier the serving attempt recorded, per account, keyed
    /// `five_hour`, `seven_day`, then `seven_day_<family>`, since a share names one account's plan. A shared window the
    /// account's set lacks today is left out, and the account's key stays, so a key with no window under it still marks
    /// that a plan paid.
    pub shares: BTreeMap<String, BTreeMap<String, f64>>,
    /// The same figures over the router's cache probes of the session.
    pub probes: SessionSpendPart,
}

/// `SessionSpend` without its probes.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionSpendPart {
    pub requests: u64,
    pub usd: f64,
    pub billed_usd: BTreeMap<String, f64>,
    pub billed_usd_by_account: BTreeMap<String, f64>,
    pub equivalent: Option<f64>,
    pub shares: BTreeMap<String, BTreeMap<String, f64>>,
}

/// The session's client requests of the last 24 hours by class, and their spend.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionDay {
    /// By `request_class` as the ledger holds it, `other` for a request with none.
    pub requests: BTreeMap<String, u64>,
    pub usd: f64,
    pub billed_usd: BTreeMap<String, f64>,
    pub equivalent: Option<f64>,
}

impl Default for SessionDay {
    /// The day of a row the ledger holds no figures for, which reads as a session with no request in its day reads: no
    /// plan request priced, which reads the Pro-week figure 0.
    fn default() -> SessionDay {
        day_of(DayFigures::default())
    }
}

/// One agent's requests of the session over every model it ran, from the ledger.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentRequests {
    pub agent_id: String,
    /// The canonical model of the agent's newest served client request, or of its newest served probe where the ledger
    /// holds no served client request of the agent.
    pub model: String,
    /// The newest request's agent type.
    pub agent_type: Option<String>,
    /// The newest served client request's serving account.
    pub account_id: Option<String>,
    /// The newest served client request.
    pub last_request_at: Option<i64>,
    /// The agent's first client request over the session's life, refused ones included, null where the ledger holds
    /// no request of the agent that is no probe. It moves when the purge removes the first requests.
    pub first_request_at: Option<i64>,
    /// The agent's client requests and their spend over every model it ran, `spend.requests` their count.
    pub spend: SessionSpend,
}

/// The class word of a request that named none.
const UNCLASSED: &str = "other";

/// The billed dollars by funding under `Funding::as_str`'s words, since the sums rows file an API key account's
/// dollars under `billed`.
fn billed_of(money: &SpendMoney) -> BTreeMap<String, f64> {
    let mut billed = BTreeMap::new();
    for (funding, usd) in &money.billed_usd {
        *billed.entry(funding.as_str().to_string()).or_default() += usd;
    }
    billed
}

fn part_of(figures: SpendFigures) -> SessionSpendPart {
    let SpendFigures {
        requests,
        money,
        billed_usd_by_account,
        shares,
    } = figures;
    SessionSpendPart {
        requests,
        usd: money.usd,
        billed_usd: billed_of(&money),
        billed_usd_by_account,
        equivalent: money.equivalent,
        shares: shares
            .into_iter()
            .map(|(account_id, windows)| {
                (
                    account_id,
                    windows
                        .into_iter()
                        .map(|(window, share)| (window.to_string(), share))
                        .collect(),
                )
            })
            .collect(),
    }
}

pub(crate) fn spend_of(spend: SpendFigures, probes: SpendFigures) -> SessionSpend {
    let SessionSpendPart {
        requests,
        usd,
        billed_usd,
        billed_usd_by_account,
        equivalent,
        shares,
    } = part_of(spend);
    SessionSpend {
        requests,
        usd,
        billed_usd,
        billed_usd_by_account,
        equivalent,
        shares,
        probes: part_of(probes),
    }
}

/// The day by class, a request with no class under `other`.
pub(crate) fn day_of(day: DayFigures) -> SessionDay {
    let DayFigures {
        mut requests,
        money,
    } = day;
    if let Some(count) = requests.remove(NO_CLASS) {
        *requests.entry(UNCLASSED.to_string()).or_default() += count;
    }
    SessionDay {
        requests,
        usd: money.usd,
        billed_usd: billed_of(&money),
        equivalent: money.equivalent,
    }
}

pub(crate) fn agent_of(agent: AgentFigures) -> AgentRequests {
    AgentRequests {
        agent_id: agent.agent_id,
        model: agent.model,
        agent_type: agent.agent_type,
        account_id: agent.account_id,
        last_request_at: agent.last_request_at,
        first_request_at: agent.first_request_at,
        spend: spend_of(agent.spend, agent.probes),
    }
}
