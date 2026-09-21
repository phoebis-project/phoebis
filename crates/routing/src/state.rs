//! What the router task owns between selections: pins, cooldowns, the newest overage evidence per account, and
//! the newest utilization sample per account. Every mutation reports whether it changed anything so the task
//! knows what to write through.

use std::collections::HashMap;

use phoebis_store::{AccountState, CooldownRow, OverageRow, PinRow, RoutingLoad};

use crate::RegistrySnapshot;

/// The sample TTL a state nobody configured ages readings on. The router task replaces it with its own
/// configured interval; this matches `RouterConfig::default().sample_ttl`.
const DEFAULT_SAMPLE_TTL_MS: i64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinRef {
    pub key_id: String,
    pub session_id: String,
    pub model: String,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceSource {
    Head,
    Sample,
}

impl EvidenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceSource::Head => "head",
            EvidenceSource::Sample => "sample",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverageEvidence {
    pub in_use: bool,
    pub since: Option<i64>,
    pub reset: Option<i64>,
    pub observed_at: i64,
    pub source: EvidenceSource,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UtilizationSample {
    /// The model-scoped weekly window, present only for an account whose plan carries one.
    pub fable: Option<f64>,
    pub fable_reset: Option<i64>,
    pub five_hour: Option<f64>,
    pub five_hour_reset: Option<i64>,
    pub seven_day: Option<f64>,
    pub seven_day_reset: Option<i64>,
    pub observed_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StateWrite {
    /// The relay's verdict on an account, fenced by the generations the attempt that formed it went out
    /// with. It travels with the rest of the routing writes so shutdown drains it like any other.
    AccountState {
        account_id: String,
        state: AccountState,
        lifecycle_generation: u64,
        credential_generation: u64,
    },
    DeleteCooldown(String, String),
    DeletePin(String, String, String),
    UpsertCooldown(CooldownRow),
    UpsertOverage(OverageRow),
    UpsertPin(PinRow),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub cooldowns: Vec<(String, String)>,
    /// Each dropped pin with the one fact that dropped it: `deleted`, `expired`, or `unserved`.
    pub pins: Vec<((String, String, String), &'static str)>,
}

/// A candidate's sort key in `choose`: group, priority, and the overage bit under the kind order; 0, priority,
/// and the overage bit otherwise.
pub type Tier = (u8, u32, u8);

pub struct RoutingState {
    cooldowns: HashMap<(String, String), CooldownRow>,
    next_epoch: u64,
    overage: HashMap<String, OverageEvidence>,
    pins: HashMap<(String, String, String), PinRow>,
    round_robin: HashMap<(String, Tier, String), usize>,
    sample_ttl_ms: i64,
    samples: HashMap<String, UtilizationSample>,
}

impl RoutingState {
    pub fn from_load(load: RoutingLoad) -> RoutingState {
        let next_epoch = load.pins.iter().map(|p| p.epoch).max().unwrap_or(0) + 1;
        RoutingState {
            cooldowns: load
                .cooldowns
                .into_iter()
                .map(|c| ((c.account_id.clone(), c.scope.clone()), c))
                .collect(),
            next_epoch,
            overage: load
                .overage
                .into_iter()
                .map(|o| {
                    (
                        o.account_id.clone(),
                        OverageEvidence {
                            in_use: o.in_use,
                            since: o.since,
                            reset: o.reset,
                            observed_at: o.observed_at,
                            source: if o.source == "sample" {
                                EvidenceSource::Sample
                            } else {
                                EvidenceSource::Head
                            },
                        },
                    )
                })
                .collect(),
            pins: load
                .pins
                .into_iter()
                .map(|p| ((p.key_id.clone(), p.session_id.clone(), p.model.clone()), p))
                .collect(),
            round_robin: HashMap::new(),
            sample_ttl_ms: DEFAULT_SAMPLE_TTL_MS,
            samples: HashMap::new(),
        }
    }

    pub(crate) fn fresh_epoch(&mut self) -> u64 {
        let e = self.next_epoch;
        self.next_epoch += 1;
        e
    }

    pub fn pin(&self, key_id: &str, session_id: &str, model: &str) -> Option<&PinRow> {
        self.pins.get(&(
            key_id.to_string(),
            session_id.to_string(),
            model.to_string(),
        ))
    }

    pub(crate) fn put_pin(&mut self, pin: PinRow) {
        self.pins.insert(
            (
                pin.key_id.clone(),
                pin.session_id.clone(),
                pin.model.clone(),
            ),
            pin,
        );
    }

    pub(crate) fn remove_pin(
        &mut self,
        key_id: &str,
        session_id: &str,
        model: &str,
    ) -> Option<PinRow> {
        self.pins.remove(&(
            key_id.to_string(),
            session_id.to_string(),
            model.to_string(),
        ))
    }

    /// The next index into `cohort` for a pool's tier under round robin. The key carries the cohort because
    /// `resolve` builds a candidate list per model: a singleton model and a three-account model share a pool and
    /// a tier but rotate apart, and a cohort whose membership changed is a key of its own that starts at 0. An
    /// account id is a slug, so joining the sorted ids names one cohort and no other; the caller sorts, and an
    /// unsorted cohort would split one rotation into several. In memory, so a restart starts every round again.
    pub(crate) fn round_robin_next(&mut self, pool_id: &str, tier: Tier, cohort: &[&str]) -> usize {
        let key = (pool_id.to_string(), tier, cohort.join(","));
        let slot = self.round_robin.entry(key).or_insert(0);
        let index = *slot % cohort.len();
        *slot = index + 1;
        index
    }

    /// The later deadline always wins, as in the store, and `set_at` advances on its own whenever the
    /// incoming rejection is the newer one. True when either moved, which is what puts the row on the writer.
    pub fn record_cooldown(&mut self, row: CooldownRow) -> bool {
        let key = (row.account_id.clone(), row.scope.clone());
        let Some(current) = self.cooldowns.get_mut(&key) else {
            self.cooldowns.insert(key, row);
            return true;
        };
        // The deadline only ever grows, but every rejection dates itself whatever deadline it announces: a
        // repeat under the standing one still moves the fence recovery has to be newer than, or a sample
        // taken between the two rejections clears a row the upstream had just set again.
        let later_deadline = row.until > current.until;
        let later_rejection = row.set_at > current.set_at;
        if later_deadline {
            let set_at = current.set_at.max(row.set_at);
            *current = row;
            current.set_at = set_at;
        } else if later_rejection {
            current.set_at = row.set_at;
        }
        later_deadline || later_rejection
    }

    pub fn clear_cooldown(&mut self, account_id: &str, scope: &str) -> bool {
        self.cooldowns
            .remove(&(account_id.to_string(), scope.to_string()))
            .is_some()
    }

    /// The live rate-limit rows on the account that `sample` shows recovered: the window the row names has
    /// allowance again, read from a sample taken after the rejection that set the row. A window the sample
    /// never read is not evidence, and neither is a row no usage window answers for. A row already past its
    /// deadline is left to the purge tick, which says so. Only an OAuth account is ever sampled, which is what
    /// keeps an api-key account's rows out of here without a registry lookup.
    pub fn recovered_cooldowns(
        &self,
        account_id: &str,
        sample: &UtilizationSample,
        now: i64,
    ) -> Vec<CooldownRow> {
        let below = |window: Option<f64>| window.is_some_and(|u| u < 1.0);
        let recovered = |row: &CooldownRow| {
            // A deadline the upstream handed out names no window, whatever the row's scope, so a reading is a
            // guess about which window the rejection was for.
            if row.window == "retry_after" {
                return false;
            }
            if row.scope.starts_with("model:") {
                return below(sample.fable);
            }
            match row.window.as_str() {
                "five_hour" => below(sample.five_hour),
                "seven_day" => below(sample.seven_day),
                // Any other representative claim: it does not say which shared window was exhausted, so the
                // account is only back once both have allowance.
                _ => below(sample.five_hour) && below(sample.seven_day),
            }
        };
        let mut rows: Vec<_> = self
            .cooldowns
            .values()
            .filter(|row| {
                row.account_id == account_id
                    && row.reason == "rate_limit"
                    && row.until > now
                    && sample.observed_at > row.set_at
                    && recovered(row)
            })
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.scope.cmp(&b.scope));
        rows
    }

    /// The live row that blocks `canonical_model` on the account, the later of the account-wide and the model row.
    pub fn cooldown(
        &self,
        account_id: &str,
        canonical_model: &str,
        now: i64,
    ) -> Option<&CooldownRow> {
        let account = self
            .cooldowns
            .get(&(account_id.to_string(), "account".to_string()))
            .filter(|c| c.until > now);
        let model = self
            .cooldowns
            .get(&(account_id.to_string(), format!("model:{canonical_model}")))
            .filter(|c| c.until > now);
        match (account, model) {
            (Some(a), Some(m)) => Some(if a.until >= m.until { a } else { m }),
            (a, m) => a.or(m),
        }
    }

    /// Every live cooldown row of one account, whatever its scope, in no particular order. `cooldown` answers
    /// the one row that blocks a model; this is for a reader asking whether anything on the account is still
    /// standing.
    pub fn cooldowns_of<'a>(
        &'a self,
        account_id: &'a str,
        now: i64,
    ) -> impl Iterator<Item = &'a CooldownRow> + 'a {
        self.cooldowns
            .values()
            .filter(move |row| row.account_id == account_id && row.until > now)
    }

    pub fn cooldowns(&self) -> Vec<CooldownRow> {
        let mut v: Vec<_> = self.cooldowns.values().cloned().collect();
        v.sort_by(|a, b| (&a.account_id, &a.scope).cmp(&(&b.account_id, &b.scope)));
        v
    }

    /// Only newer evidence replaces the current state, so a slow response cannot roll it back.
    pub fn record_overage(&mut self, account_id: &str, evidence: OverageEvidence) -> bool {
        let current = self.overage.get(account_id);
        if current.is_some_and(|c| c.observed_at >= evidence.observed_at) {
            return false;
        }
        let since = since_of(current, &evidence);
        self.overage.insert(
            account_id.to_string(),
            OverageEvidence { since, ..evidence },
        );
        true
    }

    pub fn in_overage(&self, account_id: &str) -> bool {
        self.overage.get(account_id).is_some_and(|e| e.in_use)
    }
    pub fn overage(&self, account_id: &str) -> Option<&OverageEvidence> {
        self.overage.get(account_id)
    }
    pub fn overages(&self) -> Vec<(String, OverageEvidence)> {
        let mut v: Vec<_> = self
            .overage
            .iter()
            .map(|(id, e)| (id.clone(), e.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// When the account counts as cooled under `never`: the latest reset among its exhausted shared windows (the
    /// evidence's `reset` is built that way on both paths), else `fallback_ms` (the bootstrap's
    /// `cooldown_fallback_seconds`) past the observation and then re-checked, because a permanent skip would
    /// need an operator to notice.
    pub fn overage_deadline(&self, account_id: &str, fallback_ms: i64) -> Option<i64> {
        let evidence = self.overage.get(account_id).filter(|e| e.in_use)?;
        Some(evidence.reset.unwrap_or(evidence.observed_at + fallback_ms))
    }

    pub fn record_sample(&mut self, account_id: &str, sample: UtilizationSample) -> bool {
        match self.samples.get(account_id) {
            Some(current) if current.observed_at >= sample.observed_at => false,
            _ => {
                self.samples.insert(account_id.to_string(), sample);
                true
            }
        }
    }

    pub fn sample(&self, account_id: &str) -> Option<&UtilizationSample> {
        self.samples.get(account_id)
    }

    /// Every account's latest reading, by id: one snapshot for a caller that evaluates thresholds over all of them.
    pub fn samples(&self) -> Vec<(String, UtilizationSample)> {
        let mut out: Vec<_> = self
            .samples
            .iter()
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// How long a reading stands when it dates no refund of its own: the interval the router pulls samples
    /// on, so a window read full is re-read rather than believed for ever.
    pub(crate) fn sample_ttl_ms(&self) -> i64 {
        self.sample_ttl_ms
    }

    pub(crate) fn set_sample_ttl_ms(&mut self, ms: i64) {
        self.sample_ttl_ms = ms;
    }

    /// A late failure from account A must not remove a newer pin on account B, so both have to match.
    pub fn invalidate_pin(&mut self, pin: &PinRef, account_id: &str) -> bool {
        let key = (
            pin.key_id.clone(),
            pin.session_id.clone(),
            pin.model.clone(),
        );
        let matches = self
            .pins
            .get(&key)
            .is_some_and(|p| p.epoch == pin.epoch && p.account_id == account_id);
        if matches {
            self.pins.remove(&key);
        }
        matches
    }

    pub fn release_session(
        &mut self,
        key_id: &str,
        session_id: &str,
    ) -> Vec<(String, String, String)> {
        let gone: Vec<_> = self
            .pins
            .keys()
            .filter(|(k, s, _)| k == key_id && s == session_id)
            .cloned()
            .collect();
        for key in &gone {
            self.pins.remove(key);
        }
        gone
    }

    pub fn pins(&self) -> Vec<PinRow> {
        let mut v: Vec<_> = self.pins.values().cloned().collect();
        v.sort_by(|a, b| {
            (&a.key_id, &a.session_id, &a.model).cmp(&(&b.key_id, &b.session_id, &b.model))
        });
        v
    }

    /// Expired cooldowns, pins past their pool's idle TTL, pins whose key or pool is gone, and everything
    /// held against an account the registry no longer has. An id is reusable, so state the deleted account
    /// left behind would otherwise be inherited by whatever is created under that id next: a two-day cooldown
    /// would keep a brand new account out of every pool it joins.
    pub fn purge(&mut self, snapshot: &RegistrySnapshot, now: i64) -> Purged {
        let gone = |account_id: &str| snapshot.account(account_id).is_none();
        let cooldowns: Vec<_> = self
            .cooldowns
            .iter()
            .filter(|((account_id, _), c)| c.until <= now || gone(account_id))
            .map(|(k, _)| k.clone())
            .collect();
        for key in &cooldowns {
            self.cooldowns.remove(key);
        }
        let pins: Vec<_> = self
            .pins
            .iter()
            .filter_map(|(k, p)| {
                if gone(&p.account_id) {
                    return Some((k.clone(), "deleted"));
                }
                let ttl = snapshot
                    .key(&p.key_id)
                    .and_then(|k| snapshot.pool(&k.pool_id))
                    .map(|pool| {
                        i64::try_from(pool.affinity_ttl_seconds)
                            .ok()
                            .and_then(|s| s.checked_mul(1_000))
                            .unwrap_or(i64::MAX)
                    });
                match ttl {
                    // No key or no pool: nothing serves the pin any more.
                    None => Some((k.clone(), "unserved")),
                    // A zero affinity is a TTL every pin is past: the pool keeps no session on an account.
                    Some(0) => Some((k.clone(), "expired")),
                    Some(ttl) if now - p.last_used_at > ttl => Some((k.clone(), "expired")),
                    Some(_) => None,
                }
            })
            .collect();
        for (key, _) in &pins {
            self.pins.remove(key);
        }
        // None of the three needs a deletion written: the overage row cascades with the account, and samples
        // and round-robin cursors are held in memory only.
        self.overage.retain(|account_id, _| !gone(account_id));
        self.samples.retain(|account_id, _| !gone(account_id));
        self.round_robin.retain(|(pool_id, _, cohort), _| {
            snapshot.pool(pool_id).is_some() && cohort.split(',').all(|id| !gone(id))
        });
        Purged { cooldowns, pins }
    }
}

/// The start of the current overage stretch, on first evidence and on every replacement alike: an account out of
/// overage has none, one already in it keeps the start it had, and one just entering falls back to the time the
/// evidence was observed.
fn since_of(current: Option<&OverageEvidence>, evidence: &OverageEvidence) -> Option<i64> {
    if !evidence.in_use {
        return None;
    }
    match current {
        Some(current) if current.in_use => current.since,
        _ => evidence.since.or(Some(evidence.observed_at)),
    }
}
