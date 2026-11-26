//! The tick's reads of a retained recipe: the pool's policy, the ceilings and the gate against the router's state.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use phoebis_ledger::{funding_of, Speed};
use phoebis_routing::{seconds_ms, Holder, RegistrySnapshot};
use phoebis_store::{AccountState, CooldownScope, OveragePolicy, PoolRow};

use crate::sessions::state::Agent;
use crate::warming::agent::AgentWarming;
use crate::warming::breakeven::{resolve_or_shipped, Ceilings, FactorRows, Funding};
use crate::warming::causes::{Reason, Wasted};
use crate::warming::config::{floor_ttl_ms, KeepalivePolicy};

/// What the ceilings decide for one retained recipe at a tick: probe on, hold the recipe under `horizon` while its wake
/// lies beyond the horizon, or end it under `idle_limit` or `parked_limit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Probe,
    Horizon,
    IdleLimit,
    ParkedLimit,
}

pub(crate) struct RouterView {
    /// `(pool, account)` of every member whose spend rows refuse it now: the gate holds a probe pinned there.
    pub budgets: BTreeSet<(String, String)>,
    /// A retained model name to the one the router keys its rows on, for the names that differ.
    pub canonical: HashMap<String, String>,
    /// `(account, scope)` of every live cooldown row to its window, so the gate can tell a plan-window row apart.
    pub cooled: BTreeMap<(String, CooldownScope), String>,
    /// The accounts whose usage credits block the fast lane: the gate holds a fast probe pinned there.
    pub credits: BTreeSet<String>,
    pub overage: BTreeSet<String>,
    /// `(key, session, agent, canonical model)` to the pinned account's id and the incarnation the pin was made on:
    /// the holder a probe would actually run on, so a sibling's pin never overwrites an agent's own.
    pub pins: BTreeMap<(String, String, String, String), Holder>,
    /// Each pool's keepalive policy, parsed once per tick rather than once per recipe.
    pub policies: HashMap<String, KeepalivePolicy>,
    pub snapshot: Arc<RegistrySnapshot>,
    /// The usage-based seats at their spend cap: the gate holds a probe pinned there.
    pub spent: BTreeSet<String>,
}

/// Every pool's keepalive policy in `snapshot`, by pool id.
pub(crate) fn pool_policies(snapshot: &RegistrySnapshot) -> HashMap<String, KeepalivePolicy> {
    snapshot
        .pools()
        .map(|pool| {
            (
                pool.id.clone(),
                KeepalivePolicy::from_json(&pool.keepalive_json),
            )
        })
        .collect()
}

/// What the gate decided for a due recipe: wait with a reason, or drop it with the label its probes settle under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    Drop(Reason, Wasted),
    Hold(Reason),
}

/// The key's opt-out drops the recipe, a disabled pool holds it, and the pool's current id lands on the agent. Answers
/// the key's pool and its current policy, which the ceilings and the gate read, so a horizon raised in the console
/// reaches a held recipe at the next tick and an opt-out discards a held prompt rather than leaving it retained, and
/// whether the agent's pool moved.
pub(crate) fn refresh_policy<'v>(
    view: &'v RouterView,
    agent: &Agent,
    warming: &mut AgentWarming,
) -> Result<(&'v PoolRow, &'v KeepalivePolicy, bool), Gate> {
    // The operator's opt-out reaches an idle session here, not only at its next request: the recipe is dropped
    // rather than held, because nothing on that key may be probed again until its keepalive permission is back.
    let Some(key) = view
        .snapshot
        .key(&agent.key_id)
        .filter(|k| k.companion_permissions.keepalive)
    else {
        return Err(Gate::Drop(Reason::KeyOptedOut, Wasted::Released));
    };
    // The view's map is read in the same lookup, so a pool the map lacks holds as a disabled one does.
    let Some((pool, policy)) = view
        .snapshot
        .pool(&key.pool_id)
        .filter(|p| p.enabled)
        .zip(view.policies.get(&key.pool_id))
    else {
        return Err(Gate::Hold(Reason::PoolDisabled));
    };
    // The pool's policy may have changed since admission, and an admin may have moved the key to another pool
    // without a request following. The ceilings, the budget and the probe's own delta all read the pool the key
    // belongs to now.
    let moved = warming.pool_id != key.pool_id;
    warming.pool_id = key.pool_id.clone();
    Ok((pool, policy, moved))
}

/// The ceilings of one agent at a tick: its policy, the factors of its model under its account's funding (dollars
/// at the fast cells for a fast recipe, dollars for an account the view lacks) from the tick's rows, its TTL and the
/// agent's timing. Tables that give no break-even (no priced row, or a free read) fall back to the shipped price
/// table's tightest row at the recipe's speed, so a ceiling is never unbounded. A timing whose lead is the whole TTL
/// leaves no probe interval, and nothing is probed.
pub(crate) fn ceilings_for(
    warming: &AgentWarming,
    policy: &KeepalivePolicy,
    view: &RouterView,
    rows: &FactorRows,
) -> Ceilings {
    let fast = warming.fast();
    let funding = warming
        .account
        .as_deref()
        .and_then(|id| view.snapshot.account(id))
        .map_or(Funding::ApiKey, |account| {
            funding_of(
                Some(account.kind),
                view.snapshot.usage_based(&account.id),
                Some(view.overage.contains(&account.id)),
                Speed::Standard,
            )
        });
    resolve_or_shipped(
        policy,
        rows.factors(
            funding,
            &warming.canonical_model,
            fast,
            warming.current.as_ref().map_or(0, |o| o.expected),
        ),
        floor_ttl_ms(warming.ttl_ms),
        &warming.timing,
        fast,
        rows,
    )
    .unwrap_or(Ceilings {
        idle_ms: 0,
        parked_ms: 0,
        horizon_ms: 0,
        break_even_ms: 0,
        idle_share: 0.0,
    })
}

/// The spec's "Scheduling against router state" row, in its order, after `refresh_policy` answered `pool`. `Err`
/// names why the due probe waits. `Ok` lets the probe run on the holder it names, the recipe's holder: the account id
/// with the incarnation the recipe was last answered on. The pinned account's subscription term holds the probe under
/// `ineligible_credential` once it ended, and while the probe's due time plus its interval (the TTL less the lead)
/// passes the end, since the cache that probe writes dies at the end. The end is `term::end_of`'s, the one the router's
/// wind-down reads, so the last interval before a trial end still ahead holds too and probing resumes once the trial
/// end passes.
pub(crate) fn gate(
    view: &RouterView,
    agent: &Agent,
    warming: &AgentWarming,
    session_id: &str,
    pool: &PoolRow,
    policy: &KeepalivePolicy,
    now: i64,
) -> Result<Holder, Reason> {
    let key_id = agent.key_id.clone();
    // A probe keeps the session on the account it warmed only while the pin outlives the probe's due time:
    // the recipe's own TTL less the lead, so a five-minute recipe needs about four minutes of pin and a
    // one-hour one about fifty-four. The save-time floor of 300 s stays the one hard refusal.
    let due_after_ms = floor_ttl_ms(warming.ttl_ms) - warming.timing.lead_ms(warming.ttl_ms);
    if seconds_ms(pool.affinity_ttl_seconds) < due_after_ms {
        return Err(Reason::AffinityTooShort);
    }
    if !policy.enabled {
        return Err(Reason::KeepaliveOff);
    }
    // A recipe that names no incarnation of its holder cannot tell a re-paired account from the one that answered it,
    // so it waits for the agent's next request, which arms it.
    let Some(incarnation) = warming.account_incarnation else {
        return Err(Reason::PinMoved);
    };
    // The probe replays the prompt on the holder that last answered the recipe. A pin that names another holder has
    // moved since, and a probe there would write the whole prompt cold, so the recipe waits for the next request that
    // arms it on the pin's account.
    if view
        .pins
        .get(&(
            key_id.clone(),
            session_id.to_string(),
            agent.id.clone(),
            warming.canonical_model.clone(),
        ))
        .is_some_and(|pinned| {
            warming.account.as_deref() != Some(pinned.account_id.as_str())
                || pinned.incarnation != incarnation
        })
    {
        return Err(Reason::PinMoved);
    }
    let Some(account_id) = warming.account.as_deref() else {
        return Err(Reason::Paused);
    };
    let Some(account) = view
        .snapshot
        .account(account_id)
        .filter(|account| account.enabled)
    else {
        return Err(Reason::Paused);
    };
    if matches!(
        account.state,
        AccountState::ReauthRequired | AccountState::FundingRequired
    ) {
        return Err(Reason::Reauth);
    }
    // A hold rather than a pause, read live on every poll like the budget below: an operator who turns the cancel
    // off, or a profile read that clears the plan-ended flag, releases the probe on the next tick.
    if let Some(term) = view.snapshot.term(&account.id) {
        let last_interval = warming
            .due_at()
            .zip(phoebis_routing::term::end_of(term, now))
            .is_some_and(|(due, end)| due + due_after_ms > end);
        if last_interval || phoebis_routing::term::ended(term, now) {
            return Err(Reason::IneligibleCredential);
        }
    }
    let fast = warming.fast();
    // The router's own rule decides which live row holds the probe: a fast recipe passes a plan-window row on the
    // account and holds on its own lane's, as a fast request does.
    let cooled = view.cooled.iter().any(|((id, scope), window)| {
        id == &account.id
            && phoebis_routing::cooldown_holds(scope, window, &warming.canonical_model, fast)
    });
    if cooled {
        return Err(Reason::Cooled);
    }
    // A fast recipe draws usage credits, not a plan window, so plan overage does not hold it.
    if !fast && view.overage.contains(&account.id) && pool.overage_policy != OveragePolicy::Allow {
        return Err(Reason::Overage);
    }
    // Read live on every poll like the two holds above: the period's end or a raised budget clears it on the
    // next tick, and a real request that moves the pin clears it, since it arms the recipe on the pin's account.
    if view
        .budgets
        .contains(&(pool.id.clone(), account.id.clone()))
    {
        return Err(Reason::Budget);
    }
    // The selector's own two tests on the pin, read live like the holds above, so such a probe writes no rejected
    // request row at each rung of the ladder. The spend funds every lane, and the credits the fast one alone.
    if view.spent.contains(&account.id) {
        return Err(Reason::Spent);
    }
    if fast && view.credits.contains(&account.id) {
        return Err(Reason::Credits);
    }
    Ok(Holder {
        account_id: account_id.to_owned(),
        incarnation,
    })
}

#[cfg(test)]
mod tests {
    use phoebis_ledger::PlanTables;
    use phoebis_store::AccountKind;

    use crate::engine::Inner;
    use crate::sessions::config::SessionCaps;
    use crate::sessions::state::{Session, SessionKey, AGENT_HELD};
    use crate::warming::agent::warming_mut;
    use crate::warming::config::{KeepaliveLimits, KeepaliveTiming};
    use crate::warming::observation::Observation;
    use crate::warming::recipe::make_recipe;
    use crate::warming::savings::AttemptFunding;

    use super::*;

    /// One pool holding one key and two members, subscription account `o` and api_key account `a`, with keepalive
    /// on.
    fn pool_view(o_in_overage: bool) -> RouterView {
        pool_view_under(o_in_overage, OveragePolicy::Allow)
    }

    /// `pool_view` with the pool's overage policy named, for a case the overage hold has to reach.
    fn pool_view_under(o_in_overage: bool, overage_policy: OveragePolicy) -> RouterView {
        let policy = KeepalivePolicy {
            enabled: true,
            ..KeepalivePolicy::default()
        };
        let account = |id: &str, kind| phoebis_store::AccountRow::for_tests(id, kind);
        let member = |account_id: &str| phoebis_store::PoolMember {
            pool_id: "main".into(),
            account_id: account_id.into(),
            priority: 0,
            budget_daily_usd: None,
            budget_monthly_usd: None,
        };
        let load = phoebis_store::RegistryLoad {
            accounts: vec![
                account("a", AccountKind::ApiKey),
                account("o", AccountKind::Oauth),
            ],
            keys: vec![phoebis_store::ApiKeyRow {
                id: "k".into(),
                label: "k".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                model_rules: Vec::new(),
                model_rules_mode: phoebis_store::RulesMode::default(),
                long_context_twins: false,
                companion_permissions: phoebis_store::CompanionPermissions::default(),
                body_capture_hours: None,
                secret_digest: vec![0; 32],
                prefix: "00000000".into(),
                incarnation: 1,
                created_at: 0,
                updated_at: 0,
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                auto_mode_server: None,
                feature_accounts: phoebis_store::KeyFeatureAccounts::default(),
            }],
            members: vec![member("a"), member("o")],
            pools: vec![phoebis_store::PoolRow {
                overage_policy,
                keepalive_json: serde_json::to_string(&policy).unwrap(),
                prefer_subscription: false,
                ..phoebis_store::PoolRow::for_tests("main")
            }],
            ..phoebis_store::RegistryLoad::default()
        };
        let snapshot = Arc::new(RegistrySnapshot::from_load(load, [0; 32], 0, 1));
        RouterView {
            budgets: BTreeSet::new(),
            canonical: HashMap::new(),
            cooled: BTreeMap::new(),
            credits: BTreeSet::new(),
            overage: if o_in_overage {
                BTreeSet::from(["o".to_string()])
            } else {
                BTreeSet::new()
            },
            pins: BTreeMap::new(),
            policies: pool_policies(&snapshot),
            snapshot,
            spent: BTreeSet::new(),
        }
    }

    /// An observation whose recipe `make_recipe` built from a body sent in fast mode, or at standard speed.
    fn observation(fast: bool) -> Observation {
        let speed = if fast { r#""speed":"fast","# } else { "" };
        let body = format!(
            r#"{{"model":"claude-opus-5-5",{speed}"max_tokens":1024,"messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        let recipe = make_recipe(&[], body.as_bytes(), "claude-opus-5-5", 1 << 20).unwrap();
        assert_eq!(recipe.fast, fast, "{body}");
        Observation {
            id: "r".into(),
            attempt: 1,
            started: 0,
            received_at: 0,
            recipe,
            ready: true,
            expected: 2_000,
            ttl_ms: 0,
            prior: None,
            poisoned: false,
            dispatched_ms: None,
            funding: AttemptFunding::default(),
            client_label: None,
        }
    }

    /// The gate's answer for `main` holding a recipe of that speed answered on `account`, with no pin in the view.
    fn gated_at(view: &RouterView, account: &str, fast: bool) -> Option<Gate> {
        let agent = Agent::new("main", "k", 1, 0);
        let mut warming = AgentWarming::new(0, KeepaliveTiming::default());
        warming.account = Some(account.into());
        warming.account_incarnation = Some(1);
        warming.canonical_model = "claude-opus-5-5".into();
        warming.current = Some(observation(fast));
        refresh_policy(view, &agent, &mut warming)
            .and_then(|(pool, policy, _)| {
                gate(view, &agent, &warming, "s", pool, policy, 0).map_err(Gate::Hold)
            })
            .err()
    }

    /// A recipe's ceilings read the price set its prompt selects, its length the tokens the cache evidence counted: the
    /// fixture's body is under a hundred bytes, and its 2,000 tokens put it over the threshold.
    #[test]
    fn a_recipes_ceilings_read_the_price_set_its_prompt_tokens_select() {
        let pricing = phoebis_ledger::PriceRevisions::from_yaml(
            r#"
revisions:
  - effective: 2026-01-01
    models:
      claude-opus-5-5:
        input: 4
        cache_write_5m: 1.25
        cache_write_1h: 2
        cache_read: 0.1
        output: 20
        long_context: {over: 1000, input: 8, cache_write_5m: 2.5, cache_write_1h: 4, cache_read: 0.5, output: 40}
"#,
            "test",
        )
        .unwrap();
        let rows = FactorRows::of(pricing.latest(), &PlanTables::shipped(), 0);
        let timing = KeepaliveTiming::default();
        let mut warming = AgentWarming::new(0, timing);
        warming.account = Some("a".into());
        warming.canonical_model = "claude-opus-5-5".into();
        warming.current = Some(observation(false));
        let policy = KeepalivePolicy::default();
        let break_even = |warming: &AgentWarming| {
            ceilings_for(warming, &policy, &pool_view(false), &rows).break_even_ms
        };
        assert_eq!(
            break_even(&warming),
            960_000,
            "over the threshold the long-context cells' 0.5 against 2.5 break even after 16 minutes"
        );
        warming.current.as_mut().unwrap().expected = 1_000;
        assert_eq!(
            break_even(&warming),
            2_760_000,
            "at the threshold the standard cells' 0.1 against 1.25 break even after 46 minutes"
        );
    }

    #[test]
    fn a_fast_recipe_passes_a_plan_window_hold_takes_no_overage_hold_and_holds_on_a_fast_row() {
        let cooled = |scope: &str, window: &str| RouterView {
            cooled: BTreeMap::from([(
                ("o".to_string(), CooldownScope::parse(scope).unwrap()),
                window.to_string(),
            )]),
            ..pool_view(false)
        };
        let plan_window = cooled("account", "five_hour");
        assert_eq!(
            (
                gated_at(&plan_window, "o", true),
                gated_at(&plan_window, "o", false)
            ),
            (None, Some(Gate::Hold(Reason::Cooled))),
            "an account row on a plan window holds the standard recipe alone"
        );
        let fast_row = cooled("fast", "fast");
        assert_eq!(
            (
                gated_at(&fast_row, "o", true),
                gated_at(&fast_row, "o", false)
            ),
            (Some(Gate::Hold(Reason::Cooled)), None),
            "the fast lane's row holds the fast recipe alone"
        );
        let overage = pool_view_under(true, OveragePolicy::Never);
        assert_eq!(
            (
                gated_at(&overage, "o", true),
                gated_at(&overage, "o", false)
            ),
            (None, Some(Gate::Hold(Reason::Overage))),
            "plan overage holds the standard recipe alone"
        );
    }

    #[test]
    fn the_gate_holds_on_a_cooldown_row_exactly_when_the_routers_rule_does() {
        // Each row's pair is whether the row holds a standard recipe and a fast one.
        let rows = [
            ("account", "five_hour", (true, false)),
            ("account", "seven_day", (true, false)),
            ("account", "seven_day_fable", (true, false)),
            ("account", "retry_after", (true, true)),
            ("fast", "fast", (false, true)),
            ("model:claude-opus-5-5", "retry_after", (true, true)),
            ("model:claude-sonnet-5", "retry_after", (false, false)),
        ];
        for (scope, window, holds) in rows {
            let view = RouterView {
                cooled: BTreeMap::from([(
                    ("o".to_string(), CooldownScope::parse(scope).unwrap()),
                    window.to_string(),
                )]),
                ..pool_view(false)
            };
            assert_eq!(
                (
                    gated_at(&view, "o", false) == Some(Gate::Hold(Reason::Cooled)),
                    gated_at(&view, "o", true) == Some(Gate::Hold(Reason::Cooled))
                ),
                holds,
                "{scope} at {window}"
            );
        }
    }

    /// `main` of session `s` holding a standard recipe last answered on `account` under `incarnation`.
    fn holding(account: &str, incarnation: Option<u64>) -> AgentWarming {
        let mut warming = AgentWarming::new(0, KeepaliveTiming::default());
        warming.account = Some(account.into());
        warming.account_incarnation = incarnation;
        warming.canonical_model = "claude-opus-5-5".into();
        warming.current = Some(observation(false));
        warming
    }

    /// `pool_view` with `main`'s pin on `account` under `incarnation`.
    fn pinned_on(account: &str, incarnation: u64) -> RouterView {
        RouterView {
            pins: BTreeMap::from([(
                (
                    "k".to_string(),
                    "s".to_string(),
                    "main".to_string(),
                    "claude-opus-5-5".to_string(),
                ),
                holder(account, incarnation),
            )]),
            ..pool_view(false)
        }
    }

    /// The gate's whole answer for main with the record `warming` under `view`, after the policy's refresh.
    fn gate_of(view: &RouterView, warming: &mut AgentWarming) -> Result<Holder, Gate> {
        let agent = Agent::new("main", "k", 1, 0);
        refresh_policy(view, &agent, warming).and_then(|(pool, policy, _)| {
            gate(view, &agent, warming, "s", pool, policy, 0).map_err(Gate::Hold)
        })
    }

    fn holder(account: &str, incarnation: u64) -> Holder {
        Holder {
            account_id: account.into(),
            incarnation,
        }
    }

    #[test]
    fn a_due_probe_holds_while_its_pin_stands_on_another_holder() {
        assert_eq!(
            gate_of(&pinned_on("o", 1), &mut holding("a", Some(1))),
            Err(Gate::Hold(Reason::PinMoved)),
            "a pin on another account holds the recipe rather than writing its prompt cold there"
        );
        assert_eq!(
            gate_of(&pinned_on("a", 1), &mut holding("a", Some(1))),
            Ok(holder("a", 1)),
            "a pin on the recipe's own holder lets the probe run there"
        );
        assert_eq!(
            gate_of(&pinned_on("a", 2), &mut holding("a", Some(1))),
            Err(Gate::Hold(Reason::PinMoved)),
            "a pin on the same id paired again is another holder"
        );
    }

    #[test]
    fn a_recipe_with_no_holder_incarnation_holds_with_no_pin_in_the_view() {
        assert_eq!(
            gate_of(&pool_view(false), &mut holding("a", None)),
            Err(Gate::Hold(Reason::PinMoved))
        );
    }

    #[test]
    fn a_recipe_put_back_after_a_failed_request_gates_on_its_own_holder() {
        let key: SessionKey = ("k".into(), "s".into());
        let mut inner = Inner::empty(
            KeepaliveTiming::default(),
            KeepaliveLimits::default(),
            SessionCaps::default(),
            0,
        );
        let mut armed = holding("a", Some(1));
        armed.expires = Some(300_000);
        inner.sessions.entries.insert(
            key.clone(),
            Session {
                key_generation: 0,
                instance: "i".into(),
                sequence: 0,
                lease: i64::MAX,
                attended: true,
                ended: false,
                last_report_at: 0,
                last_real_at: Some(0),
                slotted: true,
                restored_until: None,
                instance_mismatch_warned: false,
                agents: BTreeMap::from([("main".to_string(), Agent::new("main", "k", 1, 0))]),
                cache_warming: true,
            },
        );
        let warming_agents = BTreeMap::from([("main".to_string(), armed)]);
        inner.warming.agents.insert(key.clone(), warming_agents);
        let changes = inner
            .warming
            .set_aside(&inner.sessions, &key, "main", 1_000);
        inner.apply_changes(changes);
        // The arrival's selection moved the agent onto the holder of the request that then failed.
        let warming = warming_mut(&mut inner.warming.agents, &key, "main").expect(AGENT_HELD);
        warming.account = Some("o".into());
        warming.account_incarnation = Some(3);
        let aside = inner
            .warming
            .take_aside(&inner.sessions, &key, "main")
            .expect("the arrival set the recipe aside");
        let (changes, rearmed) =
            inner
                .warming
                .rearm_aside(&inner.sessions, &key, "main", aside, 2_000);
        inner.apply_changes(changes);
        assert!(rearmed);
        let warming = warming_mut(&mut inner.warming.agents, &key, "main").expect(AGENT_HELD);
        assert_eq!(gate_of(&pinned_on("a", 1), warming), Ok(holder("a", 1)));
    }

    #[test]
    fn a_probe_pinned_on_a_seat_at_its_spend_cap_holds_as_spent() {
        let spent = RouterView {
            spent: BTreeSet::from(["a".to_string()]),
            ..pool_view(false)
        };
        assert_eq!(
            (
                gated_at(&spent, "a", false),
                gated_at(&pool_view(false), "a", false)
            ),
            (Some(Gate::Hold(Reason::Spent)), None)
        );
    }

    #[test]
    fn a_fast_recipe_holds_on_blocked_credits_and_a_standard_one_passes() {
        let blocked = RouterView {
            credits: BTreeSet::from(["a".to_string()]),
            ..pool_view(false)
        };
        assert_eq!(
            (
                gated_at(&blocked, "a", true),
                gated_at(&blocked, "a", false)
            ),
            (Some(Gate::Hold(Reason::Credits)), None)
        );
    }
}
