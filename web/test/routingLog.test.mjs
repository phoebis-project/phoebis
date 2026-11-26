import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { pathToFileURL } from 'node:url';

// Every clock time below is read in UTC whatever the host's zone; `node --test` gives each file a process of its own.
process.env.TZ = 'UTC';

const d = await import('./dist/api.js');

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const NOW = Date.UTC(2026, 8, 16, 14, 30);
const ms = (h, m, s = 0) => Date.UTC(2026, 8, 16, h, m, s);
const iso = (t) => new Date(t).toISOString();
const MIDNIGHT = Date.UTC(2026, 8, 17);
// The test tables size the Pro 7-day window 50 and the Ultra one 1000, so the Ultra week reads a Pro-week figure at
// 0.05 of it.
const TABLES = JSON.parse(readFileSync(new URL('./fixtures/test_tables.json', import.meta.url), 'utf8')).body.subscription_costs;
const COSTS = d.decodeSubscriptionCosts(TABLES.content, TABLES.unit);
const PRO = { failure: null, scale: d.weekScale(COSTS, 'pro'), week: 'pro' };
const ULTRA = { failure: null, scale: d.weekScale(COSTS, 'ultra'), week: 'ultra' };

const fact = (key, value, unit, entity = null) => ({ entity, key, unit, value });
const decision = (o) => ({
  type: 'decision',
  accountId: 'primary',
  agentId: 'main',
  at: iso(ms(14, 27, 28)),
  attempt: 1,
  attemptCount: 1,
  by: { kind: 'none', tier: null, value: '' },
  cache: null,
  exhaustion: null,
  fact: [],
  fast: false,
  fromAccountId: null,
  hold: null,
  keyId: 'desktop',
  kind: 'placed',
  model: 'claude-sonnet-5',
  poolId: 'all',
  probe: false,
  refusal: null,
  requestId: 'req_1d9c3e55-0b2',
  rule: null,
  rung: null,
  runnerUp: null,
  score: null,
  seq: 1,
  sessionId: '407dd39b-dd60-462e-ae66-63f766db9314',
  skipped: 0,
  status: 200,
  strategy: 'pace',
  tier: null,
  warmth: null,
  ...o,
});
const event = (kind, detail, o = {}) => ({ type: 'event', accountId: 'primary', actor: null, agentId: null, at: iso(ms(14, 0)), detail, id: 7, keyId: null, kind, model: null, poolId: null, requestId: null, sessionId: null, ...o });
const strategy = (value, tier) => ({ kind: 'strategy', tier, value });
const by = (kind, value) => ({ kind, tier: null, value });

/** The By chip and the Fact pairs as the row draws them: `label value` per pair. */
const read = (row, page = []) => ({ by: d.byOf(row, page).value, facts: d.factsOf(row, NOW, page).map((p) => `${p.label} ${p.value}`) });

/** One case per row of the decision table: the summary as the router writes it, and the words the row draws. */
const DECISIONS = [
  ['renewed, warm', decision({ kind: 'renewed', fact: [fact('idle', 161_000, 'ms')] }), 'no rule fired', ['idle 2 min 41 s']],
  ['renewed, cold', decision({ kind: 'renewed', fact: [fact('cause', 'compacted', 'text')], warmth: { cause: 'compacted', verdict: 'cold' } }), 'no rule fired', ['cause compacted']],
  ['renewed on a hot account', decision({ kind: 'renewed', fact: [fact('five_hour', 0.98, 'percent')] }), 'no rule fired', ['5h 98%']],
  ['renewed on a hot 7-day window', decision({ kind: 'renewed', fact: [fact('seven_day', 0.91, 'percent')] }), 'no rule fired', ['7d 91%']],
  ['renewed on a hot 7-day Fable window', decision({ kind: 'renewed', fact: [fact('seven_day_fable', 0.9, 'percent')] }), 'no rule fired', ['Fable 90%']],
  ['placed by the strategy', decision({ by: strategy('pace', [0, 0, 0]), fact: [fact('gap', 0.0134, 'percent_per_hour'), fact('over', 'secondary', null, { id: 'secondary', kind: 'account' })], tier: [0, 0, 0] }), 'Pace P0', ['gap 1.34 %/h', 'over secondary']],
  ['placed by round robin', decision({ by: strategy('round_robin', [0, 0, 0]), fact: [fact('turn', '2 of 4', 'text')], strategy: 'round_robin' }), 'round robin P0', ['turn 2 of 4']],
  ['placed by priority', decision({ by: strategy('priority', [0, 0, 0]), fact: [fact('rank', '1 of 2', 'text')], strategy: 'priority' }), 'priority P0', ['rank 1 of 2']],
  ['placed by its static prefix', decision({ by: by('rung', 'static'), fact: [fact('prefix', 31_200, 'tokens'), fact('depth', 2, 'count')] }), 'static rung', ['prefix 31.2K', 'depth 2']],
  ['placed by its messages', decision({ by: by('rung', 'content'), fact: [fact('prefix', 98_304, 'tokens'), fact('depth', 6, 'count')] }), 'content rung', ['prefix 98.3K', 'depth 6']],
  ['placed with the main conversation', decision({ by: by('rung', 'with_main') }), 'with main rung', []],
  ['placed after a hold', decision({ by: by('hold', 'cohort'), fact: [fact('held', 2_000, 'ms')] }), 'cohort', ['held 2.0 s']],
  ['placed on the session pin after a hold', decision({ by: by('hold', 'session_pin'), fact: [fact('in_flight', '1/1', 'text'), fact('held', 2_000, 'ms')] }), 'session pin', ['in flight 1/1', 'held 2.0 s']],
  ['placed, a failover attempt', decision({ attempt: 2, attemptCount: 3, by: by('rung', 'static'), fact: [fact('prefix', 31_200, 'tokens'), fact('depth', 2, 'count')], fromAccountId: 'secondary' }), 'static rung', ['prefix 31.2K', 'depth 2']],
  ["placed, a failover's one candidate", decision({ attempt: 2, attemptCount: 2, by: strategy('pace', [0, 1, 0]), fact: [fact('pick', 'only candidate', 'text')] }), 'Pace P1', ['pick only candidate']],
  ['placed, the last attempt', decision({ attempt: 3, attemptCount: 3, by: strategy('pace', [0, 1, 0]), fact: [fact('pick', 'last account', 'text')] }), 'Pace P1', ['pick last account']],
  ['moved to a cool destination', decision({ by: by('rule', 'spent'), fact: [fact('prefix', 31_200, 'tokens'), fact('depth', 2, 'count')], fromAccountId: 'secondary', kind: 'moved', rule: 'spent' }), 'spent rule', ['prefix 31.2K', 'depth 2']],
  ['moved, Pace among the cool', decision({ by: by('rule', 'shed'), fact: [fact('gap', 0.0134, 'percent_per_hour'), fact('over', 'nightly', null, { id: 'nightly', kind: 'account' })], fromAccountId: 'secondary', kind: 'moved' }), 'shed rule', ['gap 1.34 %/h', 'over nightly']],
  ['moved, none cool', decision({ by: by('rule', 'overage'), fact: [fact('pick', 'none cool', 'text')], kind: 'moved' }), 'overage rule', ['pick none cool']],
  ['moved back to plan quota', decision({ by: by('rule', 'return_to_plan'), fact: [fact('pick', 'only candidate', 'text')], kind: 'moved' }), 'return to plan rule', ['pick only candidate']],
  ['moved off a winding-down account', decision({ by: by('rule', 'drain'), fact: [fact('prefix', 18_400, 'tokens'), fact('depth', 1, 'count')], kind: 'moved' }), 'drain rule', ['prefix 18.4K', 'depth 1']],
  ['moved, the runway the gate read on the destination', decision({ by: by('rule', 'spent'), fact: [fact('runway', 7_800_000, 'ms'), fact('prefix', 31_200, 'tokens')], fromAccountId: 'secondary', kind: 'moved', rule: 'spent' }), 'spent rule', ['runway 2 h 10 min', 'prefix 31.2K']],
  ['moved, no destination fit', decision({ by: by('rule', 'shed'), fact: [fact('runway', 'none', 'text'), fact('fit', 'least bad', 'text')], fromAccountId: 'secondary', kind: 'moved', rule: 'shed' }), 'shed rule', ['runway none', 'fit least bad']],
  ["held at its cohort's account", decision({ by: by('hold', 'cohort'), fact: [fact('in_flight', '16/16', 'text')], kind: 'held' }), 'cohort', ['in flight 16/16']],
  ["held on the session's pin", decision({ by: by('hold', 'session_pin'), fact: [fact('in_flight', '1/1', 'text')], kind: 'held' }), 'session pin', ['in flight 1/1']],
  ["held on the strategy's pick", decision({ by: by('hold', 'strategy_pick'), fact: [fact('in_flight', '16/16', 'text')], kind: 'held', tier: [0, 0, 0] }), 'Pace P0', ['in flight 16/16']],
  ['held, the bound ran out', decision({ by: by('hold', 'cohort'), fact: [fact('answer', 429, 'count'), fact('held', 10_000, 'ms')], kind: 'held' }), 'cohort', ['answer 429', 'held 10 s']],
  ['held for the slow-down', decision({ by: by('hold', 'slow_down'), fact: [fact('until', ms(18, 20), 'time'), fact('factor', 0.5, null)], kind: 'held' }), 'slow-down', ['until 18:20', 'speed 0.5x']],
  ['held for the rewrite in flight', decision({ by: by('hold', 'rewrite'), fact: [fact('until', ms(11, 3), 'time')], kind: 'held' }), 'rewrite in flight', ['until 11:03']],
  ['renewed after the rewrite in flight', decision({ by: by('hold', 'rewrite'), kind: 'renewed' }), 'rewrite in flight', []],
  ['exhausted, a budget', decision({ accountId: null, by: by('exhaustion', 'timed'), exhaustion: { deadline: iso(MIDNIGHT), kind: 'timed', obstacle: null, reason: null, window: 'overage' }, fact: [fact('retry', MIDNIGHT, 'time')], kind: 'exhausted' }), 'budget', ['retry 00:00']],
  ['exhausted, every account cooling', decision({ accountId: null, by: by('exhaustion', 'timed'), exhaustion: { deadline: iso(ms(15, 42)), kind: 'timed', obstacle: null, reason: null, window: 'five_hour' }, fact: [fact('retry', ms(15, 42), 'time')], kind: 'exhausted' }), 'cooldown', ['retry 15:42']],
  ['exhausted, the spend cap', decision({ accountId: null, by: by('exhaustion', 'timed'), exhaustion: { deadline: iso(MIDNIGHT), kind: 'timed', obstacle: null, reason: null, window: 'spend' }, fact: [fact('retry', MIDNIGHT, 'time')], kind: 'exhausted' }), 'spend cap', ['retry 00:00']],
  ['exhausted, every account rejects the model', decision({ accountId: null, by: by('exhaustion', 'model_rejected'), exhaustion: { deadline: null, kind: 'model_rejected', obstacle: null, reason: null, window: null }, fact: [fact('retry', 'none', 'text')], kind: 'exhausted' }), 'model rejected', ['retry none']],
  ['exhausted, the fast lane', decision({ accountId: null, by: by('exhaustion', 'fast_lane'), exhaustion: { deadline: iso(ms(14, 42)), kind: 'fast_lane', obstacle: null, reason: null, window: null }, fact: [fact('retry', ms(14, 42), 'time')], kind: 'exhausted' }), 'fast lane', ['retry 14:42']],
  ['exhausted, credits', decision({ accountId: null, by: by('exhaustion', 'fast_credits'), exhaustion: { deadline: null, kind: 'fast_credits', obstacle: null, reason: 'declined', window: null }, fact: [fact('cause', 'declined', 'text')], kind: 'exhausted' }), 'credits', ['cause declined']],
  ['exhausted with nothing dated', decision({ accountId: null, by: by('exhaustion', 'untimed'), exhaustion: { deadline: null, kind: 'untimed', obstacle: null, reason: null, window: null }, fact: [fact('retry', 'none', 'text')], kind: 'exhausted' }), 'no deadline', ['retry none']],
  ['refused, client drift', decision({ accountId: null, by: by('refusal', 'client'), fact: [fact('client', '2.1.274', 'text')], kind: 'refused', refusal: { kind: 'client', reason: 'hash' } }), 'client drift', ['client 2.1.274']],
  ['refused, a key revoked after admission', decision({ accountId: null, by: by('refusal', 'key'), fact: [fact('key', 'laptop', null, { id: 'laptop', kind: 'key' })], kind: 'refused', refusal: { code: 'revoked', kind: 'key', reason: 'the key was revoked' } }), 'key revoked', ['key laptop']],
  ['probe, renewed its pin', decision({ fact: [fact('epoch', 50, 'count')], kind: 'renewed', probe: true }), 'keepalive', ['epoch 50']],
  ['probe, answered exhausted', decision({ accountId: null, by: by('exhaustion', 'timed'), exhaustion: { deadline: iso(MIDNIGHT), kind: 'timed', obstacle: null, reason: null, window: 'budget' }, fact: [fact('retry', MIDNIGHT, 'time')], kind: 'exhausted', probe: true }), 'budget', ['retry 00:00']],
];

const at = ms(14, 0);
/** The static placement the pin created below follows, in the same second, session and account. */
const placement = decision({ at: iso(at + 400), by: by('rung', 'static'), fact: [fact('prefix', 31_200, 'tokens'), fact('depth', 2, 'count')], sessionId: 's-created' });
/**
 * A failover's two decisions: the first attempt's on secondary, joined to the 429 that attempt answered, and the
 * second's on primary in the move's own second.
 */
const failover = (reason, status) => [
  decision({ accountId: 'primary', at: iso(at + 300), attempt: 2, attemptCount: 2, by: strategy('pace', [0, 0, 0]), fast: reason === 'fast_lane', fromAccountId: 'secondary', requestId: `req_${reason}`, seq: 2, sessionId: 's-moved', status: 200, tier: [0, 0, 0] }),
  decision({ accountId: 'secondary', at: iso(at - 1_200), attempt: 1, attemptCount: 2, by: strategy('pace', [0, 0, 0]), fast: reason === 'fast_lane', requestId: `req_${reason}`, seq: 1, sessionId: 's-moved', status, tier: [0, 0, 0] }),
];
/** A pool's whole row under the names it carried before the renames: each field's value, with no old one beside it. */
const WHOLE_POOL_ROW = {
  label: 'All', enabled: true, strategy: 'pace', affinity_ttl_seconds: 600, overage_policy: 'prefer_plan', shed_ahead_of_pace: 0.9, select_below_utilization: null, model_rules: [],
  body_capture_hours: 24, prefer_subscription: true, shed_share: 0.5, burn_window_seconds: 600, source_dwell_seconds: 900, migrate_when_spent: true, warm_sessions: 'when_needed',
  retry_in_place_seconds: 60, in_flight_cap: 16, pace_floor: 0.05, overage_move_grace_minutes: 30, wind_down_minutes: 30, cache_ttl: {}, cache_diagnostics: false, affinity_mode: 'always',
  subagent_placement: 'by_cohort', session_placement: 'by_strategy', min_shared_prefix_tokens: 20_000, auto_mode_server: true,
};

/** One case per row of the event table: the event as its writer stores it, and the words the row draws. */
const EVENTS = [
  ['pin created by a placement', event('pin_created', { agent_id: 'main', epoch: 52 }, { sessionId: 's-created' }), 'static rung', ['epoch 52'], [placement]],
  ['pin moved by a rule', event('pin_migrated', { agent_id: 'main', epoch: 53, from_account: 'secondary', reason: 'spent' }), 'window spent', ['epoch 53']],
  ['pin moved by the shed rule', event('pin_migrated', { agent_id: 'main', epoch: 23, from_account: 'secondary', reason: 'shed' }), 'shed', ['epoch 23']],
  ['pin moved, a failover', event('pin_migrated', { agent_id: 'main', epoch: 54, from_account: 'secondary', reason: 'cooldown' }, { sessionId: 's-moved' }), d.PIN_MIGRATION_LABEL.cooldown, ['answer 429'], failover('cooldown', 429)],
  // On the failover's page, so a row read by the word alone would draw the failover's word and its answer.
  ["pin moved, an account's state forced it", event('pin_migrated', { agent_id: 'main', epoch: 57, from_account: 'secondary', reason: 'cooldown', reasons: ['cooldown', 'overage'] }, { sessionId: 's-moved' }), 'account cooled', ['epoch 57'], failover('cooldown', 429)],
  ['pin moved, the fast lane', event('pin_migrated', { agent_id: 'main', epoch: 55, from_account: 'secondary', reason: 'fast_lane' }, { sessionId: 's-moved' }), 'fast lane cooling', ['answer 429'], failover('fast_lane', 429)],
  ['pin moved, a mark', event('pin_migrated', { agent_id: 'main', cause: { cause: 'funding_refused', state: 'funding_required' }, epoch: 56, from_account: 'secondary', reason: 'mark' }, { sessionId: 's-moved' }), d.PIN_MIGRATION_LABEL.mark, ['answer 400'], failover('mark', 400)],
  ['pin expired, found idle by the sweep', event('pin_expired', { agent_id: 'main', reasons: ['expired'] }), 'idle', []],
  ['pin expired, any other reason', event('pin_expired', { agent_id: 'main', reasons: ['cooldown', 'spent'] }), 'account cooled', []],
  ['pin released by hand', event('pin_released', { agent_id: 'main' }, { actor: 'admin' }), 'by hand', []],
  ['cooldown set, the whole account', event('cooldown_set', { reason: 'rate_limit', scope: 'account', source_request_id: 'req_1', until: ms(15, 42), window: 'five_hour' }), '429 on 5h', ['until 15:42']],
  ['cooldown set, a retry-after', event('cooldown_set', { reason: 'rate_limit', scope: 'account', until: ms(14, 1, 20), window: 'retry_after' }), '429 retry-after', ['until 14:01']],
  ["cooldown set, a retry-after inside the row's minute", event('cooldown_set', { reason: 'rate_limit', scope: 'account', until: ms(14, 0, 40), window: 'retry_after' }), '429 retry-after', []],
  ['cooldown set, a spend cap', event('cooldown_set', { reason: 'spend_cap', scope: 'account', until: ms(16, 0), window: 'overage' }), 'spend cap', ['until 16:00']],
  ['cooldown set, the fast lane', event('cooldown_set', { reason: 'fast_rate_limit', scope: 'fast', until: ms(14, 42), window: 'fast' }), 'fast lane 429', ['until 14:42']],
  ['cooldown set, a model', event('cooldown_set', { reason: 'model_rejected', scope: 'model:claude-haiku-4-5-20251001', until: ms(15, 24), window: 'model' }), '404 on haiku-4-5', ['until 15:24']],
  ["cooldown set, a permission error under a model's scope", event('cooldown_set', { reason: 'permission', scope: 'model:claude-sonnet-4-5', until: ms(15, 24), window: 'retry_after' }), '403 on sonnet-4-5', ['until 15:24']],
  ['cooldown cleared, its deadline', event('cooldown_cleared', { cause: 'expired', scope: 'account' }), 'deadline passed', []],
  ['cooldown cleared by hand', event('cooldown_cleared', { cause: 'admin', scope: 'account' }), 'by hand', []],
  ['cooldown cleared, recovered', event('cooldown_cleared', { cause: 'recovered', scope: 'account' }), 'recovered', []],
  ['overage started', event('overage_started', {}), 'in overage', []],
  ['overage ended', event('overage_ended', { since: at - (24 + 4) * HOUR }), '7d reset', ['lasted 1 d 4 h']],
  ['model rejected', event('model_rejected', { attempt: 1, request_id: 'req_011', source_request_id: 'req_1', wire_model: 'claude-haiku-4-5' }), '404', []],
  ['model permitted', event('model_permitted', { attempt: 2, rejected_by: ['primary'], request_id: 'req_012', source_request_id: 'req_1', wire_model: 'claude-haiku-4-5' }), 'after 404', ['attempt 2 of 2']],
  ['registry changed, one field', event('registry_changed', { diff: { shed_ahead_of_pace: { from: null, to: 0.9 } }, kind: 'pool_updated', target: 'all' }, { accountId: null, actor: 'admin', poolId: 'all' }), 'pool edited', ['field shed ahead of pace (removed)', 'to 90%']],
  ['registry changed, several fields', event('registry_changed', { diff: { in_flight_cap: { from: 16, to: 8 }, pace_floor: { from: 0, to: 0.1 }, strategy: { from: 'pace', to: 'priority' } }, kind: 'pool_updated', target: 'all' }, { accountId: null, actor: 'admin', poolId: 'all' }), 'pool edited', ['fields 3']],
  ['registry changed, the whole pool row', event('registry_changed', { diff: WHOLE_POOL_ROW, kind: 'pool_updated', target: 'all' }, { accountId: null, actor: 'admin', poolId: 'all' }), 'pool edited', []],
  ['registry changed, a key revoked', event('registry_changed', { diff: {}, kind: 'key_revoked', target: 'ci' }, { accountId: null, actor: 'admin', keyId: 'ci' }), 'key revoked', []],
  ['registry changed, an account edit of one field', event('registry_changed', { diff: { label: { from: 'annex', to: 'Studio annex' } }, kind: 'account_updated', target: 'annex' }, { accountId: null, actor: 'admin' }), 'account edited', ['field label', 'to Studio annex']],
  ['registry changed, a field beside a replaced secret', event('registry_changed', { diff: { label: { from: 'annex', to: 'Studio annex' }, secret_replaced: true }, kind: 'account_updated', target: 'annex' }, { accountId: null, actor: 'admin' }), 'account edited', ['field label', 'to Studio annex']],
  ['registry changed, a replaced secret alone', event('registry_changed', { diff: { secret_replaced: true }, kind: 'account_updated', target: 'annex' }, { accountId: null, actor: 'admin' }), 'account edited', ['secret replaced']],
  ['registry changed, an end', event('registry_changed', { diff: { subscription_ends_at: { from: null, to: ms(18, 0) } }, kind: 'account_updated', target: 'secondary' }, { accountId: null, actor: 'admin' }), 'account edited', ['field subscription ends at', 'to 18:00']],
  ['registry changed, a key expiry', event('registry_changed', { diff: { expires_at: { from: null, to: MIDNIGHT + 9 * 24 * HOUR } }, kind: 'key_updated', target: 'ci' }, { accountId: null, actor: 'admin' }), 'key edited', ['field expires at', 'to Sep 26 00:00']],
  ['registry changed, a list', event('registry_changed', { diff: { guardrails: { from: [], to: ['no-secrets', 'size-cap'] } }, kind: 'pool_updated', target: 'all' }, { accountId: null, actor: 'admin', poolId: 'all' }), 'pool edited', ['field guardrails', 'to no-secrets, size-cap']],
  ['registry changed, a list emptied', event('registry_changed', { diff: { guardrails: { from: ['no-secrets'], to: [] } }, kind: 'pool_updated', target: 'all' }, { accountId: null, actor: 'admin', poolId: 'all' }), 'pool edited', ['field guardrails', 'to none']],
  ['refresh succeeded', event('refresh_succeeded', { access_expires_at: ms(20, 52), refresh_expires_at: null, until: ms(20, 52) }), 'refreshed', ['until 20:52']],
  ['refresh failed', event('refresh_failed', { cause: 'invalid_grant', error: 'refresh rejected with status 400 (invalid_grant)' }), 'refresh failed', ['cause invalid_grant']],
  ['refresh uncertain', event('refresh_uncertain', { cause: 'refresh_response_lost', error: 'refresh response lost: connection reset' }), 'refresh uncertain', ['cause refresh_response_lost']],
  ['refresh succeeded, its bundle replaced since', event('refresh_succeeded', { access_expires_at: null, refresh_expires_at: null, until: null }), 'refreshed', []],
  ['ledger gap', event('ledger_gap', { reason: 'chunk_dropped', request_id: 'req_1' }, { accountId: null, requestId: 'req_1' }), 'chunk dropped', ['capture truncated']],
  ['shedding round', event('shedding_round', { marks: [{}, {}, {}], pins: [{}, {}, {}, {}], utilization: 0.92, window: 'five_hour' }), '92% on 5h', ['pins 3 of 4']],
  ['fast credits changed, blocked', event('fast_credits_changed', { after: { disabled_reason: 'org_spend_cap_reached', enabled: true, utilization: 1 }, blocked_after: 'org_spend_cap_reached', blocked_before: null }), 'spend cap reached', ['state unavailable']],
  ['fast credits changed, room again', event('fast_credits_changed', { after: { disabled_reason: null, enabled: true, utilization: 0.31 }, blocked_after: null, blocked_before: 'org_spend_cap_reached' }), 'room', ['state available']],
  ['window spent', event('window_spent', { reset: ms(15, 42), source: 'head', source_request_id: 'req_1', utilization: 1, window: 'five_hour' }), 'head on 5h', ['resets 15:42']],
  ['window spent, read by the poll', event('window_spent', { reset: ms(15, 42), source: 'poll', utilization: 1, window: 'seven_day' }), 'poll on 7d', ['resets 15:42']],
  ['window cleared', event('window_cleared', { reset: ms(15, 42), source: 'poll', source_request_id: null, utilization: 0, utilization_before: 1, window: 'five_hour' }), '5h cleared', []],
  ['window reset', event('window_reset', { next_reset: ms(19, 0), observed: 'sample', reset_at: at, utilization_before: 0.92, window: 'five_hour' }), '5h reset', ['was 92%']],
  ['router started', event('router_started', { pins_loaded: 10, version: '0.1.0' }, { accountId: null }), 'restart', ['pins 10']],
  ['warm prefix expired', event('warm_prefix_expired', { cause: 'ttl', deepest: { depth: 1, prefix_tokens: 18_400, tier: 'message', ttl_ms: 300_000 }, entries: 2, root: 's0' }), '5 min tier', ['prefix 18.4K', 'depth 1']],
  ['poll backoff started', event('poll_backoff_started', { attempts: 1, endpoint: 'usage', last_clean_poll_at: ms(13, 40), next_attempt_at: ms(13, 53), reason: 'the usage endpoint rate-limited the account' }, { at: iso(ms(13, 52, 30)) }), 'rate limited', ['next 13:53']],
  ['poll backoff ended', event('poll_backoff_ended', { attempts: 3, duration_ms: 25 * MINUTE, resumed_at: at }), 'clean poll', ['lasted 25 min']],
  ['budget reached', event('budget_reached', { budget_usd: 20, period: 'day', period_end: MIDNIGHT, spent_usd: 20.4 }, { poolId: 'jobs' }), 'daily budget', ['spent $20.40 of $20.00']],
  ['budget period ended', event('budget_period_ended', { period: 'month', period_start: at, previous_spent_usd: 18.2 }, { poolId: 'jobs' }), 'monthly budget', ['spent $18.20']],
  ['wind-down started', event('wind_down_started', { end_source: 'canceled', ends_at: ms(15, 0), wind_down_minutes: 60 }, { poolId: 'all' }), 'subscription ends', ['until 15:00']],
  ['slow-down started', event('slow_down_started', { demand: 0.12, factor: 0.5, reset: ms(18, 20), stretch_ms: 2_520_000, threshold_ms: 1_800_000 }, { poolId: 'work' }), 'slow-down', ['speed 0.5x']],
  ['slow-down ended', event('slow_down_ended', { duration_ms: 5_180_000, factor_min: 0.35, held_ms: 36_000, since: at - 5_300_000 }, { poolId: 'work' }), 'slow-down', ['slowest 0.35x']],
  ['catalog changed, a model added', event('catalog_changed', { added: ['claude-opus-5-5'], count: 12, removed: [] }), 'model added', ['models 12']],
  ['catalog changed, a model removed', event('catalog_changed', { added: [], count: 11, removed: ['claude-sonnet-4-5'] }), 'model removed', ['models 11']],
  ['catalog changed, models changed', event('catalog_changed', { added: ['claude-sonnet-5-5'], count: 12, removed: ['claude-sonnet-4-5'] }), 'models changed', ['models 12']],
  ['account state changed, a credential refused', event('account_state_changed', { cause: 'auth_failed', detail: null, from: 'ready', to: 'reauth_required' }), 'credential refused', ['state re-auth required']],
  ['account state changed, the credit balance', event('account_state_changed', { cause: 'funding_refused', detail: 'Your credit balance is too low', from: 'ready', to: 'funding_required' }), 'credit balance', ['state funding required']],
  ['account state changed, a funding retry', event('account_state_changed', { cause: 'funding_retry', detail: null, from: 'funding_required', to: 'ready' }), 'funding retried', ['state ready']],
  ['account state changed, a refresh failed', event('account_state_changed', { cause: 'refresh_failed', detail: null, from: 'ready', to: 'reauth_required' }), 'refresh failed', ['state re-auth required']],
  ['account state changed by hand', event('account_state_changed', { cause: 'operator', detail: null, from: 'funding_required', to: 'ready' }), 'by hand', ['state ready']],
  ['account state changed, paired', event('account_state_changed', { cause: 'paired', detail: null, from: 'unpaired', to: 'ready' }), 'paired', ['state ready']],
  ["account state changed, the account's last row", event('account_state_changed', { cause: 'deleted', detail: null, from: 'ready', to: 'deleted' }), 'deleted', ['state deleted']],
];

for (const [name, row, by, facts] of DECISIONS) {
  test(`a decision row reads its By and Fact: ${name}`, () => {
    assert.deepEqual(read(row), { by, facts });
  });
}

for (const [name, row, by, facts, page] of EVENTS) {
  test(`an event row reads its By and Fact: ${name}`, () => {
    assert.deepEqual(read(row, page), { by, facts });
  });
}

test("a pin reads the first placement of its second, and a failover the answer of its request's latest earlier attempt", () => {
  const pin = event('pin_created', { agent_id: 'main', epoch: 52 }, { sessionId: 's-created' });
  const later = decision({ at: iso(at + 700), by: strategy('pace', [0, 0, 0]), sessionId: 's-created' });
  assert.equal(read(pin, [placement, later]).by, 'static rung', "two placements in the pin's second, the first in the page");
  const moved = event('pin_migrated', { agent_id: 'main', epoch: 54, from_account: 'secondary', reason: 'cooldown' }, { sessionId: 's-moved' });
  const attempt = (o) => decision({ requestId: 'req_twice', sessionId: 's-moved', ...o });
  const attempts = [
    attempt({ accountId: 'secondary', at: iso(at - 2_000), seq: 1, status: 529 }),
    attempt({ accountId: 'secondary', at: iso(at - 1_000), seq: 2, status: 429 }),
    attempt({ at: iso(at + 300), seq: 3, status: 200 }),
  ];
  assert.deepEqual(read(moved, attempts).facts, ['answer 429'], "two failed attempts before the one in the move's second, the later one");
});

test("a cooldown's words read the router's spellings alone, and any other reads as sent", () => {
  assert.equal(read(event('cooldown_cleared', { cause: 'deadline', scope: 'account' })).by, 'deadline', 'the router writes a cooldown that ran out as expired');
  assert.equal(read(event('cooldown_cleared', { cause: 'operator', scope: 'account' })).by, 'operator', 'and one cleared by hand as admin');
  assert.equal(read(event('cooldown_set', { reason: 'rate_limited', scope: 'account', until: ms(15, 42), window: 'five_hour' })).by, 'rate limited', 'and a rate limit as rate_limit');
});

test("the story's funding retry reads funding retried", async () => {
  // The mock draws its story from the clock it loads at, so the case reads a copy of the bundle loaded at NOW.
  const wall = Date.now;
  Date.now = () => NOW;
  let pinned;
  try {
    pinned = await import(`./dist/api.js?at=${NOW}`);
  } finally {
    Date.now = wall;
  }
  const page = await new pinned.MockApi().listRoutingLog({ from: iso(NOW - 7 * 24 * HOUR), kinds: ['account_state_changed'], limit: 25, to: iso(NOW) });
  const retry = page.rows.find((r) => r.detail.cause === 'funding_retry');
  assert.ok(retry, 'the story holds a funding retry');
  assert.deepEqual([retry.accountId, retry.detail.from, retry.detail.to], ['overflow', 'funding_required', 'ready']);
  assert.deepEqual(read(retry, page.rows), { by: 'funding retried', facts: ['state ready'] });
});

test('a pin created with no placement beside it reads the placement word', () => {
  assert.equal(d.byOf(event('pin_created', { agent_id: 'main', epoch: 52 }, { sessionId: 's-created' }), []).value, 'placement');
});

test('a failover or fast-lane move whose failed attempt is not drawn reads no answer and never the epoch', () => {
  for (const reason of ['cooldown', 'fast_lane', 'mark']) {
    const move = event('pin_migrated', { agent_id: 'main', epoch: 54, from_account: 'secondary', reason }, { sessionId: 's-moved' });
    const [second] = failover(reason, 429);
    assert.deepEqual(read(move).facts, [], `${reason}, no rows drawn`);
    assert.deepEqual(read(move, [second]).facts, [], `${reason}, the second attempt drawn alone`);
  }
});

test('a value that names an account or a key keeps its entity, so the cell draws it as a link', () => {
  const [, over] = d.factsOf(DECISIONS[5][1], NOW);
  const [key] = d.factsOf(DECISIONS.find(([name]) => name.startsWith('refused, a key'))[1], NOW);
  assert.deepEqual(over.entity, { id: 'secondary', kind: 'account' });
  assert.deepEqual(key.entity, { id: 'laptop', kind: 'key' });
});

test('the pin reason reads not looked up and pinning off', () => {
  const pin = (pinReason) => d.pinRows(record({ pin: null, pinReason }), NOW).map((r) => [r.label, texts(r.chips)]);
  assert.deepEqual([pin('not_looked_up'), pin('pinning_off')], [[['Pin', ['not looked up']]], [['Pin', ['pinning off']]]]);
});

test('a fact never repeats the time of its row and never runs past two pairs', () => {
  const row = decision({ at: iso(ms(14, 16, 40)), fact: [fact('retry', ms(14, 16, 59), 'time'), fact('pick', 'only candidate', 'text'), fact('depth', 2, 'count'), fact('prefix', 1_000, 'tokens')] });
  assert.deepEqual(read(row).facts, ['pick only candidate', 'depth 2']);
});

test('a fact key the console has no label for is dropped rather than drawn under a guess', () => {
  assert.deepEqual(read(decision({ fact: [fact('mystery', 1, 'count'), fact('depth', 2, 'count')] })).facts, ['depth 2']);
});

test("the eligible step's none reads critical where another step's reads neutral, and a verdict the console does not know reads neutral", () => {
  assert.equal(d.verdictTone('none', 'eligible'), 'critical');
  assert.equal(d.verdictTone('none', 'pin'), 'neutral');
  assert.equal(d.verdictTone('match', 'eligible'), 'good');
  assert.equal(d.verdictTone('a_word_added_later'), 'neutral');
});

test("the spent rule's wait keeps its word, no line writes a weekly demand, and a window's word follows its noun", () => {
  assert.equal(d.WALK_KEY.wait_ms, 'wait', "the spent rule's wait keeps its word");
  assert.equal(d.WALK_KEY.demand_seven_day, undefined, 'no line writes a weekly demand');
  // A window's word follows the noun as the horizon's does.
  assert.deepEqual(['horizon', 'over_since', 'wait'].map((k) => d.keyWord(`${k}_seven_day_fable`)), ['horizon Fable', 'over since Fable', 'wait Fable']);
});

/**
 * The keys a crate's source writes on a walk line, read off every .rs file under it, its modules' directories
 * included, so the walk-key case and the regression case read one derivation. A key written over two lines counts, and
 * so does a partition's name. `src` is a directory URL ending in a slash.
 */
const walkKeysOf = (src) => {
  const written = new Set();
  for (const file of readdirSync(src, { recursive: true }).filter((f) => f.endsWith('.rs'))) {
    const text = readFileSync(new URL(file, src), 'utf8');
    for (const m of text.matchAll(/Fact::[a-z_]+\(\s*"([a-z_0-9]+)"/g)) written.add(m[1]);
    for (const m of text.matchAll(/narrow\(\s*[^)]*?"([a-z_0-9]+)"/g)) written.add(m[1]);
  }
  return written;
};

test("every walk key the router writes reads words of its own, the pick's ties as tied and its draw as drawn", () => {
  assert.deepEqual([d.keyWord('ties'), d.keyWord('drawn')], ['tied', 'drawn']);
  // The keys are read off the crate's source and the fixtures, so a key the router gains with no words of its own fails
  // here rather than reading through the underscore fallback.
  const written = walkKeysOf(new URL('../../crates/routing/src/', import.meta.url));
  assert.ok(written.size > 0, "the read finds the crate's keys");
  const fixtures = new URL('./fixtures/', import.meta.url);
  const walked = (o) => (Array.isArray(o) ? o.flatMap(walked) : o !== null && typeof o === 'object' ? [...(Array.isArray(o.walk) ? o.walk.flatMap((s) => (s.facts ?? []).map((f) => f.key)) : []), ...Object.values(o).flatMap(walked)] : []);
  for (const file of readdirSync(fixtures).filter((f) => f.endsWith('.json'))) for (const key of walked(JSON.parse(readFileSync(new URL(file, fixtures), 'utf8')))) written.add(key);
  // The account a fact names is its chip, a rung's hash is drawn from the candidate table, and a score or a standing
  // names itself.
  const drawnOtherwise = new Set(['account_id', 'hash', 'score', 'standing']);
  assert.deepEqual([...written].sort().filter((key) => !drawnOtherwise.has(key) && !Object.hasOwn(d.WALK_KEY, key)), []);
});

test("the walk-key read reaches a key written in a module's own directory", () => {
  // The routing crate keeps its modules flat, so a synthetic source with keys no file of the tree writes proves the
  // read descends into a module's directory.
  const dir = mkdtempSync(join(tmpdir(), 'walk-keys-'));
  try {
    writeFileSync(join(dir, 'lib.rs'), 'fn flat() -> Fact {\n    Fact::text("flat_probe_key", "x")\n}\n');
    mkdirSync(join(dir, 'nested'));
    writeFileSync(join(dir, 'nested', 'inner.rs'), 'fn inner() -> Fact {\n    Fact::number(\n        "nested_probe_key",\n        1.0,\n    )\n}\n');
    assert.deepEqual([...walkKeysOf(pathToFileURL(`${dir}/`))].sort(), ['flat_probe_key', 'nested_probe_key']);
  } finally {
    rmSync(dir, { force: true, recursive: true });
  }
});

test('a fact value is drawn by its unit', () => {
  const now = NOW;
  assert.equal(d.formatFact(0.0134, 'percent_per_hour', now), '1.34 %/h');
  assert.equal(d.formatFact(ms(15, 42), 'time', now), '15:42');
  assert.equal(d.formatFact(Date.UTC(2026, 8, 18, 8, 42), 'time', now), 'Sep 18 08:42');
  assert.equal(d.formatFact(161_000, 'ms', now), '2 min 41 s');
  assert.equal(d.formatFact(23 * MINUTE, 'ms', now), '23 min');
  assert.equal(d.formatFact(HOUR + 2 * MINUTE, 'ms', now), '1 h 2 min');
  assert.equal(d.formatFact(800, 'ms', now), '800 ms');
  assert.equal(d.formatFact(31_200, 'tokens', now), '31.2K');
  assert.equal(d.formatFact(20.4, 'usd', now), '$20.40');
  assert.equal(d.formatFact(0.98, 'percent', now), '98%');
  assert.equal(d.formatFact(1_234, 'count', now), '1,234');
  assert.equal(d.formatFact('16/16', 'text', now), '16/16');
  assert.equal(d.formatFact('primary', null, now), 'primary');
});

test("a window fact reads the Fable tag under the stored key's spelling, and a window the console does not know reads its key's words", () => {
  assert.equal(d.valueText('window', '7d_oi', null, NOW), 'Fable');
  assert.equal(d.valueText('window', 'seven_day_opus', null, NOW), 'seven day opus');
});

test('releaseLabel words a known reason and keeps an unknown one as sent', () => {
  assert.equal(d.releaseLabel('cooldown'), 'account cooled');
  assert.equal(d.releaseLabel('a_reason_added_later'), 'a_reason_added_later');
});

test('each entity links to the route of its kind', () => {
  assert.equal(d.entityHref('account', 'primary'), '/accounts/primary');
  assert.equal(d.entityHref('request', 'req_1d9c3e55-0b2'), '/requests/req_1d9c3e55-0b2');
  // The link opens the Sessions view on the one session, with Finished sessions on, so a finished row is not hidden.
  const session = new URL(d.entityHref('session', '407dd39b-dd60'), 'http://console.invalid');
  const address = d.SESSIONS_CODEC.fromParams(session.searchParams);
  assert.deepEqual([session.pathname, address.session, address.search, address.finished], ['/sessions', '407dd39b-dd60', '', true]);
  assert.equal(d.entityHref('pool', 'all'), '/pools#all');
  assert.equal(d.entityHref('key', 'laptop'), '/keys#laptop');
  assert.equal(d.entityHref('key', 'ci key/2'), '/keys#ci%20key%2F2');
  assert.equal(d.entityHref('model', 'claude-sonnet-5'), '/routing-log?models=claude-sonnet-5&range=24h', "a model opens the log on its rows over the last day");
});

test('each link in builds its URL with its range', () => {
  const at = ms(14, 2, 10);
  const finished = ms(14, 2, 40);
  assert.deepEqual(d.aroundRequest(at, finished, NOW), { from: at - 15 * MINUTE, to: finished + 15 * MINUTE });
  assert.deepEqual(d.aroundRequest(at, null, NOW), { from: at - 15 * MINUTE, to: at + 15 * MINUTE });
  // `to` stops at now.
  assert.deepEqual(d.aroundRequest(ms(14, 25), null, NOW), { from: ms(14, 10), to: NOW });
  assert.deepEqual(d.requestLink('req_1d9c3e55-0b2', at, finished, NOW), { from: at - 15 * MINUTE, request: 'req_1d9c3e55-0b2', to: finished + 15 * MINUTE });
  assert.deepEqual(d.aroundSubject(null, NOW), { range: '1h' });
  assert.deepEqual(d.aroundSubject(ms(11, 0), NOW), { from: ms(10, 0), to: ms(12, 0) });
  assert.deepEqual(d.aroundSubject(ms(14, 10), NOW), { from: ms(13, 10), to: NOW });
  assert.deepEqual(d.sinceCondition(ms(12, 41), NOW), { from: ms(12, 41), to: NOW });

  const href = d.logHref({ accounts: ['primary', 'secondary'], kinds: ['exhausted', 'moved'], models: ['claude-sonnet-5'], pools: ['all'], from: ms(13, 30), to: NOW });
  const url = new URL(href, 'http://phoebis.test');
  assert.equal(url.pathname, '/routing-log');
  assert.deepEqual([...url.searchParams.keys()], ['accounts', 'kinds', 'models', 'pools', 'from', 'to']);
  assert.equal(url.searchParams.get('accounts'), 'primary,secondary');
  assert.equal(url.searchParams.get('kinds'), 'exhausted,moved');
  assert.equal(url.searchParams.get('from'), '2026-09-16T13:30:00.000Z');
  assert.equal(url.searchParams.get('to'), '2026-09-16T14:30:00.000Z');
  const request = new URL(d.logHref(d.requestLink('req_1d9c3e55-0b2', at, finished, NOW)), 'http://phoebis.test').searchParams;
  assert.deepEqual(Object.fromEntries(request), { from: '2026-09-16T13:47:10.000Z', request: 'req_1d9c3e55-0b2', to: '2026-09-16T14:17:40.000Z' });
  const session = new URL(d.logHref({ session: '407dd39b', ...d.aroundSubject(ms(11, 0), NOW) }), 'http://phoebis.test').searchParams;
  assert.deepEqual(Object.fromEntries(session), { from: '2026-09-16T10:00:00.000Z', session: '407dd39b', to: '2026-09-16T12:00:00.000Z' });
});

test("logHref writes the link's agent id as agent_id beside its session", () => {
  const agent = new URL(d.logHref({ session: '407dd39b', agentId: 'c21e', ...d.aroundSubject(at, NOW) }), 'http://phoebis.test').searchParams;
  assert.deepEqual(Object.fromEntries(agent), { agent_id: 'c21e', from: '2026-09-16T13:00:00.000Z', session: '407dd39b', to: '2026-09-16T14:30:00.000Z' });
});

test('logHref writes a preset by name and a window as two instants', () => {
  assert.equal(d.logHref({ range: '24h' }), '/routing-log?range=24h');
  assert.equal(d.logHref({ range: '1h' }), '/routing-log');
  assert.equal(d.logHref({ kinds: ['held'], range: '6h' }), '/routing-log?kinds=held&range=6h');
  const instants = new URL(d.logHref({ from: ms(13, 30) + 123, to: ms(14, 29, 59) + 999 }), 'http://phoebis.test').searchParams;
  assert.deepEqual(Object.fromEntries(instants), { from: '2026-09-16T13:30:00.123Z', to: '2026-09-16T14:29:59.999Z' });
});

test("the log's address keeps a kind this build does not know, so the route's refusal reaches the page", () => {
  const filters = d.ROUTING_LOG_CODEC.fromParams(new URLSearchParams('kinds=placed,budget_refilled&accounts=primary'));
  assert.deepEqual([filters.kinds, filters.accounts], [['placed', 'budget_refilled'], ['primary']]);
  assert.equal(d.ROUTING_LOG_CODEC.toParams(filters).get('kinds'), 'placed,budget_refilled');
});

test('a kind named after an Object.prototype member reads the fallback word', () => {
  const filters = d.ROUTING_LOG_CODEC.fromParams(new URLSearchParams('kinds=constructor'));
  assert.deepEqual(d.kindsChipText(filters.kinds), { text: 'kinds: constructor', title: 'constructor', values: 'constructor' });
});

test('this minute is the clock minute that holds the moment, its last millisecond the end, never past now', () => {
  assert.deepEqual(d.clockMinute(ms(14, 2, 10), NOW), { from: ms(14, 2), to: ms(14, 3) - 1 });
  assert.deepEqual(d.clockMinute(ms(14, 29, 10), ms(14, 29, 30)), { from: ms(14, 29), to: ms(14, 29, 30) });
});

/** `count` decisions a second apart, newest first, and the route over them: pages of 25, the cursor a row index. */
function arrivals(count) {
  const rows = Array.from({ length: count }, (_, i) => decision({ at: iso(ms(14, 29) - i * 1_000), requestId: `req_${String(i).padStart(3, '0')}` }));
  const reads = [];
  const read = async (cursor) => {
    reads.push(cursor);
    const start = cursor === undefined ? 0 : Number(cursor);
    return { counts: null, nextCursor: start + 25 < rows.length ? String(start + 25) : null, rows: rows.slice(start, start + 25) };
  };
  return { read, reads, rows };
}

test('an auto reload poll reads every page that arrived down to a row the list holds', async () => {
  // The list holds the oldest 25, and 100 more arrived before the poll.
  const route = arrivals(125);
  const shown = route.rows.slice(100);
  const polled = await d.followPages(route.read, new Set(shown.map(d.identityOf)), d.identityOf, (r) => d.compareRows(r, shown.at(-1)) > 0);
  assert.deepEqual(route.reads, [undefined, '25', '50', '75', '100'], 'the fifth page reaches the rows the list holds');
  assert.equal(polled.overrun, null);
  assert.deepEqual(d.merged(shown, polled.rows, d.identityOf, d.compareRows).map(d.identityOf), route.rows.map(d.identityOf), 'every row once, in the route order');
});

test('an auto reload poll stops at eight pages and names the cursor that reaches the rows it left', async () => {
  // 300 arrived over the 25 the list holds: eight pages bring the newest 200.
  const route = arrivals(325);
  const shown = route.rows.slice(300);
  const polled = await d.followPages(route.read, new Set(shown.map(d.identityOf)), d.identityOf, (r) => d.compareRows(r, shown.at(-1)) > 0);
  assert.equal(route.reads.length, 8);
  assert.deepEqual(polled.rows.map(d.identityOf), route.rows.slice(0, 200).map(d.identityOf));
  assert.equal(polled.overrun, '200');
  const gap = await route.read(polled.overrun);
  assert.equal(d.identityOf(gap.rows[0]), d.identityOf(route.rows[200]), 'the overrun cursor reads the first row the poll left');
});

// The tier order a catalog change lists its models in, and the empty pair every other event's detail is read under,
// since none of them reads the tiers.
const SHIPPED_TIERS = { modelTier: {}, tierOrder: ['fable', 'opus', 'sonnet', 'haiku'] };
const NO_TIERS = { modelTier: {}, tierOrder: [] };

// The decision card's half of the vocabulary: the record's walk, candidates, folds and outcome, and the event detail.

const HASH_A = 'b516991a19001eb9a6a8ed4038c4fba86a7dda733fe0815bb091af24559b7cc8';
const HASH_B = '346174344a41e85040e356784130327108bd8dd30f611d806bd1ea14f02a3878';
const HASH_C = '69468b2ca1e7947c6a628ae77f451b737d2b4fcb14728d917c707b8633f8a2a3';
const chipText = (c) => (c.label ? `${c.label} ${c.value}` : c.value);
const texts = (chips) => chips.map(chipText);
const wfact = (key, value, unit = null, accountId = null) => ({ accountId, key, unit, value });
const wstep = (step, verdict, facts = []) => ({ facts, step, verdict });
const entry = (o) => ({ depth: 2, hash: HASH_B, live: true, prefixTokens: 31_200, refusedBy: null, ttlMs: 3_600_000, writtenAt: iso(ms(13, 58, 2)), ...o });
const NO_PREFIX = { content: null, contentMatch: null, static: null, staticMatch: null, withMain: false };
const window5h = (utilization, reset, source = 'head', ageMs = 4_000) => ({ ageMs, ahead: null, observedAt: iso(ms(14, 27, 24)), reset: reset === null ? null : iso(reset), source, utilization });
const candidate = (o) => ({
  accountId: 'primary',
  allowanceKnown: true,
  budget: [],
  burn: {},
  cooldowns: [],
  credits: null,
  enabled: true,
  factor: null,
  inFlight: 3,
  inFlightCap: 16,
  inOverage: false,
  incarnation: 1,
  kind: 'oauth',
  obstacle: null,
  overage: null,
  pace: { burn: null, deadline: null, end: null, fableReservation: null, hours: 120, remaining: 0.91, reset: null, score: 0.0076, window: null },
  partition: null,
  planTier: 'default_claude_max_20x',
  prefix: NO_PREFIX,
  priority: 0,
  rank: null,
  score: null,
  state: 'ready',
  term: null,
  tier: [0, 0, 0],
  verdict: 'eligible',
  windows: { five_hour: window5h(0.62, ms(16, 44)), seven_day: window5h(0.41, Date.UTC(2026, 8, 19, 20, 10)), seven_day_fable: window5h(0.12, Date.UTC(2026, 8, 19, 20, 10)) },
  ...o,
});
const completion = (o) => ({ cacheMapWritten: null, cacheReadTokens: null, cacheWriteTokens: null, completedAt: null, freed: false, heldMs: null, hit: null, inputTokens: null, inputTransformations: null, promisedTokens: null, status: null, ...o });
const SETTINGS = {
  affinityMode: 'when_cached', affinityTtlSeconds: 600, burnAverageSeconds: 1_800, fableReservationShedding: true, fiveHourCeiling: 0.95, inFlightCap: 16, legacy: null, loadShedding: true, migrateWhenSpent: true, minSharedPrefixTokens: 20_000,
  overageMoveGraceMinutes: 30, overagePolicy: 'prefer_plan', poolUpdatedAt: iso(ms(9, 5, 12)), preferSubscription: true, retryInPlaceSeconds: 60, returnToPlan: 'when_cold', returnToPlanDwellSeconds: 900,
  sessionPlacement: 'by_warm_prefix', shedCap: 0.5, shedDwellSeconds: 900, slowDown: true, slowDownHoldSeconds: 120, slowDownStretchSeconds: 1_800, strategy: 'pace', subagentPlacement: 'by_cohort', warmSessions: 'when_needed',
  weeklyCeiling: 1, windDownMinutes: 30,
};
/** The five settings a record written before the slow-down and the return mode carries no value for. */
const BEFORE_SLOW_DOWN = { returnToPlan: null, returnToPlanDwellSeconds: null, slowDown: null, slowDownHoldSeconds: null, slowDownStretchSeconds: null };
/**
 * The settings a version 1 record read: the three renamed under their new members, no switch, no ceilings, and the
 * three removed.
 */
const LEGACY_SETTINGS = { ...SETTINGS, ...BEFORE_SLOW_DOWN, fiveHourCeiling: null, legacy: { paceFloor: 0.05, selectBelowUtilization: null, shedAheadOfPace: 0.9 }, loadShedding: null, weeklyCeiling: null };
const record = (o = {}) => ({
  candidates: [],
  outcome: {
    accountId: 'primary', cohortHold: null, completion: null, exhaustion: null, fromAccountId: null, hold: null, inOverage: false, kind: 'placed', markCleared: null, pinEvents: [], pinWritten: null, refusal: null,
    reservation: null, rule: null, rung: null, shedMarks: null, strategy: 'pace', tier: null, upstreamModel: 'claude-sonnet-5-5', ...o.outcome,
  },
  pin: null,
  pinReason: 'pinning_off',
  raw: null,
  request: {
    agentId: 'main', at: iso(ms(14, 27, 28)), attempt: 1, attempted: [], canonicalModel: 'claude-sonnet-5-5', chain: { boundaries: 6, staticBoundaries: 2, staticHash: true }, clientClass: 'claude_code',
    flags: { contextCompacted: false, count: false, fast: false, marker: true, portable: true, probe: false }, holds: [], keyGeneration: 1,
    keyId: 'desktop', lane: { effort: 'high', speed: 'standard', thinking: 'enabled' }, parentAgentId: null, poolId: 'all', registryRevision: 412, requestClass: 'main', requestId: 'req_f8085653-422',
    seq: 1, sessionId: '7bea4514-0000-4000-8000-000000000000', ...o.request,
  },
  requestId: 'req_f8085653-422',
  seq: 1,
  settings: SETTINGS,
  version: 2,
  walk: [],
  ...o,
  ...(o.outcome ? { outcome: { ...record().outcome, ...o.outcome } } : {}),
  ...(o.request ? { request: { ...record().request, ...o.request } } : {}),
});

test("a lead's line names the end its pace runs to beside the elapsed share, and a pace to the reset names none", () => {
  const r = record();
  const at = Date.parse(r.request.at);
  const end = at + 30 * 3_600_000;
  const shed = wstep('move:shed', 'no', [wfact('ahead_five_hour', 0.1, 'percent'), wfact('elapsed_five_hour', 0, 'percent'), wfact('ahead_seven_day', 0, 'percent'), wfact('elapsed_seven_day', 0.5, 'percent'), wfact('deadline_seven_day', end, 'time'), wfact('threshold', 0.2, 'percent')]);
  const cool = wstep('cool', 'preferred', [wfact('elapsed_five_hour', 0.8, 'percent', 'secondary'), wfact('deadline_five_hour', end, 'time', 'secondary')]);
  const [shedLine, coolLine] = d.walkLines({ ...r, walk: [shed, cool] });
  assert.deepEqual(shedLine.chips.map((c) => c.label), ['5h ahead', '5h elapsed', '7d ahead', '7d elapsed', "7d to its subscription's end at", 'threshold']);
  assert.equal(shedLine.chips[4].value, d.formatFact(end, 'time', at), "the end in the time's words");
  assert.equal(coolLine.chips[1].value, `5h to its subscription's end at ${d.formatFact(end, 'time', at)}`, 'a fact about a member names it as the key');
  const plain = d.walkLines({ ...r, walk: [wstep('move:shed', 'no', shed.facts.filter((f) => f.key !== 'deadline_seven_day'))] });
  assert.ok(plain[0].chips.every((c) => !c.label.includes('to its end')), 'a pace to the reset draws no end');
});

test('the walk collapses the rules after a fired one and marks the rung lines with their entry', () => {
  const r = record({
    candidates: [
      candidate({ prefix: { ...NO_PREFIX, content: entry({ depth: 4, hash: HASH_C, refusedBy: 'lane' }), static: entry({ depth: 3, hash: HASH_A, refusedBy: 'expired' }), staticMatch: entry({}) } }),
      candidate({ accountId: 'secondary', prefix: { ...NO_PREFIX, content: entry({ depth: 6, hash: HASH_A, refusedBy: 'at_cap' }) } }),
    ],
    walk: [
      wstep('pin', 'match', [wfact('found', 'live'), wfact('account_id', 'secondary', null, 'secondary'), wfact('idle_ms', 48_000, 'ms'), wfact('cause', 'continues_pin'), wfact('marked', false)]),
      wstep('eligible', 'match', [wfact('standing', 'eligible', null, 'primary'), wfact('standing', 'cooldown', null, 'secondary')]),
      wstep('move:spent', 'fired', [wfact('holds_allowance', false), wfact('destinations', ['primary'])]),
      wstep('move:overage', 'not_run'),
      wstep('move:return_to_plan', 'not_run'),
      wstep('move:drain', 'not_run'),
      wstep('move:shed', 'not_run'),
      wstep('cool', 'preferred', [wfact('band', 0.85, 'percent', 'primary'), wfact('in_flight', '4/16', 'count', 'primary'), wfact('cool', true, null, 'primary')]),
      wstep('cap_filter', 'passed', [wfact('in_flight', '4/16', 'count', 'primary')]),
      wstep('rung:content', 'none', [wfact('reason', 'no_entry')]),
      wstep('rung:with_main', 'none', [wfact('reason', 'class')]),
      wstep('rung:static', 'match', [wfact('depth', 2, 'count'), wfact('prefix_tokens', 31_200, 'tokens'), wfact('account_id', 'primary', null, 'primary'), wfact('hash', HASH_B)]),
    ],
  });
  const lines = d.walkLines(r);
  assert.deepEqual(lines.map((l) => l.step), ['Pin', 'Eligible', 'Spent', 'Rules after spent', 'Cool reading', 'Cap filter', 'Rung: content', 'Rung: with main', 'Rung: static']);
  assert.deepEqual(lines.map((l) => l.number), [1, 2, 3, 4, 5, 6, 7, 8, 9]);
  const rules = lines[3];
  assert.equal(rules.verdict, 'not_run');
  assert.deepEqual(texts(rules.chips), ['overage', 'return to plan', 'drain', 'shed']);
  assert.deepEqual(rules.collapsed, ['move:overage', 'move:return_to_plan', 'move:drain', 'move:shed']);
  assert.deepEqual(texts(lines[0].chips), ['found live', 'on secondary', 'idle 48 s', 'cause continues the pin', 'marked no']);
  assert.deepEqual(lines[0].chips[1].entity, { id: 'secondary', kind: 'account' });
  assert.deepEqual(texts(lines[1].chips), ['primary eligible', 'secondary cooldown']);
  assert.deepEqual(lines[1].chips[0].labelEntity, { id: 'primary', kind: 'account' });
  assert.deepEqual(texts(lines[2].chips), ['holds allowance no', 'destination primary']);
  assert.deepEqual(lines[2].chips[1].entity, { id: 'primary', kind: 'account' });
  assert.deepEqual(texts(lines[4].chips), ['primary band 85%', 'primary in flight 4/16', 'primary cool yes']);
  // Only the rung lines carry an entry: the deepest refused one on a line that matched nothing, the taken one on a
  // match.
  assert.deepEqual(lines.slice(0, 6).map((l) => l.hash), [undefined, undefined, undefined, undefined, undefined, undefined]);
  assert.equal(lines[6].hash.hash, HASH_A);
  assert.equal(lines[6].hash.refusedBy, 'at_cap');
  assert.equal(lines[7].hash, null);
  assert.equal(lines[8].hash.hash, HASH_B);
  assert.deepEqual(texts(lines[8].chips), ['depth 2', 'prefix 31.2K', 'on primary']);
  assert.ok(!lines.some((l) => l.chips.some((c) => c.value.length >= 64)), 'no chip draws a whole hash');
});

test("the pin step's credit fact names its account once, as an account chip labeled credit issuer", () => {
  // The router writes the fact about the account it names, and an id with an underscore keeps its spelling.
  const r = record({ walk: [wstep('pin', 'match', [wfact('found', 'live'), wfact('credit', 'team_seat', 'text', 'team_seat')])] });
  const [pin] = d.walkLines(r);
  assert.deepEqual(pin.chips.slice(1), [{ entity: { id: 'team_seat', kind: 'account' }, label: 'credit issuer', value: 'team_seat' }]);
});

test("a classifier call's Pin fold reads the turn it judged, that turn's account or none, and where the call went", () => {
  // The router writes every fact of the step as text, the judged account about itself, and no turn in a session that
  // holds none. A call a fallback credit sends to its issuer names the issuer and no direction. The account a fact is
  // about tells an account from the word, so an account whose id is the text `none` reads as an account, as the strip
  // reads it.
  const call = (facts) => record({ pin: null, pinReason: 'classifier', walk: [wstep('pin', 'none', [wfact('reason', 'classifier', 'text'), ...facts])] });
  const fold = (r) => d.pinRows(r, NOW).map((row) => [row.label, row.chips]);
  const turn = [wfact('judged_agent', 'a5e1', 'text'), wfact('judged_model', 'claude-haiku-4-5-20251001', 'text'), wfact('judged', 'primary', 'text', 'primary')];
  const followed = call([...turn, wfact('direction', 'turn_account', 'text')]);
  const unjudged = call([wfact('judged', 'none', 'text'), wfact('direction', 'strategy', 'text')]);
  const namedNone = call([wfact('judged', 'none', 'text', 'none'), wfact('direction', 'strategy', 'text')]);
  const credited = call([wfact('credit', 'secondary', 'text', 'secondary'), ...turn]);
  const pin = { value: 'classifier call without a pin' };
  assert.deepEqual(
    {
      credited: fold(credited).find(([label]) => label === 'Sent to'),
      followed: fold(followed),
      namedNone: fold(namedNone).find(([label]) => label === 'Judged account'),
      unjudged: fold(unjudged),
      unjudgedChips: d.factChips(unjudged.walk[0].facts[1], unjudged, NOW),
    },
    {
      credited: ['Sent to', [{ entity: { id: 'secondary', kind: 'account' }, label: 'credit issuer', value: 'secondary' }]],
      followed: [
        ['Pin', [pin]],
        ['Judged turn', [{ value: 'a5e1' }, { entity: { id: 'claude-haiku-4-5-20251001', kind: 'model' }, mono: true, value: 'claude-haiku-4-5-20251001' }]],
        ['Judged account', [{ entity: { id: 'primary', kind: 'account' }, value: 'primary' }]],
        ['Sent to', [{ value: "the judged turn's account" }]],
      ],
      namedNone: ['Judged account', [{ entity: { id: 'none', kind: 'account' }, value: 'none' }]],
      unjudged: [['Pin', [pin]], ['Judged account', [{ value: 'none' }]], ['Sent to', [{ value: 'placed by the strategy' }]]],
      unjudgedChips: [{ label: 'judged account', value: 'none' }],
    },
  );
});

test("a refusal's walk is two lines, the admission's reason and the rest of the walk not run", () => {
  const r = record({ outcome: { accountId: null, kind: 'refused', refusal: { kind: 'client', reason: 'hash' }, upstreamModel: null }, pin: null, pinReason: 'not_looked_up', walk: [wstep('admission', 'refused', [wfact('reason', 'hash')]), wstep('selection', 'not_run')] });
  // The version rides the summary's `client` fact, so the reason names the trigger alone.
  const lines = d.walkLines(r, decision({ accountId: null, by: by('refusal', 'client'), fact: [fact('client', '2.1.274', 'text')], kind: 'refused', refusal: { kind: 'client', reason: 'hash' } }));
  assert.deepEqual(lines.map((l) => [l.number, l.step, l.verdict, texts(l.chips)]), [
    [1, 'Admission', 'refused', ['drift hash', 'client 2.1.274']],
    [2, 'Rest of the walk', 'not_run', ['moves', 'rungs', 'partitions']],
  ]);
  assert.equal(d.verdictWord('not_run'), 'not run');
  assert.equal(d.verdictWord('dropped'), 'all at cap');
});

test('the pick line and the Pick station name the tied accounts, the one the runway kept and whether a draw was left', () => {
  const tied = { pace: { burn: 0, deadline: null, end: null, fableReservation: null, hours: 120, remaining: 0.91, reset: null, score: 0.0076, window: 'seven_day' }, rank: 1, score: 0.0076 };
  const r = record({
    candidates: [candidate({ ...tied, runwayMs: 3_600_000 }), candidate({ ...tied, accountId: 'secondary', rank: 2, runwayMs: 1_800_000 })],
    walk: [
      wstep('pin', 'none', [wfact('found', 'none')]),
      wstep('eligible', 'match', [wfact('standing', 'eligible', null, 'primary'), wfact('standing', 'eligible', null, 'secondary')]),
      wstep('partitions', 'match', [wfact('spent', 'off'), wfact('at_cap', 0), wfact('no_fit', 0), wfact('winding_down', 0)]),
      wstep('rung:content', 'none', [wfact('reason', 'no_entry')]),
      wstep('rung:with_main', 'none', [wfact('reason', 'class')]),
      wstep('rung:static', 'none', [wfact('reason', 'no_entry')]),
      wstep('pick', 'chosen', [
        wfact('strategy', 'pace'), wfact('tier', '0/0/0'),
        wfact('score', 0.0076, 'percent_per_hour', 'primary'), wfact('burn', 0, 'percent_per_hour', 'primary'),
        wfact('score', 0.0076, 'percent_per_hour', 'secondary'), wfact('burn', 0, 'percent_per_hour', 'secondary'),
        wfact('ties', ['primary', 'secondary']), wfact('by_runway', ['primary']), wfact('drawn', false),
      ]),
      wstep('reserve', 'none', [wfact('account_id', 'primary', null, 'primary')]),
    ],
  });
  const line = d.walkLines(r).find((l) => l.step === 'Pick');
  // Each tied id is a chip of its own under the label, an account entity as every account a line names.
  assert.deepEqual(texts(line.chips).slice(-4), ['tied primary', 'tied secondary', 'by runway primary', 'drawn no']);
  assert.deepEqual(line.chips.slice(-4, -1).map((c) => c.entity), [{ id: 'primary', kind: 'account' }, { id: 'secondary', kind: 'account' }, { id: 'primary', kind: 'account' }]);
  const plan = d.anchorPlan({ id: 'pick', kind: 'station' }, d.stationModel(r, NOW), r, undefined, NOW);
  const rows = Object.fromEntries(plan.block.table.loose.map((row) => [row.label, row.chips.map(chipText)]));
  assert.deepEqual([rows.tied, rows['by runway'], rows.drawn], [['tied primary', 'tied secondary'], ['by runway primary'], ['drawn no']]);
});

test('the candidate cells word every verdict and its chip', () => {
  const cell = (o, r = record()) => d.candidateCells(candidate(o), r, NOW);
  const verdict = (o, r) => {
    const v = cell(o, r).verdict;
    return [v.badge, v.chip === null ? null : v.chip.value, v.chip?.title ?? null];
  };
  const chosen = cell({ verdict: 'chosen' });
  assert.deepEqual(chosen.account, { entity: { id: 'primary', kind: 'account' }, value: 'primary' });
  assert.equal(chosen.plan, 'Ultra');
  // A record carries the plan word the router resolved, and a record from before it the tier word it was written with.
  assert.equal(cell({ planTier: 'max_20x' }).plan, 'Ultra', 'the resolved word through the label table');
  assert.equal(cell({ planTier: 'free' }).plan, 'Free');
  assert.equal(cell({ planTier: 'usage_based' }).plan, 'Usage-based');
  assert.equal(cell({ planTier: 'default_claude_max_20x' }).plan, 'Ultra', "an older record's tier word through its tier label");
  assert.deepEqual(chosen.windows, [['62%', '16:44', 'head 4 s'], ['41%', 'Sep 19 20:10', 'head 4 s'], ['12%', 'Sep 19 20:10', 'head 4 s']]);
  assert.equal(chosen.inFlight, '3 of 16');
  assert.equal(chosen.pace, '0.76 %/h');
  assert.deepEqual(chosen.verdict, { badge: 'chosen', badgeTone: 'accent', chip: null, chosen: true, tone: null });
  assert.deepEqual(cell({ windows: { five_hour: window5h(1, ms(15, 42), 'poll', 120_000), seven_day: window5h(0.88, null, 'poll', 120_000) } }).windows, [['100%', '15:42', 'poll 2 min'], ['88%', 'no reset', 'poll 2 min'], ['not reported']]);
  const key = cell({ accountId: 'overflow', kind: 'api_key', pace: { deadline: null, end: null, fableReservation: null, hours: null, remaining: 0.96, reset: null, score: 0.96, window: null }, planTier: null, priority: 1, tier: [2, 1, 0], windows: { worst_bucket: window5h(0.04, ms(14, 28, 8)) } });
  assert.deepEqual([key.plan, key.tier, key.windows, key.pace], ['API key', 'paid P1', [['worst bucket 4%']], '96% left']);
  assert.equal(cell({ inOverage: true, tier: [1, 0, 1] }).tier, 'overage P0');
  assert.equal(cell({ pace: null }).pace, '-');
  const until = (h, m) => iso(ms(h, m));
  assert.deepEqual(verdict({ obstacle: { name: 'cooldown', reason: 'rate_limit', scope: 'account', until: until(15, 42), window: 'five_hour' }, verdict: 'out' }), ['out', '429 on 5h until 15:42', 'in a cooldown']);
  assert.deepEqual(verdict({ obstacle: { name: 'cooldown', reason: 'rate_limited', scope: 'account', until: until(15, 42), window: 'five_hour' }, verdict: 'out' }), ['out', 'rate limited until 15:42', 'in a cooldown'], 'the router spells its rate limit rate_limit alone');
  // The scope names what the cooled words leave out, the model a cooldown holds. The fast lane's two rows name the lane
  // and its credits in their own words, whatever window the router filed them under.
  assert.deepEqual(verdict({ obstacle: { name: 'cooldown', reason: 'model_rejected', scope: 'model:claude-opus-5', until: until(15, 24), window: 'model_rejection' }, verdict: 'out' }), ['out', 'model 404 until 15:24 for claude-opus-5', 'in a cooldown']);
  assert.deepEqual(verdict({ obstacle: { name: 'cooldown', reason: 'fast_rate_limit', scope: 'fast', until: until(14, 42), window: 'retry_after' }, verdict: 'out' }), ['out', 'fast lane 429 until 14:42', 'in a cooldown']);
  assert.deepEqual(verdict({ obstacle: { name: 'cooldown', reason: 'fast_credits', scope: 'fast_credits', until: until(15, 42), window: 'org_spend_cap_reached' }, verdict: 'out' }), ['out', 'fast credits until 15:42', 'in a cooldown']);
  const operator = { name: 'operator', reason: null, until: null, window: null };
  assert.deepEqual(verdict({ enabled: false, obstacle: operator, verdict: 'out' }), ['out', 'held by hand', 'held by an operator']);
  assert.deepEqual(verdict({ obstacle: operator, state: 'funding_required', verdict: 'out' }), ['out', 'needs funding', 'the account leaves every eligible set until funded']);
  assert.deepEqual(verdict({ obstacle: operator, state: 'reauth_required', verdict: 'out' }), ['out', 'needs re-auth', 'the account leaves every eligible set until it is paired again']);
  assert.deepEqual(verdict({ enabled: false, obstacle: operator, state: 'deleted', verdict: 'out' }), ['out', 'deleted', 'the account left the registry']);
  // An exhausted line's chip reads its member's row where the row's own obstacle carries the chip's name, an operator
  // chip the member's state whatever the row carries, and any other chip the partition's word or the obstacle's label.
  // The router names a member's own obstacle, or `spent` over a row with none where a plan window or a model's week
  // passed the member over, so the fifth and sixth entries hold the rule's halves on records it does not write.
  const capped = { name: 'spent', reason: 'spent', scope: null, until: until(15, 42), window: 'spend' };
  const cooled = { name: 'cooldown', reason: 'rate_limit', scope: 'account', until: until(15, 42), window: 'five_hour' };
  const held = record({
    candidates: [
      candidate({ accountId: 'overflow', obstacle: operator, state: 'funding_required', verdict: 'out' }),
      candidate({ accountId: 'nightly', enabled: false, obstacle: operator, verdict: 'out' }),
      candidate({ accountId: 'contract', obstacle: capped, verdict: 'out' }),
      candidate({ accountId: 'secondary', obstacle: cooled, verdict: 'out' }),
      candidate({ accountId: 'reserve', obstacle: cooled, state: 'funding_required', verdict: 'out' }),
      candidate({ accountId: 'studio', verdict: 'out' }),
    ],
    outcome: { accountId: null, exhaustion: { deadline: null, kind: 'untimed', obstacle: null, reason: null, window: null }, kind: 'exhausted', upstreamModel: null },
    walk: [wstep('exhausted', 'exhausted', [wfact('obstacles', ['overflow:operator', 'nightly:operator', 'contract:spent', 'secondary:cooldown', 'contract:cooldown', 'reserve:operator', 'studio:spent'])])],
  });
  const exhausted = d.walkLines(held)[0].chips;
  assert.deepEqual(texts(exhausted), ['overflow needs funding', 'nightly held by hand', 'contract spend cap reached until 15:42', 'secondary 429 on 5h until 15:42', 'contract in cooldown', 'reserve needs funding', 'studio window spent']);
  assert.equal(exhausted[2].title, 'at its spend cap until 15:42');
  assert.deepEqual(verdict({ budget: [{ budgetUsd: 5, observedAt: iso(NOW), period: 'day', periodEnd: iso(MIDNIGHT), periodStart: iso(ms(0, 0)), spentUsd: 1.1 }], obstacle: { name: 'budget', reason: 'day', until: iso(MIDNIGHT), window: null }, verdict: 'out' }), ['out', 'budget $1.10 of $5.00', 'at its spend budget for the period']);
  const daily = { budgetUsd: 5, observedAt: iso(NOW), period: 'day', periodEnd: iso(MIDNIGHT), periodStart: iso(ms(0, 0)), spentUsd: 1 };
  const monthly = { budgetUsd: 10, observedAt: iso(NOW), period: 'month', periodEnd: iso(Date.UTC(2026, 9, 1)), periodStart: iso(Date.UTC(2026, 8, 1)), spentUsd: 12 };
  const month = { name: 'budget', reason: 'month', until: iso(Date.UTC(2026, 9, 1)), window: null };
  assert.deepEqual(verdict({ budget: [daily, monthly], obstacle: month, verdict: 'out' }), ['out', 'budget $12.00 of $10.00', 'at its spend budget for the period']);
  assert.deepEqual(verdict({ budget: [daily, monthly], obstacle: { ...month, reason: null }, verdict: 'out' }), ['out', 'budget', 'at its spend budget for the period'], 'an obstacle that names no period draws no budget detail');
  // A spent member names the reset it waits for, on its chip and on its hover.
  assert.deepEqual(verdict({ obstacle: { name: 'spent', reason: null, scope: null, until: until(15, 42), window: 'five_hour' }, verdict: 'out' }), ['out', '5h spent until 15:42', 'its 5h window spent until 15:42']);
  assert.deepEqual(verdict({ obstacle: { name: 'credits', reason: 'org_spend_cap_reached', until: null, window: null }, verdict: 'out' }), ['out', 'credits spend cap reached', 'its usage credits unavailable for fast mode']);
  assert.deepEqual(verdict({ obstacle: { name: 'budget_unknown', reason: null, until: null, window: null }, verdict: 'out' }), ['out', 'budget unknown', 'its spend unknown while the ledger is degraded']);
  assert.deepEqual(verdict({ inFlight: 16, partition: 'at_cap', verdict: 'out' }), ['at cap', 'at its cap', 'at its in-flight cap']);
  assert.deepEqual(verdict({ inFlight: 16, verdict: 'held_on' }), ['held on', 'at its cap', null]);
  assert.deepEqual(verdict({ fit: 'rewrite_only', inFlight: 16, verdict: 'held_on' }), ['held on', 'at its cap', null], 'the cap explains a hold whatever the member fit');
  // A destination the gate found short names its fit in the tone the card draws, a fitting one keeps its own chip.
  assert.deepEqual(['least_bad', 'rewrite_only', 'ending', 'fits'].map((fit) => cell({ fit, verdict: 'destination' }).verdict), [
    { badge: 'destination', badgeTone: 'neutral', chip: { value: 'least bad' }, chosen: false, tone: 'warning' },
    { badge: 'destination', badgeTone: 'neutral', chip: { value: 'rewrite only' }, chosen: false, tone: 'warning' },
    { badge: 'destination', badgeTone: 'neutral', chip: { value: 'ending' }, chosen: false, tone: 'neutral' },
    { badge: 'destination', badgeTone: 'neutral', chip: { value: 'not placed' }, chosen: false, tone: null },
  ]);
  assert.deepEqual(verdict({ verdict: 'destination' }), ['destination', 'not placed', null]);
  assert.deepEqual(verdict({ rank: 2, verdict: 'ranked' }), ['ranked 2nd', null, null]);
  assert.deepEqual(verdict({ verdict: 'not_needed' }), ['not needed', null, null]);
  assert.deepEqual(verdict({ verdict: 'renewed' }), ['renewed', null, null]);
  assert.deepEqual(verdict({ verdict: 'source', windows: { five_hour: window5h(1, ms(15, 42)), seven_day: window5h(0.5, null) } }), ['source', '5h spent', null]);
});

test("the candidate's Runway reads unbounded where the gate read a fit and no runway, none at 0 and a dash where it read no fit or the fit spent", () => {
  // The record leaves the runway out where no burn bounds it, where it passes the binding window's reset and on a
  // member that has no allowance left.
  const runway = (o) => d.candidateCells(candidate(o), record(), NOW).runway;
  assert.deepEqual([runway({ fit: 'fits', runwayMs: null }), runway({ fit: 'least_bad', runwayMs: 0 }), runway({ fit: null, runwayMs: null }), runway({ fit: 'spent', runwayMs: null })], ['unbounded', 'none', '-', '-']);
});

test('the outcome rows read the cache tier off the deepest map entry and pending before it', () => {
  const rows = (r, row) => d.outcomeRows(r, NOW, row).map((line) => [line.label, texts(line.chips)]);
  const answered = record({
    outcome: {
      completion: completion({ cacheMapWritten: [{ accountId: 'primary', depth: 7, prefixTokens: 50_000, tier: 'message', ttlMs: 300_000 }, { accountId: 'primary', depth: 8, prefixTokens: 52_000, tier: 'message', ttlMs: 3_600_000 }], cacheReadTokens: 48_100, completedAt: iso(ms(14, 27, 34)), hit: true, inputTokens: 3_900, status: 200 }),
      kind: 'renewed',
      pinWritten: { action: 'renewed', epoch: 41, hashesStamped: true, lane: 'claude-sonnet-5-5|standard|enabled|high', refreshStamped: true },
    },
  });
  assert.deepEqual(rows(answered), [
    ['Pin written', ['renewed on primary', 'epoch 41', 'refresh 14:27:28']],
    ['Cache-map entries', ['on primary', 'message depth 7', 'message depth 8', 'prefix 52K']],
    ['Cache read', ['hit', 'read 48.1K of 52K input', 'tier 1h']],
  ]);
  const pending = record({ outcome: { completion: completion({}), pinWritten: { action: 'created', epoch: 52, hashesStamped: true, lane: 'x|standard|none|-', refreshStamped: true } } });
  assert.deepEqual(rows(pending), [
    ['Pin written', ['created on primary', 'epoch 52', 'refresh 14:27:28']],
    ['Cache-map entries', ['pending']],
    ['Cache read', ['pending']],
  ]);
  const promised = record({ outcome: { completion: completion({ cacheMapWritten: [{ accountId: 'primary', depth: 1, prefixTokens: 31_000, tier: 'static', ttlMs: 300_000 }], cacheReadTokens: 31_000, completedAt: iso(NOW), hit: true, inputTokens: 200, promisedTokens: 31_200, status: 200 }), rung: { depth: 2, prefixTokens: 31_200, rung: 'static' } } });
  assert.deepEqual(rows(promised).find(([label]) => label === 'Cache read'), ['Cache read', ['hit', 'read 31K of 31.2K promised', 'tier 5 min']]);
  const held = record({ outcome: { hold: { accountId: 'primary', boundMs: 10_000, cohort: 'primary' }, kind: 'held' }, walk: [wstep('hold', 'held', [wfact('place', 'cohort')])] });
  assert.deepEqual(rows(held), [['Hold', ['on primary', 'cohort', 'bound 10 s']]]);
  const freed = record({ outcome: { cohortHold: { cohort: 'primary', heldMs: 2_000, waitedOn: ['primary'], waits: [] }, completion: completion({ completedAt: iso(NOW), freed: true, heldMs: 2_000, status: 200 }) } });
  assert.deepEqual(rows(freed).find(([label]) => label === 'Hold'), ['Hold', ['on primary', 'cohort', 'freed after 2.0 s']]);
  const exhausted = record({ outcome: { accountId: null, exhaustion: { deadline: iso(ms(15, 42)), kind: 'timed', obstacle: null, reason: null, status: 429, window: 'five_hour' }, kind: 'exhausted', upstreamModel: null } });
  assert.deepEqual(rows(exhausted), [['Answer', ['status 429', 'window 5h', 'retry after 15:42']]]);
  const refused = record({ outcome: { accountId: null, kind: 'refused', refusal: { kind: 'client', reason: 'hash' }, upstreamModel: null }, request: { keyId: 'laptop' } });
  const drift = decision({ accountId: null, by: by('refusal', 'client'), fact: [fact('client', '2.1.274', 'text')], kind: 'refused', refusal: { kind: 'client', reason: 'hash' } });
  assert.deepEqual(rows(refused, drift), [['Answer', ['client drift', 'client 2.1.274', 'key laptop']]]);
  assert.deepEqual(d.outcomeRows(refused, NOW, drift)[0].chips[2].entity, { id: 'laptop', kind: 'key' });
  const spend = record({ candidates: [candidate({ budget: [{ budgetUsd: 5, observedAt: iso(NOW), period: 'day', periodEnd: iso(MIDNIGHT), periodStart: iso(ms(0, 0)), spentUsd: 1.1 }] })] });
  assert.deepEqual(rows(spend), [['Spend', ['daily budget $1.10 of $5.00']]]);
  // The member a monthly budget excluded while its daily one had room: the Spend row names the monthly one.
  const daily = { budgetUsd: 5, observedAt: iso(NOW), period: 'day', periodEnd: iso(MIDNIGHT), periodStart: iso(ms(0, 0)), spentUsd: 1 };
  const monthly = { budgetUsd: 10, observedAt: iso(NOW), period: 'month', periodEnd: iso(Date.UTC(2026, 9, 1)), periodStart: iso(Date.UTC(2026, 8, 1)), spentUsd: 12 };
  const reachedBy = (reason) =>
    record({
      candidates: [candidate({ accountId: 'research', budget: [daily, monthly], obstacle: { name: 'budget', reason, until: iso(Date.UTC(2026, 9, 1)), window: null }, verdict: 'out' })],
      outcome: { accountId: null, exhaustion: { deadline: iso(Date.UTC(2026, 9, 1)), kind: 'timed', obstacle: null, reason: null, window: 'overage' }, kind: 'exhausted', upstreamModel: null },
    });
  assert.deepEqual(rows(reachedBy('month')).find(([label]) => label === 'Spend'), ['Spend', ['research $12.00 of $10.00']]);
  assert.equal(rows(reachedBy(null)).find(([label]) => label === 'Spend'), undefined, 'an obstacle that names no period draws no budget detail');
});

test("an exhausted record's Answer row draws the status that the record carries", () => {
  const exhausted = record({ outcome: { accountId: null, exhaustion: { deadline: null, kind: 'untimed', obstacle: null, reason: null, status: 429, window: null }, kind: 'exhausted', upstreamModel: null } });
  assert.deepEqual(d.outcomeRows(exhausted, NOW).map((line) => [line.label, texts(line.chips)]), [['Answer', ['status 429', 'retry none']]]);
});

test('a record stored before the status draws its Answer row with no status chip', () => {
  const exhausted = record({ outcome: { accountId: null, exhaustion: { deadline: null, kind: 'untimed', obstacle: null, reason: null, status: null, window: null }, kind: 'exhausted', upstreamModel: null } });
  assert.deepEqual(d.outcomeRows(exhausted, NOW).map((line) => [line.label, texts(line.chips)]), [['Answer', ['retry none']]]);
});

test("the Probe line words the pinned member's obstacle as the row's By cell does", () => {
  const r = record({ walk: [wstep('probe', 'answered', [wfact('obstacle', 'spent')])] });
  assert.deepEqual(d.walkLines(r).map((l) => [l.step, l.verdict, texts(l.chips)]), [['Probe', 'answered', ['obstacle spend cap reached']]]);
});

test("the cache figure's base holds the write", () => {
  // A turn that wrote most of its prompt to the cache paid for the write, so the figure reads no full hit for it.
  const written = record({ outcome: { completion: completion({ cacheReadTokens: 19_330, cacheWriteTokens: 27_246, completedAt: iso(NOW), hit: true, inputTokens: 2, status: 200 }) } });
  assert.equal(d.verdictChips(written, decision({ by: strategy('pace', [0, 0, 0]) })).find((c) => c.label === 'cache hit')?.value, '19.3K of 46.6K');
});

test('a cache-map row of four entries or more draws one chip per tier and the three deepest message depths, with every entry behind its control', () => {
  const entry = (tier, depth, prefixTokens, accountId = 'primary') => ({ accountId, depth, prefixTokens, tier, ttlMs: 3_600_000 });
  const mapRow = (entries) => d.outcomeRows(record({ outcome: { completion: completion({ cacheMapWritten: entries, completedAt: iso(NOW), status: 200 }) } }), NOW).find((r) => r.label === 'Cache-map entries');
  const titles = (chips) => chips.filter((c) => c.title !== undefined).map((c) => c.title);
  // Three entries keep the row as it stands, one depth chip per entry.
  const three = mapRow([entry('message', 1, 40_000), entry('message', 2, 40_000), entry('static', 1, 9_000)]);
  assert.deepEqual(texts(three.chips), ['on primary', 'message depth 1', 'message depth 2', 'static depth 1', 'prefix 40K']);
  assert.equal(three.more, undefined, 'a short row has no control');
  // The router writes the message chain's rows, each at the answer's one prefix, then the static chain's, each in its
  // chain's order. The next turn reads the deepest message depths, so those stand beside the tier chips.
  const long = [...Array.from({ length: 11 }, (_, i) => entry('message', i + 1, 40_000)), entry('static', 1, 9_000), entry('static', 2, 12_000), entry('static', 3, 15_000)];
  const fourteen = mapRow(long);
  assert.deepEqual(texts(fourteen.chips), ['on primary', 'message ×11 depth 1 to 11', 'static ×3 depth 1 to 3', 'message depth 9', 'message depth 10', 'message depth 11', 'prefix 40K']);
  assert.deepEqual(titles(fourteen.chips), ['11 cache-map entries on the message tier', '3 cache-map entries on the static tier']);
  assert.equal(fourteen.more?.count, 14);
  assert.deepEqual(texts(fourteen.more?.all ?? []), ['on primary', ...long.map((e) => `${e.tier} depth ${e.depth}`), 'prefix 40K']);
  // The threshold's own count folds too, and a tier of one is one entry.
  const four = mapRow([entry('message', 1, 40_000), entry('message', 2, 40_000, 'secondary'), entry('message', 3, 40_000), entry('static', 1, 9_000)]);
  assert.equal(d.CACHE_MAP_COMPACT_FROM, 4);
  assert.deepEqual(texts(four.chips), ['on primary', 'on secondary', 'message ×3 depth 1 to 3', 'static ×1 depth 1', 'message depth 1', 'message depth 2', 'message depth 3', 'prefix 40K']);
  assert.deepEqual(titles(four.chips), ['3 cache-map entries on the message tier', '1 cache-map entry on the static tier']);
  assert.deepEqual(texts(four.more?.all ?? []), ['on primary', 'on secondary', 'message depth 1', 'message depth 2', 'message depth 3', 'static depth 1', 'prefix 40K']);
  // Fewer message entries draw fewer depth chips, and a record without one draws its tier chips alone.
  const two = mapRow([entry('message', 1, 40_000), entry('message', 2, 40_000), entry('static', 1, 9_000), entry('static', 2, 12_000)]);
  assert.deepEqual(texts(two.chips), ['on primary', 'message ×2 depth 1 to 2', 'static ×2 depth 1 to 2', 'message depth 1', 'message depth 2', 'prefix 40K']);
  const none = mapRow([1, 2, 3, 4].map((depth) => entry('static', depth, 9_000 + depth * 1_000)));
  assert.deepEqual(texts(none.chips), ['on primary', 'static ×4 depth 1 to 4', 'prefix 13K']);
});

test('a refusal reads its words by code, and an unknown code reads through the fallback', () => {
  const refused = (refusal) => decision({ accountId: null, by: by('refusal', refusal.kind), kind: 'refused', refusal });
  assert.equal(read(refused({ code: 'pool_missing', kind: 'key', reason: 'x' })).by, 'pool missing', 'the code names the words whatever the reason says');
  assert.equal(read(refused({ code: 'drift', kind: 'client', reason: 'hash' })).by, 'client drift');
  assert.equal(read(refused({ code: 'later', kind: 'key', reason: 'a new reason' })).by, 'key refused', 'a code the console has no words for reads as a key refusal');
  // The card's Answer row reads the same words, and the reason's sentence for a code it has none for.
  const answer = (refusal) => d.outcomeRows(record({ outcome: { accountId: null, kind: 'refused', refusal, upstreamModel: null }, request: { keyId: 'laptop' } }), NOW).map((line) => [line.label, texts(line.chips)]);
  assert.deepEqual(answer({ code: 'pool_missing', kind: 'key', reason: 'x' }), [['Answer', ['pool missing', 'key laptop']]]);
  assert.deepEqual(answer({ code: 'later', kind: 'key', reason: 'a new reason' }), [['Answer', ['a new reason', 'key laptop']]]);
});

test("a guardrail's block reads guardrail blocked with the guardrail's name as a second chip, on the row, the Admission line and the Answer row", () => {
  const refusal = { code: 'guardrail_blocked', kind: 'guardrail', reason: 'no-secrets' };
  const summary = decision({ accountId: null, by: by('refusal', 'guardrail'), fact: [fact('key', 'laptop', null, { id: 'laptop', kind: 'key' })], kind: 'refused', refusal });
  assert.equal(read(summary).by, 'guardrail blocked', "the row's By chip reads the block's first chip");
  const r = record({ outcome: { accountId: null, kind: 'refused', refusal, upstreamModel: null }, pin: null, pinReason: 'not_looked_up', request: { keyId: 'laptop' }, walk: [wstep('admission', 'refused', [wfact('reason', 'no-secrets')]), wstep('selection', 'not_run')] });
  assert.deepEqual(d.walkLines(r, summary).map((l) => [l.step, texts(l.chips)]), [['Admission', ['guardrail blocked', 'guardrail no-secrets']], ['Rest of the walk', ['moves', 'rungs', 'partitions']]]);
  assert.deepEqual(d.outcomeRows(r, NOW, summary).map((line) => [line.label, texts(line.chips)]), [['Answer', ['guardrail blocked', 'guardrail no-secrets', 'key laptop']]]);
});

test('a_contract_refusal_reads_client_contract_with_the_items_word', () => {
  const refusal = { code: 'contract', kind: 'client', reason: 'class_header' };
  const summary = decision({ accountId: null, by: by('refusal', 'client'), fact: [fact('client', '2.1.271', 'text')], kind: 'refused', refusal });
  assert.equal(read(summary).by, 'client contract', 'the row names the contract, not a drift');
  const r = record({ outcome: { accountId: null, kind: 'refused', refusal, upstreamModel: null }, pin: null, pinReason: 'not_looked_up', request: { keyId: 'laptop' }, walk: [wstep('admission', 'refused', [wfact('reason', 'class_header')]), wstep('selection', 'not_run')] });
  assert.deepEqual(
    d.walkLines(r, summary).map((l) => [l.step, texts(l.chips)]),
    [['Admission', ['contract class header']], ['Rest of the walk', ['moves', 'rungs', 'partitions']]],
    "the admission's chip is the item's word, with no client version beside it",
  );
  assert.deepEqual(d.outcomeRows(r, NOW, summary).map((line) => [line.label, texts(line.chips)]), [['Answer', ['client contract', 'contract class header', 'key laptop']]]);
  // A drift's words stand beside the contract's.
  const drift = { code: 'drift', kind: 'client', reason: 'hash' };
  const drifted = decision({ accountId: null, by: by('refusal', 'client'), fact: [fact('client', '2.1.271', 'text')], kind: 'refused', refusal: drift });
  assert.equal(read(drifted).by, 'client drift');
  assert.deepEqual(d.outcomeRows(record({ outcome: { accountId: null, kind: 'refused', refusal: drift, upstreamModel: null }, request: { keyId: 'laptop' } }), NOW, drifted).map((line) => [line.label, texts(line.chips)]), [['Answer', ['client drift', 'client 2.1.271', 'key laptop']]]);
});

test('an exhausted record after a run-out draws ran out on its hold', () => {
  const rows = (r) => d.outcomeRows(r, NOW).map((line) => [line.label, texts(line.chips)]);
  const exhausted = (holds) =>
    record({ outcome: { accountId: null, exhaustion: { deadline: null, kind: 'model_rejected', obstacle: null, reason: null, status: 404, window: null }, kind: 'exhausted', upstreamModel: null }, request: { holds } });
  const hold = (accountId, ranOut, cohort = null) => ({ accountId, attempt: 1, cohort, heldMs: 60_000, ranOut });
  assert.deepEqual(rows(exhausted([hold('primary', false), hold('secondary', true)])), [
    ['Hold', ['on secondary', 'strategy pick', 'held 1 min', 'ran out']],
    ['Answer', ['status 404', 'retry none']],
  ]);
  assert.deepEqual(rows(exhausted([hold('primary', false), hold('secondary', false)])), [
    ['Hold', ['on secondary', 'strategy pick', 'held 1 min']],
    ['Answer', ['status 404', 'retry none']],
  ], 'a hold that did not run out draws no ran out chip');
  assert.deepEqual(rows(exhausted([hold('primary', true, 'primary')]))[0], ['Hold', ['on primary', 'cohort', 'held 1 min', 'ran out']], "a cohort's hold reads its place as the cohort row does");
  assert.deepEqual(rows(exhausted([])), [['Answer', ['status 404', 'retry none']]], 'a record that held nowhere draws no hold');
});

test('an exhausted record draws no hold an earlier attempt waited through', () => {
  const rows = (r) => d.outcomeRows(r, NOW).map((line) => [line.label, texts(line.chips)]);
  const exhausted = (attempt, holds) =>
    record({ outcome: { accountId: null, exhaustion: { deadline: null, kind: 'model_rejected', obstacle: null, reason: null, status: 404, window: null }, kind: 'exhausted', upstreamModel: null }, request: { attempt, holds } });
  const hold = (attempt) => ({ accountId: 'primary', attempt, cohort: null, heldMs: 60_000, ranOut: false });
  // Held and freed on attempt 1, then failed over: the relay keeps attempt 1's hold on the request that attempt 2 exhausts.
  assert.deepEqual(rows(exhausted(2, [hold(1)])), [['Answer', ['status 404', 'retry none']]]);
  assert.deepEqual(rows(exhausted(2, [hold(1), hold(2)])), [['Hold', ['on primary', 'strategy pick', 'held 1 min']], ['Answer', ['status 404', 'retry none']]], "a hold on the record's own attempt still draws");
});

test("a held outcome's verdict chips and Hold row read the slow-down's figures one value a chip", () => {
  const slowHold = (o = {}) => ({ accountId: 'studio', boundMs: 120_000, cohort: null, cycleMs: 20_000, factor: 0.5, reason: 'slow_down', speed: 0.5, stretchMs: 2_520_000, thresholdMs: 1_800_000, until: iso(ms(18, 20)), ...o });
  const held = (hold) => record({ candidates: [candidate({ accountId: 'studio', verdict: 'held_on' })], outcome: { accountId: 'studio', hold, kind: 'held' } });
  const row = decision({ accountId: 'studio', by: by('hold', 'slow_down'), fact: [fact('until', ms(18, 20), 'time'), fact('factor', 0.5, null)], kind: 'held' });
  assert.deepEqual(texts(d.verdictChips(held(slowHold()), row)), ['tier plan quota P0', 'slow-down', 'held for the slow-down until 18:20', '0.5x']);
  const rows = (r) => d.outcomeRows(r, NOW).map((line) => [line.label, texts(line.chips)]);
  assert.deepEqual(rows(held(slowHold())), [['Hold', ['on studio', 'for the slow-down until 18:20', 'speed 0.5x', 'stretch 42 min of 30 min', 'bound 2 min', 'cycle 20 s']]]);
  // The wait is the slow-down's and no slot's, so the row carries no in-flight cap hover.
  assert.equal(d.outcomeRows(held(slowHold()), NOW)[0].title, undefined);
  // A pin with no cycle yet draws no cycle chip, and a rejoin on a pool whose switch is off names its deadline alone.
  assert.deepEqual(rows(held(slowHold({ cycleMs: null }))), [['Hold', ['on studio', 'for the slow-down until 18:20', 'speed 0.5x', 'stretch 42 min of 30 min', 'bound 2 min']]]);
  const rejoin = held(slowHold({ cycleMs: null, factor: null, speed: null, stretchMs: null, thresholdMs: null }));
  assert.deepEqual(rows(rejoin), [['Hold', ['on studio', 'for the slow-down until 18:20', 'bound 2 min']]]);
  assert.deepEqual(texts(d.verdictChips(rejoin, row)).slice(-1), ['held for the slow-down until 18:20']);
  // The routing log's row reads the deadline and the factor as the chips print it.
  assert.deepEqual(read(row), { by: 'slow-down', facts: ['until 18:20', 'speed 0.5x'] });
  assert.deepEqual(read(decision({ ...row, fact: [fact('until', ms(18, 20), 'time'), fact('factor', 0.4, null)] })).facts, ['until 18:20', 'speed 0.4x']);
});

test("a rewrite hold's verdict chips and Hold row name the request it waits for, its link one chip of its own", () => {
  const rewriteHold = { accountId: 'nightly', boundMs: 120_000, cohort: null, cycleMs: null, factor: null, reason: 'rewrite', speed: null, stretchMs: null, thresholdMs: null, until: iso(ms(11, 3)), waitsFor: 'req_5b2e8d47-c19' };
  const held = record({ candidates: [candidate({ accountId: 'nightly', verdict: 'held_on' })], outcome: { accountId: 'nightly', hold: rewriteHold, kind: 'held' } });
  const row = decision({ accountId: 'nightly', by: by('hold', 'rewrite'), fact: [fact('until', ms(11, 3), 'time')], kind: 'held' });
  assert.deepEqual(texts(d.verdictChips(held, row)).slice(-2), ['rewrite in flight', 'held for the rewrite in flight']);
  const rows = (r) => d.outcomeRows(r, NOW).map((line) => [line.label, texts(line.chips)]);
  assert.deepEqual(rows(held), [['Hold', ['on nightly', 'for the rewrite in flight of', 'req_5b2e8d47-c19', 'until 11:03', 'bound 2 min']]]);
  const link = d.outcomeRows(held, NOW)[0].chips.find((c) => c.value === 'req_5b2e8d47-c19');
  assert.ok(link?.href, 'the request it waited for is a link');
  const unnamed = record({ candidates: [candidate({ accountId: 'nightly', verdict: 'held_on' })], outcome: { accountId: 'nightly', hold: { ...rewriteHold, waitsFor: null }, kind: 'held' } });
  assert.deepEqual(rows(unnamed), [['Hold', ['on nightly', 'for the rewrite in flight', 'until 11:03', 'bound 2 min']]], 'a hold that names no request reads no dangling of');
});

test("a renewal after a slow-down hold reads the hold's label and one Hold row per wait with its reason", () => {
  // The router names the hold the renewal followed, as a placement after a hold names its own.
  assert.deepEqual(read(decision({ by: by('hold', 'slow_down'), fact: [fact('held', 42_000, 'ms')], kind: 'renewed' })), { by: 'slow-down', facts: ['held 42 s'] });
  assert.deepEqual([read(decision({ kind: 'renewed' })).by, read(decision({ kind: 'renewed', probe: true })).by], ['no rule fired', 'keepalive'], 'every other renewal keeps its words');
  const rows = (cohortHold, freed = true) =>
    d
      .outcomeRows(record({ outcome: { cohortHold, completion: completion({ completedAt: iso(NOW), freed, heldMs: cohortHold.heldMs, status: 200 }), kind: 'renewed' } }), NOW)
      .filter((line) => line.label === 'Hold')
      .map((line) => [texts(line.chips), line.title]);
  // Each wait reads its reason and its own span, oldest first, and the last the form its end took.
  assert.deepEqual(rows({ cohort: null, heldMs: 81_000, waitedOn: ['studio'], waits: [{ heldMs: 39_000, reason: 'reset' }, { heldMs: 42_000, reason: 'slow_down' }] }), [
    [['on studio', 'for the reset', 'held 39 s'], undefined],
    [['on studio', 'for the slow-down', 'freed after 42 s'], undefined],
  ]);
  // A slot's wait keeps its place and the in-flight cap's hover, which speaks of no other wait.
  assert.deepEqual(rows({ cohort: 'primary', heldMs: 44_000, waitedOn: ['primary'], waits: [{ heldMs: 2_000, reason: 'cap' }, { heldMs: 42_000, reason: 'slow_down' }] }, false), [
    [['on primary', 'cohort', 'held 2.0 s'], d.HOLD_HINT],
    [['on primary', 'for the slow-down', 'held 42 s'], undefined],
  ]);
  // Each wait names the account it waited on: the slow-down's on its pin's account, then a slot's on the strategy's pick.
  assert.deepEqual(rows({ cohort: null, heldMs: 44_000, waitedOn: ['studio', 'primary'], waits: [{ heldMs: 42_000, reason: 'slow_down' }, { heldMs: 2_000, reason: 'cap' }] }), [
    [['on studio', 'for the slow-down', 'held 42 s'], undefined],
    [['on primary', 'strategy pick', 'freed after 2.0 s'], d.HOLD_HINT],
  ]);
  // A row written before the waits reads one row of the summed wait.
  assert.deepEqual(rows({ cohort: null, heldMs: 2_000, waitedOn: ['primary'], waits: [] }), [[['on primary', 'strategy pick', 'freed after 2.0 s'], d.HOLD_HINT]]);
  // A wait for the head of the request that carried a move reads in the drawer's words.
  assert.deepEqual(rows({ cohort: null, heldMs: 1_000, waitedOn: ['nightly'], waits: [{ heldMs: 1_000, reason: 'rewrite' }] }), [[['on nightly', 'for the rewrite in flight', 'freed after 1.0 s'], undefined]]);
});

test("an outcome with a cause reads it in the event card's words", () => {
  const rows = (r) => Object.fromEntries(d.outcomeRows(r, NOW).map((line) => [line.label, texts(line.chips)]));
  const cooled = { cause: null, reason: 'rate_limit', scope: 'account', state: null, window: 'five_hour' };
  const marked = { cause: 'funding_refused', reason: null, scope: null, state: 'funding_required', window: null };
  // A failover's landing is a placement, and a rule's move with a cause draws it too.
  assert.deepEqual(rows(record({ outcome: { cause: cooled, fromAccountId: 'secondary', kind: 'placed' } })).Cause, ['429 on 5h']);
  assert.deepEqual(rows(record({ outcome: { cause: marked, fromAccountId: 'secondary', kind: 'moved' } })).Cause, ['funding required by credit balance']);
  // The event card's row of the same move spells each cause alike.
  for (const [reason, cause] of [['cooldown', cooled], ['mark', marked], ['fast_lane', { cause: null, reason: 'fast_rate_limit', scope: 'fast', state: null, window: 'fast' }]]) {
    const pair = d.detailPairs(event('pin_migrated', { agent_id: 'main', cause, epoch: 54, from_account: 'secondary', reason }), NOW, [], PRO, NO_TIERS).find((p) => p.label === 'Cause');
    assert.deepEqual(texts(pair.chips), rows(record({ outcome: { cause, kind: 'placed' } })).Cause, reason);
  }
  assert.equal(rows(record({ outcome: { cause: null, kind: 'placed' } })).Cause, undefined, 'an outcome with no cause draws no Cause row');
});

test('the pin fold reads the cycle and the slowed-until time', () => {
  const pin = {
    accountId: 'studio', createdAt: iso(ms(13, 47, 24)), cycleMs: 42_000, epoch: 208, found: 'live', idleMs: 42_000, incarnation: 1, lane: 'x|standard|enabled|high', lastRefreshAt: iso(ms(14, 3, 36)), lastUsedAt: iso(ms(14, 3, 36)),
    marked: false, markedAt: null, markedOn: null, markedWindow: null, refreshAgeMs: 42_000, released: [], shed: null, slowDownUntil: iso(ms(14, 5)), warmth: 'refresh', warmthCause: 'continues_pin',
  };
  const rows = (p) => d.pinRows(record({ pin: p, pinReason: null }), NOW).map((r) => [r.label, texts(r.chips)]);
  assert.deepEqual(rows(pin).slice(-2), [['Cycle', ['42 s']], ['Slowed until', ['14:05']]]);
  assert.deepEqual(rows(pin).map(([label]) => label), ['Pin on', 'Epoch', 'Created', 'Idle', 'Refresh age', 'Pin lane', 'Warmth', 'Cause', 'Cycle', 'Slowed until']);
  // A pin with no cycle and no standing deadline, a record from before the members among them, draws neither row.
  assert.deepEqual(rows({ ...pin, cycleMs: null, slowDownUntil: null }).slice(-2).map(([label]) => label), ['Warmth', 'Cause']);
  assert.deepEqual(rows({ ...pin, slowDownUntil: null }).slice(-1), [['Cycle', ['42 s']]], 'a cycle stands without a deadline');
});

test("the pin fold's Shed mark reads a round's move as shed from its source and every other stamp as failed over from it", () => {
  const pin = {
    accountId: 'primary', createdAt: iso(ms(14, 7, 51)), cycleMs: null, epoch: 61, found: 'live', idleMs: 314_000, incarnation: 1, lane: 'x|standard|enabled|high', lastRefreshAt: iso(ms(14, 7, 51)), lastUsedAt: iso(ms(14, 7, 51)),
    marked: false, markedAt: null, markedOn: null, markedWindow: null, refreshAgeMs: 314_000, released: [], shed: null, slowDownUntil: null, warmth: 'refresh', warmthCause: 'continues_pin',
  };
  const mark = (p) => d.pinRows(record({ pin: p, pinReason: null }), NOW).find((r) => r.label === 'Shed mark');
  const stamped = (reason) => texts(mark({ ...pin, shed: { at: iso(ms(14, 7, 51)), from: 'secondary', reason } }).chips);
  assert.deepEqual(stamped('round'), ['shed from secondary', 'at 14:07'], "a shedding round's move");
  for (const reason of ['cooldown', 'fast_lane', 'mark']) assert.deepEqual(stamped(reason), ['failed over from secondary', 'at 14:07'], `a failover's landing on ${reason}`);
  // A marked pin reads its mark first, and the stamp of the move that brought it after it.
  const marked = { ...pin, marked: true, markedAt: iso(ms(14, 12, 30)), markedOn: 'primary', markedWindow: 'five_hour', shed: { at: iso(ms(14, 7, 51)), from: 'secondary', reason: 'cooldown' } };
  assert.deepEqual(texts(mark(marked).chips), ['marked on primary', 'at 14:12', 'by the 5h trigger', 'failed over from secondary', 'at 14:07']);
  assert.equal(mark(pin), undefined, 'a pin no round marked and nothing stamped draws no Shed mark');
});

/** Event details in the shapes their writers store. */
const DETAIL_EVENTS = {
  pin_migrated: { agent_id: 'main', binding: 'five_hour', cause: { cause: 'funding_refused', state: 'funding_required' }, epoch: 53, fit: 'least_bad', from_account: 'secondary', reason: 'mark', runway_ms: 0 },
  shedding_round: {
    burn_average_seconds: 1_800, burn_sum: 0.12, ceiling: 0.95, dwell_seconds: 900, dwell_until: ms(14, 15), excess: 0.06, factor: null, horizon: ms(16, 0),
    marks: [{ agent_id: 'c21e', key_id: 'desktop', model: 'claude-fable-5-1', session_id: 's1' }],
    outcome: 'marked 1', persisted: true,
    pins: [
      { agent_id: 'c21e', burn: 0.07, cold: true, key_id: 'desktop', last_input_tokens: 48_100, marked: true, model: 'claude-fable-5-1', note: null, per_rewrite: 0.004, planned: 'secondary', rewrite: 0.005, rewrite_window: 'seven_day_fable', runway_ms: null, session_id: 's1', skip: null },
      { agent_id: 'd93b', burn: null, cold: false, key_id: 'desktop', last_input_tokens: 48_000, marked: false, model: 'claude-opus-5', note: null, per_rewrite: null, planned: null, rewrite: null, rewrite_window: null, runway_ms: null, session_id: 's2', skip: 'off_window' },
    ],
    projection: 0.99, reason: null, removed: 0.07, shed_cap: 0.2, stretch_ms: null, target: 0.024, threshold_ms: null, trigger: 'projection', unit: 'window_share', utilization: 0.71, warm_sessions: 'when_needed',
    window: 'seven_day_fable',
  },
  window_cleared: { reset: ms(15, 42), source: 'poll', source_request_id: null, utilization: 0, utilization_before: 1, window: 'five_hour' },
};

test("a mark's move reads as a failover with its cause, an event with no account opens on When and its own pair, and a round draws its pins and whether it survives a restart", () => {
  // A mark's move is a failover, and its cause reads in the account state card's words.
  const marked = Object.fromEntries(d.detailPairs(event('pin_migrated', DETAIL_EVENTS.pin_migrated), NOW, [], PRO, NO_TIERS).map((p) => [p.label, texts(p.chips)]));
  assert.deepEqual([marked.Reason, marked.Cause, marked['Moved by']], [[d.PIN_MIGRATION_LABEL.mark], ['funding required by credit balance'], ['failover']]);
  const without = d.detailPairs(event('router_started', { pins_loaded: 10 }, { accountId: null }), NOW, [], PRO, NO_TIERS);
  assert.deepEqual(without.slice(0, 2).map((p) => p.label), ['When', 'Pins loaded']);
  const round = d.detailPairs(event('shedding_round', DETAIL_EVENTS.shedding_round), NOW, [], PRO, NO_TIERS).find((p) => p.label === 'Pins');
  assert.deepEqual(round.table.head, ['Session', 'Agent', 'Warmth', 'Burn', 'Last prompt', 'Rewrite', 'Destination', 'Mark']);
  assert.deepEqual(round.table.rows.map((r) => r.map(chipText)), [
    ['s1', 'c21e', 'cold', '7.0 %/h', '48.1K', '0.5% of 20x 7-day Fable', 'secondary', 'marked'],
    ['s2', 'd93b', 'warm', '-', '48K', '-', '-', 'off window'],
  ]);
  const restart = d.detailPairs(event('shedding_round', DETAIL_EVENTS.shedding_round), NOW, [], PRO, NO_TIERS).find((p) => p.label === 'Survive a restart');
  assert.deepEqual(texts(restart.chips), ['persisted']);
});

test("a round's Pins table draws the thinking each pin would drop, and a dash where the round read none", () => {
  const [first, second] = DETAIL_EVENTS.shedding_round.pins;
  const drawn = (pins) => {
    const { head, rows } = d.detailPairs(event('shedding_round', { ...DETAIL_EVENTS.shedding_round, pins }), NOW, [], PRO, NO_TIERS).find((p) => p.label === 'Pins').table;
    return { head, thinking: rows.map((r) => chipText(r[5])) };
  };
  const head = ['Session', 'Agent', 'Warmth', 'Burn', 'Last prompt', 'Thinking dropped', 'Rewrite', 'Destination', 'Mark'];
  // A null is a pair the round read as kept or learned nothing of, or a pin with no destination, so it draws a dash
  // where a count would claim a figure.
  assert.deepEqual(drawn([{ ...first, dropped_thinking_tokens: 2_400 }, { ...second, dropped_thinking_tokens: null }]), { head, thinking: ['2.4K', '-'] });
  // A bound pair with no sum reads 0, a learned figure the dash would hide.
  assert.deepEqual(drawn([{ ...first, dropped_thinking_tokens: 0 }, { ...second, dropped_thinking_tokens: null }]), { head, thinking: ['0', '-'] });
  // The key on a pin is what draws the column, so a round whose every pin holds it at null still draws it.
  assert.deepEqual(drawn([{ ...first, dropped_thinking_tokens: null }, { ...second, dropped_thinking_tokens: null }]), { head, thinking: ['-', '-'] });
});

test('a used limit reset claim draws its Cleared pair and a Resets left of 0 between its Result and its Status', () => {
  // The smoke's per-kind read takes workshop's refused claim, which draws neither of these two pairs.
  const used = { cleared: ['seven_day_fable'], cooldown_until: null, error: null, grant_id: 'rg-a-1', label: 'Launch reset', outcome: 'used', reason: null, request_id: '00000000-0000-4000-8000-0000000000c1', resets_left: 0, result: 'reset', status: 200, weekly_resets_at: null };
  const labels = d.detailPairs(event('limit_reset_claimed', used), NOW, [], PRO, NO_TIERS).map((p) => p.label);
  assert.deepEqual(labels, ['When', 'Account', 'Grant', 'Outcome', 'Result', 'Cleared', 'Resets left', 'Status', 'Request id', 'Actor']);
});

test("a move an account's state forced reads its cause in the release's words, and the same word with no list reads the failover's", () => {
  const pairs = (detail) => Object.fromEntries(d.detailPairs(event('pin_migrated', { agent_id: 'main', cause: null, epoch: 57, from_account: 'secondary', ...detail }), NOW, [], PRO, NO_TIERS).map((p) => [p.label, texts(p.chips)]));
  // The router writes the replaced pin's whole list beside the cause, the first word of its order the list holds.
  const caused = pairs({ reason: 'cooldown', reasons: ['cooldown', 'overage'] });
  assert.deepEqual([caused.Reason, caused.Reasons, caused['Moved by']], [['account cooled'], ['account cooled', 'in overage'], ["the account's state"]]);
  assert.deepEqual(pairs({ reason: 'cooldown' }).Reason, ['failed over']);
});

test("a round's off-window pin reads its word and the rewrite cell names its unit", () => {
  const detail = DETAIL_EVENTS.shedding_round;
  // A round that names no rewrite unit is an earlier build's, whose figure prints as stored under any chosen week, so
  // the cells read under the Ultra week, whose scale would shrink a converted figure.
  const drawn = (body) => {
    const pairs = d.detailPairs(event('shedding_round', body), NOW, [], ULTRA, NO_TIERS);
    return { rows: pairs.find((p) => p.label === 'Pins').table.rows.map((r) => [chipText(r[5]), chipText(r[7])]), skipped: texts(pairs.find((p) => p.label === 'Skipped')?.chips ?? []) };
  };
  // The Fable pin's rewrite is its week's 20x equivalent, and the Opus pin, which draws no 7-day Fable window, sat out.
  assert.deepEqual(drawn(detail), { rows: [['0.5% of 20x 7-day Fable', 'marked'], ['-', 'off window']], skipped: ['s2: off window'] });
  assert.deepEqual(read(event('shedding_round', detail)).facts, ['pins 1 of 2'], 'the count reads every listed pin, the one left out included');
  // On a 5-hour round every pin is on the window, and a model with no week of its own prices its rewrite on the 7-day.
  const fiveHour = (pin) => drawn({ ...detail, pins: [pin], window: 'five_hour' }).rows[0][0];
  const sonnet = { ...detail.pins[0], model: 'claude-sonnet-5', rewrite_window: 'seven_day' };
  assert.equal(fiveHour(sonnet), '0.5% of 20x 7-day');
  assert.equal(fiveHour({ ...sonnet, rewrite: 0 }), '0% of 20x 7-day', 'a zero rewrite keeps the window word');
  const unnamed = { ...sonnet };
  delete unnamed.rewrite_window;
  assert.equal(fiveHour(unnamed), '0.5%', 'a pin whose record names no window draws the bare share');
});

test("a round's Pro-week rewrite reads in the chosen week, titled with the window the pick compared on", () => {
  const detail = { ...DETAIL_EVENTS.shedding_round, rewrite_unit: 'pro_week' };
  const pin = { ...detail.pins[0], rewrite: 0.0222 };
  const cell = (p, week) => {
    const c = d.detailPairs(event('shedding_round', { ...detail, pins: [p] }), NOW, [], week, NO_TIERS).find((x) => x.label === 'Pins').table.rows[0][5];
    return [chipText(c), c.title ?? null];
  };
  // The Ultra week holds twenty Pro weeks on the test tables, so 2.22% of a Pro week is 0.111% of an Ultra week.
  assert.deepEqual(cell(pin, ULTRA), ['0.111% Ultra week', 'compared on the 7-day Fable window']);
  assert.deepEqual(cell(pin, PRO), ['2.22% Pro week', 'compared on the 7-day Fable window']);
  assert.deepEqual(cell({ ...pin, model: 'claude-sonnet-5', rewrite_window: 'seven_day' }, ULTRA), ['0.111% Ultra week', 'compared on the 7-day window']);
});

test("a round's Pro-week rewrite whose pin names no window still reads in the chosen week, with no hover", () => {
  const detail = { ...DETAIL_EVENTS.shedding_round, rewrite_unit: 'pro_week' };
  const pin = { ...detail.pins[0], rewrite: 0.0222, rewrite_window: null };
  const c = d.detailPairs(event('shedding_round', { ...detail, pins: [pin] }), NOW, [], ULTRA, NO_TIERS).find((x) => x.label === 'Pins').table.rows[0][5];
  // The pick compared the pin on no window of its own, and its 2.22% of a Pro week is still 0.111% of an Ultra week.
  assert.deepEqual([chipText(c), c.title ?? null], ['0.111% Ultra week', null]);
});

test("a round's Pro-week rewrite waits on the week's scale, and a failed settings read titles its dash with the error", () => {
  const detail = { ...DETAIL_EVENTS.shedding_round, rewrite_unit: 'pro_week' };
  const cell = (week) => {
    const c = d.detailPairs(event('shedding_round', detail), NOW, [], week, NO_TIERS).find((x) => x.label === 'Pins').table.rows[0][5];
    return [chipText(c), c.title ?? null];
  };
  assert.deepEqual(cell({ failure: null, scale: null, week: 'ultra' }), ['-', null]);
  assert.deepEqual(cell({ failure: { text: 'settings read failed', unpriced: false }, scale: null, week: 'ultra' }), ['-', 'settings read failed']);
});

test("an approved account's booking names its 7-day reservation in the strip's reserved chip", () => {
  const reservation = { accountId: 'primary', burn: { seven_day: 0.002 }, expiresAt: iso(ms(14, 30)), rewrite: { seven_day: 0.0121 } };
  const chips = texts(d.verdictChips(record({ outcome: { kind: 'placed', reservation } }), decision({})));
  assert.ok(chips.includes('reserved 1.21% of 7d'), chips.join(' / '));
});

test('a cleared window reads its readings before and after, its unchanged reset and its source, and nothing of why', () => {
  const pairs = (detail) => Object.fromEntries(d.detailPairs(event('window_cleared', detail), NOW, [], PRO, NO_TIERS).map((p) => [p.label, texts(p.chips)]));
  assert.deepEqual(pairs(DETAIL_EVENTS.window_cleared), {
    When: [d.fmtAbsolute(iso(ms(14, 0)))],
    Account: ['primary'],
    Window: ['5h'],
    Before: ['100%'],
    After: ['0%'],
    Reset: ['15:42'],
    'Read from': ['the poll'],
  });
  const head = { reset: ms(15, 42), source: 'head', source_request_id: 'req_1', utilization: 0.4, utilization_before: 1.02, window: 'seven_day' };
  // The chip draws the window's word and its title the sentence whole, which the By column has no room for.
  assert.deepEqual(d.byOf(event('window_cleared', head)), { title: '7d cleared to 40% under its reset', value: '7d cleared' });
  assert.deepEqual(d.byOf(event('window_cleared', DETAIL_EVENTS.window_cleared)), { title: '5h cleared to 0% under its reset', value: '5h cleared' });
  assert.deepEqual(pairs(head)['Read from'], ['the response head']);
});

test('the two slow-down events read their pairs and effects', () => {
  const pairs = (kind, detail) => Object.fromEntries(d.detailPairs(event(kind, detail, { poolId: 'work' }), NOW, [], PRO, NO_TIERS).map((p) => [p.label, texts(p.chips)]));
  const started = pairs('slow_down_started', { demand: 0.12, factor: 0.5, reset: ms(18, 20), stretch_ms: 2_520_000, threshold_ms: 1_800_000 });
  assert.deepEqual(started, {
    When: [d.fmtAbsolute(iso(ms(14, 0)))],
    Account: ['primary'],
    Pool: ['work'],
    Speed: ['0.5x'],
    Stretch: ['42 min of 30 min'],
    Demand: ['12.0 %/h'],
    Reset: ['18:20'],
    Effect: ["requests of the account's sessions wait their cycle times the slow-down up to the hold"],
  });
  // The span runs to the read that returned the factor to 1, without the wait that closes it.
  const ended = pairs('slow_down_ended', { duration_ms: 5_180_000, factor_min: 0.35, held_ms: 36_000, since: ms(9, 12) });
  assert.deepEqual(ended, { When: [d.fmtAbsolute(iso(ms(14, 0)))], Account: ['primary'], Pool: ['work'], Lasted: ['1 h 26 min'], Slowest: ['0.35x'], Held: ['36 s'], Effect: ["requests of the account's sessions pass unheld again"] });
  // Two decimals with a trailing zero dropped, as the speed chips read the factor.
  assert.deepEqual(pairs('slow_down_started', { demand: 0.1286, factor: 0.734, reset: ms(18, 20), stretch_ms: 1_620_000, threshold_ms: 1_800_000 }).Speed, ['0.73x']);
});

test("a round's Trigger reads the stretch under the slow-down, its Demand pair the burn and the reset, and its Fired by pair names the source", () => {
  const round = (more) =>
    Object.fromEntries(
      d.detailPairs(
        event('shedding_round', { burn_average_seconds: 1_800, burn_sum: 0.12, ceiling: 0.95, dwell_seconds: 900, dwell_until: ms(14, 15), excess: 0.06, horizon: ms(16, 0), marks: [], outcome: 'nothing_to_mark', persisted: true, pins: [], projection: 0.99, reason: 'no_runway', removed: 0, shed_cap: 0.5, target: 0.06, unit: 'window_share', utilization: 0.86, warm_sessions: 'when_needed', window: 'five_hour', ...more }, { requestId: 'req_1', sessionId: 's1-0000-4000' }),
        NOW,
        [],
        PRO,
        NO_TIERS,
      ).map((p) => [p.label, texts(p.chips)]),
    );
  // Under the slow-down the horizon is the reset less the threshold, so the reset reads the horizon put back.
  const stretched = round({ factor: 0.5, horizon: ms(17, 50), stretch_ms: 2_520_000, threshold_ms: 1_800_000, trigger: 'slow_down' });
  assert.deepEqual([stretched.Trigger, stretched.Demand, stretched['Fired by']], [['5h stretch 42 min over the threshold 30 min'], ['12.0 %/h to the reset at 18:20'], ['the slow-down']]);
  assert.deepEqual(Object.keys(stretched).slice(0, 7), ['When', 'Account', 'Started by', 'Trigger', 'Demand', 'Fired by', 'Excess']);
  const projected = round({ factor: null, stretch_ms: null, threshold_ms: null, trigger: 'projection' });
  assert.deepEqual([projected.Trigger, projected.Demand, projected['Fired by']], [['5h projection 99% at 16:00 over the ceiling 95%'], undefined, ['the projection']]);
  // A round written before the trigger carried its source names none.
  const unnamed = round({});
  assert.deepEqual([unnamed.Trigger, unnamed['Fired by']], [['5h projection 99% at 16:00 over the ceiling 95%'], undefined]);
});

test("a round the Fable reservation fired reads its 7-day room under the reservation and the burn its pins walked", () => {
  const pairs = Object.fromEntries(
    d.detailPairs(
      event(
        'shedding_round',
        { burn_average_seconds: 1_800, burn_sum: 0.004, ceiling: 0.78, dwell_seconds: 900, dwell_until: ms(14, 15), excess: 0.004, fable_reservation: 0.32, factor: null, horizon: ms(19, 0), marks: [], outcome: 'nothing_to_mark', persisted: true, pins: [], projection: 0.85, reason: 'unpriced', removed: 0, shed_cap: 0.5, stretch_ms: null, target: 0.002, threshold_ms: null, trigger: 'fable_reservation', unit: 'window_share', utilization: 0.7, warm_sessions: 'when_needed', window: 'seven_day' },
        { requestId: 'req_1', sessionId: 's1-0000-4000' },
      ),
      NOW,
      [],
      PRO,
      NO_TIERS,
    ).map((p) => [p.label, texts(p.chips)]),
  );
  // The room is 1 less the 7-day reading of 0.7, under the reservation of 0.32, and the projection of 0.85 is no part
  // of the trigger. The rate the router records for such a round is the burn of the pins it walked.
  assert.deepEqual([pairs.Trigger, pairs['Walked burn'], pairs.Excess], [['7d room 30% under the Fable reservation 32%'], ['0.4 %/h'], undefined]);
});

test("a captured round the Fable reservation fired draws the reservation its detail holds", () => {
  // A detail built under the console's own wire name passes a route that renames or moves the member, so this case
  // reads the captured round, whose reservation of 0.12 draws as 12%.
  const page = d.decodeRoutingLogPage(JSON.parse(readFileSync(new URL('./fixtures/routing_log.json', import.meta.url), 'utf8')).body);
  const round = page.rows.find((r) => r.type === 'event' && r.detail.trigger === 'fable_reservation');
  assert.equal(typeof round.detail.fable_reservation, 'number', "the capture answers a number in the round's detail");
  const pairs = Object.fromEntries(d.detailPairs(round, NOW, [], PRO, NO_TIERS).map((p) => [p.label, texts(p.chips)]));
  assert.deepEqual(pairs['Fable reservation'], ['12%']);
});

test('a mark healed under the threshold says so', () => {
  const page = d.decodeRoutingLogPage({
    counts: null,
    next_cursor: null,
    rows: [
      { type: 'event', account_id: 'studio', actor: null, agent_id: 'main', at: ms(14, 20), detail: { agent_id: 'main', marked_at: ms(14, 5), marked_on: 'studio', marked_window: 'five_hour', projection: 0.93, reason: 'healed', stretch_ms: 1_440_000, threshold_ms: 2_700_000 }, id: 31, key_id: 'notebook', kind: 'mark_cleared', model: 'claude-sonnet-5-5', pool_id: 'work', request_id: null, session_id: 's1-0000-4000' },
    ],
  });
  const reason = (row) => texts(d.detailPairs(row, NOW, [], PRO, NO_TIERS).find((p) => p.label === 'Reason').chips);
  // The detail's own threshold, 45 minutes here, since the pool's setting may have moved since the heal.
  assert.deepEqual(reason(page.rows[0]), ['healed: the stretch read 24 min under the threshold 45 min']);
});

test('a mark healed by the projection names the line it read under: the wall, the ceiling with its figure, or neither on a stored event', () => {
  const healed = (projection, limit) => ({
    type: 'event', account_id: 'secondary', actor: null, agent_id: 'main', at: ms(14, 20),
    detail: { agent_id: 'main', marked_at: ms(13, 0), marked_on: 'secondary', marked_window: 'five_hour', projection, reason: 'healed', stretch_ms: null, threshold_ms: null, ...limit },
    id: 33, key_id: 'desktop', kind: 'mark_cleared', model: 'claude-sonnet-5-5', pool_id: 'all', request_id: null, session_id: 's2-0000-4000',
  });
  const page = d.decodeRoutingLogPage({ counts: null, next_cursor: null, rows: [healed(0.98, { limit: 1 }), healed(0.91, { limit: 0.95 }), healed(0.91, {})] });
  const reason = (row) => texts(d.detailPairs(row, NOW, [], PRO, NO_TIERS).find((p) => p.label === 'Reason').chips);
  assert.deepEqual(page.rows.map(reason), [['healed: the 5h projection read 98% under the wall'], ['healed: the 5h projection read 91% under the ceiling 95%'], ['healed: the 5h projection read 91% under the ceiling']]);
});

test("a shedding round's rates read share per hour of the window that fired: the cap with its target and the burn removed, and each pin's burn", () => {
  const fixture = d.decodeRoutingLogPage(JSON.parse(readFileSync(new URL('./fixtures/routing_log.json', import.meta.url), 'utf8')).body);
  const round = fixture.rows.find((r) => r.type === 'event' && r.kind === 'shedding_round');
  const drawn = (row) => {
    const pairs = d.detailPairs(row, NOW, [], PRO, NO_TIERS);
    return { cap: texts(pairs.find((p) => p.label === 'Cap').chips), pins: pairs.find((p) => p.label === 'Pins').table.rows.map((r) => chipText(r[3])), share: pairs.some((p) => p.label === 'Share'), unit: texts(pairs.find((p) => p.label === 'Unit').chips) };
  };
  // The router's row: a cap of 0.2, a target of 0.024 an hour, and a pin of 0.07 an hour that is the whole burn removed.
  assert.deepEqual([round.detail.unit, round.detail.shed_cap, round.detail.target, round.detail.removed, round.detail.pins[0].burn], ['window_share', 0.2, 0.024, 0.07, 0.07]);
  assert.deepEqual(drawn(round), { cap: ['20%', 'target 2.4 %/h', 'removed 7.0 %/h'], pins: ['7.0 %/h'], share: false, unit: ['window share'] });
  // A round whose order read the dollar table names it, and its rates still read window share per hour.
  const dollars = { ...round, detail: { ...round.detail, unit: 'usd' } };
  assert.deepEqual(drawn(dollars), { cap: ['20%', 'target 2.4 %/h', 'removed 7.0 %/h'], pins: ['7.0 %/h'], share: false, unit: ['usd'] });
});

test('a failed usage poll and a stream cut after its head read their presets and their pairs', () => {
  const presetOf = (kind) => d.KIND_PRESETS.find((p) => p.members.includes(kind))?.label;
  assert.deepEqual([presetOf('poll_failed'), presetOf('stream_interrupted')], ['account state and windows', 'exhaustion and refusals']);
  const pairs = (kind, detail, o) => d.detailPairs(event(kind, detail, o), NOW, [], PRO, NO_TIERS).map((p) => [p.label, texts(p.chips)]);
  // A transport failure answered no status, so the pair says what failed in its place.
  assert.deepEqual(pairs('poll_failed', { endpoint: 'profile', error: 'transport: connection refused (os error 111)', status: null, transport: true }).slice(1), [
    ['Account', ['primary']],
    ['Endpoint', ['profile']],
    ['Status', ['transport failure']],
    ['Error', ['transport: connection refused (os error 111)']],
  ]);
  assert.deepEqual(pairs('poll_failed', { endpoint: 'usage', error: 'usage request rejected with status 503', status: 503, transport: false }).slice(2), [
    ['Endpoint', ['usage']],
    ['Status', ['503']],
    ['Error', ['usage request rejected with status 503']],
  ]);
  // The upstream's own request id keeps its underscores, as the request drawer prints it.
  const cut = { attempt: 2, bytes_relayed: 18_432, code: 'stream_stalled', text: 'no chunk for 30 s', upstream_request_id: 'req_011CUcut_7a' };
  const own = { model: 'claude-sonnet-4-5', requestId: 'req_5e2f7a19-b03', sessionId: 's1-0000-4000' };
  assert.deepEqual(pairs('stream_interrupted', cut, own).slice(2), [
    ['Request', ['req_5e2f7a19-b03']],
    ['Code', ['stream_stalled']],
    ['Text', ['no chunk for 30 s']],
    ['Bytes relayed', ['18,432']],
    ['Upstream request', ['req_011CUcut_7a']],
    ['Attempt', ['2']],
  ]);
  const request = d.detailPairs(event('stream_interrupted', cut, own), NOW, [], PRO, NO_TIERS).find((p) => p.label === 'Request').chips[0];
  assert.equal(request.href, '/requests/req_5e2f7a19-b03');
  assert.deepEqual(pairs('stream_interrupted', { ...cut, upstream_request_id: null }, own).map(([label]) => label).slice(2), ['Request', 'Code', 'Text', 'Bytes relayed', 'Attempt'], 'a head with no request id draws no Upstream request');
});

test("a stream cut's code prints in the mono face, as the request drawer's Error row prints it", () => {
  const cut = { attempt: 2, bytes_relayed: 18_432, code: 'stream_stalled', text: 'no chunk for 30 s', upstream_request_id: 'req_011CUcut_7a' };
  const code = d.detailPairs(event('stream_interrupted', cut, { requestId: 'req_5e2f7a19-b03' }), NOW, [], PRO, NO_TIERS).find((p) => p.label === 'Code').chips;
  assert.deepEqual(code.map((c) => [c.value, c.mono === true]), [['stream_stalled', true]]);
});

test("a cut stream's chip reads its code without the stream stem", () => {
  const cut = (code) => read(event('stream_interrupted', { attempt: 1, bytes_relayed: 846, code, text: 'no chunk for 30 s', upstream_request_id: null }, { requestId: 'req_5e2f7a19-b03' })).by;
  assert.deepEqual([cut('stream_stalled'), cut('account_canceled')], ['stalled', 'account canceled']);
});

test("a cooldown under the relay's Fable key reads the Fable week", () => {
  const fable = (scope) => event('cooldown_set', { reason: 'rate_limit', scope, until: ms(15, 42), window: 'seven_day_overage_included' });
  // The relay files a Fable model's 429 on its own week under the model's scope, and a shared status's under the
  // account, so both read the 429 where the model's other cooldowns read its 404.
  assert.deepEqual([read(fable('account')).by, read(fable('model:claude-fable-5-1')).by], ['429 on Fable', '429 on Fable']);
  assert.equal(read(event('cooldown_set', { reason: 'model_rejected', scope: 'model:claude-fable-5-1', until: ms(15, 24), window: 'model_rejection' })).by, '404 on fable-5-1');
  const window = d.detailPairs(fable('account'), NOW, [], PRO, NO_TIERS).find((p) => p.label === 'Window');
  assert.deepEqual(texts(window.chips), ['Fable']);
  const moved = event('pin_migrated', { agent_id: 'main', cause: { reason: 'rate_limit', scope: 'account', window: 'seven_day_overage_included' }, epoch: 54, from_account: 'secondary', reason: 'cooldown' });
  assert.deepEqual(texts(d.detailPairs(moved, NOW, [], PRO, NO_TIERS).find((p) => p.label === 'Cause').chips), ['429 on Fable']);
});

test("a spent window's chip names its source", () => {
  const spent = (source) => read(event('window_spent', { reset: ms(15, 42), source, utilization: 1, window: 'five_hour' })).by;
  assert.deepEqual([spent('predicted'), spent('stored')], ['prediction on 5h', 'stored on 5h']);
});

test('Fact words a cause and leaves an id as stored', () => {
  const row = decision({ kind: 'renewed', fact: [fact('cause', 'class_auxiliary', 'text'), fact('over', 'team_a', null, { id: 'team_a', kind: 'account' })] });
  assert.deepEqual(read(row).facts, ['cause auxiliary class', 'over team_a']);
});

test("a cleared mark's chip reads its reason", () => {
  assert.equal(read(event('mark_cleared', { agent_id: 'main', limit: null, marked_at: ms(13, 0), marked_on: 'primary', marked_window: 'five_hour', projection: null, reason: 'moved' })).by, 'moved');
});

test("an account's return to ready is neutral", () => {
  const ready = event('account_state_changed', { cause: 'funding_retry', detail: null, from: 'funding_required', to: 'ready' });
  const refused = event('account_state_changed', { cause: 'auth_failed', detail: null, from: 'ready', to: 'reauth_required' });
  assert.deepEqual([d.eventTone(ready), d.eventTone(refused)], ['neutral', 'serious']);
});

test("an ended plan and its return read their words, their preset and the read's source with the profile's three members", () => {
  assert.deepEqual([d.EVENT_KIND_LABEL.plan_ended, d.EVENT_KIND_LABEL.plan_restored], ['plan ended', 'plan restored']);
  const presetOf = (kind) => d.KIND_PRESETS.find((p) => p.members.includes(kind))?.label;
  assert.deepEqual([presetOf('plan_ended'), presetOf('plan_restored')], ['account state and windows', 'account state and windows']);
  // The router's two rows on the fixture's account, as the routing log route answers them.
  const fixture = d.decodeRoutingLogPage(JSON.parse(readFileSync(new URL('./fixtures/routing_log.json', import.meta.url), 'utf8')).body);
  const rowOf = (kind) => fixture.rows.find((r) => r.type === 'event' && r.kind === kind);
  const pairs = (row) => d.detailPairs(row, NOW, [], PRO, NO_TIERS).map((p) => [p.label, texts(p.chips)]);
  assert.deepEqual(pairs(rowOf('plan_ended')).slice(1), [
    ['Account', ['business']],
    ['Source', ['usage 403']],
    ['Organization type', ['claude free']],
    ['Rate-limit tier', ['default claude ai']],
    ['Subscription status', ['canceled']],
  ]);
  assert.deepEqual(pairs(rowOf('plan_restored')).slice(2), [
    ['Source', ['profile read']],
    ['Organization type', ['claude max']],
    ['Rate-limit tier', ['default claude max 5x']],
    ['Subscription status', ['active']],
  ]);
  // A pairing's confirm restores the plan too, and a row lacking a member draws no pair for it.
  const restored = rowOf('plan_restored');
  const { rate_limit_tier: _, ...partial } = restored.detail;
  assert.deepEqual(pairs({ ...restored, detail: { ...partial, source: 'pairing' } }).slice(2).map(([label, chips]) => `${label}: ${chips.join(' ')}`), ['Source: pairing confirm', 'Organization type: claude max', 'Subscription status: active']);
});

test("a captured drop that changed the plan reads Plan changed, and a captured drop with no such member reads no such line", () => {
  // A detail built under the console's own wire name passes a route that renames the member, so this case reads the
  // captured drops: the 7-day window's carries the member true and the Fable week's carries none.
  const fixture = d.decodeRoutingLogPage(JSON.parse(readFileSync(new URL('./fixtures/routing_log.json', import.meta.url), 'utf8')).body);
  const dropOf = (window) => fixture.rows.find((r) => r.type === 'event' && r.kind === 'window_dropped' && r.detail.window === window);
  const planChanged = (row) => d.detailPairs(row, NOW, [], PRO, NO_TIERS).filter((p) => p.label === 'Plan changed').map((p) => texts(p.chips));
  assert.deepEqual([planChanged(dropOf('seven_day')), planChanged(dropOf('seven_day_fable'))], [[['yes']], []]);
});

test("a round that walked no pin and left one out off the window that fired reads off window as its reason", () => {
  const pin = (o) => ({ agent_id: 'main', burn: null, cold: false, key_id: 'desktop', last_input_tokens: 61_000, marked: false, model: 'claude-opus-5', note: null, per_rewrite: null, planned: null, rewrite: null, runway_ms: null, session_id: 'e3b8d1f6-4a29', skip: 'off_window', ...o });
  const detail = {
    burn_average_seconds: 1_800, burn_sum: 0, ceiling: 0.95, dwell_seconds: 900, dwell_until: ms(14, 15), excess: 0.012, horizon: ms(18, 20), marks: [], outcome: 'nothing_to_mark', persisted: true,
    pins: [pin({ model: 'claude-fable-5-1', note: 'current', session_id: '89cd1c92-5f0e', skip: 'fast_lane' }), pin({})], projection: 0.97, reason: 'off_window', removed: 0, shed_cap: 0.5, target: 0,
    unit: 'window_share', utilization: 0.65, warm_sessions: 'when_needed', window: 'seven_day_fable',
  };
  const outcome = d.detailPairs(event('shedding_round', detail), NOW, [], PRO, NO_TIERS).find((p) => p.label === 'Outcome');
  // Guards: the Outcome row prints any reason through its words, and an unknown word reads neutral.
  assert.deepEqual(texts(outcome.chips), ['nothing to mark: off window']);
  assert.equal(d.verdictTone('off_window'), 'neutral');
  assert.ok(d.TONED_VERDICTS.has('off_window'), 'the word is one the scheme names a tone for, as no_runway is');
});

test('the pin moved, shedding round, cleared mark and expired reservation details read their pairs', () => {
  const pairs = (kind, detail, o) => Object.fromEntries(d.detailPairs(event(kind, detail, { requestId: 'req_1', sessionId: 's1-0000-4000', ...o }), NOW, [], PRO, NO_TIERS).map((p) => [p.label, p.table ? p.table : texts(p.chips)]));
  const moved = (more) => pairs('pin_migrated', { agent_id: 'main', epoch: 53, from_account: 'secondary', reason: 'shed', ...more });
  assert.deepEqual([moved({ binding: 'five_hour', fit: 'fits', runway_ms: 7_800_000 })['Destination fit'], moved({ binding: 'five_hour', fit: 'fits', runway_ms: 7_800_000 }).Runway], [['fits'], ['2 h 10 min']]);
  assert.deepEqual([moved({ binding: 'five_hour', fit: 'least_bad', runway_ms: 0 })['Destination fit'], moved({ binding: 'five_hour', fit: 'least_bad', runway_ms: 0 }).Runway], [['least bad'], ['none']]);
  assert.deepEqual([moved({ binding: null, fit: 'rewrite_only', runway_ms: null })['Destination fit'], moved({ binding: null, fit: 'rewrite_only', runway_ms: null }).Runway], [['rewrite only'], ['unbounded']]);
  const landed = moved({ binding: null, fit: 'spent', reason: 'cooldown', runway_ms: null });
  assert.deepEqual([landed['Destination fit'], landed.Runway], [['window spent'], undefined], "a failover's landing on a member with no allowance left states its fit and no runway");
  assert.deepEqual(Object.keys(moved({})), ['When', 'Account', 'From', 'To', 'Reason', 'Moved by', 'Session', 'Agent'], 'a move the ledger wrote before the gate reads neither pair');
  // A failover's cause reads as the cooldown's own words, and a rule's move or a cause-less failover draws no Cause row.
  const cooled = moved({ cause: { reason: 'rate_limit', scope: 'account', window: 'five_hour' }, reason: 'cooldown' });
  assert.deepEqual([cooled.Cause, cooled['Moved by']], [['429 on 5h'], ['failover']]);
  assert.deepEqual(moved({ cause: { reason: 'fast_rate_limit', scope: 'fast', window: 'fast' }, reason: 'fast_lane' }).Cause, ['fast lane 429']);
  assert.deepEqual(Object.keys(cooled), ['When', 'Account', 'From', 'To', 'Reason', 'Cause', 'Moved by', 'Session', 'Agent']);
  assert.equal(moved({ cause: null, reason: 'mark' }).Cause, undefined);

  const pin = (o) => ({ agent_id: 'c21e', burn: 0.07, cold: false, key_id: 'desktop', last_input_tokens: 48_100, marked: false, model: 'claude-sonnet-5', note: null, per_rewrite: 0.004, planned: null, rewrite: null, runway_ms: null, session_id: 'bcc805b6-0588', skip: null, ...o });
  const round = (more) => pairs('shedding_round', { burn_average_seconds: 1_800, burn_sum: 0.146, ceiling: 0.95, dwell_seconds: 900, dwell_until: ms(14, 15), excess: 0.095, horizon: ms(18, 20), marks: [], outcome: 'nothing_to_mark', persisted: true, projection: 0.99, reason: 'no_runway', removed: 0.073, shed_cap: 0.5, target: 0.075, unit: 'window_share', utilization: 0.9, warm_sessions: 'when_needed', window: 'five_hour', ...more });
  const skipped = round({ pins: [pin({ skip: 'no_runway' }), pin({ agent_id: 'main', session_id: 'd7167642-751d', skip: 'no_runway' })] });
  assert.deepEqual([skipped.Trigger, skipped.Excess, skipped.Cap], [['5h projection 99% at 18:20 over the ceiling 95%'], ['9.5 %/h'], ['50%', 'target 7.5 %/h', 'removed 7.3 %/h']]);
  assert.deepEqual([skipped.Unit, skipped['Burn average'], skipped.Skipped, skipped['Planned destinations'], skipped.Outcome], [['window share'], ['30 min'], ['bcc805b6: no runway', 'd7167642: no runway'], undefined, ['nothing to mark: no runway']]);
  assert.deepEqual([skipped['Next round after'], skipped.Dwell, skipped['Survive a restart']], [['14:15'], ['15 min'], ['persisted']]);
  assert.deepEqual(skipped.Pins.head, ['Session', 'Agent', 'Warmth', 'Burn', 'Last prompt', 'Rewrite', 'Destination', 'Mark']);
  assert.deepEqual(skipped.Pins.rows.map((r) => r.map(chipText)), [['bcc805b6', 'c21e', 'warm', '7.0 %/h', '48.1K', '-', '-', 'no runway'], ['d7167642', 'main', 'warm', '7.0 %/h', '48.1K', '-', '-', 'no runway']]);
  // A round on the 7-day window projects one 5-hour length ahead, which reads as the span.
  const weekly = round({ horizon: ms(14, 0) + 5 * HOUR, outcome: 'marked 2', pins: [pin({ marked: true, planned: 'nightly', rewrite: 0.004 }), pin({ marked: true, planned: 'secondary', rewrite: 0.003, session_id: 'e2f0a1b3-0000' })], projection: 0.96, reason: null, window: 'seven_day' });
  assert.deepEqual([weekly.Trigger, weekly['Planned destinations'], weekly.Outcome, weekly.Skipped], [['7d projection 96% in 5 h over the ceiling 95%'], ['nightly', 'secondary'], ['marked 2'], undefined]);
  assert.deepEqual(weekly.Pins.rows.map((r) => r.slice(5).map(chipText)), [['0.4%', 'nightly', 'marked'], ['0.3%', 'secondary', 'marked']]);
  // The share floor a sliver is weighed against reads beside the cap.
  assert.deepEqual(round({ mark_share: 0.1 })['Mark share'], ['10%']);

  // A round written before the projection trigger carries per-second rates in its own unit and a threshold in place of
  // a ceiling: it keeps its conversion and its reading, so a burn of 0.0001 a second reads 36 %/h or $0.36/h.
  const legacy = (unit) =>
    Object.fromEntries(
      d.detailPairs(
        event(
          'shedding_round',
          {
            ahead: 0.91, burn_sum: 0.0005, burn_window_seconds: 600, dwell_seconds: 900, dwell_until: ms(14, 15), elapsed_share: 0.06, margin: 0.01, marks: [], persisted: true,
            pins: [{ agent_id: 'c21e', burn: 0.0001, cold: true, key_id: 'desktop', last_input_tokens: 48_100, marked: true, model: 'claude-sonnet-5', note: null, per_rewrite: 0.0001 / 48_100, session_id: 's1' }],
            shed_share: 0.5, target: 0.00025, threshold: 0.9, unit, utilization: 0.97, warm_sessions: 'when_needed', window: 'five_hour',
          },
          { requestId: 'req_1', sessionId: 's1-0000-4000' },
        ),
        NOW,
        [],
        PRO,
        NO_TIERS,
      ).map((p) => [p.label, p.table ? p.table.rows.map((r) => r.map(chipText)) : texts(p.chips)]),
    );
  const share = legacy('window_share');
  assert.deepEqual(Object.keys(share), ['When', 'Account', 'Started by', 'Reading', 'Unit', 'Burn window', 'Share', 'Warm sessions', 'Pins', 'Marks kept until', 'Dwell', 'Survive a restart']);
  assert.deepEqual([share.Reading, share.Share, share.Pins], [['5h 97%', 'threshold 90%'], ['share 50%', 'burn 180.0 %/h', 'to shed 90.0 %/h'], [['s1', 'c21e', 'cold', '36.0 %/h', '48.1K', 'marked']]]);
  const dollars = legacy('usd');
  assert.deepEqual([dollars.Share, dollars.Pins[0][3]], [['share 50%', 'burn $1.80/h', 'to shed $0.90/h'], '$0.36/h']);

  const cleared = (more) => pairs('mark_cleared', { agent_id: 'main', marked_at: ms(13, 0), marked_on: 'primary', marked_window: 'five_hour', projection: null, ...more }, { keyId: 'desktop', model: 'claude-sonnet-5' });
  const healed = cleared({ projection: 0.91, reason: 'healed' });
  assert.deepEqual([healed.Reason, healed.Marked, healed['Standing for']], [['healed: the 5h projection read 91% under the ceiling'], ['13:00', 'on primary'], ['1 h']]);
  // A round that tested the wall records a ceiling of 1, the window's whole share.
  assert.deepEqual(round({ ceiling: 1, projection: 1.04 }).Trigger, ['5h projection 104% at 18:20 over the wall']);
  assert.deepEqual([cleared({ reason: 'moved' }).Reason, cleared({ reason: 'released' }).Reason], [['moved'], ['released']]);

  const expired = pairs('reservation_expired', { age_ms: 42_000, attempt: 1, cause: 'transport_error', shares: { five_hour: 0.104, seven_day: 0.012 } });
  assert.deepEqual(['Request', 'Attempt', 'Cause', 'Reserved', 'Held for', 'Effect'].map((label) => expired[label]), [['req_1'], ['1'], ['transport error'], ['5h 10.4%', '7d 1.2%'], ['42 s'], ["the booked share left the account's prediction"]]);
  for (const cause of ['canceled', 'exhausted', 'refused', 'timeout']) assert.deepEqual(pairs('reservation_expired', { age_ms: 1_000, attempt: 2, cause, shares: {} }).Cause, [cause]);

  assert.deepEqual(pairs('window_spent', { reset: ms(15, 42), source: 'predicted', source_request_id: 'req_1', utilization: 1, window: 'five_hour' })['Read from'], ['the prediction']);
});

test("the hash chip is eight characters and warm-until reads the entry's TTL", () => {
  assert.equal(d.shortHash(HASH_B), '34617434');
  assert.equal(d.warmUntil(entry({ ttlMs: 3_600_000, writtenAt: iso(ms(14, 20)) }), NOW), 'warm until 15:20');
  assert.equal(d.warmUntil(entry({ ttlMs: 300_000, writtenAt: iso(ms(14, 20)) }), NOW), 'warm until 14:25');
  assert.equal(d.warmUntil(entry({ ttlMs: 0 }), NOW), null);
  assert.equal(d.hashTitle(entry({})), HASH_B);
  assert.equal(d.hashTitle(entry({ refusedBy: 'band' })), `${HASH_B}\nrefused by the band`);
});

test('the rung line and the prefix cell read the matched entry, not the deeper refused one', () => {
  const primary = candidate({ prefix: { ...NO_PREFIX, static: entry({ depth: 3, hash: HASH_A, live: false, prefixTokens: 33_900, refusedBy: 'expired', writtenAt: iso(ms(13, 5, 10)) }), staticMatch: entry({}) }, verdict: 'chosen' });
  const nightly = candidate({ accountId: 'nightly', prefix: { ...NO_PREFIX, static: entry({ hash: HASH_C, refusedBy: 'band', writtenAt: iso(ms(13, 30)) }) }, verdict: 'not_needed' });
  const r = record({
    candidates: [primary, nightly],
    outcome: { rung: { depth: 2, prefixTokens: 31_200, rung: 'static' } },
    walk: [wstep('rung:content', 'none', [wfact('reason', 'no_entry')]), wstep('rung:static', 'match', [wfact('depth', 2, 'count'), wfact('prefix_tokens', 31_200, 'tokens'), wfact('account_id', 'primary', null, 'primary'), wfact('hash', HASH_B)])],
  });
  const line = d.walkLines(r).find((l) => l.step === 'Rung: static');
  assert.equal(line.hash.hash, HASH_B);
  assert.equal(d.shortHash(line.hash.hash), '34617434');
  assert.equal(d.warmUntil(line.hash, NOW), 'warm until 14:58');
  const mine = d.candidateCells(primary, r, NOW).prefix;
  assert.deepEqual([mine.hash.hash, mine.until], [HASH_B, 'warm until 14:58']);
  const theirs = d.candidateCells(nightly, r, NOW).prefix;
  assert.deepEqual([theirs.hash.hash, theirs.hash.refusedBy, theirs.until], [HASH_C, 'band', 'warm until 14:30']);
  assert.ok(d.hashTitle(theirs.hash).includes('band'));
  assert.deepEqual(d.candidateCells(candidate({ accountId: 'secondary' }), r, NOW).prefix, { hash: null, until: null });
});

test('the verdict line words the kind with its accounts and its pool as entities, then its chips', () => {
  const words = (r, row) => d.verdictSentence(r, row).map((c) => (c.entity ? `[${c.entity.kind} ${c.value}]` : c.value)).join(' ');
  const renewed = record({ candidates: [candidate({ verdict: 'renewed' })], outcome: { kind: 'renewed' }, pin: { accountId: 'primary', createdAt: iso(ms(13, 58)), epoch: 41, found: 'live', idleMs: 161_000, incarnation: 1, lane: 'x|standard|enabled|high', lastRefreshAt: iso(ms(14, 24)), lastUsedAt: iso(ms(14, 24)), marked: false, refreshAgeMs: 161_000, released: [], shed: null, warmth: 'refresh', warmthCause: 'continues_pin' }, pinReason: null });
  assert.equal(words(renewed, decision({ kind: 'renewed' })), 'Renewed on [account primary]');
  assert.deepEqual(texts(d.verdictChips(renewed, decision({ kind: 'renewed' }))), ['tier plan quota P0', 'no rule fired', 'pin warm']);
  const moved = record({ outcome: { fromAccountId: 'secondary', kind: 'moved', rule: 'spent', rung: { depth: 2, prefixTokens: 31_200, rung: 'static' } } });
  assert.equal(words(moved, decision({ by: by('rule', 'spent'), kind: 'moved' })), 'Moved [account secondary] to [account primary]');
  const failover = record({ candidates: [candidate({ verdict: 'chosen' })], outcome: { completion: completion({ cacheReadTokens: 31_000, completedAt: iso(NOW), hit: true, inputTokens: 200, promisedTokens: 31_200, status: 200 }), rung: { depth: 2, prefixTokens: 31_200, rung: 'static' } } });
  const attempt = decision({ attempt: 3, attemptCount: 3, by: by('rung', 'static') });
  assert.equal(words(failover, attempt), 'Attempt 3 of 3: placed on [account primary]');
  assert.deepEqual(texts(d.verdictChips(failover, attempt)), ['tier plan quota P0', 'static rung', 'depth 2', 'cache hit 31K of 31.2K promised']);
  assert.equal(words(record({ outcome: { hold: { accountId: 'primary', boundMs: 10_000, cohort: 'primary' }, kind: 'held' } }), decision({ kind: 'held' })), 'Held on [account primary]');
  assert.equal(
    words(record({ outcome: { hold: { accountId: 'nightly', boundMs: 120_000, cohort: null, reason: 'rewrite', until: iso(ms(11, 3)), waitsFor: 'req_5b2e8d47-c19' }, kind: 'held' } }), decision({ kind: 'held' })),
    'Held on [account nightly] for the rewrite in flight of req_5b2e8d47-c19 until 11:03',
  );
  assert.equal(words(record({ outcome: { accountId: null, kind: 'exhausted' }, request: { poolId: 'jobs' } }), decision({ kind: 'exhausted' })), 'Exhausted in [pool jobs]');
  assert.equal(words(record({ outcome: { accountId: null, kind: 'refused' } }), decision({ kind: 'refused' })), 'Refused before selection');
  const pending = record({ outcome: { completion: completion({}) } });
  assert.deepEqual(texts(d.verdictChips(pending, decision({ by: strategy('pace', [0, 0, 0]) }))).slice(-1), ['cache pending']);
});

test('a move draws the account-bound thinking its answer dropped, and no chip for the prefix entry', () => {
  const PREFIX = 'thinking_dropped.prefix_binding_mismatch';
  // A failover's landing is a placement whose pin moved, so the move reads off its pin event and never its kind.
  const landing = (inputTransformations) => record({ outcome: { completion: completion({ completedAt: iso(NOW), inputTransformations, status: 200 }), kind: 'placed', pinEvents: [{ kind: 'pin_migrated', reason: 'cooldown' }] } });
  const note = (r) => d.verdictChips(r, decision({})).find((c) => c.label === 'account-bound thinking');
  const dropped = note(
    landing({
      'thinking_dropped.end_user_binding_mismatch': { blocks: 153, firstMessage: 2 },
      'thinking_dropped.organization_binding_mismatch': { blocks: 1, firstMessage: 4 },
      [PREFIX]: { blocks: 1, firstMessage: 2 },
    }),
  );
  assert.deepEqual([dropped?.value, dropped?.title], ['154 blocks', 'Dropped, other end user: 153 blocks\nDropped, other organization: 1 block']);
  assert.equal(note(landing({ [PREFIX]: { blocks: 1, firstMessage: 2 } })), undefined, "the client's standing prefix entry is no block the move cost");
});

test('a placement that created its pin draws no chip where its answer lists an account key', () => {
  // The router places a session behind an expired pin with a new pin and no move, while its answer still lists the key.
  const placed = record({ outcome: { completion: completion({ completedAt: iso(NOW), inputTransformations: { 'thinking_dropped.end_user_binding_mismatch': { blocks: 1, firstMessage: 2 } }, status: 200 }), kind: 'placed', pinEvents: [{ kind: 'pin_created', reason: null }] } });
  const note = d.verdictChips(placed, decision({})).find((c) => c.label === 'account-bound thinking');
  assert.equal(note, undefined, 'the account key stands on each answer from the move to the next compaction');
});

test('the settings fold lists twenty-six rows, draws the five as not recorded on a record from before, and a return dwell of 0 as at once', () => {
  const rows = (settings) => d.settingsRows(record({ settings })).map((r) => [r.label, texts(r.chips)]);
  assert.deepEqual(rows(SETTINGS).flatMap(([, chips]) => chips), [
    'Pace', 'subscriptions first', 'prefer plan', '10 min', 'when cached', 'by warm prefix', '20K', 'by cohort', '95%', '100%', 'on', 'on', '30 min', 'when cold', '15 min', '30 min', '50%', '30 min', '15 min',
    'on', 'on', '30 min', '2 min', 'when needed', '16', '1 min',
  ]);
  assert.deepEqual(rows({ ...SETTINGS, loadShedding: false }).find(([label]) => label === 'Load shedding'), ['Load shedding', ['off']]);
  // Under always a dwell of 0 returns a warm pin at once, where a zero span elsewhere reads the setting off.
  const atOnce = rows({ ...SETTINGS, returnToPlan: 'always', returnToPlanDwellSeconds: 0, shedDwellSeconds: 0 });
  assert.deepEqual(['Return to plan', 'Return dwell', 'Shed dwell'].map((label) => atOnce.find(([l]) => l === label)), [['Return to plan', ['always']], ['Return dwell', ['at once']], ['Shed dwell', ['off']]]);
  assert.deepEqual(rows({ ...SETTINGS, slowDown: false }).find(([label]) => label === 'Slow-down'), ['Slow-down', ['off']]);
  const before = rows({ ...SETTINGS, ...BEFORE_SLOW_DOWN });
  assert.deepEqual(['Return to plan', 'Return dwell', 'Slow-down', 'Stretch', 'Hold'].map((label) => before.find(([l]) => l === label)), ['Return to plan', 'Return dwell', 'Slow-down', 'Stretch', 'Hold'].map((label) => [label, ['not recorded']]));
  assert.equal(before.filter(([, [value]]) => value === 'not recorded').length, 5, 'the switch and the ceilings a record from version 2 on carries read as recorded');
  const legacy = rows(LEGACY_SETTINGS);
  assert.deepEqual(legacy.slice(8, 11), [['5-hour ceiling', ['not recorded']], ['Weekly ceiling', ['not recorded']], ['Load shedding', ['not recorded']]]);
  assert.deepEqual(legacy.slice(-3), [['Shed above (then)', ['90%']], ['Select below (then)', ['off']], ['Pace floor (then)', ['5%']]]);
  assert.equal(rows(SETTINGS).length, 26);
  const unwritten = rows({ ...SETTINGS, fableReservationShedding: null });
  assert.deepEqual([unwritten.length, unwritten.some(([label]) => label === 'Shed on the Fable reservation')], [25, false], 'a record stored before the option draws no row for it, since its pool may have held the option on');
  const refused = record({ outcome: { accountId: null, kind: 'refused', refusal: { code: 'contract', kind: 'client', reason: 'the request has no session header' }, strategy: '' } });
  assert.equal(d.settingsRows(refused), null, 'a refusal entered no setting, so it lists none');
});

test('the event detail draws the pairs its event carries: the effect, the meaning, the reading and the backoff span', () => {
  const pairs = (kind, detail, o) => Object.fromEntries(d.detailPairs(event(kind, detail, o), NOW, [], PRO, NO_TIERS).map((p) => [p.label, texts(p.chips)]));
  assert.deepEqual(pairs('account_state_changed', { cause: 'funding_refused', detail: null, from: 'ready', to: 'funding_required' }).Effect, ['the account leaves every eligible set until funded']);
  assert.deepEqual(pairs('account_state_changed', { cause: 'deleted', detail: null, from: 'ready', to: 'deleted' }).Effect, ['the account left the registry and this is its last row']);
  assert.deepEqual(pairs('account_state_changed', { cause: 'operator', detail: null, from: 'funding_required', to: 'ready' }).Effect, ['the account joins the eligible sets']);
  assert.deepEqual(pairs('account_state_changed', { cause: 'paired', detail: null, from: 'unpaired', to: 'ready' }).Effect, ['the account joins the eligible sets']);
  assert.deepEqual(pairs('window_spent', { reset: ms(15, 42), source: 'head', utilization: 1, window: 'five_hour' }).Effect, ['a pinned session moves by the spent rule at its next request or waits in place for a reset within the retry bound']);
  const skipped = pairs('ledger_gap', { reason: 'chunk_skipped', request_id: 'req_1' }, { requestId: 'req_1' });
  assert.deepEqual([skipped.Meaning, skipped.Effect], [['an interior chunk of the capture was dropped at the bus'], ['the captured response is stored truncated']]);
  const missing = pairs('ledger_gap', { reason: 'attempt_missing', request_id: 'req_1' }, { requestId: 'req_1' });
  assert.deepEqual([missing.Meaning, missing.Effect], [['the bus dropped an account selection and the trail is short an attempt row'], undefined]);
  for (const reason of ['attempt_missing', 'capture_budget', 'chunk_dropped', 'chunk_skipped', 'expired', 'in_flight_cap', 'poison']) assert.ok(pairs('ledger_gap', { reason }).Meaning?.[0], reason);
  const edit = pairs('registry_changed', { diff: { pace_floor: { from: 0.05, to: 0.1 }, shed_ahead_of_pace: { from: null, to: 0.9 } }, kind: 'pool_updated', target: 'all' }, { accountId: null, actor: 'admin', at: iso(ms(9, 5, 12)), poolId: 'all' });
  // A null old value reads one word in the field's own row and in the reading.
  assert.deepEqual(edit['shed ahead of pace (removed)'], ['from none', 'to 90%']);
  assert.deepEqual(edit['Reading it'], ['decisions before 09:05 carry pace floor (removed) 5%', 'decisions before 09:05 carry shed ahead of pace (removed) none']);
  assert.deepEqual(pairs('poll_backoff_ended', { attempts: 3, duration_ms: 25 * MINUTE, resumed_at: ms(8, 15, 3) })['Backed off'], ['from 07:50', 'to 08:15']);
});

test("a backoff's close reads by its cause, and a window reset names where the reset was read from", () => {
  const pairs = (kind, detail) => d.detailPairs(event(kind, detail), NOW, [], PRO, NO_TIERS).map((p) => [p.label, texts(p.chips)]).slice(2);
  const by = (detail) => d.byOf(event('poll_backoff_ended', detail), []).value;
  const close = { attempts: 3, duration_ms: 25 * MINUTE, resumed_at: ms(8, 15, 3) };
  const resumed = [['Backed off', ['from 07:50', 'to 08:15']], ['Attempts', ['3']], ['Resumed', ['08:15']]];
  assert.deepEqual(pairs('poll_backoff_ended', { ...close, cause: 'clean_poll' }), resumed);
  assert.deepEqual(pairs('poll_backoff_ended', close), resumed, 'a row written before the cause reads as a clean poll');
  // The plan's end pauses the poll, so the close names the pause's instant in place of a resumption.
  const ended = { attempts: 1, cause: 'plan_ended', duration_ms: 6 * MINUTE, resumed_at: ms(10, 30) };
  assert.deepEqual(pairs('poll_backoff_ended', ended), [['Backed off', ['from 10:24', 'to 10:30']], ['Attempts', ['1']], ["Ended by the plan's end", ['10:30']]]);
  assert.deepEqual([by({ ...close, cause: 'clean_poll' }), by(close), by(ended)], ['clean poll', 'clean poll', 'plan ended']);
  const reset = (observed) => pairs('window_reset', { next_reset: ms(19, 0), observed, reset_at: ms(14, 0), utilization_before: 0.92, window: 'five_hour' });
  assert.deepEqual(reset('sample'), [['Window', ['5h']], ['Before the reset', ['92%']], ['Read from', ['the usage sample']], ['Prediction', ['0% until the next head']], ['Next reset', ['19:00']]]);
  assert.deepEqual(reset('clock')[2], ['Read from', ['the clock walk']]);
});

test("an account edit's drawer reads each pair and the replaced secret beside them, with no reading of decisions", () => {
  const pairs = (diff, kind = 'account_updated', target = 'annex') =>
    d.detailPairs(event('registry_changed', { diff, kind, target }, { accountId: null, actor: 'admin', at: iso(ms(9, 8, 30)) }), NOW, [], PRO, NO_TIERS).map((p) => [p.label, texts(p.chips)]);
  assert.deepEqual(pairs({ label: { from: 'annex', to: 'Studio annex' }, secret_replaced: true }), [
    ['When', [d.fmtAbsolute(iso(ms(9, 8, 30)))]],
    ['Change', ['account edited', 'annex']],
    ['label', ['from annex', 'to Studio annex']],
    ['Secret', ['replaced']],
    ['Actor', ['admin']],
  ]);
  assert.deepEqual(pairs({ secret_replaced: true }).slice(1), [['Change', ['account edited', 'annex']], ['Secret', ['replaced']], ['Actor', ['admin']]], 'the flag alone');
  // A decision record carries the pool's settings it was decided under, so a pool's edit alone reads one.
  const key = pairs({ enabled: { from: true, to: false }, expires_at: { from: null, to: MIDNIGHT + 9 * 24 * HOUR } }, 'key_updated', 'ci');
  assert.deepEqual(key.slice(1), [['Change', ['key edited', 'ci']], ['enabled', ['from on', 'to off']], ['expires at', ['from none', 'to Sep 26 00:00']], ['Actor', ['admin']]]);
  const end = pairs({ canceled: { from: false, to: true }, subscription_ends_at: { from: null, to: ms(18, 0) } }, 'account_updated', 'secondary');
  assert.deepEqual(end.slice(2), [['canceled', ['from off', 'to on']], ['subscription ends at', ['from none', 'to 18:00']], ['Actor', ['admin']]]);
  const pool = pairs({ five_hour_ceiling: { from: 0.95, to: 0.75 } }, 'pool_updated', 'all');
  assert.deepEqual(pool.slice(-1), [['Reading it', ['decisions before 09:08 carry five hour ceiling 95%']]], 'a pool edit keeps its reading');
  const listed = pairs({ guardrails: { from: [], to: ['no-secrets', 'size-cap'] } }, 'pool_updated', 'all');
  assert.deepEqual(listed[2], ['guardrails', ['from none', 'to no-secrets, size-cap']], 'a list reads its items where the count stood');
});

test('a registry change reads the renamed pool settings by their new word, marks the removed ones and reads each fraction as a percent', () => {
  const pool = { accountId: null, actor: 'admin', at: iso(ms(9, 5, 12)), poolId: 'all' };
  const change = (diff) => event('registry_changed', { diff, kind: 'pool_updated', target: 'all' }, pool);
  const pairs = (diff) => Object.fromEntries(d.detailPairs(change(diff), NOW, [], PRO, NO_TIERS).map((p) => [p.label, texts(p.chips)]));
  // A row written while the pool held one ceiling for every window keeps its name, and its share reads as a percent.
  assert.deepEqual(read(change({ window_ceiling: { from: 0.95, to: 0.9 } })), { by: 'pool edited', facts: ['field window ceiling', 'to 90%'] });
  assert.deepEqual(read(change({ shed_cap: { from: 0.5, to: 0.2 } })), { by: 'pool edited', facts: ['field shed cap', 'to 20%'] });
  assert.deepEqual(read(change({ shed_share: { from: 0.5, to: 0.25 } })), { by: 'pool edited', facts: ['field shed cap (then shed_share)', 'to 25%'] });
  assert.deepEqual(read(change({ pace_floor: { from: 0.05, to: 0.1 } })), { by: 'pool edited', facts: ['field pace floor (removed)', 'to 10%'] });
  const renamed = pairs({ burn_window_seconds: { from: 1_800, to: 900 }, shed_share: { from: 0.5, to: 0.25 }, source_dwell_seconds: { from: 900, to: 600 } });
  assert.deepEqual(renamed['burn average seconds (then burn_window_seconds)'], ['from 1800', 'to 900']);
  assert.deepEqual(renamed['shed cap (then shed_share)'], ['from 50%', 'to 25%']);
  assert.deepEqual(renamed['shed dwell seconds (then source_dwell_seconds)'], ['from 900', 'to 600']);
  const removed = pairs({ pace_floor: { from: 0.05, to: 0.1 }, select_below_utilization: { from: null, to: 0.8 }, shed_ahead_of_pace: { from: 0.9, to: 0.85 } });
  assert.deepEqual(removed['pace floor (removed)'], ['from 5%', 'to 10%']);
  assert.deepEqual(removed['select below utilization (removed)'], ['from none', 'to 80%']);
  assert.deepEqual(removed['shed ahead of pace (removed)'], ['from 90%', 'to 85%']);
  const current = pairs({ five_hour_ceiling: { from: 0.95, to: 0.9 }, shed_cap: { from: 0.5, to: 0.2 }, weekly_ceiling: { from: 1, to: 0.8 } });
  assert.deepEqual([current['five hour ceiling'], current['shed cap'], current['weekly ceiling']], [['from 95%', 'to 90%'], ['from 50%', 'to 20%'], ['from 100%', 'to 80%']]);
  assert.deepEqual(current['Reading it'], ['decisions before 09:05 carry five hour ceiling 95%', 'decisions before 09:05 carry shed cap 50%', 'decisions before 09:05 carry weekly ceiling 100%']);
});

test("a pool save's keepalive policy reads as its dotted fields, each under its label, and no decision reads them", () => {
  const pool = { accountId: null, actor: 'admin', at: iso(ms(9, 5, 12)), poolId: 'all' };
  const change = (diff) => event('registry_changed', { diff, kind: 'pool_updated', target: 'all' }, pool);
  const pairs = (diff) => d.detailPairs(change(diff), NOW, [], PRO, NO_TIERS).map((p) => [p.label, texts(p.chips)]).slice(2);
  // A decision record carries no keepalive policy, so a policy-only save reads no Reading it row.
  assert.deepEqual(pairs({ 'keepalive.max_idle_seconds': { from: 43_200, to: 86_400 }, 'keepalive.enabled': { from: null, to: true } }), [
    ['Change', ['pool edited', 'all']],
    ['keepalive idle window seconds', ['from 43200', 'to 86400']],
    ['keepalive', ['from none', 'to on']],
    ['Actor', ['admin']],
  ]);
  const beside = Object.fromEntries(pairs({ 'keepalive.max_idle_seconds': { from: 43_200, to: 3_600 }, min_shared_prefix_tokens: { from: 10_000, to: 20_000 } }));
  assert.deepEqual(beside['Reading it'], ['decisions before 09:05 carry min shared prefix tokens 10000'], 'a setting beside the policy keeps its reading');
});

test("a model rejection reads its until off the page's cooldown row and has no pair without one", () => {
  const at = iso(ms(14, 12, 5));
  const rejection = event('model_rejected', { attempt: 1, request_id: 'req_011', source_request_id: 'req_011', wire_model: 'claude-haiku-4-5' }, { at, model: 'claude-haiku-4-5-20251001', requestId: 'req_011' });
  const cooldown = (o = {}, detail = {}) => event('cooldown_set', { reason: 'model_rejected', scope: 'model:claude-haiku-4-5-20251001', source_request_id: 'req_011', until: ms(15, 24), window: 'model', ...detail }, { at, id: 8, requestId: 'req_011', ...o });
  const until = (rows) => d.detailPairs(rejection, NOW, rows, PRO, NO_TIERS).find((p) => p.label === 'Until')?.chips.map(chipText);
  assert.deepEqual(until([cooldown()]), ['15:24']);
  assert.deepEqual(d.detailPairs(rejection, NOW, [cooldown()], PRO, NO_TIERS).slice(2).map((p) => p.label), ['Model sent', 'Model resolved', 'Cooldown scope', 'Attempt', 'Until', 'Request']);
  assert.deepEqual(until([decision({ at }), cooldown()]), ['15:24'], 'the decisions beside it are passed over');
  assert.equal(until([]), undefined, 'no cooldown row on the page');
  assert.equal(until([cooldown({ accountId: 'secondary' })]), undefined, "another account's cooldown");
  assert.equal(until([cooldown({ requestId: 'req_012' }, { source_request_id: 'req_012' })]), undefined, "another request's cooldown");
  assert.equal(until([cooldown({}, { scope: 'model:claude-sonnet-5' })]), undefined, 'a cooldown on another model');
  assert.equal(until([cooldown({}, { scope: 'account' })]), undefined, 'a cooldown on the whole account');
});

test("a model rejection reads the until off its own request's cooldown row, whatever instant the router dated each", () => {
  const at = ms(14, 12, 5);
  const rejection = (o = {}) => event('model_rejected', { attempt: 1, request_id: 'req_011', source_request_id: 'req_011', wire_model: 'claude-haiku-4-5' }, { at: iso(at), model: 'claude-haiku-4-5-20251001', requestId: 'req_011', ...o });
  const cooldown = (o = {}, detail = {}) => event('cooldown_set', { reason: 'model_rejected', scope: 'model:claude-haiku-4-5-20251001', source_request_id: 'req_011', until: ms(15, 24), window: 'model', ...detail }, { at: iso(at + 3), id: 8, requestId: 'req_011', ...o });
  const until = (row, rows) => d.detailPairs(row, NOW, rows, PRO, NO_TIERS).find((p) => p.label === 'Until')?.chips.map(chipText);
  assert.deepEqual(until(rejection(), [cooldown()]), ['15:24'], 'the router dated the cooldown 3 ms after the rejection');
  assert.deepEqual(until(rejection(), [cooldown({ at: iso(at - 2) })]), ['15:24'], 'and 2 ms before it');
  assert.equal(until(rejection(), [cooldown({ requestId: 'req_012' }, { source_request_id: 'req_012' })]), undefined, "another request's cooldown a few milliseconds on");
  assert.equal(until(rejection({ requestId: null }), [cooldown({ requestId: null }, { source_request_id: null })]), undefined, 'a rejection that names no request reads no cooldown');
});

test("the catalog change lists its models in the cost model's tier order, newest first", () => {
  const pairs = Object.fromEntries(d.detailPairs(event('catalog_changed', { added: ['claude-haiku-4-5', 'claude-opus-5', 'claude-fable-5-1', 'claude-opus-5-5'], count: 12, removed: ['claude-sonnet-4-5', 'claude-opus-4-5', 'claude-sonnet-5'] }), NOW, [], PRO, SHIPPED_TIERS).map((p) => [p.label, texts(p.chips)]));
  assert.deepEqual(pairs.Added, ['claude-fable-5-1', 'claude-opus-5-5', 'claude-opus-5', 'claude-haiku-4-5']);
  assert.deepEqual(pairs.Removed, ['claude-opus-4-5', 'claude-sonnet-5', 'claude-sonnet-4-5']);
});

test('the verdict line reads the cache pending only while the answer has not ended, and keys a landing depth by its rung', () => {
  const chips = (r, row) => texts(d.verdictChips(r, row));
  const ended = record({ outcome: { completion: completion({ completedAt: iso(ms(14, 27, 30)), status: 200 }) } });
  assert.ok(!chips(ended, decision({ by: strategy('pace', [0, 0, 0]) })).some((c) => c.includes('pending')));
  const open = record({ outcome: { completion: completion({}) } });
  assert.equal(chips(open, decision({ by: strategy('pace', [0, 0, 0]) })).at(-1), 'cache pending');
  const moved = record({ candidates: [candidate({ verdict: 'chosen' })], outcome: { fromAccountId: 'secondary', kind: 'moved', rule: 'spent', rung: { depth: 2, prefixTokens: 31_200, rung: 'static' } } });
  const drawn = d.verdictChips(moved, decision({ by: by('rule', 'spent'), kind: 'moved' }));
  assert.deepEqual(texts(drawn), ['tier plan quota P0', 'spent rule', 'static rung depth 2']);
  assert.equal(drawn[2].title, d.PLACED.static(2));
});

test('the card carries the hover hints of the hold, the promise and the placement', () => {
  const held = record({ outcome: { hold: { accountId: 'primary', boundMs: 10_000, cohort: 'primary' }, kind: 'held' } });
  assert.equal(d.outcomeRows(held, NOW).find((r) => r.label === 'Hold').title, d.HOLD_HINT);
  const promised = record({ outcome: { completion: completion({ cacheReadTokens: 31_000, completedAt: iso(NOW), hit: true, inputTokens: 200, promisedTokens: 31_200, status: 200 }), rung: { depth: 2, prefixTokens: 31_200, rung: 'static' } } });
  assert.equal(d.outcomeRows(promised, NOW).find((r) => r.label === 'Cache read').title, d.PROMISE_HINT.static);
  const row = decision({ by: by('rung', 'static') });
  const verdict = d.verdictChips(promised, row);
  assert.equal(verdict.find((c) => c.value === 'static rung').title, d.PLACED.static(2));
  assert.equal(verdict.find((c) => c.label === 'cache hit').title, d.PROMISE_HINT.static);
});

test("the Pace cell's title names the share, the hours, the deadline, the burn and the spare rate, and the header names the window", () => {
  const title = (pace, o = {}) => d.candidateCells(candidate({ pace, ...o }), record(), NOW).paceTitle;
  const reset = Date.UTC(2026, 8, 19, 8, 30);
  const read = { burn: 0.004, deadline: iso(reset), end: null, fableReservation: null, hours: 66, remaining: 0.66, reset: iso(reset), score: 0.006, window: 'seven_day' };
  const tail = '. Inside its tier, the highest spare rate wins.';
  assert.equal(title(read), `66% of the 7-day window left over 66 h to its reset at Sep 19 08:30, less 0.40 %/h of burn: 0.60 %/h${tail}`);
  const end = ms(16, 44);
  assert.equal(title({ burn: 0.05, deadline: iso(end), end: iso(end), fableReservation: null, hours: 2.23, remaining: 0.45, reset: iso(reset), score: 0.45 / 2.23 - 0.05, window: 'seven_day_fable' }), `45% of the 7-day Fable window left over 2 h to its subscription's end at 16:44, less 5.00 %/h of burn: 15.18 %/h${tail}`);
  assert.equal(title({ burn: 0, deadline: null, end: null, fableReservation: null, hours: 168, remaining: 1, reset: null, score: 1 / 168, window: 'seven_day' }), `100% of the 7-day window left over 168 h with no reset read, less 0.00 %/h of burn: 0.60 %/h${tail}`);
  assert.equal(title({ ...read, window: null }), `66% of its window left over 66 h to its reset at Sep 19 08:30, less 0.40 %/h of burn: 0.60 %/h${tail}`);
  // Under an hour the span reads in minutes, and from an hour on in whole hours.
  assert.equal(title({ ...read, hours: 0.42 }), `66% of the 7-day window left over 25 min to its reset at Sep 19 08:30, less 0.40 %/h of burn: 0.60 %/h${tail}`);
  assert.equal(title({ ...read, hours: 1.4 }), `66% of the 7-day window left over 1 h to its reset at Sep 19 08:30, less 0.40 %/h of burn: 0.60 %/h${tail}`);
  assert.equal(title({ ...read, hours: 0.2 }), `66% of the 7-day window left over 12 min to its reset at Sep 19 08:30, less 0.40 %/h of burn: 0.60 %/h${tail}`);
  // A version 1 record read no burn, and its score was the share left an hour above the pace floor.
  assert.equal(title({ ...read, burn: null, score: 0.01 }), '66% of the 7-day window left over 66 h to its reset at Sep 19 08:30: 1.00 %/h. Inside its tier, the highest above the pace floor wins.');
  const keyPace = { burn: null, deadline: null, end: null, fableReservation: null, hours: null, remaining: 0.45, reset: null, score: 0.45, window: null };
  const key = candidate({ accountId: 'overflow', kind: 'api_key', pace: keyPace, planTier: null, tier: [2, 0, 0], windows: { worst_bucket: window5h(0.55, ms(14, 28, 8)) } });
  assert.equal(d.candidateCells(key, record(), NOW).paceTitle, '45% of its worst live bucket left. Scored by the remaining share alone, with no division by hours.');
  assert.equal(title(null), null);

  const header = (window, pace = read) => d.paceHeader(record({ candidates: [key, candidate({ pace: { ...pace, window } })] }));
  assert.equal(header('seven_day_fable'), 'Spare rate, 7-day Fable per hour', 'the first subscription member names it, past a key');
  assert.equal(header('seven_day'), 'Spare rate, 7-day per hour');
  assert.equal(header('seven_day_opus'), 'Spare rate, seven day opus per hour');
  assert.equal(header(null), 'Pace');
  assert.equal(header('seven_day', { ...read, burn: null }), 'Pace, 7-day left per hour', 'a version 1 record names the share left per hour');
  assert.equal(d.paceHeader(record({ candidates: [key] })), 'Pace', 'API key members alone name no window');
});

test("a Pace hover names the Fable reservation where the candidate carries one", () => {
  // A null read as zero would draw the clause on a Fable request, on a member outside the first tier, on a member with
  // no Fable reading and on a seat with no spend entry.
  const title = (pace) => d.candidateCells(candidate({ pace }), record(), NOW).paceTitle;
  const reset = Date.UTC(2026, 8, 19, 8, 30);
  const read = { burn: 0.004, deadline: iso(reset), end: null, fableReservation: null, hours: 66, remaining: 0.66, reset: iso(reset), score: 0.006, window: 'seven_day' };
  const tail = '. Inside its tier, the highest spare rate wins.';
  // (0.66 less 0.12) over 66 h less 0.004 of burn.
  assert.equal(title({ ...read, fableReservation: 0.12, score: 0.00418 }), `66% of the 7-day window left, less 12% reserved for Fable, over 66 h to its reset at Sep 19 08:30, less 0.40 %/h of burn: 0.42 %/h${tail}`);
  assert.equal(title({ ...read, fableReservation: null }), `66% of the 7-day window left over 66 h to its reset at Sep 19 08:30, less 0.40 %/h of burn: 0.60 %/h${tail}`);
});

test("a By chip reads the strategy with its tier or the rung, and a priority or rung row's chip carries no hint", () => {
  const placed = d.byOf(decision({ by: strategy('pace', [0, 0, 0]), tier: [0, 0, 0] }));
  assert.equal(placed.value, 'Pace P0');
  const held = d.byOf(decision({ by: by('hold', 'strategy_pick'), kind: 'held', tier: [0, 0, 0] }));
  assert.deepEqual(held, { title: placed.title, value: 'Pace P0' }, "a held row's Pace chip carries the hover of a placed row's");
  assert.deepEqual(d.byOf(decision({ by: strategy('priority', [0, 1, 0]), strategy: 'priority' })), { value: 'priority P1' });
  assert.deepEqual(d.byOf(decision({ by: by('rung', 'static') })), { value: 'static rung' });
});

test("a usage-based candidate's one window cell reads its spend share over the reset, marked where the reset is estimated, at any tier", () => {
  // The engine writes the spend entry for a paid source whatever its tier, and files it in group 2 only while the pool
  // prefers subscriptions, so the entry decides the cells at any tier.
  const seat = (windows, tier = [2, 1, 0]) => d.candidateCells(candidate({ accountId: 'seat', planTier: 'default_claude_zero', priority: 1, tier, windows }), record(), NOW);
  const reset = Date.UTC(2026, 9, 1);
  const estimated = seat({ spend: window5h(0.41, reset, 'estimated', 300_000) });
  assert.deepEqual([estimated.tier, estimated.windows], ['paid P1', [['spend 41%', 'Oct 1 00:00', 'estimated']]], "the tier word is the engine's, the cell the spend window");
  assert.deepEqual(seat({ spend: window5h(0.41, reset, 'head') }).windows, [['spend 41%', 'Oct 1 00:00']], "a reset a head named reads as a plan window's");
  assert.deepEqual(seat({ spend: window5h(0.41, null, 'estimated') }).windows, [['spend 41%', 'no reset', 'estimated']]);
  assert.deepEqual(seat({ spend: window5h(0.41, reset, 'estimated') }, [0, 1, 0]).windows, [['spend 41%', 'Oct 1 00:00', 'estimated']], 'a pool that does not prefer subscriptions files the seat in group 0');
  assert.deepEqual(seat({}).windows, [['not reported'], ['not reported'], ['not reported']], 'a share expired at its reset leaves no entry, so the record reads an oauth candidate with no window');
  assert.deepEqual(d.candidateCells(candidate({}), record(), NOW).windows.length, 3, 'a plan candidate keeps its three window cells');
});

test("a paid member's tier reads paid whatever its group", () => {
  // A paid source stands in group 0 where the pool does not prefer subscriptions, so the member decides the word.
  const tier = (o) => d.candidateCells(candidate({ tier: [0, 0, 0], ...o }), record(), NOW).tier;
  const seat = { accountId: 'seat', planTier: 'usage_based' };
  assert.deepEqual([tier(seat), tier({ planTier: 'max_20x' }), tier({ accountId: 'research', kind: 'api_key', planTier: null })], ['paid P0', 'plan P0', 'paid P0']);
  const placed = record({ candidates: [candidate({ ...seat, tier: [0, 0, 0], verdict: 'chosen' })], outcome: { accountId: 'seat' } });
  assert.equal(texts(d.verdictChips(placed, decision({ accountId: 'seat', by: strategy('pace', [0, 0, 0]) })))[0], 'tier paid P0');
});

test("a usage-based candidate's Pace cell reads its remaining allowance, and the header passes over it to a subscription candidate", () => {
  // The engine's paid-source pace: no hours, no window, no burn, and the remaining allowance as the score.
  const paid = { burn: 0, deadline: null, end: null, fableReservation: null, hours: null, remaining: 1, reset: null, score: 1, window: null };
  const seat = candidate({ accountId: 'seat', pace: paid, planTier: 'default_claude_zero', priority: 1, tier: [0, 1, 0], windows: { spend: window5h(0.41, Date.UTC(2026, 9, 1), 'estimated') } });
  const cells = d.candidateCells(seat, record(), NOW);
  assert.deepEqual(
    [cells.windows, cells.pace, cells.paceTitle],
    [[['spend 41%', 'Oct 1 00:00', 'estimated']], '100% left', '100% of its allowance left. Scored by the remaining share alone, with no division by hours.'],
    'the one spend cell and the paid-source pace at tier [0, 1, 0]',
  );
  const plan = candidate({ pace: { burn: 0.004, deadline: null, end: null, fableReservation: null, hours: 66, remaining: 0.66, reset: null, score: 0.006, window: 'seven_day' } });
  assert.equal(d.paceHeader(record({ candidates: [seat, plan] })), 'Spare rate, 7-day per hour', 'the seat listed first names no window, so the next subscription candidate heads the column');
});

test("a Pace title reads a paid source by its pace's shape whether or not the record carries the spend window, and its cell keeps the record's unit", () => {
  // A seat whose share lapsed at its reset, or was not yet read, carries no spend entry while the engine still writes
  // the paid source's pace: no hours, no deadline and no window.
  const paid = { burn: 0, deadline: null, end: null, fableReservation: null, hours: null, remaining: 1, reset: null, score: 1, window: null };
  const bare = d.candidateCells(candidate({ accountId: 'seat', pace: paid, planTier: 'default_claude_zero', priority: 1, tier: [0, 1, 0], windows: {} }), record(), NOW);
  assert.deepEqual(
    [bare.windows, bare.pace, bare.paceTitle],
    [[['not reported'], ['not reported'], ['not reported']], '100%', '100% of its allowance left. Scored by the remaining share alone, with no division by hours.'],
    "the spend sentence by the shape, the windows cells without the entry and the score in the record's unit",
  );
  const key = d.candidateCells(candidate({ accountId: 'overflow', kind: 'api_key', pace: paid, planTier: null, tier: [2, 0, 0], windows: {} }), record(), NOW);
  assert.equal(key.paceTitle, '100% of its worst live bucket left. Scored by the remaining share alone, with no division by hours.', 'an API key keeps its own sentence');
  const plan = d.candidateCells(candidate({ pace: { ...paid, hours: 66 }, windows: {} }), record(), NOW);
  assert.equal(plan.paceTitle, '100% of its window left over 66 h with no reset read, less 0.00 %/h of burn: 100.00 %/h. Inside its tier, the highest spare rate wins.', 'a pace with its hours is a plan pace');
});

test('the Pace header passes over a subscription candidate whose pace names no window to the first one whose pace names one', () => {
  const pace = { burn: 0.004, deadline: null, end: null, fableReservation: null, hours: 66, remaining: 0.66, reset: null, score: 0.006 };
  const lapsed = candidate({ accountId: 'lapsed', pace: { ...pace, window: null } });
  const plan = candidate({ accountId: 'secondary', pace: { ...pace, window: 'seven_day' } });
  assert.equal(d.paceHeader(record({ candidates: [lapsed, plan] })), 'Spare rate, 7-day per hour', "the second candidate's version 3 pace heads the column");
  assert.equal(d.paceHeader(record({ candidates: [lapsed] })), 'Pace', 'no subscription candidate names a window');
});

test('the Pace header under the fable spelling reads the 7-day Fable word, and an unknown spelling keeps its raw text', () => {
  const pace = { burn: 0.004, deadline: null, end: null, fableReservation: null, hours: 66, remaining: 0.66, reset: null, score: 0.006 };
  const header = (window) => d.paceHeader(record({ candidates: [candidate({ pace: { ...pace, window } })] }));
  assert.equal(header('fable'), 'Spare rate, 7-day Fable per hour', "the router's own windows spell the Fable week fable");
  assert.equal(header('monthly'), 'Spare rate, monthly per hour', 'a window the console does not know reads as sent');
});

test("a Pace cell reads the score in the unit the record's pick step gives it, else the unit its pace's shape names, and a seat's title reads its allowance", () => {
  // A lapsed seat carries no spend entry, so its cells read by the plan arm, while the engine scores it as a paid
  // source: a percent with a burn of 0, and a pace with no hours, no deadline and no window, which its title reads.
  const lapsed = candidate({ accountId: 'seat', pace: { burn: 0, deadline: null, end: null, fableReservation: null, hours: null, remaining: 0.9, reset: null, score: 0.42, window: null }, planTier: 'default_claude_zero', priority: 1, tier: [0, 1, 0], windows: {} });
  const allowance = '90% of its allowance left. Scored by the remaining share alone, with no division by hours.';
  const plan = candidate({ pace: { burn: 0.004, deadline: null, end: null, fableReservation: null, hours: 66, remaining: 0.66, reset: null, score: 0.006, window: 'seven_day' } });
  const picked = (c, facts, candidates = [c]) => d.candidateCells(c, record({ candidates, walk: [wstep('pick', 'chosen', facts)] }), NOW);
  const seat = picked(lapsed, [wfact('score', 0.42, 'percent', 'seat')]);
  assert.equal(seat.paceTitle, allowance, "the seat's title reads its allowance by its pace's shape");
  assert.equal(seat.pace, '42%', "the cell reads the score in the pick step's unit");
  // The engine writes score facts for the members of the pick's first tier alone, so a seat ranked behind a plan
  // member finds none about itself.
  const lower = { ...lapsed, tier: [2, 1, 0] };
  const below = picked(lower, [wfact('score', 0.006, 'percent_per_hour', 'primary')], [plan, lower]);
  assert.deepEqual([below.pace, below.paceTitle], ['42%', allowance], "a seat below the pick's tier reads the percent its pace names, not another member's unit, and its title its allowance");
  const later = { ...plan, priority: 2, tier: [0, 2, 0] };
  const behind = picked(later, [wfact('score', 0.42, 'percent', 'seat')], [lapsed, later]);
  assert.deepEqual([behind.pace, behind.paceTitle.includes(': 0.60 %/h.')], ['0.60 %/h', true], "a plan member below a seat's tier reads the plan unit its pace names");
  const rate = picked(plan, [wfact('score', 0.006, 'percent_per_hour', 'primary')]);
  assert.deepEqual([rate.pace, rate.paceTitle.includes(': 0.60 %/h.')], ['0.60 %/h', true], 'a plan candidate reads its spare rate');
  // The pick step's fact wins over the pace's shape, so a plan-shaped pace under a percent fact reads a percent.
  const shaped = { ...plan, pace: { ...plan.pace, score: 0.42 } };
  const ruled = picked(shaped, [wfact('score', 0.42, 'percent', 'primary')]);
  assert.deepEqual([ruled.pace, ruled.paceTitle.includes(': 42%.')], ['42%', true], `the fact's unit wins over the pace's hours and window: ${ruled.paceTitle}`);
  const planBare = d.candidateCells(plan, record({ candidates: [plan] }), NOW);
  assert.deepEqual([planBare.pace, planBare.paceTitle.includes(': 0.60 %/h.')], ['0.60 %/h', true], 'a plan member on a record with no pick step reads the plan unit off its hours and window');
  const bare = d.candidateCells(lapsed, record({ candidates: [lapsed] }), NOW);
  assert.deepEqual([bare.pace, bare.paceTitle], ['42%', allowance], 'a lapsed seat on a record with no pick step reads the percent its pace names');
  // A version 1 record as the decoder gives one: no burn, the pace's hours always written, and no window on a record
  // from before the engine named it.
  const old = candidate({ pace: { burn: null, deadline: null, end: null, fableReservation: null, hours: 66, remaining: 0.66, reset: null, score: 0.01, window: null } });
  const v1 = d.candidateCells(old, record({ candidates: [old], settings: LEGACY_SETTINGS, version: 1 }), NOW);
  assert.deepEqual([v1.pace, v1.paceTitle.includes(': 1.00 %/h.')], ['1.00 %/h', true], 'a version 1 plan member reads the plan unit');
});
