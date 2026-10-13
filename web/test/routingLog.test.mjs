import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

// Every clock time below is read in UTC whatever the host's zone; `node --test` gives each file a process of its own.
process.env.TZ = 'UTC';

const d = await import('./dist/api.js');

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const NOW = Date.UTC(2026, 8, 16, 14, 30);
const ms = (h, m, s = 0) => Date.UTC(2026, 8, 16, h, m, s);
const iso = (t) => new Date(t).toISOString();
const MIDNIGHT = Date.UTC(2026, 8, 17);

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
  ['renewed on a hot Fable week', decision({ kind: 'renewed', fact: [fact('seven_day_fable', 0.9, 'percent')] }), 'no rule fired', ['Fable 90%']],
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
  ["held at its cohort's account", decision({ by: by('hold', 'cohort'), fact: [fact('in_flight', '16/16', 'text')], kind: 'held' }), 'cohort', ['in flight 16/16']],
  ["held on the session's pin", decision({ by: by('hold', 'session_pin'), fact: [fact('in_flight', '1/1', 'text')], kind: 'held' }), 'session pin', ['in flight 1/1']],
  ["held on the strategy's pick", decision({ by: by('hold', 'strategy_pick'), fact: [fact('in_flight', '16/16', 'text')], kind: 'held', tier: [0, 0, 0] }), 'Pace P0', ['in flight 16/16']],
  ['held, the bound ran out', decision({ by: by('hold', 'cohort'), fact: [fact('answer', 429, 'count'), fact('held', 10_000, 'ms')], kind: 'held' }), 'cohort', ['answer 429', 'held 10 s']],
  ['exhausted, a budget', decision({ accountId: null, by: by('exhaustion', 'timed'), exhaustion: { deadline: iso(MIDNIGHT), kind: 'timed', obstacle: null, reason: null, window: 'overage' }, fact: [fact('retry', MIDNIGHT, 'time')], kind: 'exhausted' }), 'budget', ['retry 00:00']],
  ['exhausted, every account cooling', decision({ accountId: null, by: by('exhaustion', 'timed'), exhaustion: { deadline: iso(ms(15, 42)), kind: 'timed', obstacle: null, reason: null, window: 'five_hour' }, fact: [fact('retry', ms(15, 42), 'time')], kind: 'exhausted' }), 'cooldown', ['retry 15:42']],
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
/** The pool row as the store writes it on every edit: each field's current value, with no record of the old one. */
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
  ['pin moved, the fast lane', event('pin_migrated', { agent_id: 'main', epoch: 55, from_account: 'secondary', reason: 'fast_lane' }, { sessionId: 's-moved' }), 'fast lane cooling', ['answer 429'], failover('fast_lane', 429)],
  ['pin expired, found idle by the sweep', event('pin_expired', { agent_id: 'main', reasons: ['expired'] }), 'idle', []],
  ['pin expired, any other reason', event('pin_expired', { agent_id: 'main', reasons: ['cooldown', 'spent'] }), 'account cooled', []],
  ['pin released by hand', event('pin_released', { agent_id: 'main' }, { actor: 'admin' }), 'by hand', []],
  ['cooldown set, the whole account', event('cooldown_set', { reason: 'rate_limit', scope: 'account', source_request_id: 'req_1', until: ms(15, 42), window: 'five_hour' }), '429 on 5h', ['until 15:42']],
  ['cooldown set, a retry-after', event('cooldown_set', { reason: 'rate_limit', scope: 'account', until: ms(14, 1, 20), window: 'retry_after' }), '429 retry-after', ['until 14:01']],
  ["cooldown set, a retry-after inside the row's minute", event('cooldown_set', { reason: 'rate_limit', scope: 'account', until: ms(14, 0, 40), window: 'retry_after' }), '429 retry-after', []],
  ['cooldown set, a spend cap', event('cooldown_set', { reason: 'spend_cap', scope: 'account', until: ms(16, 0), window: 'overage' }), 'spend cap', ['until 16:00']],
  ['cooldown set, the fast lane', event('cooldown_set', { reason: 'fast_rate_limit', scope: 'fast', until: ms(14, 42), window: 'fast' }), 'fast lane 429', ['until 14:42']],
  ['cooldown set, a model', event('cooldown_set', { reason: 'model_rejected', scope: 'model:claude-haiku-4-5-20251001', until: ms(15, 24), window: 'model' }), '404 on haiku-4-5', ['until 15:24']],
  ['cooldown cleared, its deadline', event('cooldown_cleared', { cause: 'expired', scope: 'account' }), 'deadline passed', []],
  ['cooldown cleared by hand', event('cooldown_cleared', { cause: 'admin', scope: 'account' }), 'by hand', []],
  ['cooldown cleared, recovered', event('cooldown_cleared', { cause: 'recovered', scope: 'account' }), 'recovered', []],
  ['overage started', event('overage_started', {}), 'in overage', []],
  ['overage ended', event('overage_ended', { since: at - (24 + 4) * HOUR }), '7d reset', ['lasted 1 d 4 h']],
  ['model rejected', event('model_rejected', { attempt: 1, request_id: 'req_011', source_request_id: 'req_1', wire_model: 'claude-haiku-4-5' }), '404', []],
  ['model permitted', event('model_permitted', { attempt: 2, rejected_by: ['primary'], request_id: 'req_012', source_request_id: 'req_1', wire_model: 'claude-haiku-4-5' }), 'after 404', ['attempt 2 of 2']],
  ['registry changed, one field', event('registry_changed', { diff: { shed_ahead_of_pace: { from: null, to: 0.9 } }, kind: 'pool_updated', target: 'all' }, { accountId: null, actor: 'admin', poolId: 'all' }), 'pool edited', ['field shed ahead of pace', 'to 90%']],
  ['registry changed, several fields', event('registry_changed', { diff: { in_flight_cap: { from: 16, to: 8 }, pace_floor: { from: 0, to: 0.1 }, strategy: { from: 'pace', to: 'priority' } }, kind: 'pool_updated', target: 'all' }, { accountId: null, actor: 'admin', poolId: 'all' }), 'pool edited', ['fields 3']],
  ['registry changed, the whole pool row', event('registry_changed', { diff: WHOLE_POOL_ROW, kind: 'pool_updated', target: 'all' }, { accountId: null, actor: 'admin', poolId: 'all' }), 'pool edited', []],
  ['registry changed, a key revoked', event('registry_changed', { diff: {}, kind: 'key_revoked', target: 'ci' }, { accountId: null, actor: 'admin', keyId: 'ci' }), 'key revoked', []],
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
  ['window reset', event('window_reset', { next_reset: ms(19, 0), observed: 'sample', reset_at: at, utilization_before: 0.92, window: 'five_hour' }), '5h reset', ['was 92%']],
  ['router started', event('router_started', { pins_loaded: 10, version: '0.1.0' }, { accountId: null }), 'restart', ['pins 10']],
  ['warm prefix expired', event('warm_prefix_expired', { cause: 'ttl', deepest: { depth: 1, prefix_tokens: 18_400, tier: 'message', ttl_ms: 300_000 }, entries: 2, root: 's0' }), '5 min tier', ['prefix 18.4K', 'depth 1']],
  ['poll backoff started', event('poll_backoff_started', { attempts: 1, endpoint: 'usage', last_clean_poll_at: ms(13, 40), next_attempt_at: ms(13, 53), reason: 'the usage endpoint rate-limited the account' }, { at: iso(ms(13, 52, 30)) }), 'rate limited', ['next 13:53']],
  ['poll backoff ended', event('poll_backoff_ended', { attempts: 3, duration_ms: 25 * MINUTE, resumed_at: at }), 'clean poll', ['lasted 25 min']],
  ['budget reached', event('budget_reached', { budget_usd: 20, period: 'day', period_end: MIDNIGHT, spent_usd: 20.4 }, { poolId: 'jobs' }), 'daily budget', ['spent $20.40 of $20.00']],
  ['budget period ended', event('budget_period_ended', { period: 'month', period_start: at, previous_spent_usd: 18.2 }, { poolId: 'jobs' }), 'monthly budget', ['spent $18.20']],
  ['wind-down started', event('wind_down_started', { end_source: 'cancelled', ends_at: ms(15, 0), wind_down_minutes: 60 }, { poolId: 'all' }), 'subscription ends', ['until 15:00']],
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

test("a cooldown's words read the router's spellings alone, and any other reads as sent", () => {
  assert.equal(read(event('cooldown_cleared', { cause: 'deadline', scope: 'account' })).by, 'deadline', 'the router writes a cooldown that ran out as expired');
  assert.equal(read(event('cooldown_cleared', { cause: 'operator', scope: 'account' })).by, 'operator', 'and one cleared by hand as admin');
  assert.equal(read(event('cooldown_set', { reason: 'rate_limited', scope: 'account', until: ms(15, 42), window: 'five_hour' })).by, 'rate limited', 'and a rate limit as rate_limit');
});

test("the story's funding retry reads funding retried", async () => {
  const to = Date.now();
  const page = await new d.MockApi().listRoutingLog({ from: iso(to - 7 * 24 * HOUR), kinds: ['account_state_changed'], limit: 25, to: iso(to) });
  const retry = page.rows.find((r) => r.detail.cause === 'funding_retry');
  assert.ok(retry, 'the story holds a funding retry');
  assert.deepEqual([retry.accountId, retry.detail.from, retry.detail.to], ['overflow', 'funding_required', 'ready']);
  assert.deepEqual(read(retry, page.rows), { by: 'funding retried', facts: ['state ready'] });
});

test('a pin created with no placement beside it reads the placement word', () => {
  assert.equal(d.byOf(event('pin_created', { agent_id: 'main', epoch: 52 }, { sessionId: 's-created' }), []).value, 'placement');
});

test('a failover or fast-lane move whose failed attempt is not drawn reads no answer and never the epoch', () => {
  for (const reason of ['cooldown', 'fast_lane']) {
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

test("the session pin's two rows read the in-flight count alone, then with the wait stacked beneath it", () => {
  const held = read(decision({ by: by('hold', 'session_pin'), fact: [fact('in_flight', '1/1', 'text')], kind: 'placed' }));
  const afterHold = read(decision({ by: by('hold', 'session_pin'), fact: [fact('in_flight', '1/1', 'text'), fact('held', 2_000, 'ms')], kind: 'placed' }));
  assert.deepEqual(held, { by: 'session pin', facts: ['in flight 1/1'] });
  assert.deepEqual(afterHold, { by: 'session pin', facts: ['in flight 1/1', 'held 2.0 s'] });
});

test('the pin reason reads not looked up and pinning off', () => {
  assert.deepEqual(d.PIN_REASON_LABEL, { not_looked_up: 'not looked up', pinning_off: 'pinning off' });
});

test('a fact never repeats the time of its row and never runs past two pairs', () => {
  const row = decision({ at: iso(ms(14, 16, 40)), fact: [fact('retry', ms(14, 16, 59), 'time'), fact('pick', 'only candidate', 'text'), fact('depth', 2, 'count'), fact('prefix', 1_000, 'tokens')] });
  assert.deepEqual(read(row).facts, ['pick only candidate', 'depth 2']);
});

test('a fact key the console has no label for is dropped rather than drawn under a guess', () => {
  assert.deepEqual(read(decision({ fact: [fact('mystery', 1, 'count'), fact('depth', 2, 'count')] })).facts, ['depth 2']);
});

const TABLE_LABELS = ['5h', '7d', 'Fable', 'answer', 'attempt', 'capture', 'cause', 'client', 'depth', 'epoch', 'field', 'fields', 'gap', 'held', 'idle', 'in flight', 'key', 'lasted', 'models', 'next', 'over', 'pick', 'pins', 'prefix', 'rank', 'resets', 'retry', 'spent', 'state', 'to', 'turn', 'until', 'was'];

test('every label of the table is used at least once and no fact key lacks a label', () => {
  assert.equal(TABLE_LABELS.length, 33);
  assert.deepEqual([...new Set(Object.values(d.FACT_LABEL))].sort(), [...TABLE_LABELS].sort());
  assert.equal(Object.keys(d.FACT_LABEL).length, 33);
  const emitted = [...DECISIONS, ...EVENTS].flatMap(([, row, , , page]) => d.factsOf(row, NOW, page).map((p) => p.label));
  assert.deepEqual([...new Set(emitted)].sort(), [...TABLE_LABELS].sort());
  for (const label of emitted) assert.ok(Object.values(d.FACT_LABEL).includes(label), label);
});

test('every event kind has its words and its tone', () => {
  const kinds = [
    'account_state_changed', 'budget_period_ended', 'budget_reached', 'catalog_changed', 'cooldown_cleared', 'cooldown_set', 'fast_credits_changed', 'ledger_gap', 'model_permitted', 'model_rejected',
    'overage_ended', 'overage_started', 'pin_created', 'pin_expired', 'pin_migrated', 'pin_released', 'poll_backoff_ended', 'poll_backoff_started', 'refresh_failed',
    'refresh_succeeded', 'refresh_uncertain', 'registry_changed', 'router_started', 'shedding_round', 'warm_prefix_expired', 'wind_down_started', 'window_reset', 'window_spent',
  ];
  assert.deepEqual(Object.keys(d.EVENT_KIND_LABEL).sort(), kinds);
  assert.equal(d.EVENT_KIND_LABEL.pin_migrated, 'pin moved');
  assert.equal(d.EVENT_KIND_LABEL.shedding_round, 'shedding round');
  const tones = Object.fromEntries(kinds.map((k) => [k, d.eventTone(k)]).filter(([, t]) => t !== 'neutral'));
  assert.deepEqual(tones, { account_state_changed: 'serious', budget_reached: 'serious', cooldown_set: 'warning', ledger_gap: 'serious', model_rejected: 'warning', window_spent: 'warning' });
  assert.equal(d.eventTone('a_kind_added_later'), 'neutral');
  assert.equal(d.eventTone('reauth_required'), 'neutral', 'the ledger folds the kind into the state change, so it has no tone of its own');
});

test('every decision kind has its words and its tone', () => {
  assert.deepEqual(d.DECISION_KIND_LABEL, { exhausted: 'exhausted', held: 'held', moved: 'moved', placed: 'placed', refused: 'refused', renewed: 'renewed' });
  assert.deepEqual(d.DECISION_KIND_TONE, { exhausted: 'critical', held: 'warning', moved: 'warning', placed: 'good', refused: 'critical', renewed: 'good' });
});

test('the step and verdict words', () => {
  const steps = {
    pin: 'Pin', probe: 'Probe', eligible: 'Eligible', bound: 'Bound', 'move:spent': 'Spent', 'move:overage': 'Overage', 'move:return_to_plan': 'Return to plan', 'move:drain': 'Drain', 'move:shed': 'Shed', cool: 'Cool reading',
    cap_filter: 'Cap filter', renew: 'Renew', 'rung:content': 'Rung: content', 'rung:with_main': 'Rung: with main', 'rung:static': 'Rung: static', partitions: 'Partitions', pick: 'Pick', hold: 'Hold', exhausted: 'Exhausted', admission: 'Admission',
  };
  assert.equal(Object.keys(steps).length, 20);
  for (const [step, name] of Object.entries(steps)) assert.equal(d.STEP_NAME[step], name, step);
  assert.equal(d.STEP_NAME.selection, 'Rest of the walk');
  const tones = {
    answered: 'critical', blocked: 'warning', bound: 'good', chosen: 'good', dropped: 'warning', exhausted: 'critical', fell_through: 'warning', fired: 'warning', found: 'neutral', held: 'warning', match: 'good',
    moved: 'warning', no: 'neutral', none: 'neutral', not_run: 'neutral', off: 'neutral', passed: 'good', placed: 'good', preferred: 'warning', refused: 'critical', released: 'warning', renewed: 'good',
  };
  for (const [verdict, tone] of Object.entries(tones)) assert.equal(d.verdictTone(verdict), tone, verdict);
  assert.deepEqual([...d.TONED_VERDICTS].sort(), Object.keys(tones).sort());
  assert.equal(d.verdictTone('none', 'eligible'), 'critical');
  assert.equal(d.verdictTone('none', 'pin'), 'neutral');
  assert.equal(d.verdictTone('match', 'eligible'), 'good');
  assert.equal(d.verdictTone('a_word_added_later'), 'neutral');
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

test('PIN_RELEASE_LABEL words every release reason and releaseLabel keeps an unknown one as sent', () => {
  assert.equal(Object.keys(d.PIN_RELEASE_LABEL).length, d.PIN_RELEASE_REASON_VALUES.length);
  assert.equal(d.releaseLabel('cooldown'), 'account cooled');
  assert.equal(d.releaseLabel('expired'), 'pin idle past the affinity window');
  assert.equal(d.releaseLabel('a_reason_added_later'), 'a_reason_added_later');
});

test('each entity links to the route of its kind', () => {
  assert.equal(d.entityHref('account', 'primary'), '/accounts/primary');
  assert.equal(d.entityHref('request', 'req_1d9c3e55-0b2'), '/requests/req_1d9c3e55-0b2');
  // The Sessions and Models pages read no key for the entity, so the link opens the view itself.
  assert.equal(d.entityHref('session', '407dd39b-dd60'), '/sessions');
  assert.equal(d.entityHref('pool', 'all'), '/pools#all');
  assert.equal(d.entityHref('key', 'laptop'), '/keys#laptop');
  assert.equal(d.entityHref('key', 'ci key/2'), '/keys#ci%20key%2F2');
  assert.equal(d.entityHref('model', 'claude-sonnet-5'), '/models');
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

test('logHref writes a preset by name and a window as two instants', () => {
  assert.equal(d.logHref({ range: '24h' }), '/routing-log?range=24h');
  assert.equal(d.logHref({ range: '1h' }), '/routing-log');
  assert.equal(d.logHref({ kinds: ['held'], range: '3h' }), '/routing-log?kinds=held&range=3h');
  const instants = new URL(d.logHref({ from: ms(13, 30) + 123, to: ms(14, 29, 59) + 999 }), 'http://phoebis.test').searchParams;
  assert.deepEqual(Object.fromEntries(instants), { from: '2026-09-16T13:30:00.123Z', to: '2026-09-16T14:29:59.999Z' });
});

test("the log's address keeps a kind this build does not know, so the route's refusal reaches the page", () => {
  const filters = d.ROUTING_LOG_CODEC.fromParams(new URLSearchParams('kinds=placed,budget_refilled&accounts=primary'));
  assert.deepEqual([filters.kinds, filters.accounts], [['placed', 'budget_refilled'], ['primary']]);
  assert.equal(d.ROUTING_LOG_CODEC.toParams(filters).get('kinds'), 'placed,budget_refilled');
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
  assert.equal(route.reads.length, d.FOLLOW_PAGES);
  assert.equal(d.FOLLOW_PAGES, 8);
  assert.deepEqual(polled.rows.map(d.identityOf), route.rows.slice(0, 200).map(d.identityOf));
  assert.equal(polled.overrun, '200');
  const gap = await route.read(polled.overrun);
  assert.equal(d.identityOf(gap.rows[0]), d.identityOf(route.rows[200]), 'the overrun cursor reads the first row the poll left');
  assert.equal(d.FOLLOW_OVERRUN, 'More rows arrived than auto reload could fetch. Reload to read them.');
});

test('model ids order fable, opus, sonnet, haiku, newest version first', () => {
  const ids = ['claude-haiku-4-5-20251001', 'claude-sonnet-5', 'claude-opus-5', 'claude-fable-5-1', 'claude-sonnet-5-5', 'claude-opus-5-5', 'claude-opus-4-5-20251101', 'some-other-model'];
  assert.deepEqual([...ids].sort(d.compareModels), ['claude-fable-5-1', 'claude-opus-5-5', 'claude-opus-5', 'claude-opus-4-5-20251101', 'claude-sonnet-5-5', 'claude-sonnet-5', 'claude-haiku-4-5-20251001', 'some-other-model']);
});

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
  cooldowns: [],
  credits: null,
  enabled: true,
  inFlight: 3,
  inFlightCap: 16,
  inOverage: false,
  incarnation: 1,
  kind: 'oauth',
  obstacle: null,
  overage: null,
  pace: { deadline: null, end: null, hours: 120, remaining: 0.91, reset: null, score: 0.0076 },
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
const completion = (o) => ({ cacheMapWritten: null, cacheReadTokens: null, completedAt: null, freed: false, heldMs: null, hit: null, inputTokens: null, promisedTokens: null, status: null, ...o });
const SETTINGS = {
  affinityMode: 'when_cached', affinityTtlSeconds: 600, burnWindowSeconds: 600, inFlightCap: 16, migrateBand: 0.05, migrateWhenSpent: true, minSharedPrefixTokens: 20_000,
  overageMoveGraceMinutes: 30, overagePolicy: 'prefer_plan', paceFloor: 0.05, poolUpdatedAt: iso(ms(9, 5, 12)), preferSubscription: true, retryInPlaceSeconds: 60, selectBelowUtilization: null,
  sessionPlacement: 'by_warm_prefix', shedAheadOfPace: 0.9, shedShare: 0.5, sourceDwellSeconds: 900, strategy: 'pace', subagentPlacement: 'by_cohort', warmSessions: 'when_needed', windDownMinutes: 30,
};
const record = (o = {}) => ({
  candidates: [],
  outcome: {
    accountId: 'primary', cohortHold: null, completion: null, exhaustion: null, fromAccountId: null, hold: null, inOverage: false, kind: 'placed', pinEvents: [], pinWritten: null, refusal: null, rule: null,
    rung: null, shedMarks: null, strategy: 'pace', tier: null, upstreamModel: 'claude-sonnet-5-5', ...o.outcome,
  },
  pin: null,
  pinReason: 'pinning_off',
  raw: null,
  request: {
    agentId: 'main', at: iso(ms(14, 27, 28)), attempt: 1, attempted: [], canonicalModel: 'claude-sonnet-5-5', chain: { boundaries: 6, staticBoundaries: 2, staticHash: true }, clientClass: 'claude_code',
    flags: { contextCompacted: false, count: false, fast: false, marker: true, portable: true, probe: false }, holds: [], keyGeneration: 1,
    keyId: 'desktop', lane: { effort: 'high', speed: 'standard', thinking: 'enabled' }, parentAgentId: null, poolId: 'all', registryRevision: 412, requestClass: 'main', requestId: 'req_f8085653-422',
    seq: 1, ...o.request,
  },
  requestId: 'req_f8085653-422',
  seq: 1,
  settings: SETTINGS,
  version: 1,
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

test('the candidate cells word every verdict and its chip', () => {
  const cell = (o, r = record()) => d.candidateCells(candidate(o), r, NOW);
  const verdict = (o, r) => {
    const v = cell(o, r).verdict;
    return [v.badge, v.chip === null ? null : v.chip.value, v.chip?.title ?? null];
  };
  const chosen = cell({ verdict: 'chosen' });
  assert.deepEqual(chosen.account, { entity: { id: 'primary', kind: 'account' }, value: 'primary' });
  assert.equal(chosen.plan, 'Max 20x');
  assert.equal(chosen.tier, 'plan P0');
  assert.deepEqual(chosen.windows, [['62%', '16:44', 'head 4 s'], ['41%', 'Sep 19 20:10', 'head 4 s'], ['12%', 'Sep 19 20:10', 'head 4 s']]);
  assert.equal(chosen.inFlight, '3 of 16');
  assert.equal(chosen.pace, '0.76 %/h');
  assert.deepEqual(chosen.verdict, { badge: 'chosen', chip: null });
  assert.deepEqual(cell({ windows: { five_hour: window5h(1, ms(15, 42), 'poll', 120_000), seven_day: window5h(0.88, null, 'poll', 120_000) } }).windows, [['100%', '15:42', 'poll 2 min'], ['88%', 'no reset', 'poll 2 min'], ['not reported']]);
  const key = cell({ accountId: 'overflow', kind: 'api_key', pace: { deadline: null, end: null, hours: null, remaining: 0.96, reset: null, score: 0.96 }, planTier: null, priority: 1, tier: [2, 1, 0], windows: { worst_bucket: window5h(0.04, ms(14, 28, 8)) } });
  assert.deepEqual([key.plan, key.tier, key.windows, key.pace], ['API key', 'keys P1', [['worst bucket 4%']], '96% left']);
  assert.equal(cell({ inOverage: true, tier: [1, 0, 1] }).tier, 'overage P0');
  assert.equal(cell({ pace: null }).pace, '-');
  const until = (h, m) => iso(ms(h, m));
  assert.deepEqual(verdict({ obstacle: { name: 'cooldown', reason: 'rate_limit', until: until(15, 42), window: 'five_hour' }, verdict: 'out' }), ['out', '429 on 5h until 15:42', 'in a cooldown']);
  assert.deepEqual(verdict({ obstacle: { name: 'cooldown', reason: 'rate_limited', until: until(15, 42), window: 'five_hour' }, verdict: 'out' }), ['out', 'rate limited until 15:42', 'in a cooldown'], 'the router spells its rate limit rate_limit alone');
  assert.deepEqual(verdict({ obstacle: { name: 'cooldown', reason: 'model_rejected', until: until(15, 24), window: 'model_rejection' }, verdict: 'out' }), ['out', 'model 404 until 15:24', 'in a cooldown']);
  assert.deepEqual(verdict({ obstacle: { name: 'cooldown', reason: 'fast_rate_limit', until: until(14, 42), window: 'fast' }, verdict: 'out' }), ['out', 'fast lane 429 until 14:42', 'in a cooldown']);
  const operator = { name: 'operator', reason: null, until: null, window: null };
  assert.deepEqual(verdict({ enabled: false, obstacle: operator, verdict: 'out' }), ['out', 'held by hand', 'held by an operator']);
  assert.deepEqual(verdict({ obstacle: operator, state: 'funding_required', verdict: 'out' }), ['out', 'needs funding', 'the account leaves every eligible set until funded']);
  assert.deepEqual(verdict({ obstacle: operator, state: 'reauth_required', verdict: 'out' }), ['out', 'needs re-auth', 'the account leaves every eligible set until it is paired again']);
  assert.deepEqual(verdict({ enabled: false, obstacle: operator, state: 'deleted', verdict: 'out' }), ['out', 'deleted', 'the account left the registry']);
  // The exhausted line reads each operator obstacle as the member's own row does.
  const held = record({
    candidates: [candidate({ accountId: 'overflow', obstacle: operator, state: 'funding_required', verdict: 'out' }), candidate({ accountId: 'nightly', enabled: false, obstacle: operator, verdict: 'out' })],
    outcome: { accountId: null, exhaustion: { deadline: null, kind: 'untimed', obstacle: null, reason: null, window: null }, kind: 'exhausted', upstreamModel: null },
    walk: [wstep('exhausted', 'exhausted', [wfact('obstacles', ['overflow:operator', 'nightly:operator'])])],
  });
  assert.deepEqual(texts(d.walkLines(held)[0].chips), ['overflow needs funding', 'nightly held by hand']);
  assert.deepEqual(verdict({ budget: [{ budgetUsd: 5, observedAt: iso(NOW), period: 'day', periodEnd: iso(MIDNIGHT), periodStart: iso(ms(0, 0)), spentUsd: 1.1 }], obstacle: { name: 'budget', reason: 'day', until: iso(MIDNIGHT), window: null }, verdict: 'out' }), ['out', 'budget $1.10 of $5.00', 'at its spend budget for the period']);
  const daily = { budgetUsd: 5, observedAt: iso(NOW), period: 'day', periodEnd: iso(MIDNIGHT), periodStart: iso(ms(0, 0)), spentUsd: 1 };
  const monthly = { budgetUsd: 10, observedAt: iso(NOW), period: 'month', periodEnd: iso(Date.UTC(2026, 9, 1)), periodStart: iso(Date.UTC(2026, 8, 1)), spentUsd: 12 };
  const month = { name: 'budget', reason: 'month', until: iso(Date.UTC(2026, 9, 1)), window: null };
  assert.deepEqual(verdict({ budget: [daily, monthly], obstacle: month, verdict: 'out' }), ['out', 'budget $12.00 of $10.00', 'at its spend budget for the period']);
  assert.deepEqual(verdict({ budget: [daily, monthly], obstacle: { ...month, reason: null }, verdict: 'out' }), ['out', 'budget', 'at its spend budget for the period'], 'an obstacle that names no period draws no budget detail');
  assert.deepEqual(verdict({ obstacle: { name: 'spent', reason: null, until: until(15, 42), window: 'five_hour' }, verdict: 'out' }), ['out', '5h spent', 'its window spent']);
  assert.deepEqual(verdict({ obstacle: { name: 'credits', reason: 'org_spend_cap_reached', until: null, window: null }, verdict: 'out' }), ['out', 'credits spend cap reached', 'its usage credits unavailable for fast mode']);
  assert.deepEqual(verdict({ obstacle: { name: 'budget_unknown', reason: null, until: null, window: null }, verdict: 'out' }), ['out', 'budget unknown', 'its spend unknown while the ledger is degraded']);
  assert.deepEqual(verdict({ inFlight: 16, partition: 'at_cap', verdict: 'out' }), ['at cap', 'at its cap', 'at its in-flight cap']);
  assert.deepEqual(verdict({ inFlight: 16, verdict: 'held_on' }), ['held on', 'at its cap', null]);
  assert.deepEqual(verdict({ verdict: 'destination' }), ['destination', 'not placed', null]);
  assert.deepEqual(verdict({ rank: 2, verdict: 'ranked' }), ['ranked 2nd', null, null]);
  assert.deepEqual(verdict({ verdict: 'not_needed' }), ['not needed', null, null]);
  assert.deepEqual(verdict({ verdict: 'renewed' }), ['renewed', null, null]);
  assert.deepEqual(verdict({ verdict: 'source', windows: { five_hour: window5h(1, ms(15, 42)), seven_day: window5h(0.5, null) } }), ['source', '5h spent', null]);
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
  const freed = record({ outcome: { cohortHold: { cohort: 'primary', heldMs: 2_000, waitedOn: ['primary'] }, completion: completion({ completedAt: iso(NOW), freed: true, heldMs: 2_000, status: 200 }) } });
  assert.deepEqual(rows(freed).find(([label]) => label === 'Hold'), ['Hold', ['on primary', 'cohort', 'freed after 2.0 s']]);
  const exhausted = record({ outcome: { accountId: null, exhaustion: { deadline: iso(ms(15, 42)), kind: 'timed', obstacle: null, reason: null, window: 'five_hour' }, kind: 'exhausted', upstreamModel: null } });
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

test('an exhausted record after a run-out draws ran out on its hold and 429 on its answer', () => {
  const rows = (r) => d.outcomeRows(r, NOW).map((line) => [line.label, texts(line.chips)]);
  const exhausted = (holds) =>
    record({ outcome: { accountId: null, exhaustion: { deadline: null, kind: 'model_rejected', obstacle: null, reason: null, window: null }, kind: 'exhausted', upstreamModel: null }, request: { holds } });
  const hold = (accountId, ranOut, cohort = null) => ({ accountId, attempt: 1, cohort, heldMs: 60_000, ranOut });
  assert.deepEqual(rows(exhausted([hold('primary', false), hold('secondary', true)])), [
    ['Hold', ['on secondary', 'strategy pick', 'held 1 min', 'ran out']],
    ['Answer', ['status 429', 'retry none']],
  ]);
  assert.deepEqual(rows(exhausted([hold('primary', false), hold('secondary', false)])), [
    ['Hold', ['on secondary', 'strategy pick', 'held 1 min']],
    ['Answer', ['status 404', 'retry none']],
  ], 'a hold that did not run out leaves the status the kind names');
  assert.deepEqual(rows(exhausted([hold('primary', true, 'primary')]))[0], ['Hold', ['on primary', 'cohort', 'held 1 min', 'ran out']], "a cohort's hold reads its place as the cohort row does");
  assert.deepEqual(rows(exhausted([])), [['Answer', ['status 404', 'retry none']]], 'a record that held nowhere draws no hold');
});

test('an exhausted record draws no hold an earlier attempt waited through', () => {
  const rows = (r) => d.outcomeRows(r, NOW).map((line) => [line.label, texts(line.chips)]);
  const exhausted = (attempt, holds) =>
    record({ outcome: { accountId: null, exhaustion: { deadline: null, kind: 'model_rejected', obstacle: null, reason: null, window: null }, kind: 'exhausted', upstreamModel: null }, request: { attempt, holds } });
  const hold = (attempt) => ({ accountId: 'primary', attempt, cohort: null, heldMs: 60_000, ranOut: false });
  // Held and freed on attempt 1, then failed over: the relay keeps attempt 1's hold on the request that attempt 2 exhausts.
  assert.deepEqual(rows(exhausted(2, [hold(1)])), [['Answer', ['status 404', 'retry none']]]);
  assert.deepEqual(rows(exhausted(2, [hold(1), hold(2)])), [['Hold', ['on primary', 'strategy pick', 'held 1 min']], ['Answer', ['status 404', 'retry none']]], "a hold on the record's own attempt still draws");
});

/** One event per kind, its detail in the shape its writer stores, each naming an account and a pool. */
const DETAIL_EVENTS = {
  account_state_changed: [{ cause: 'funding_refused', detail: 'Your credit balance is too low', from: 'ready', to: 'funding_required' }, ['From', 'To', 'Cause', 'Detail', 'Effect']],
  budget_period_ended: [{ period: 'day', period_start: ms(0, 0) - 24 * HOUR, previous_spent_usd: 18.2 }, ['Previous period', 'Spent', 'Period start']],
  budget_reached: [{ budget_usd: 20, period: 'day', period_end: MIDNIGHT, source_request_id: 'req_1', spent_usd: 20.4 }, ['Budget', 'Spent', 'Period ends', 'Effect', 'Crossed by']],
  catalog_changed: [{ added: ['claude-opus-5-5'], count: 12, removed: ['claude-sonnet-4-5'] }, ['Added', 'Removed', 'Models']],
  cooldown_cleared: [{ cause: 'expired', scope: 'account' }, ['Scope', 'Cleared']],
  cooldown_set: [{ reason: 'rate_limit', scope: 'account', source_request_id: 'req_1', until: ms(15, 42), window: 'five_hour' }, ['Scope', 'Reason', 'Window', 'Until', 'Set by']],
  fast_credits_changed: [{ after: { disabled_reason: null, enabled: true, utilization: 0.31 }, before: { disabled_reason: null, enabled: true, utilization: 1 }, blocked_after: null, blocked_before: 'org_spend_cap_reached', observed_at: ms(11, 30) }, ['Before', 'After', 'Cause', 'Effect']],
  ledger_gap: [{ reason: 'chunk_skipped', request_id: 'req_1' }, ['Reason', 'Meaning', 'Effect', 'Request']],
  model_permitted: [{ attempt: 2, rejected_by: ['secondary'], request_id: 'req_012', source_request_id: 'req_1', wire_model: 'claude-haiku-4-5' }, ['Rejected by', 'Served at', 'Model sent', 'Model resolved', 'Request']],
  model_rejected: [{ attempt: 1, request_id: 'req_011', source_request_id: 'req_1', wire_model: 'claude-haiku-4-5' }, ['Model sent', 'Model resolved', 'Cooldown scope', 'Attempt', 'Request']],
  overage_ended: [{ since: ms(10, 0) - 24 * HOUR }, ['Since', 'Lasted']],
  overage_started: [{}, ['Effect']],
  pin_created: [{ agent_id: 'main', epoch: 52 }, ['Key', 'Session', 'Agent', 'Model', 'Epoch']],
  pin_expired: [{ agent_id: 'main', reasons: ['cooldown', 'spent'] }, ['Key', 'Session', 'Agent', 'Model', 'Reasons', 'Found by']],
  pin_migrated: [{ agent_id: 'main', epoch: 53, from_account: 'secondary', reason: 'spent' }, ['From', 'To', 'Reason', 'Moved by', 'Key', 'Session', 'Agent', 'Model']],
  pin_released: [{ agent_id: 'main' }, ['Actor', 'Key', 'Session', 'Agent', 'Model']],
  poll_backoff_ended: [{ attempts: 3, duration_ms: 25 * MINUTE, resumed_at: ms(8, 15, 3) }, ['Backed off', 'Attempts', 'Resumed']],
  poll_backoff_started: [{ attempts: 1, endpoint: 'usage', last_clean_poll_at: ms(13, 43), next_attempt_at: ms(13, 53), reason: '429' }, ['Reason', 'Attempts', 'Next attempt', 'Last clean poll', 'Effect']],
  refresh_failed: [{ cause: 'invalid_grant', error: 'refresh rejected with status 400 (invalid_grant)' }, ['Error']],
  refresh_succeeded: [{ access_expires_at: ms(20, 52), refresh_expires_at: ms(20, 52) + 58 * 24 * HOUR, until: ms(20, 52) }, ['Access token', 'Refresh token']],
  refresh_uncertain: [{ cause: 'refresh_response_lost', error: 'refresh response lost: connection reset' }, ['Error']],
  registry_changed: [{ diff: { shed_ahead_of_pace: { from: null, to: 0.9 } }, kind: 'pool_updated', target: 'all' }, ['Change', 'shed ahead of pace', 'Actor', 'Reading it']],
  router_started: [{ affinity_entries_loaded: 34, down_since: ms(6, 11, 58), holds_lost: true, pins_loaded: 10, shedding_marks_loaded: 0, shedding_rounds_stale: 0, version: '0.1.0', windows_loaded: 5 }, ['Version', 'Pins loaded', 'Shedding marks loaded', 'Window readings loaded', 'Affinity entries loaded', 'Down from', 'Held requests']],
  shedding_round: [
    {
      ahead: 0.91, burn_sum: 0.299 / 3_600, burn_window_seconds: 600, dwell_seconds: 900, dwell_until: ms(11, 15), elapsed_share: 0.06, margin: 0.01, marks: [{ agent_id: 'c21e', key_id: 'desktop', model: 'claude-sonnet-5', session_id: 's1' }], persisted: true,
      pins: [{ agent_id: 'c21e', burn: 0.07 / 3_600, cold: true, key_id: 'desktop', last_input_tokens: 48_100, marked: true, model: 'claude-sonnet-5', note: null, per_rewrite: 0.07 / 3_600 / 48_100, session_id: 's1' }],
      shed_share: 0.5, target: 0.1495 / 3_600, threshold: 0.9, unit: 'window_share', utilization: 0.97, warm_sessions: 'when_needed', window: 'five_hour',
    },
    ['Started by', 'Reading', 'Unit', 'Burn window', 'Share', 'Warm sessions', 'Pins', 'Marks kept until', 'Dwell', 'Survive a restart'],
  ],
  warm_prefix_expired: [{ cause: 'ttl', deepest: { depth: 1, prefix_tokens: 18_400, tier: 'message', ttl_ms: 300_000, written_at: ms(14, 19, 52) }, entries: 1, root: 's0' }, ['Boundary', 'Depth', 'Size', 'Tier', 'Written', 'Entries', 'Listed']],
  wind_down_started: [{ end_source: 'cancelled', ends_at: ms(15, 0), wind_down_minutes: 30 }, ['Subscription ends', 'Wind-down', 'Effect', 'Cold pins', 'End from']],
  window_reset: [{ next_reset: ms(19, 0), observed: 'sample', reset_at: ms(14, 0), utilization_before: 0.92, window: 'five_hour' }, ['Window', 'Before the reset', 'Prediction', 'Next reset']],
  window_spent: [{ reset: ms(15, 42), source: 'head', source_request_id: 'req_1', utilization: 1, window: 'five_hour' }, ['Window', 'Reading', 'Read from', 'Resets', 'Effect', 'Request']],
};

test('every event kind has its detail pairs starting with When, Account and Pool', () => {
  assert.deepEqual(Object.keys(DETAIL_EVENTS).sort(), Object.keys(d.EVENT_KIND_LABEL).sort());
  for (const [kind, [detail, own]] of Object.entries(DETAIL_EVENTS)) {
    const row = event(kind, detail, { actor: 'admin', agentId: 'main', keyId: 'desktop', model: 'claude-haiku-4-5-20251001', poolId: 'all', requestId: 'req_1', sessionId: 's1-0000-4000' });
    const pairs = d.detailPairs(row, NOW, []);
    assert.deepEqual(pairs.slice(0, 3).map((p) => [p.label, texts(p.chips)]), [['When', ['2026-09-16 14:00:00 UTC']], ['Account', ['primary']], ['Pool', ['all']]], kind);
    assert.deepEqual(pairs[1].chips[0].entity, { id: 'primary', kind: 'account' }, kind);
    assert.deepEqual(pairs[2].chips[0].entity, { id: 'all', kind: 'pool' }, kind);
    assert.deepEqual(pairs.slice(3).map((p) => p.label), own, kind);
    for (const p of pairs) for (const c of p.chips) assert.ok(!/(?<!\d),|,(?!\d)|;/.test(chipText(c)), `${kind} ${p.label}: ${chipText(c)}`);
  }
  const without = d.detailPairs(event('router_started', { pins_loaded: 10 }, { accountId: null }), NOW, []);
  assert.deepEqual(without.slice(0, 2).map((p) => p.label), ['When', 'Pins loaded']);
  const round = d.detailPairs(event('shedding_round', DETAIL_EVENTS.shedding_round[0]), NOW, []).find((p) => p.label === 'Pins');
  assert.deepEqual(round.table.head, ['Session', 'Agent', 'Warmth', 'Burn', 'Last prompt', 'Mark']);
  assert.deepEqual(round.table.rows.map((r) => r.map(chipText)), [['s1', 'c21e', 'cold', '7.0 %/h', '48.1K', 'marked']]);
  const restart = d.detailPairs(event('shedding_round', DETAIL_EVENTS.shedding_round[0]), NOW, []).find((p) => p.label === 'Survive a restart');
  assert.deepEqual(texts(restart.chips), ['persisted']);
});

test("a shedding round's rates are the engine's per second, drawn per hour in the round's unit", () => {
  const fixture = d.decodeRoutingLogPage(JSON.parse(readFileSync(new URL('./fixtures/routing_log.json', import.meta.url), 'utf8')).body);
  const round = fixture.rows.find((r) => r.type === 'event' && r.kind === 'shedding_round');
  const drawn = (row) => {
    const pairs = d.detailPairs(row, NOW, []);
    return { pins: pairs.find((p) => p.label === 'Pins').table.rows.map((r) => chipText(r[3])), share: texts(pairs.find((p) => p.label === 'Share').chips) };
  };
  // The router's row: a burn sum of 0.12 and a pin of 0.07 a second, in shares of the 5-hour window.
  assert.deepEqual([round.detail.unit, round.detail.burn_sum, round.detail.pins[0].burn], ['window_share', 0.12, 0.07]);
  assert.deepEqual(drawn(round), { pins: ['25200.0 %/h'], share: ['share 50%', 'burn 43200.0 %/h', 'to shed 21600.0 %/h'] });
  const dollars = { ...round, detail: { ...round.detail, burn_sum: 0.0005, pins: [{ ...round.detail.pins[0], burn: 0.0002 }], target: 0.00025, unit: 'usd' } };
  assert.deepEqual(drawn(dollars), { pins: ['$0.72/h'], share: ['share 50%', 'burn $1.80/h', 'to shed $0.90/h'] });
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
  assert.equal(words(record({ outcome: { accountId: null, kind: 'exhausted' }, request: { poolId: 'jobs' } }), decision({ kind: 'exhausted' })), 'Exhausted in [pool jobs]');
  assert.equal(words(record({ outcome: { accountId: null, kind: 'refused' } }), decision({ kind: 'refused' })), 'Refused before selection');
  const pending = record({ outcome: { completion: completion({}) } });
  assert.deepEqual(texts(d.verdictChips(pending, decision({ by: strategy('pace', [0, 0, 0]) }))).slice(-1), ['cache pending']);
});

test('the settings in force read the twenty in the card order', () => {
  assert.deepEqual(d.SETTINGS_ROWS.map((s) => s.label), [
    'Strategy', 'Tier order', 'Overage', 'Pin window', 'Pin when', 'Sessions placed by', 'Minimum shared prefix', 'Subagents placed by', 'Select below', 'Shed above', 'Move when spent',
    'Overage grace', 'Wind-down', 'Shed share', 'Burn window', 'Shed dwell', 'Warm sessions', 'In-flight cap', 'Retry in place', 'Pace floor',
  ]);
  assert.deepEqual(d.SETTINGS_ROWS.map((s) => d.settingValue(s.key, SETTINGS)), ['Pace', 'subscriptions first', 'prefer plan', '10 min', 'when cached', 'by warm prefix', '20K', 'by cohort', 'off', '90%', 'on', '30 min', '30 min', '50%', '10 min', '15 min', 'when needed', '16', '1 min', '5%']);
  assert.equal(Object.keys(d.OBSTACLE_HINT).length, 14);
  assert.equal(d.OBSTACLE_HINT.budget_unknown, 'its spend unknown while the ledger is degraded');
});

test('the event detail draws the pairs its event carries: the effect, the meaning, the reading and the backoff span', () => {
  const pairs = (kind, detail, o) => Object.fromEntries(d.detailPairs(event(kind, detail, o), NOW, []).map((p) => [p.label, texts(p.chips)]));
  assert.deepEqual(pairs('account_state_changed', { cause: 'funding_refused', detail: null, from: 'ready', to: 'funding_required' }).Effect, ['the account leaves every eligible set until funded']);
  assert.deepEqual(pairs('account_state_changed', { cause: 'deleted', detail: null, from: 'ready', to: 'deleted' }).Effect, ['the account left the registry and this is its last row']);
  assert.deepEqual(pairs('account_state_changed', { cause: 'operator', detail: null, from: 'funding_required', to: 'ready' }).Effect, ['the account joins the eligible sets']);
  assert.deepEqual(pairs('account_state_changed', { cause: 'paired', detail: null, from: 'unpaired', to: 'ready' }).Effect, ['the account joins the eligible sets']);
  assert.deepEqual(pairs('window_spent', { reset: ms(15, 42), source: 'head', utilization: 1, window: 'five_hour' }).Effect, ['a pinned session moves by the spent rule at its next request']);
  const skipped = pairs('ledger_gap', { reason: 'chunk_skipped', request_id: 'req_1' }, { requestId: 'req_1' });
  assert.deepEqual([skipped.Meaning, skipped.Effect], [['an interior chunk of the capture was dropped at the bus'], ['the captured response is stored truncated']]);
  const missing = pairs('ledger_gap', { reason: 'attempt_missing', request_id: 'req_1' }, { requestId: 'req_1' });
  assert.deepEqual([missing.Meaning, missing.Effect], [['the bus dropped an account selection and the trail is short an attempt row'], undefined]);
  for (const reason of ['attempt_missing', 'capture_budget', 'chunk_dropped', 'chunk_skipped', 'expired', 'in_flight_cap', 'poison']) assert.ok(pairs('ledger_gap', { reason }).Meaning?.[0], reason);
  const edit = pairs('registry_changed', { diff: { pace_floor: { from: 0.05, to: 0.1 }, shed_ahead_of_pace: { from: null, to: 0.9 } }, kind: 'pool_updated', target: 'all' }, { accountId: null, actor: 'admin', at: iso(ms(9, 5, 12)), poolId: 'all' });
  // A null old value reads one word in the field's own row and in the reading.
  assert.deepEqual(edit['shed ahead of pace'], ['from none', 'to 90%']);
  assert.deepEqual(edit['Reading it'], ['decisions before 09:05 carry pace floor 5%', 'decisions before 09:05 carry shed ahead of pace none']);
  assert.deepEqual(pairs('poll_backoff_ended', { attempts: 3, duration_ms: 25 * MINUTE, resumed_at: ms(8, 15, 3) })['Backed off'], ['from 07:50', 'to 08:15']);
});

test("a model rejection reads its until off the page's cooldown row and has no pair without one", () => {
  const at = iso(ms(14, 12, 5));
  const rejection = event('model_rejected', { attempt: 1, request_id: 'req_011', source_request_id: 'req_011', wire_model: 'claude-haiku-4-5' }, { at, model: 'claude-haiku-4-5-20251001', requestId: 'req_011' });
  const cooldown = (o = {}, detail = {}) => event('cooldown_set', { reason: 'model_rejected', scope: 'model:claude-haiku-4-5-20251001', source_request_id: 'req_011', until: ms(15, 24), window: 'model', ...detail }, { at, id: 8, requestId: 'req_011', ...o });
  const until = (rows) => d.detailPairs(rejection, NOW, rows).find((p) => p.label === 'Until')?.chips.map(chipText);
  assert.deepEqual(until([cooldown()]), ['15:24']);
  assert.deepEqual(d.detailPairs(rejection, NOW, [cooldown()]).slice(2).map((p) => p.label), ['Model sent', 'Model resolved', 'Cooldown scope', 'Attempt', 'Until', 'Request']);
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
  const until = (row, rows) => d.detailPairs(row, NOW, rows).find((p) => p.label === 'Until')?.chips.map(chipText);
  assert.deepEqual(until(rejection(), [cooldown()]), ['15:24'], 'the router dated the cooldown 3 ms after the rejection');
  assert.deepEqual(until(rejection(), [cooldown({ at: iso(at - 2) })]), ['15:24'], 'and 2 ms before it');
  assert.equal(until(rejection(), [cooldown({ requestId: 'req_012' }, { source_request_id: 'req_012' })]), undefined, "another request's cooldown a few milliseconds on");
  assert.equal(until(rejection({ requestId: null }), [cooldown({ requestId: null }, { source_request_id: null })]), undefined, 'a rejection that names no request reads no cooldown');
});

test('the catalog change lists its models fable, opus, sonnet, haiku, newest first', () => {
  const pairs = Object.fromEntries(d.detailPairs(event('catalog_changed', { added: ['claude-haiku-4-5', 'claude-opus-5', 'claude-fable-5-1', 'claude-opus-5-5'], count: 12, removed: ['claude-sonnet-4-5', 'claude-opus-4-5', 'claude-sonnet-5'] }), NOW, []).map((p) => [p.label, texts(p.chips)]));
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
  assert.equal(d.HOLD_HINT, 'Every account the placement could land on was at its in-flight cap, so the request waited first');
  assert.equal(d.PROMISE_HINT.content, 'A placement by its messages hits when the cache reads the whole prefix it was promised');
  assert.equal(d.PLACED.static(2), 'placed by its static prefix, depth 2');
  const held = record({ outcome: { hold: { accountId: 'primary', boundMs: 10_000, cohort: 'primary' }, kind: 'held' } });
  assert.equal(d.outcomeRows(held, NOW).find((r) => r.label === 'Hold').title, d.HOLD_HINT);
  const promised = record({ outcome: { completion: completion({ cacheReadTokens: 31_000, completedAt: iso(NOW), hit: true, inputTokens: 200, promisedTokens: 31_200, status: 200 }), rung: { depth: 2, prefixTokens: 31_200, rung: 'static' } } });
  assert.equal(d.outcomeRows(promised, NOW).find((r) => r.label === 'Cache read').title, d.PROMISE_HINT.static);
  const row = decision({ by: by('rung', 'static') });
  const verdict = d.verdictChips(promised, row);
  assert.equal(verdict.find((c) => c.value === 'static rung').title, d.PLACED.static(2));
  assert.equal(verdict.find((c) => c.label === 'cache hit').title, d.PROMISE_HINT.static);
});

test("the Pace cell's title names the share, the hours, the deadline and the rate, and the header names the window", () => {
  const title = (pace, o = {}) => d.candidateCells(candidate({ pace, ...o }), record(), NOW).paceTitle;
  const reset = Date.UTC(2026, 8, 19, 8, 30);
  const read = { deadline: iso(reset), end: null, hours: 66, remaining: 0.08, reset: iso(reset), score: 0.08 / 66, window: 'seven_day' };
  assert.equal(title(read), '8% of the 7-day window left over 66 h to its reset at Sep 19 08:30: 0.12 %/h. Inside its tier, the highest above the pace floor wins.');
  const end = ms(16, 44);
  assert.equal(
    title({ deadline: iso(end), end: iso(end), hours: 2.23, remaining: 0.45, reset: iso(reset), score: 0.45 / 2.23, window: 'seven_day_fable' }),
    "45% of the Fable week left over 2 h to its subscription's end at 16:44: 20.18 %/h. Inside its tier, the highest above the pace floor wins.",
  );
  assert.equal(
    title({ deadline: null, end: null, hours: 168, remaining: 1, reset: null, score: 1 / 168, window: 'seven_day' }),
    '100% of the 7-day window left over 168 h with no reset read: 0.60 %/h. Inside its tier, the highest above the pace floor wins.',
  );
  assert.equal(title({ ...read, window: null }), '8% of its window left over 66 h to its reset at Sep 19 08:30: 0.12 %/h. Inside its tier, the highest above the pace floor wins.');
  // Under an hour the span reads in minutes, and from an hour on in whole hours.
  assert.equal(title({ ...read, hours: 0.42, score: 0.08 / 0.42 }), '8% of the 7-day window left over 25 min to its reset at Sep 19 08:30: 19.05 %/h. Inside its tier, the highest above the pace floor wins.');
  assert.equal(title({ ...read, hours: 1.4, score: 0.08 / 1.4 }), '8% of the 7-day window left over 1 h to its reset at Sep 19 08:30: 5.71 %/h. Inside its tier, the highest above the pace floor wins.');
  assert.equal(title({ ...read, hours: 0.2, score: 0.08 / 0.2 }), '8% of the 7-day window left over 12 min to its reset at Sep 19 08:30: 40.00 %/h. Inside its tier, the highest above the pace floor wins.');
  assert.equal(title({ ...read, hours: 3.4, score: 0.08 / 3.4 }), '8% of the 7-day window left over 3 h to its reset at Sep 19 08:30: 2.35 %/h. Inside its tier, the highest above the pace floor wins.');
  // A pace written without the field reads as the null window.
  assert.equal(d.candidateCells(candidate({}), record(), NOW).paceTitle, '91% of its window left over 120 h with no reset read: 0.76 %/h. Inside its tier, the highest above the pace floor wins.');
  const keyPace = { deadline: null, end: null, hours: null, remaining: 0.45, reset: null, score: 0.45, window: null };
  const key = candidate({ accountId: 'overflow', kind: 'api_key', pace: keyPace, planTier: null, tier: [2, 0, 0], windows: { worst_bucket: window5h(0.55, ms(14, 28, 8)) } });
  assert.equal(d.candidateCells(key, record(), NOW).paceTitle, '45% of its worst live bucket left. Scored by the remaining share alone, with no division by hours.');
  assert.equal(title(null), null);

  const header = (window) => d.paceHeader(record({ candidates: [key, candidate({ pace: { ...read, window } })] }));
  assert.equal(header('seven_day_fable'), 'Pace, Fable week left per hour', 'the first subscription member names it, past a key');
  assert.equal(header('seven_day'), 'Pace, 7-day left per hour');
  assert.equal(header('seven_day_opus'), 'Pace, seven day opus left per hour');
  assert.equal(header(null), 'Pace');
  assert.equal(d.paceHeader(record({ candidates: [key] })), 'Pace', 'API key members alone name no window');
});

test("a Pace row's By chip carries the strategy's hint and a rung row's none", () => {
  assert.equal(
    d.PACE_HINT,
    "Pace: the account with the most of its window left per hour until the window resets or the subscription ends wins. A request reads the model's own window when the model has one, else the 7-day window, and a window the account has not read counts as untouched.",
  );
  const placed = d.byOf(decision({ by: strategy('pace', [0, 0, 0]), tier: [0, 0, 0] }));
  assert.deepEqual([placed.value, placed.title], ['Pace P0', d.PACE_HINT]);
  const held = d.byOf(decision({ by: by('hold', 'strategy_pick'), kind: 'held', tier: [0, 0, 0] }));
  assert.deepEqual([held.value, held.title], ['Pace P0', d.PACE_HINT]);
  assert.deepEqual(d.byOf(decision({ by: strategy('priority', [0, 1, 0]), strategy: 'priority' })), { value: 'priority P1' });
  assert.deepEqual(d.byOf(decision({ by: by('rung', 'static') })), { value: 'static rung' });
});
