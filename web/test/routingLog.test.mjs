import assert from 'node:assert/strict';
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
const read = (row, page = []) => ({ by: d.byOf(row, page).value, facts: d.factsOf(row, NOW).map((p) => `${p.label} ${p.value}`) });

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
  ['refused, client drift', decision({ accountId: null, by: by('refusal', 'client'), fact: [fact('client', '2.1.274', 'text')], kind: 'refused', refusal: { kind: 'client', reason: 'hash; client 2.1.274' } }), 'client drift', ['client 2.1.274']],
  ['refused, a key revoked after admission', decision({ accountId: null, by: by('refusal', 'key'), fact: [fact('key', 'laptop', null, { id: 'laptop', kind: 'key' })], kind: 'refused', refusal: { kind: 'key', reason: 'the key was revoked' } }), 'key revoked', ['key laptop']],
  ['probe, renewed its pin', decision({ fact: [fact('epoch', 50, 'count')], kind: 'renewed', probe: true }), 'keepalive', ['epoch 50']],
  ['probe, answered exhausted', decision({ accountId: null, by: by('exhaustion', 'timed'), exhaustion: { deadline: iso(MIDNIGHT), kind: 'timed', obstacle: null, reason: null, window: 'budget' }, fact: [fact('retry', MIDNIGHT, 'time')], kind: 'exhausted', probe: true }), 'budget', ['retry 00:00']],
];

const at = ms(14, 0);
/** The static placement the pin created below follows, in the same second, session and account. */
const placement = decision({ at: iso(at + 400), by: by('rung', 'static'), fact: [fact('prefix', 31_200, 'tokens'), fact('depth', 2, 'count')], sessionId: 's-created' });

/** One case per row of the event table: the event as its writer stores it, and the words the row draws. */
const EVENTS = [
  ['pin created by a placement', event('pin_created', { agent_id: 'main', epoch: 52 }, { sessionId: 's-created' }), 'static rung', ['epoch 52'], [placement]],
  ['pin moved by a rule', event('pin_migrated', { agent_id: 'main', epoch: 53, from_account: 'secondary', reason: 'spent' }), 'window spent', ['epoch 53']],
  ['pin moved by the shed rule', event('pin_migrated', { agent_id: 'main', epoch: 23, from_account: 'secondary', reason: 'shed' }), 'shed', ['epoch 23']],
  ['pin moved, a failover', event('pin_migrated', { agent_id: 'main', epoch: 54, from_account: 'secondary', reason: 'cooldown' }), d.PIN_MIGRATION_LABEL.cooldown, ['epoch 54']],
  ['pin moved, the fast lane', event('pin_migrated', { agent_id: 'main', epoch: 55, from_account: 'primary', reason: 'fast_lane' }), 'fast lane cooling', ['epoch 55']],
  ['pin expired, found idle by the sweep', event('pin_expired', { agent_id: 'main', reasons: ['expired'] }), 'idle', []],
  ['pin expired, any other reason', event('pin_expired', { agent_id: 'main', reasons: ['cooldown', 'spent'] }), 'account cooled', []],
  ['pin released by hand', event('pin_released', { agent_id: 'main' }, { actor: 'admin' }), 'by hand', []],
  ['cooldown set, the whole account', event('cooldown_set', { reason: 'rate_limit', scope: 'account', source_request_id: 'req_1', until: ms(15, 42), window: 'five_hour' }), '429 on 5h', ['until 15:42']],
  ['cooldown set, a retry-after', event('cooldown_set', { reason: 'rate_limit', scope: 'account', until: ms(14, 1, 20), window: 'retry_after' }), '429 retry-after', ['until 14:01']],
  ['cooldown set, a spend cap', event('cooldown_set', { reason: 'spend_cap', scope: 'account', until: ms(16, 0), window: 'overage' }), 'spend cap', ['until 16:00']],
  ['cooldown set, the fast lane', event('cooldown_set', { reason: 'fast_rate_limit', scope: 'fast', until: ms(14, 42), window: 'fast' }), 'fast lane 429', ['until 14:42']],
  ['cooldown set, a model', event('cooldown_set', { reason: 'model_rejected', scope: 'model:claude-haiku-4-5-20251001', until: ms(15, 24), window: 'model' }), '404 on haiku-4-5', ['until 15:24']],
  ['cooldown cleared, its deadline', event('cooldown_cleared', { cause: 'deadline', scope: 'account' }), 'deadline passed', []],
  ['cooldown cleared by hand', event('cooldown_cleared', { cause: 'admin', scope: 'account' }), 'by hand', []],
  ['cooldown cleared, recovered', event('cooldown_cleared', { cause: 'recovered', scope: 'account' }), 'recovered', []],
  ['overage started', event('overage_started', {}), 'in overage', []],
  ['overage ended', event('overage_ended', { since: at - (24 + 4) * HOUR }), '7d reset', ['lasted 1 d 4 h']],
  ['model rejected', event('model_rejected', { attempt: 1, request_id: 'req_011', source_request_id: 'req_1', wire_model: 'claude-haiku-4-5' }), '404', []],
  ['model permitted', event('model_permitted', { attempt: 2, rejected_by: ['primary'], request_id: 'req_012', source_request_id: 'req_1', wire_model: 'claude-haiku-4-5' }), 'after 404', ['attempt 2 of 2']],
  ['registry changed, one field', event('registry_changed', { diff: { shed_ahead_of_pace: { from: null, to: 0.9 } }, kind: 'pool_updated', target: 'all' }, { accountId: null, actor: 'admin', poolId: 'all' }), 'pool edited', ['field shed ahead of pace', 'to 90%']],
  ['registry changed, several fields', event('registry_changed', { diff: { in_flight_cap: { from: 16, to: 8 }, pace_floor: { from: 0, to: 0.1 }, strategy: { from: 'pace', to: 'priority' } }, kind: 'pool_updated', target: 'all' }, { accountId: null, actor: 'admin', poolId: 'all' }), 'pool edited', ['fields 3']],
  ['registry changed, a key revoked', event('registry_changed', { diff: {}, kind: 'key_revoked', target: 'ci' }, { accountId: null, actor: 'admin', keyId: 'ci' }), 'key revoked', []],
  ['re-auth required', event('reauth_required', {}), 'credential refused', ['state routing off']],
  ['refresh succeeded', event('refresh_succeeded', {}), 'refreshed', []],
  ['refresh failed', event('refresh_failed', {}), 'refresh failed', []],
  ['refresh uncertain', event('refresh_uncertain', {}), 'refresh uncertain', []],
  ['ledger gap', event('ledger_gap', { reason: 'chunk_dropped', request_id: 'req_1' }, { accountId: null, requestId: 'req_1' }), 'chunk dropped', ['capture truncated']],
  ['shedding round', event('shedding_round', { marks: [{}, {}, {}], pins: [{}, {}, {}, {}], utilization: 0.92, window: 'five_hour' }), '92% on 5h', ['pins 3 of 4']],
  ['fast credits changed, blocked', event('fast_credits_changed', { after: { disabled_reason: 'org_spend_cap_reached', enabled: true, utilization: 1 }, blocked_after: 'org_spend_cap_reached', blocked_before: null }), 'spend cap reached', ['state unavailable']],
  ['fast credits changed, room again', event('fast_credits_changed', { after: { disabled_reason: null, enabled: true, utilization: 0.31 }, blocked_after: null, blocked_before: 'org_spend_cap_reached' }), 'room', ['state available']],
  ['window spent', event('window_spent', { reset: ms(15, 42), source: 'head', source_request_id: 'req_1', utilization: 1, window: 'five_hour' }), 'head on 5h', ['resets 15:42']],
  ['window spent, read by the poll', event('window_spent', { reset: ms(15, 42), source: 'poll', utilization: 1, window: 'seven_day' }), 'poll on 7d', ['resets 15:42']],
  ['window reset', event('window_reset', { next_reset: ms(19, 0), observed: 'sample', reset_at: at, utilization_before: 0.92, window: 'five_hour' }), '5h reset', ['was 92%']],
  ['router started', event('router_started', { pins_loaded: 10, version: '0.1.0' }, { accountId: null }), 'restart', ['pins 10']],
  ['warm prefix expired', event('warm_prefix_expired', { cause: 'ttl', deepest: { depth: 1, prefix_tokens: 18_400, tier: 'message', ttl_ms: 300_000 }, entries: 2, root: 's0' }), '5 min tier', ['prefix 18.4K', 'depth 1']],
  ['poll backoff started', event('poll_backoff_started', { attempts: 1, next_attempt_at: ms(13, 53), reason: '429' }), 'rate limited', ['next 13:53']],
  ['poll backoff ended', event('poll_backoff_ended', { attempts: 3, duration_ms: 25 * MINUTE, resumed_at: at }), 'clean poll', ['lasted 25 min']],
  ['budget reached', event('budget_reached', { budget_usd: 20, period: 'day', period_end: MIDNIGHT, spent_usd: 20.4 }, { poolId: 'codex' }), 'daily budget', ['spent $20.40 of $20.00']],
  ['budget period ended', event('budget_period_ended', { period: 'month', period_start: at, previous_spent_usd: 18.2 }, { poolId: 'codex' }), 'monthly budget', ['spent $18.20']],
  ['wind-down started', event('wind_down_started', { end_source: 'cancelled', ends_at: ms(15, 0), wind_down_minutes: 60 }, { poolId: 'all' }), 'subscription ends', ['until 15:00']],
  ['catalog changed, a model added', event('catalog_changed', { added: ['claude-opus-5-5'], count: 12, removed: [] }), 'model added', ['models 12']],
  ['catalog changed, a model removed', event('catalog_changed', { added: [], count: 11, removed: ['claude-sonnet-4-5'] }), 'model removed', ['models 11']],
  ['catalog changed, models changed', event('catalog_changed', { added: ['claude-sonnet-5-5'], count: 12, removed: ['claude-sonnet-4-5'] }), 'models changed', ['models 12']],
  ['account state changed, a credential refused', event('account_state_changed', { cause: 'auth_failed', detail: null, from: 'ready', to: 'reauth_required' }), 'credential refused', ['state re-auth required']],
  ['account state changed, the credit balance', event('account_state_changed', { cause: 'funding_refused', detail: 'Your credit balance is too low', from: 'ready', to: 'funding_required' }), 'credit balance', ['state funding required']],
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

test('a pin created with no placement beside it reads the placement word', () => {
  assert.equal(d.byOf(event('pin_created', { agent_id: 'main', epoch: 52 }, { sessionId: 's-created' }), []).value, 'placement');
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
  const emitted = [...DECISIONS, ...EVENTS].flatMap(([, row, , , page]) => d.factsOf(row, NOW).map((p) => p.label));
  assert.deepEqual([...new Set(emitted)].sort(), [...TABLE_LABELS].sort());
  for (const label of emitted) assert.ok(Object.values(d.FACT_LABEL).includes(label), label);
});

test('every event kind has its words and its tone', () => {
  const kinds = [
    'account_state_changed', 'budget_period_ended', 'budget_reached', 'catalog_changed', 'cooldown_cleared', 'cooldown_set', 'fast_credits_changed', 'ledger_gap', 'model_permitted', 'model_rejected',
    'overage_ended', 'overage_started', 'pin_created', 'pin_expired', 'pin_migrated', 'pin_released', 'poll_backoff_ended', 'poll_backoff_started', 'reauth_required', 'refresh_failed',
    'refresh_succeeded', 'refresh_uncertain', 'registry_changed', 'router_started', 'shedding_round', 'warm_prefix_expired', 'wind_down_started', 'window_reset', 'window_spent',
  ];
  assert.deepEqual(Object.keys(d.EVENT_KIND_LABEL).sort(), kinds);
  assert.equal(d.EVENT_KIND_LABEL.pin_migrated, 'pin moved');
  assert.equal(d.EVENT_KIND_LABEL.shedding_round, 'shedding round');
  const tones = Object.fromEntries(kinds.map((k) => [k, d.eventTone(k)]).filter(([, t]) => t !== 'neutral'));
  assert.deepEqual(tones, { account_state_changed: 'serious', budget_reached: 'serious', cooldown_set: 'warning', ledger_gap: 'serious', model_rejected: 'warning', reauth_required: 'critical', window_spent: 'warning' });
  assert.equal(d.eventTone('a_kind_added_later'), 'neutral');
});

test('every decision kind has its words and its tone', () => {
  assert.deepEqual(d.DECISION_KIND_LABEL, { exhausted: 'exhausted', held: 'held', moved: 'moved', placed: 'placed', refused: 'refused', renewed: 'renewed' });
  assert.deepEqual(d.DECISION_KIND_TONE, { exhausted: 'critical', held: 'warning', moved: 'accent', placed: 'accent', refused: 'critical', renewed: 'neutral' });
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
    answered: 'warning', blocked: 'warning', bound: 'accent', chosen: 'accent', dropped: 'warning', exhausted: 'warning', fell_through: 'warning', fired: 'accent', found: 'neutral', held: 'warning', match: 'accent',
    no: 'neutral', none: 'neutral', not_run: 'neutral', passed: 'neutral', placed: 'accent', preferred: 'accent', refused: 'warning', released: 'warning', renewed: 'accent',
  };
  for (const [verdict, tone] of Object.entries(tones)) assert.equal(d.verdictTone(verdict), tone, verdict);
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
  assert.equal(d.entityHref('session', '407dd39b-dd60'), '/sessions?session=407dd39b-dd60');
  assert.equal(d.entityHref('pool', 'all'), '/pools?pool=all');
  assert.equal(d.entityHref('key', 'laptop'), '/keys?key=laptop');
  assert.equal(d.entityHref('model', 'claude-sonnet-5'), '/models?model=claude-sonnet-5');
});

test('each link in builds its URL with its range', () => {
  const at = ms(14, 2, 10);
  const finished = ms(14, 2, 40);
  assert.deepEqual(d.aroundRequest(at, finished, NOW), { from: at - 15 * MINUTE, to: finished + 15 * MINUTE });
  assert.deepEqual(d.aroundRequest(at, null, NOW), { from: at - 15 * MINUTE, to: at + 15 * MINUTE });
  // `to` stops at now.
  assert.deepEqual(d.aroundRequest(ms(14, 25), null, NOW), { from: ms(14, 10), to: NOW });
  assert.deepEqual(d.requestLink('req_1d9c3e55-0b2', at, finished, NOW), { from: at - 15 * MINUTE, request: 'req_1d9c3e55-0b2', to: finished + 15 * MINUTE });
  assert.deepEqual(d.aroundSubject(null, NOW), { from: NOW - HOUR, to: NOW });
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

test('model ids order fable, opus, sonnet, haiku, newest version first', () => {
  const ids = ['claude-haiku-4-5-20251001', 'claude-sonnet-5', 'claude-opus-5', 'claude-fable-5-1', 'claude-sonnet-5-5', 'claude-opus-5-5', 'claude-opus-4-5-20251101', 'some-other-model'];
  assert.deepEqual([...ids].sort(d.compareModels), ['claude-fable-5-1', 'claude-opus-5-5', 'claude-opus-5', 'claude-opus-4-5-20251101', 'claude-sonnet-5-5', 'claude-sonnet-5', 'claude-haiku-4-5-20251001', 'some-other-model']);
});
