//! The warming's part of each request event: the arrival's guard and arming, the selection, the head, the stream's
//! usage, the completion with its credit, a compaction's own events, and a gap in the bus.

use std::collections::HashMap;

use phoebis_bus::{
    user_agent_of, AccountSelected, Outcome, RequestComplete, RequestObserved, ResponseChunk,
    ResponseHead,
};
use phoebis_ledger::Speed;
use phoebis_routing::{PinUsage, RegistrySnapshot};

use crate::sessions::state::{SessionKey, SessionsState, AGENT_HELD, MAIN_AGENT};
use crate::warming::agent::{
    warming_if_held_mut, warming_if_held_ref, warming_mut, warming_ref, AgentWarming,
};
use crate::warming::causes::{Reason, Wasted};
use crate::warming::config::ONE_HOUR_MS;
use crate::warming::guard::{class_guard, guard, Guard};
use crate::warming::idle::{continues_prefix, PrefixAnchor};
use crate::warming::observation::{
    prior_coverage, Answer, ChainMark, Credit, Observation, OpenCompaction, Unanswered,
};
use crate::warming::recipe::Recipe;
use crate::warming::savings::{AttemptFunding, RowClient};
use crate::warming::state::{Arriving, Changes, WarmingState};
use crate::warming::usage::ProbeUsage;

/// The prompt a completion reported: its uncached input, cache reads and both cache writes, `None` for an answer that
/// reported none of the four, which leaves the previous prompt in place.
pub(crate) fn reported_prompt(usage: Option<&phoebis_bus::Usage>) -> Option<u64> {
    let usage = usage?;
    let members = [
        usage.input_tokens,
        usage.cache_read_tokens,
        usage.cache_write_5m_tokens,
        usage.cache_write_1h_tokens,
    ];
    members.iter().any(Option::is_some).then(|| {
        let [input, cache_read, write_5m, write_1h] = members.map(|m| m.unwrap_or(0));
        PinUsage {
            input,
            cache_read,
            write_5m,
            write_1h,
            output: 0,
        }
        .prompt_tokens()
    })
}

impl WarmingState {
    /// Whether the agent's observation or its open compaction already follows `request_id`, so a repeated event of one
    /// request is read once.
    pub(crate) fn follows(&self, key: &SessionKey, agent_id: &str, request_id: &str) -> bool {
        let warming = warming_ref(&self.agents, key, agent_id).expect(AGENT_HELD);
        warming.current.as_ref().is_some_and(|o| o.id == request_id)
            || warming
                .compaction
                .as_ref()
                .is_some_and(|c| c.request_id == request_id)
    }

    /// The arrival's guard and what the request leaves on the agent ahead of its slot: the verdict, the chain's mark,
    /// the end of main's idle window, the resolutions, a compaction that opens, main's prefix and the coverage the
    /// arming reads. `None` where the request arms nothing: an auxiliary or a foreign request, and a compaction.
    pub(crate) fn arrival_guard(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        observed: &RequestObserved,
        resolve: impl Fn(&str) -> Option<String>,
        now: i64,
    ) -> (Changes, Option<Arriving>) {
        let mut changes = Changes::default();
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        // The hijack guard: an auxiliary or foreign request must not touch the recipe, the schedule, or the
        // unanswered probes a re-arming request's credit depends on. `reason` stays the recipe's gating
        // state, since a hold's `began` check, the completion's gate-reason match, and the view's armed-or-held
        // state all read that field as state. The agent's own `last_guard` carries the verdict itself, every
        // request's, for the view to show.
        let recipe_hashes = warming.current.as_ref().map(|observation| {
            (
                observation.recipe.static_hash.as_deref().unwrap_or(""),
                observation.recipe.tail_hash.as_deref().unwrap_or(""),
            )
        });
        let model_matches = warming
            .current
            .as_ref()
            .is_some_and(|observation| observation.recipe.model == observed.model);
        let verdict = guard(
            observed.client.request_class.as_deref(),
            observed.static_hash.as_deref(),
            &observed.boundaries,
            recipe_hashes,
            model_matches,
        );
        warming.last_guard = Some(verdict);
        // One touch for everything below: the lock is held to the end, so the flush reads the agent as this request
        // leaves it.
        changes.agent_row(key, agent_id);
        // Ahead of every refusal below, so a request whose recipe the engine refuses or cannot hold still reaches the
        // watch at its completion, and ahead of the store of this request's own prefix, since the mark reads the one
        // before it.
        if agent_id == MAIN_AGENT {
            self.mark_chain(sessions, key, observed, now);
        }
        // Ahead of the return below, so a foreign request on a new prefix ends the window too. The return is judged
        // against the prefix the window opened on, never on the verdict, which re-arms on any chain once a ceiling
        // has discarded the recipe.
        if agent_id == MAIN_AGENT && verdict != Guard::Auxiliary {
            changes.append(self.end_idle_episode(sessions, key, &observed.boundaries, now));
        }
        if matches!(verdict, Guard::Auxiliary | Guard::Foreign) {
            return (changes, None);
        }
        // A turn or a second compaction moves the agent past the context an open compaction was reading, so that one
        // ends as a failed one and leaves the recipe to this request's own path.
        self.end_compaction(sessions, key, agent_id);
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        // A tick asks the router only about the models of retained recipes and of open compactions, so a probe an
        // agent keeps past its recipe's end can miss a resolution: the catalog's acknowledged resolution dates the held
        // probes' spellings here, the request's own pair first, before the credit compares its model with the
        // request's, a compaction's as a turn's.
        let mut resolved = HashMap::new();
        if observed.wire_model != observed.canonical_model {
            resolved.insert(
                observed.wire_model.clone(),
                observed.canonical_model.clone(),
            );
        }
        for held in warming.held_models() {
            if resolved.contains_key(held) {
                continue;
            }
            if let Some(dated) = resolve(held).filter(|dated| dated != held) {
                resolved.insert(held.to_owned(), dated);
            }
        }
        warming.follow_resolutions(&resolved);
        if verdict == Guard::Compaction {
            // The recipe, its schedule and its probes wait as they stood, so a compaction that fails leaves the old
            // context the live one. Nothing of the turn's admission below runs: the idle window's prefix, the agent's
            // activity and model, and the agent cap all belong to the turn after it.
            let compaction = OpenCompaction {
                request_id: observed.request_id.clone(),
                received_at: observed.received_at,
                started: now,
                canonical_model: observed.canonical_model.clone(),
                fast: Speed::of(observed.shape.speed.as_deref()) == Speed::Fast,
                attempt: 0,
                account: None,
                funding: AttemptFunding::default(),
                ok_head: false,
                read: 0,
                ttl_ms: 0,
                prior: prior_coverage(warming, now),
                client: RowClient {
                    client: Some(observed.client.class.clone()),
                    user_agent: user_agent_of(&observed.headers),
                },
            };
            warming.compaction = Some(compaction);
            self.compactions.insert(
                observed.request_id.clone(),
                (key.clone(), agent_id.to_string()),
            );
            return (changes, None);
        }
        // Apart from the recipe, which keepalive off, an opt-out or a refused body leaves unretained: the next idle
        // window judges a return against this request's prefix either way. A request without both hashes leaves none.
        if agent_id == MAIN_AGENT {
            warming.last_prefix = observed
                .static_hash
                .clone()
                .zip(observed.boundaries.last().cloned())
                .map(|(static_hash, last)| PrefixAnchor {
                    static_hash,
                    last,
                    before_last: observed
                        .boundaries
                        .len()
                        .checked_sub(2)
                        .map(|at| observed.boundaries[at].clone()),
                    marked: observed
                        .marked
                        .iter()
                        .rev()
                        .find(|at| **at + 2 < observed.boundaries.len())
                        .and_then(|at| observed.boundaries.get(*at).cloned()),
                });
        }
        // A turn still pending here had no completion observed, since a success takes it off `pending` and a failure
        // ends the observation, and `set_aside` below ends it, or sets it aside, without one. It counts as the turn
        // that answered, or the gap this arrival's coverage reads would run from a turn before it. A compaction ends no
        // turn, so it returned above and leaves the open one to its own completion.
        if warming
            .current
            .as_ref()
            .is_some_and(|o| self.pending.contains_key(&o.id))
        {
            warming.answer_open_turn();
        }
        // Taken before anything below overwrites it and consumed with the observation.
        let prior = prior_coverage(warming, now);
        (changes, Some(Arriving { prior }))
    }

    /// Marks main's refreshing request whose chain holds none of the boundaries of main's last prefix, for its
    /// completion to read against the previous prompt. A compaction and the first request after one rewrite the context
    /// the chain hashes, so their chains move with no contradiction to read.
    fn mark_chain(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        observed: &RequestObserved,
        now: i64,
    ) {
        let refreshing = observed
            .client
            .request_class
            .as_deref()
            .is_some_and(|class| class_guard(class) == Guard::Rearm);
        if !refreshing || observed.client.context_compacted.is_some() {
            return;
        }
        let Some(warming) = warming_if_held_ref(&self.agents, sessions, key, MAIN_AGENT) else {
            return;
        };
        let (Some(anchor), Some(previous_prompt)) =
            (warming.last_prefix.as_ref(), warming.last_prompt_tokens)
        else {
            return;
        };
        if continues_prefix(Some(anchor), &observed.boundaries) {
            return;
        }
        self.contradiction_marks
            .retain(|_, mark| now - mark.arrived_at <= ONE_HOUR_MS);
        self.contradiction_marks.insert(
            observed.request_id.clone(),
            ChainMark {
                session: key.clone(),
                previous_prompt,
                arrived_at: now,
            },
        );
    }

    /// The writes of an arrival refused its slot: the agent saw a real request, on `canonical_model`.
    pub(crate) fn arrival_seen(
        &mut self,
        key: &SessionKey,
        agent_id: &str,
        canonical_model: &str,
        now: i64,
    ) {
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        warming.last_real_at = Some(now);
        warming.canonical_model = canonical_model.to_string();
    }

    /// An arrival that keeps no recipe: the one set aside goes, and the agent takes `cause`.
    pub(crate) fn arrival_refuse(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        cause: Reason,
    ) -> Changes {
        self.drop_aside(sessions, key, agent_id);
        self.set_reason(sessions, key, agent_id, cause)
    }

    /// The arrival's arming on a record the session engine moved to busy: the warming's writes of a real request, the
    /// refusals the arrival read ahead of the lock (`arrival_refusal`) and the session's own option, the bound on the
    /// open requests, the recipe with its byte bound, and the observation that follows the request.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn arrival_arm(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        observed: &RequestObserved,
        now: i64,
        arrival_refusal: Option<Reason>,
        recipe: impl FnOnce() -> Result<Recipe, Reason>,
        arriving: Arriving,
        poisoned: bool,
    ) -> Changes {
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        // The request restarts main's idle window at its own instant, which can be the instant of the prompt that set
        // the floor at the close, so the floor goes with it. A foreign request, an auxiliary one and one on an
        // `unknown` main returned above and move no start, so their floor stands.
        warming.idle_window_floor = None;
        warming.cache_started = now;
        warming.cold = 0;
        warming.failures = 0;
        warming.hold_counted = false;
        warming.last_real_at = Some(now);
        warming.canonical_model = observed.canonical_model.clone();
        warming.key_generation = observed.key_generation;
        // Past the activity writes and the slot, which such a request takes as any other does, and ahead of the build.
        // The pool's policy reads first, then the key's permission, then the session's own option. The option is read
        // again here, under the lock that admits the recipe: a registration can turn it off behind the read ahead of
        // the parse, and its drop has run by then, so a recipe admitted on that earlier read would stay with no tick to
        // end it.
        let session_off = sessions
            .entries
            .get(key)
            .is_some_and(|session| !session.cache_warming);
        if let Some(reason) = arrival_refusal.or(session_off.then_some(Reason::SessionOptedOut)) {
            return self.arrival_refuse(sessions, key, agent_id, reason);
        }
        if self.pending.len() >= sessions.max_agents {
            return self.arrival_refuse(sessions, key, agent_id, Reason::RequestLimit);
        }
        let built = recipe();
        let mut recipe = match built {
            Ok(recipe) => recipe,
            Err(reason) => return self.arrival_refuse(sessions, key, agent_id, reason),
        };
        let mut changes = Changes::default();
        // What the next observation's guard compares against: this request's own two hashes, not the ones a
        // prior arm happened to leave behind.
        recipe.static_hash = observed.static_hash.clone();
        recipe.tail_hash = observed.boundaries.last().cloned();
        // The agent's own aside recipe yields first, so the bound reads as it would with that recipe ended, and the
        // eviction never reaches it: an agent with a request open is no candidate.
        let max_retained_bytes = self.max_retained_bytes;
        if self.retained + recipe.size > max_retained_bytes {
            self.drop_aside(sessions, key, agent_id);
        }
        if self.retained + recipe.size > max_retained_bytes {
            let (evicted, room) = self.evict_for(sessions, max_retained_bytes, recipe.size);
            changes.append(evicted);
            if !room {
                changes.append(self.set_reason(sessions, key, agent_id, Reason::StorageLimit));
                return changes;
            }
        }
        self.retained += recipe.size;
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        // The probe replays this request's headers, so it meters under the entrypoint they carry.
        warming.entrypoint =
            phoebis_ledger::entrypoint_of_client(&observed.client.class).map(String::from);
        warming.current = Some(Observation {
            id: observed.request_id.clone(),
            attempt: 0,
            started: now,
            received_at: observed.received_at,
            recipe,
            ready: false,
            expected: 0,
            ttl_ms: 0,
            prior: arriving.prior,
            poisoned,
            dispatched_ms: None,
            funding: AttemptFunding::default(),
            client_label: Some(observed.client.class.clone()),
        });
        warming.body_dirty = true;
        // The mark is retired only by a save at the row's revision, so the new body takes a revision of its own.
        changes.agent_row(key, agent_id);
        self.pending.insert(
            observed.request_id.clone(),
            (key.clone(), agent_id.to_string()),
        );
        changes
    }

    /// Whether a compaction is open under `request_id`, which every request event reads before the agent it follows.
    pub(crate) fn compaction_open(&self, request_id: &str) -> bool {
        self.compactions.contains_key(request_id)
    }

    /// The agent whose observation follows `request_id`.
    pub(crate) fn follower(&self, request_id: &str) -> Option<(SessionKey, String)> {
        self.pending.get(request_id).cloned()
    }

    /// The agent holding the compaction `request_id` opened while it may still take a result, `None` for one its
    /// agent no longer holds, as for a request the engine never followed.
    fn compaction_agent(
        &mut self,
        sessions: &SessionsState,
        request_id: &str,
    ) -> Option<&mut AgentWarming> {
        let (key, agent_id) = self.compactions.get(request_id)?;
        if !sessions.live(key, agent_id) {
            return None;
        }
        Some(warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD)).filter(|warming| {
            warming
                .compaction
                .as_ref()
                .is_some_and(|compaction| compaction.request_id == request_id)
        })
    }

    /// A selection of the compaction open under the request.
    pub(crate) fn on_compaction_selected(
        &mut self,
        sessions: &SessionsState,
        selected: AccountSelected,
        funding: AttemptFunding,
    ) {
        if let Some(warming) = self.compaction_agent(sessions, &selected.request_id) {
            // A body the relay sent as it came carries the conversation's own markers, whose TTL the recipe's
            // evidence settled on.
            let ttl_ms = selected.cache_ttl_ms.unwrap_or(warming.ttl_ms);
            if let Some(compaction) = warming.compaction.as_mut() {
                compaction.attempt = selected.attempt;
                compaction.account = Some(selected.account_id);
                compaction.funding = funding;
                compaction.ok_head = false;
                compaction.read = 0;
                compaction.ttl_ms = ttl_ms;
            }
        }
    }

    /// A selection of the turn the agent's observation follows.
    pub(crate) fn on_selected(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        selected: AccountSelected,
        funding: AttemptFunding,
        snapshot: &RegistrySnapshot,
    ) -> Changes {
        let mut changes = Changes::default();
        if !sessions.live(key, agent_id) {
            return changes;
        }
        let eligible = snapshot.account(&selected.account_id).is_some();
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        let Some(observation) = warming.current.as_mut() else {
            return changes;
        };
        observation.attempt = selected.attempt;
        // A poisoned observation never becomes ready, so no evidence of this request can arm a schedule.
        observation.ready = eligible && !observation.poisoned;
        observation.expected = 0;
        observation.ttl_ms = 0;
        observation.dispatched_ms = selected.cache_ttl_ms;
        observation.funding = funding;
        warming.cache_started = observation.started;
        warming.expires = None;
        changes.append(self.set_reason(sessions, key, agent_id, Reason::AwaitingSelection));
        // The arrival reads the pool's policy and the key's opt-out, so a request refused by either never reaches here.
        if !eligible {
            changes.append(self.set_reason(sessions, key, agent_id, Reason::IneligibleCredential));
            return changes;
        }
        let pool_id = snapshot.key(&key.0).map(|row| row.pool_id.clone());
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        if warming.account.as_deref() != Some(selected.account_id.as_str()) {
            warming.cold = 0;
            warming.ttl_ms = 0;
        }
        warming.account = Some(selected.account_id);
        // The event's own incarnation, never the snapshot's above: an id paired again between the selection and this
        // delivery would label the recipe with the replacement.
        warming.account_incarnation = Some(selected.incarnation);
        // A key deleted since the arrival leaves the agent's pool as it stood, and the next tick's
        // `drop_sessions_of_dead_keys` ends the whole session under `session_ended`.
        if let Some(pool_id) = pool_id {
            warming.pool_id = pool_id;
        }
        changes.append(self.set_reason(sessions, key, agent_id, Reason::AwaitingCacheEvidence));
        changes
    }

    /// A head of the compaction open under the request.
    pub(crate) fn on_compaction_head(
        &mut self,
        sessions: &SessionsState,
        head: &ResponseHead,
        in_overage: bool,
    ) {
        if let Some(compaction) = self
            .compaction_agent(sessions, &head.request_id)
            .and_then(|warming| warming.compaction.as_mut())
            .filter(|compaction| compaction.attempt == head.attempt)
        {
            compaction.funding.in_overage = in_overage;
            compaction.ok_head = (200..300).contains(&head.status);
        }
    }

    /// A head of the turn the agent's observation follows.
    pub(crate) fn on_head(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        head: &ResponseHead,
        in_overage: bool,
    ) -> Changes {
        let mut changes = Changes::default();
        if !sessions.live(key, agent_id) {
            return changes;
        }
        let warming = warming_mut(&mut self.agents, key, agent_id).expect(AGENT_HELD);
        let Some(observation) = warming
            .current
            .as_mut()
            .filter(|o| o.attempt == head.attempt)
        else {
            return changes;
        };
        let ok = (200..300).contains(&head.status);
        let moved = observation.funding.in_overage != in_overage;
        observation.funding.in_overage = in_overage;
        // A failed attempt is not evidence about the cache, and the next selection re-arms the attempt.
        if !ok {
            observation.ready = false;
        }
        if !ok || moved {
            changes.agent_row(key, agent_id);
        }
        changes
    }

    /// A chunk's usage of the compaction open under the request.
    pub(crate) fn on_compaction_chunk(
        &mut self,
        sessions: &SessionsState,
        chunk: &ResponseChunk,
        usage: ProbeUsage,
    ) {
        // A failed attempt's stream is no evidence about the cache, as for a turn.
        if let Some(compaction) = self
            .compaction_agent(sessions, &chunk.request_id)
            .and_then(|warming| warming.compaction.as_mut())
            .filter(|compaction| compaction.attempt == chunk.attempt && compaction.ok_head)
        {
            compaction.read = compaction.read.max(usage.read);
            // The credit files under the TTL the compaction's own write would take, which its usage proves over
            // the selection's figure, as a turn's `observe_usage` settles it.
            compaction.ttl_ms = usage.ttl_ms(compaction.ttl_ms, 0);
        }
    }

    /// A chunk's usage of the turn the agent's observation follows.
    pub(crate) fn on_chunk(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        chunk: &ResponseChunk,
        usage: ProbeUsage,
    ) -> Changes {
        if !sessions.live(key, agent_id) {
            return Changes::default();
        }
        let current_attempt = warming_ref(&self.agents, key, agent_id)
            .expect(AGENT_HELD)
            .current
            .as_ref()
            .map(|o| o.attempt);
        if current_attempt != Some(chunk.attempt) {
            return Changes::default();
        }
        self.observe_usage(sessions, key, agent_id, usage)
    }

    /// The completion of an open compaction. Only a complete answer (`Outcome::Succeeded`) decides anything: every
    /// probe the recipe paid for, the kept ones and then the unanswered ones as one slice in exchange order, is judged
    /// against the compaction's own arrival, lineage and read, its kept-warm credit is its own, and the recipe stops
    /// under `compacted`, since it keeps a context the next turn never reads, so no probe goes out until that turn
    /// re-arms. Any other outcome leaves the recipe, its schedule and its probes as they stood, the old context the
    /// live one. The credit it earned comes back for the caller to book.
    pub(crate) fn complete_compaction(
        &mut self,
        sessions: &SessionsState,
        complete: &RequestComplete,
        now: i64,
    ) -> (Changes, Option<Credit>) {
        let mut changes = Changes::default();
        let Some((key, agent_id)) = self.compactions.remove(&complete.request_id) else {
            return (changes, None);
        };
        let live = sessions.live(&key, &agent_id);
        let Some(warming) = warming_if_held_mut(&mut self.agents, sessions, &key, &agent_id) else {
            return (changes, None);
        };
        let Some(compaction) = warming
            .compaction
            .take_if(|compaction| compaction.request_id == complete.request_id)
        else {
            return (changes, None);
        };
        if !live || complete.outcome != Outcome::Succeeded {
            return (changes, None);
        }
        let read = complete
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_read_tokens)
            .unwrap_or(compaction.read);
        // An answer without a stream proves its TTL here alone, as the stream's phases do in `on_chunk`.
        let ttl_ms = complete.usage.as_ref().map_or(compaction.ttl_ms, |usage| {
            ProbeUsage::from_bus(usage).ttl_ms(compaction.ttl_ms, 0)
        });
        let mut probes = std::mem::take(&mut warming.kept);
        probes.extend(warming.take_unanswered());
        let credit = compaction
            .prior
            .as_ref()
            .filter(|prior| {
                prior.credits(
                    compaction.started,
                    &compaction.canonical_model,
                    compaction.fast,
                    compaction.account.as_deref(),
                    read,
                )
            })
            .map(|_| Credit {
                at: compaction.received_at,
                key: key.clone(),
                key_generation: warming.key_generation,
                pool_id: warming.pool_id.clone(),
                account: compaction.account.clone(),
                model: compaction.canonical_model.clone(),
                read,
                ttl_ms,
                fast: compaction.fast,
                funding: compaction.funding,
                client: compaction.client.clone(),
            });
        if credit.is_some() {
            warming.count_credit(now);
        }
        changes.append(self.decide_against(
            &key,
            &agent_id,
            probes,
            Answer {
                started: Some(compaction.started),
                account: compaction.account,
                model: compaction.canonical_model,
                fast: compaction.fast,
                read,
            },
        ));
        // No label: a probe still in flight lands orphaned and unsettled, since nothing around a compaction reads as
        // wasted.
        changes.append(self.discard(sessions, &key, &agent_id, Reason::Compacted, None));
        (changes, credit)
    }

    /// The completion of the turn the agent's observation follows, with the credit it earned for the caller to book.
    pub(crate) fn on_complete(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
        complete: &RequestComplete,
        now: i64,
    ) -> (Changes, Option<Credit>) {
        let mut changes = Changes::default();
        changes.agent_row(key, agent_id);
        if agent_id == MAIN_AGENT {
            if let Some(prompt) = reported_prompt(complete.usage.as_ref()) {
                self.set_last_prompt(sessions, key, prompt);
            }
        }
        let read = complete
            .usage
            .as_ref()
            .and_then(|u| u.cache_read_tokens)
            .unwrap_or(0);
        let reason = match complete.outcome {
            Outcome::Succeeded => None,
            Outcome::Canceled => Some(Reason::RequestCanceled),
            Outcome::Failed | Outcome::Exhausted | Outcome::Incomplete => {
                Some(Reason::RequestFailed)
            }
            Outcome::Rejected => Some(Reason::RequestRejected),
        };
        if let Some(reason) = reason {
            // A failed or canceled turn leaves the last answered recipe armed as its arrival set it aside, and a
            // rejected one is a pause, which `end_observation` below makes by dropping what the arrival set aside.
            let aside = match reason {
                Reason::RequestCanceled | Reason::RequestFailed => {
                    self.take_aside(sessions, key, agent_id)
                }
                _ => None,
            };
            // A failed turn is no evidence about the cache, past a head or not, so it decides none of the probes it
            // kept and hands them back for the next turn to read. A label the turn found stays, and a probe that
            // carries none takes `expired`, which the lapse sweep settles it under when its chain's last entry ends if
            // no turn reads it first, as its recipe's own lapse would have. The sweep reads that chain through the
            // restored recipe's probes, so a kept probe whose entry they read back waits for the turn that reads them.
            if let Some(warming) = warming_if_held_mut(&mut self.agents, sessions, key, agent_id) {
                for kept in warming
                    .kept
                    .iter_mut()
                    .filter(|kept| kept.pending.is_none())
                {
                    kept.pending = Some(Wasted::Expired);
                }
            }
            // A probe sent while this request was open pays for the request after it, whatever this one did.
            changes.append(self.end_observation(sessions, key, agent_id, None, Unanswered::Leave));
            if let Some(aside) = aside {
                let rearmed = self.rearm_aside(sessions, key, agent_id, aside, now);
                changes.append(rearmed);
            }
            changes.append(self.set_reason(sessions, key, agent_id, reason));
            return (changes, None);
        }
        self.pending.remove(&complete.request_id);
        // The answered turn's own recipe replaces the one its arrival set aside.
        self.drop_aside(sessions, key, agent_id);
        // Read from the observation's own snapshot of what the previous recipe had proved: a probe that ran
        // while this request was open, or a TTL this request's evidence shortens below, cannot reach into it.
        let mut credit = {
            let warming = warming_ref(&self.agents, key, agent_id).expect(AGENT_HELD);
            warming.current.as_ref().and_then(|served| {
                let prior = served.prior.as_ref()?;
                let fast = served.recipe.fast;
                prior
                    .credits(
                        warming.last_real_at.unwrap_or(now),
                        &warming.canonical_model,
                        fast,
                        warming.account.as_deref(),
                        read,
                    )
                    .then(|| Credit {
                        // Its receipt picks the hour the credit files in and the revision its band and its saving
                        // price at, as its own request row's does, though the answer may land past either boundary.
                        at: served.received_at,
                        key: key.clone(),
                        key_generation: warming.key_generation,
                        pool_id: warming.pool_id.clone(),
                        account: warming.account.clone(),
                        model: warming.canonical_model.clone(),
                        read,
                        // Set below, once this request's own evidence has settled the TTL.
                        ttl_ms: 0,
                        fast,
                        funding: served.funding,
                        client: served.row_client(),
                    })
            })
        };
        // Set once the credit has read its gap from the turn before, so the next request's gap runs from this one.
        if (200..300).contains(&complete.status) {
            if let Some(warming) = warming_if_held_mut(&mut self.agents, sessions, key, agent_id) {
                warming.answer_open_turn();
            }
        }
        changes.append(self.decide_unanswered(sessions, key, agent_id, read));
        let (ready, armed) = warming_if_held_ref(&self.agents, sessions, key, agent_id)
            .map(|a| {
                (
                    a.current.as_ref().is_some_and(|o| o.ready),
                    a.current.as_ref().is_some_and(|o| o.expected > 0),
                )
            })
            .unwrap_or((false, false));
        if ready && !armed {
            if let Some(usage) = complete.usage.as_ref().map(ProbeUsage::from_bus) {
                changes.append(self.observe_usage(sessions, key, agent_id, usage));
            }
        }
        let (proved, observed_ttl, poisoned, current) =
            warming_if_held_ref(&self.agents, sessions, key, agent_id).map_or(
                (false, 0, false, None),
                |a| {
                    let o = a.current.as_ref();
                    (
                        o.is_some_and(|o| o.ready && o.expected > 0),
                        o.map_or(0, |o| o.ttl_ms),
                        o.is_some_and(|o| o.poisoned),
                        Some(a.reason),
                    )
                },
            );
        // The TTL this request's own evidence settled on, set at a stream's first event or, for an answer without a
        // stream, by the reading just above. The write a credited request avoided would have taken it, and a discard
        // below clears the observation.
        if let Some(credit) = credit.as_mut() {
            credit.ttl_ms = observed_ttl;
        }
        if !proved {
            if poisoned {
                // The gap that poisoned it is what this request proved nothing under.
                changes.append(self.discard(sessions, key, agent_id, Reason::BusGap, None));
            } else {
                // The credential's standing does not vary between attempts of one request, and the pool's toggle
                // reaches this arm through the tick's hold, so ending the observation must not recount a reason the
                // request already carries.
                match current {
                    Some(Reason::KeepaliveOff | Reason::IneligibleCredential) if !ready => {
                        changes.append(self.end_observation(
                            sessions,
                            key,
                            agent_id,
                            None,
                            Unanswered::Settle,
                        ));
                    }
                    _ => changes.append(self.discard(
                        sessions,
                        key,
                        agent_id,
                        Reason::NoCacheEvidence,
                        None,
                    )),
                }
            }
        }
        if credit.is_some() {
            if let Some(warming) = warming_if_held_mut(&mut self.agents, sessions, key, agent_id) {
                warming.count_credit(now);
            }
        }
        (changes, credit)
    }

    /// The mark of a request the chain watch follows, taken at its completion.
    pub(crate) fn take_chain_mark(&mut self, request_id: &str) -> Option<ChainMark> {
        self.contradiction_marks.remove(request_id)
    }

    /// Whether a contradiction under the client version `version` prints its line at `now`: one prints per client
    /// version per hour.
    pub(crate) fn note_chain_warning(&mut self, version: &str, now: i64) -> bool {
        let printed = self
            .chain_warned
            .get(version)
            .is_some_and(|at| now - at < ONE_HOUR_MS);
        if !printed {
            self.chain_warned.insert(version.to_string(), now);
        }
        !printed
    }

    /// Main's prompt as a completion reported it, which the next chain mark reads.
    pub(crate) fn set_last_prompt(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        prompt: u64,
    ) {
        if let Some(warming) = warming_if_held_mut(&mut self.agents, sessions, key, MAIN_AGENT) {
            warming.last_prompt_tokens = Some(prompt);
        }
    }

    /// Events were lost between the last one seen and this one, at `now`. Whatever was still open cannot be completed
    /// with confidence: the lost events may have been the completion, or a failover onto another account. An armed
    /// recipe's evidence was complete before the gap and stays.
    pub(crate) fn bus_gap(&mut self, sessions: &SessionsState, now: i64) -> Changes {
        let mut changes = Changes::default();
        self.bus_gaps += 1;
        self.last_bus_gap_at = Some(now);
        let open: Vec<(SessionKey, String)> = self.pending.values().cloned().collect();
        for (key, agent_id) in open {
            // The lost events may have been this turn's completion, so it counts as the turn that answered, as a
            // missed one does at the next arrival.
            if let Some(warming) = warming_if_held_mut(&mut self.agents, sessions, &key, &agent_id)
            {
                warming.answer_open_turn();
            }
            changes.append(self.end_observation(
                sessions,
                &key,
                &agent_id,
                None,
                Unanswered::Leave,
            ));
            changes.append(self.set_reason(sessions, &key, &agent_id, Reason::BusGap));
        }
        // A compaction's completion may be among the lost events too, and an open compaction holds its agent's recipe
        // and sweep until it completes, so each ends as a failed one.
        for (key, agent_id) in std::mem::take(&mut self.compactions).into_values() {
            self.end_compaction(sessions, &key, &agent_id);
        }
        changes
    }
}
