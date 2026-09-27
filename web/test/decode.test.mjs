import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';
import { same, shapeOf } from './shape.mjs';

const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;
const isIso = (s) => typeof s === 'string' && !Number.isNaN(Date.parse(s)) && s.endsWith('Z');

test('an account decodes with its windows, plan, pools, and models', () => {
  const body = fixture('account');
  const a = d.decodeAccount(body);
  assert.equal(a.id, 'business');
  assert.equal(a.kind, 'oauth');
  assert.equal(a.plan, 'Max', "the profile's plan flag names the plan, so the badge, the search and the chips read one name");
  assert.equal(d.decodeAccount({ ...body, subscription: { ...body.subscription, plan: null } }).plan, 'Max 5x', "a document without the flags reads the tier's own label");
  assert.equal(a.subscription.rateLimitTier, 'default_claude_max_5x');
  assert.deepEqual(
    Object.keys(a.subscription).sort(),
    ['billingType', 'derivedEndsAt', 'endsAt', 'organizationType', 'plan', 'rateLimitTier', 'since', 'trialEndsAt'],
    'the route answers the profile and subscription fields and nothing more, so the card draws nothing beside them',
  );
  assert.equal(a.subscription.plan, 'max');
  assert.ok(isIso(a.subscription.since));
  assert.equal(a.subscription.trialEndsAt, null);
  assert.deepEqual(fixture('account').windows.map((w) => w.id), ['7d', 'fable', 'five_hour'], 'the route passes each sampler spelling through, in the ledger order');
  assert.deepEqual(a.windows.map((w) => w.id), ['5h', '7d'], 'the poll spelling folds onto the UI id and the order is the drawn one');
  assert.equal(a.fableWindow.id, '7d_fable');
  assert.ok(isIso(a.windows[0].resetsAt) && isIso(a.windows[0].observedAt));
  assert.equal(a.windows[0].utilization, 0.62, 'the five_hour sample is the 5h window');
  assert.deepEqual(a.pools, [{ poolId: 'main', priority: 0 }]);
  assert.equal(a.extraUsage.monthlyLimitCredits, 5000);
  assert.equal(a.requests.lifetime, 4);
  // The fixture seeds one live model-scoped cooldown so the key-models and models-state fixtures have a row.
  assert.equal(a.status, 'cooldown');
  assert.equal(a.email, 'test@example.invalid');
  assert.deepEqual(
    Object.keys(a).sort(),
    ['cancelled', 'cooldowns', 'credential', 'email', 'extraUsage', 'fableWindow', 'id', 'kind', 'label', 'overage', 'plan', 'pools', 'requests', 'routingEnabled', 'status', 'subscription', 'windows'],
    'an account decodes to the fields the console draws and nothing more',
  );
});

test('an account decodes its cancelled flag and both ends, and a key its cap', () => {
  const body = fixture('account');
  const cancelled = d.decodeAccount({ ...body, cancelled: true, subscription: { ...body.subscription, ends_at: 5_000, derived_ends_at: 9_000 } });
  assert.deepEqual(
    [cancelled.cancelled, cancelled.subscription.endsAt, cancelled.subscription.derivedEndsAt],
    [true, d.iso(5_000), d.iso(9_000)],
  );
  assert.equal(d.decodeAccount(body).cancelled, false, 'a row the wire answers no flag for reads uncancelled');
  const key = fixture('keys').keys[0];
  assert.equal(d.decodeKey({ ...key, in_flight_cap: 4 }).inFlightCap, 4);
  assert.equal(d.decodeKey(key).inFlightCap, null, 'no cap on the wire is no cap in the type');
});

test('an account decodes its quarantine flag and the router\'s overage evidence', () => {
  const body = fixture('account');
  const quiet = d.decodeAccount(body);
  assert.deepEqual([quiet.credential.quarantined, quiet.overage], [false, null]);
  const loud = d.decodeAccount({ ...body, credential: { ...body.credential, quarantined: true }, overage: { in_use: true, observed_at: 3_000, reset: 9_000, since: 1_000 } });
  assert.equal(loud.credential.quarantined, true);
  assert.deepEqual(loud.overage, { inUse: true, observedAt: d.iso(3_000), reset: d.iso(9_000), since: d.iso(1_000) });
  assert.deepEqual(d.decodeAccount({ ...body, overage: { in_use: false, observed_at: 3_000, reset: null, since: null } }).overage, { inUse: false, observedAt: d.iso(3_000), reset: null, since: null }, 'a sample reports no start, so the line under the badge has nothing to count from');
  assert.equal(d.decodeAccount({ ...body, credential: null }).credential.quarantined, false, 'a slot with no credential has no attempt to be unsure about');
});

test('extra usage the provider switched off keeps the reason it gave', () => {
  const body = fixture('account');
  const off = d.decodeAccount({ ...body, extra_usage: { ...body.extra_usage, disabled_reason: 'card declined', in_use: false, is_enabled: false } }).extraUsage;
  assert.deepEqual([off.enabled, off.inUse, off.disabledReason], [false, false, 'card declined'], 'the reason exists for exactly the account whose extra usage is off');
  assert.equal(d.decodeAccount({ ...body, extra_usage: null }).extraUsage, null);
});

test('the account list folds state, enabled, and cooldowns onto one status', () => {
  const list = fixture('accounts').accounts.map(d.decodeAccount);
  assert.deepEqual(list.map((a) => a.id), ['business', 'direct'], 'sorted by label');
  const direct = list.find((a) => a.id === 'direct');
  assert.equal(direct.kind, 'api_key');
  assert.equal(direct.subscription, null);
  assert.equal(direct.extraUsage, null);
  assert.equal(d.decodeAccount({ ...fixture('account'), enabled: false }).status, 'paused');
  assert.equal(d.decodeAccount({ ...fixture('account'), state: 'reauth_required' }).status, 'reauth_required');
  assert.equal(d.decodeAccount({ ...fixture('account'), state: 'funding_required' }).status, 'funding_required');
  assert.equal(d.decodeAccount({ ...fixture('account'), state: 'unpaired' }).status, 'unpaired');
  assert.equal(d.decodeAccount({ ...fixture('account'), cooldowns: [{ scope: 'account', until: 1, reason: 'rate_limit', window: '5h', set_at: 0 }] }).status, 'cooldown');
});

test('a window both samplers wrote folds to one entry carrying the newer reading', () => {
  const base = fixture('account');
  const older = { id: '5h', observed_at: 1_000_000, resets_at: 4_600_000, utilization: 0.11 };
  const newer = { id: 'five_hour', observed_at: 1_900_000, resets_at: 4_600_000, utilization: 0.77 };
  for (const windows of [[older, newer], [newer, older]]) {
    const a = d.decodeAccount({ ...base, windows });
    assert.deepEqual(a.windows.map((w) => w.id), ['5h'], 'the header and the poll spelling are one window');
    assert.equal(a.windows[0].utilization, 0.77, 'the newer reading wins, whichever order the route sent the two in');
    assert.equal(a.windows[0].observedAt, new Date(1_900_000).toISOString());
  }
  const all = d.decodeAccount({
    ...base,
    windows: [{ id: 'fable', observed_at: 1_000_000, resets_at: 9, utilization: 0.1 }, newer, { id: 'seven_day', observed_at: 1_000_000, resets_at: 9, utilization: 0.4 }],
  });
  assert.deepEqual(all.windows.map((w) => w.id), ['5h', '7d'], 'the fold draws in page order, not in the order the route sent');
  assert.equal(all.fableWindow.id, '7d_fable');
});

test('a quota history both samplers wrote merges into one series in time order', () => {
  const history = d.decodeQuotaHistory({
    account_id: 'business',
    windows: [
      { id: '5h', samples: [{ at: 2_000_000, utilization: 0.5 }] },
      { id: 'seven_day', samples: [{ at: 1_000_000, utilization: 0.2 }] },
      { id: 'five_hour', samples: [{ at: 1_000_000, utilization: 0.3 }] },
    ],
  });
  assert.deepEqual(history.map((h) => h.windowId), ['5h', '7d'], 'merged, and in the order the pages draw them');
  assert.deepEqual(history[0].samples.map((s) => s.utilization), [0.3, 0.5], 'both samplers land in one series, oldest first');
  assert.equal(history[0].accountId, 'business');
});

test('plan labels read the tier', () => {
  assert.equal(d.planLabel('default_claude_max_20x'), 'Max 20x');
  assert.equal(d.planLabel('claude_pro'), 'Pro');
  assert.equal(d.planLabel('something_else'), 'something_else');
});

test('plan names read the flag, then the tier, and nothing without either', () => {
  const sub = { billingType: '', organizationType: '', plan: null, rateLimitTier: '', since: null, trialEndsAt: null };
  assert.equal(d.planName({ ...sub, plan: 'pro', rateLimitTier: 'default_claude_max_5x' }), 'Pro', 'the flag wins over the tier');
  assert.equal(d.planName({ ...sub, rateLimitTier: 'claude_team' }), 'Team');
  assert.equal(d.planName(sub), null, 'an empty tier names no plan');
  assert.equal(d.planName(null), null);
});

test('keys carry status, totals, and the created secret stays out of the row', () => {
  const [k] = fixture('keys').keys.map(d.decodeKey);
  assert.equal(k.name, 'Desktop');
  assert.equal(k.status, 'active');
  assert.deepEqual(k.modelRules, [{ not: true, pattern: 'claude-opus-*' }]);
  assert.equal(k.modelRulesMode, 'extend');
  assert.equal(k.longContextTwins, true);
  assert.equal(k.lifetime.requests, 4);
  assert.equal(k.usage7d.tokens > 0, true);
  assert.equal(typeof k.sessionsOnRetiredSecret, 'number');
  assert.equal(k.retiredSecretLastUsedAt, null, 'a key whose retired secret never authenticated carries no time');
  assert.equal(
    d.decodeKey({ ...fixture('keys').keys[0], retired_secret_last_used_at: 1790029200000 }).retiredSecretLastUsedAt,
    new Date(1790029200000).toISOString(),
    'a retired secret carries the instant it was last used, not just a type',
  );
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], revoked_at: 5 }).status, 'revoked');
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], expires_at: 5 }).status, 'expired');
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], enabled: false }).status, 'disabled', 'routing refuses a key that is switched off, so the row may not read active');
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], enabled: false, revoked_at: 5 }).status, 'revoked', 'the more final state wins');
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], enabled: false, expires_at: 5 }).status, 'expired');
  assert.equal('secret' in d.decodeKey(fixture('key_created')), false);
});

test('a key row decodes its model rules or takes their defaults when the row carries none', () => {
  const row = fixture('keys').keys[0];
  const ruled = d.decodeKey({ ...row, model_rules: [{ pattern: 'claude-sonnet-5' }, { not: true, pattern: '*' }], model_rules_mode: 'replace', long_context_twins: false });
  assert.deepEqual(ruled.modelRules, [{ not: false, pattern: 'claude-sonnet-5' }, { not: true, pattern: '*' }]);
  assert.equal(ruled.modelRulesMode, 'replace');
  assert.equal(ruled.longContextTwins, false);
  const bare = { ...row };
  delete bare.model_rules;
  delete bare.model_rules_mode;
  delete bare.long_context_twins;
  const defaults = d.decodeKey(bare);
  assert.deepEqual(defaults.modelRules, []);
  assert.equal(defaults.modelRulesMode, 'extend');
  assert.equal(defaults.longContextTwins, true);
});

test('a key keeps its enabled flag beside the status the flag folds into', () => {
  const row = fixture('keys').keys[0];
  assert.equal(d.decodeKey(row).enabled, true);
  const off = d.decodeKey({ ...row, enabled: false });
  assert.deepEqual([off.enabled, off.status], [false, 'disabled']);
  const lapsed = d.decodeKey({ ...row, enabled: false, expires_at: 5 });
  assert.deepEqual([lapsed.enabled, lapsed.status], [false, 'expired'], 'an expired key that was also switched off says so, which the status alone cannot');
  const silent = { ...row };
  delete silent.enabled;
  assert.equal(d.decodeKey(silent).enabled, true, 'a row without the flag reads as on, as its status does');
});

test('a key edit sends enabled every time and expires_at only when the dialog changed it', () => {
  const input = { bodyCaptureHours: null, enabled: false, keepaliveOptOut: false, longContextTwins: true, modelRules: [], modelRulesMode: 'extend', name: 'batch', notes: null, poolId: 'codex' };
  const kept = d.encodeKey({ ...input, expiresAt: undefined });
  assert.equal(kept.enabled, false);
  assert.equal('expires_at' in kept, false, 'an absent expiry leaves the stored one alone');
  assert.equal(d.encodeKey({ ...input, expiresAt: null }).expires_at, null, 'null clears the expiry');
  assert.equal(d.encodeKey({ ...input, enabled: true, expiresAt: '2026-10-01T00:00:00.000Z' }).expires_at, Date.parse('2026-10-01T00:00:00.000Z'), 'the route takes milliseconds');
  assert.equal(d.encodeKey({ ...input, enabled: true, expiresAt: undefined }).enabled, true);
});

test('a key\'s processed rule list decodes with the level, the rule, and the cooldowns each entry covers', () => {
  const km = d.decodeKeyModels(fixture('key_models'));
  assert.equal(km.keyId, 'desktop');
  assert.equal(km.poolId, 'main');
  assert.equal(km.modelRulesMode, 'extend');
  assert.equal(km.longContextTwins, true);
  assert.deepEqual(
    km.rules.map((r) => [r.level, r.pattern, r.not, r.cooldowns.length]),
    [['key', 'claude-opus-*', true, 0], ['pool', 'claude-haiku-*', true, 0]],
    'a denied model is never routed, so neither deny entry carries the live haiku cooldown',
  );
});

test('the model state decodes the cooldowns grouped by model, and the rejections and permits with their detail lifted', () => {
  const state = d.decodeModelsState(fixture('models_state'));
  assert.deepEqual(state.cooldowns.map((g) => [g.model, g.rows.length]), [['claude-haiku-4-5-20251001', 1]]);
  assert.equal(state.cooldowns[0].rows[0].accountId, 'business');
  assert.deepEqual(
    state.rejections.map((r) => [r.accountId, r.model, r.wireModel, r.requestId, r.upstreamRequestId, r.rejectedBy]),
    [['business', 'claude-haiku-4-5-20251001', 'claude-haiku-4-5', 'r-failover', 'req-rejected', []]],
  );
  assert.deepEqual(
    state.permits.map((p) => [p.accountId, p.wireModel, p.requestId, p.upstreamRequestId, p.rejectedBy]),
    [['direct', 'claude-haiku-4-5', 'r-failover', 'req-permitted', ['business']]],
  );
  assert.ok(isIso(state.rejections[0].at) && isIso(state.permits[0].at));
});

test('a pool decodes its policy, status, members, and traffic', () => {
  const [p] = fixture('pools').pools.map(d.decodePool);
  assert.equal(p.name, 'Main');
  assert.equal(p.strategy, 'pace');
  assert.equal(p.keepalive.costCapUsdPerDay, 4);
  assert.equal(p.keepalive.warmSubagents, true);
  assert.equal(p.cacheTtl.enabled, false);
  assert.equal(p.cacheTtl.plan.main, '1h');
  assert.equal(p.cacheDiagnostics, null);
  assert.equal(p.status, 'active');
  assert.equal(p.traffic24h.length, 24);
  assert.ok(isIso(p.traffic24h[0].at));
  assert.deepEqual(p.members.map((m) => m.accountId), ['business', 'direct']);
  assert.deepEqual(p.members.map((m) => m.eligible), ['eligible', 'eligible'], 'both members are servable in the captured pool');
  assert.equal(d.decodePool({ ...fixture('pools').pools[0], members: [{ account_id: 'business', priority: 0, eligible: 'overage' }] }).members[0].eligible, 'overage');
  assert.equal(d.decodePool({ ...fixture('pools').pools[0], members: [{ account_id: 'business', priority: 0 }] }).members[0].eligible, 'unavailable', 'a member row without the field reads as not servable');
});

test('a pool decodes its select-below threshold and encodes it back', () => {
  const row = fixture('pools').pools[0];
  assert.equal(d.decodePool(row).selectBelowUtilization, null, 'the captured pool sets none');
  const p = d.decodePool({ ...row, select_below_utilization: 0.4 });
  assert.equal(p.selectBelowUtilization, 0.4);
  const back = d.encodePool({ ...p, name: p.name, status: 'active' });
  assert.equal(back.select_below_utilization, 0.4);
  assert.equal(d.encodePool({ ...p, name: p.name, selectBelowUtilization: null, status: 'active' }).select_below_utilization, null, 'null clears it rather than leaving it alone');
});

test('a pool decodes the three strategies and the flag, and encodes the stored flag back', () => {
  const row = fixture('pools').pools[0];
  assert.equal(d.decodePool({ ...row, strategy: 'pace' }).strategy, 'pace');
  assert.equal(d.decodePool({ ...row, strategy: 'priority' }).strategy, 'priority');
  const wire = { ...row, strategy: 'round_robin', prefer_subscription: true };
  const p = d.decodePool(wire);
  assert.deepEqual([p.strategy, p.preferSubscription], ['round_robin', true]);
  const back = d.encodePool({ ...p, name: p.name, status: 'active' });
  assert.deepEqual([back.strategy, back.prefer_subscription], ['round_robin', true]);
});

test("a pool's cache TTL decodes the two rule sets and encodes them back", () => {
  const row = fixture('pools').pools[0];
  const wire = {
    ...row,
    cache_ttl: {
      enabled: true,
      inject_when_absent: true,
      plan: { keepalive: '1h', main: '5m', subagent: '1h', models: [{ model: 'claude-haiku-4-5-20251001', main: '5m', subagent: '5m' }] },
      metered: { keepalive: '5m', main: '5m', subagent: '5m', models: [] },
    },
    cache_diagnostics: true,
    keepalive: { ...row.keepalive, warm_subagents: false },
  };
  const p = d.decodePool(wire);
  assert.deepEqual([p.cacheTtl.enabled, p.cacheTtl.injectWhenAbsent, p.cacheTtl.plan.main, p.cacheTtl.plan.subagent], [true, true, '5m', '1h']);
  assert.deepEqual(p.cacheTtl.plan.models, [{ main: '5m', model: 'claude-haiku-4-5-20251001', subagent: '5m' }]);
  assert.deepEqual([p.cacheTtl.metered.main, p.cacheTtl.metered.subagent], ['5m', '5m']);
  assert.equal(p.cacheDiagnostics, true);
  assert.equal(p.keepalive.warmSubagents, false);
  const back = d.encodePool({ ...p, name: p.name, status: 'active' });
  assert.equal(back.cache_ttl.enabled, true);
  assert.equal(back.cache_ttl.inject_when_absent, true);
  assert.equal(back.cache_ttl.plan.models[0].model, 'claude-haiku-4-5-20251001');
  assert.equal(back.cache_ttl.metered.main, '5m');
  assert.equal(back.cache_diagnostics, true);
  assert.equal(back.keepalive.warm_subagents, false);
  assert.equal(d.decodePool({ ...row, cache_diagnostics: false }).cacheDiagnostics, false);
});

test('a pool decodes the engine fields and encodes them back', () => {
  const row = fixture('pools').pools[0];
  const p = d.decodePool(row);
  assert.deepEqual(
    [p.shedShare, p.burnWindowSeconds, p.sourceDwellSeconds, p.migrateWhenSpent, p.warmSessions],
    [0.5, 600, 900, true, 'when_needed'],
    'the captured pool holds the balanced shedding preset',
  );
  assert.deepEqual([p.retryInPlaceSeconds, p.inFlightCap, p.paceFloor, p.overageMoveGraceMinutes, p.windDownMinutes], [10, 16, 0.05, 30, 30], 'and the balanced failover preset');
  assert.deepEqual(p.movesPerHour, { spent: 1 });
  const wire = {
    ...row,
    shed_share: 0.3,
    burn_window_seconds: 900,
    source_dwell_seconds: 1800,
    migrate_when_spent: false,
    warm_sessions: 'never',
    retry_in_place_seconds: 20,
    in_flight_cap: 8,
    pace_floor: 0.1,
    overage_move_grace_minutes: 45,
    wind_down_minutes: 120,
    moves_per_hour: { shed: 2, spent: 0.5 },
  };
  const q = d.decodePool(wire);
  assert.deepEqual(q.movesPerHour, { shed: 2, spent: 0.5 });
  const back = d.encodePool({ ...q, name: q.name, status: 'active' });
  const fields = ['shed_share', 'burn_window_seconds', 'source_dwell_seconds', 'migrate_when_spent', 'warm_sessions', 'retry_in_place_seconds', 'in_flight_cap', 'pace_floor', 'overage_move_grace_minutes', 'wind_down_minutes'];
  assert.deepEqual(Object.fromEntries(fields.map((f) => [f, back[f]])), Object.fromEntries(fields.map((f) => [f, wire[f]])), 'every engine field goes back under the name the route reads');
  assert.equal('moves_per_hour' in back, false, 'the moves are the route\'s count, never a setting');
  assert.equal(d.decodePool({ ...row, warm_sessions: 'sometimes' }).warmSessions, 'when_needed', 'an unknown mode reads as the default');
  assert.deepEqual(d.decodePool({ ...row, moves_per_hour: { shed: 'x', spent: 2 } }).movesPerHour, { spent: 2 }, 'a count that is not a number is left out');
});

test('a pool the route answers without a cache TTL policy decodes to the defaults', () => {
  const wire = { ...fixture('pools').pools[0] };
  delete wire.cache_ttl;
  assert.deepEqual(d.decodePool(wire).cacheTtl, {
    enabled: false,
    injectWhenAbsent: false,
    metered: { keepalive: '5m', main: '5m', models: [], subagent: '5m' },
    plan: { keepalive: '1h', main: '1h', models: [], subagent: '1h' },
  });
  const partial = d.decodePool({ ...wire, cache_ttl: { enabled: true, plan: { main: '5m' } } });
  assert.deepEqual(
    [partial.cacheTtl.enabled, partial.cacheTtl.plan.main, partial.cacheTtl.plan.subagent, partial.cacheTtl.metered.main],
    [true, '5m', '1h', '5m'],
    'a field the route leaves out falls back one field at a time, each set against its own default',
  );
  const keepalive = { ...wire.keepalive };
  delete keepalive.warm_subagents;
  assert.equal(d.decodePool({ ...wire, keepalive }).keepalive.warmSubagents, true, 'a keepalive block without the flag warms subagents');
});

test('a pool decodes the pin, placement and quota fields and its members\' budgets, and encodes them back', () => {
  const row = fixture('pools').pools[0];
  const p = d.decodePool(row);
  assert.deepEqual(
    [p.affinityMode, p.subagentPlacement, p.sessionPlacement, p.minSharedPrefixTokens, p.quotaHeaders],
    ['always', 'by_cohort', 'by_strategy', 20_000, 'pass'],
    'the captured pool holds the store defaults',
  );
  const direct = p.members.find((m) => m.accountId === 'direct');
  assert.deepEqual(
    [direct.kind, direct.budgetUsd, direct.budgetPeriod, direct.spentUsd, direct.periodEnd],
    ['api_key', 50, 'month', row.members[1].spent_usd, new Date(row.members[1].period_end).toISOString()],
    'the budgeted API-key member reads its budget and the spend the ledger reported against it',
  );
  const business = p.members.find((m) => m.accountId === 'business');
  assert.deepEqual([business.kind, business.budgetUsd, business.spentUsd, business.periodEnd], ['oauth', null, null, null], 'a member without a budget reads no spend');
  assert.deepEqual(p.keys, [{ enabled: true, id: 'desktop', label: 'Desktop', prefix: row.keys[0].prefix }]);

  const wire = {
    ...row,
    affinity_mode: 'when_cached',
    subagent_placement: 'with_main',
    session_placement: 'by_warm_prefix',
    min_shared_prefix_tokens: 30_000,
    quota_headers: 'hide',
    members: [{ ...row.members[1], budget_usd: 12.5, budget_period: 'day', eligible: 'budget' }],
    keys: [{ ...row.keys[0], enabled: false }],
  };
  const q = d.decodePool(wire);
  assert.deepEqual([q.affinityMode, q.subagentPlacement, q.sessionPlacement, q.minSharedPrefixTokens, q.quotaHeaders], ['when_cached', 'with_main', 'by_warm_prefix', 30_000, 'hide']);
  assert.deepEqual([q.members[0].budgetUsd, q.members[0].budgetPeriod, q.members[0].eligible], [12.5, 'day', 'budget'], 'a member at its budget reads the verdict the route names');
  assert.equal(q.keys[0].enabled, false);
  const back = d.encodePool({ ...q, name: q.name, status: 'active' });
  assert.deepEqual(
    [back.affinity_mode, back.subagent_placement, back.session_placement, back.min_shared_prefix_tokens, back.quota_headers],
    ['when_cached', 'with_main', 'by_warm_prefix', 30_000, 'hide'],
  );
  assert.deepEqual(back.members, [{ account_id: 'direct', budget_period: 'day', budget_usd: 12.5, priority: 1 }], 'a member goes back with its budget and nothing the route reports');
  assert.equal('keys' in back, false, 'the keys are the registry\'s, never a pool setting');

  const bare = { ...row, members: [{ account_id: 'direct', priority: 0, eligible: 'eligible' }], keys: [{ id: 'k', label: 'K', prefix: 'p' }] };
  for (const field of ['affinity_mode', 'subagent_placement', 'session_placement', 'min_shared_prefix_tokens', 'quota_headers']) delete bare[field];
  const defaults = d.decodePool(bare);
  assert.deepEqual(
    [defaults.affinityMode, defaults.subagentPlacement, defaults.sessionPlacement, defaults.minSharedPrefixTokens, defaults.quotaHeaders],
    ['always', 'by_cohort', 'by_strategy', 20_000, 'pass'],
    'a field the route leaves out reads the store default',
  );
  assert.deepEqual([defaults.members[0].budgetUsd, defaults.members[0].budgetPeriod, defaults.members[0].kind], [null, 'month', 'oauth'], 'a member row without the fields reads no budget and no API key');
  assert.equal(defaults.keys[0].enabled, true, 'a key row without the fields reads the store defaults');
  assert.equal(d.decodePool({ ...wire, affinity_mode: 'sometimes', quota_headers: 'strip' }).affinityMode, 'always', 'an unknown spelling reads the default');
});

test('a request page carries the trail with zero-based attempt indexes and the cursor', () => {
  const page = d.decodeRequestPage(fixture('requests'));
  assert.equal(page.nextCursor, null);
  const failover = page.requests.find((r) => r.id === 'r-failover');
  assert.deepEqual(failover.attempts.map((a) => [a.index, a.accountId, a.upstreamStatus, a.outcome]), [[0, 'direct', 429, 'rate_limited'], [1, 'business', 200, 'succeeded']]);
  assert.equal(failover.attempts[0].cooldownSet.reason, 'account');
  assert.ok(isIso(failover.at) && isIso(failover.attempts[0].startedAt));
  assert.equal(failover.status, 200);
  assert.equal(failover.transport, 'sse');
  assert.equal(failover.usage.cacheRead, 500);
  assert.equal(failover.usage.thinking, 40);
  assert.deepEqual(failover.shape, { contextEdits: ['clear_thinking_20251015'], effort: 'high', maxTokens: 32_000, messageCount: 1, requestedServiceTier: null, requestedSpeed: null, systemBlocks: 4, temperature: null, thinkingBudget: 31_999, thinkingType: 'enabled', toolCount: 12 });
  assert.deepEqual(failover.attempts[1].response, { clearedInputTokens: null, inferenceGeo: 'us', refusalCategory: null, refusalRecommendedModel: null, servedModel: null, speed: 'standard', stopReason: 'end_turn', stopSequence: null });
  assert.equal(failover.responseTruncated, false);
  const failed = page.requests.find((r) => r.id === 'r-failed');
  assert.equal(failed.errorCode, 'upstream_error');
  assert.equal(failed.clientDrift, 'hash');
  assert.equal(failed.outcome, 'failed');
  assert.equal(d.decodeRequest(fixture('request').request).id, 'r-failover');
  const moved = fixture('request').events.map(d.decodeRequestEvent);
  assert.deepEqual([moved[0].kind, moved[0].accountId, moved[0].detail.from_account, moved[0].detail.reason], ['pin_migrated', 'business', 'direct', 'spent']);
  assert.deepEqual([moved[0].keyId, moved[0].sessionId, moved[0].model], ['desktop', 's1', 'claude-sonnet-5'], 'the pin the event belongs to');
  assert.ok(isIso(moved[0].at));
  assert.deepEqual([moved[1].kind, moved[1].accountId, moved[1].detail.reasons], ['pin_expired', 'business', ['cooldown', 'spent']]);
  assert.equal(d.decodeRequest(fixture('request').request).canonicalModel, 'claude-sonnet-5');
});

test('a request row carries the generation of the key it was read under', () => {
  const body = fixture('requests');
  const live = body.requests.find((r) => r.id === 'r-captured').key_generation;
  const earlier = body.requests.find((r) => r.id === 'r-earlier-holder').key_generation;
  assert.ok(Number.isInteger(live) && Number.isInteger(earlier) && live > earlier, 'the fixture holds two generations of desktop');
  const rows = d.decodeRequestPage(body).requests;
  assert.deepEqual(rows.map((r) => [r.id, r.keyGeneration]), body.requests.map((r) => [r.id, r.key_generation]));
  assert.equal(d.decodeRequest(fixture('request').request).keyGeneration, fixture('request').request.key_generation, 'the detail route answers the same row');
});

test('a request row carries the client class, the hint headers, and the turn origin', () => {
  const detail = d.decodeRequest(fixture('request').request);
  assert.deepEqual(
    [detail.client, detail.agentType, detail.contextCompacted, detail.prevToolMs, detail.turnOrigin, detail.compaction],
    ['claude-code/cli', 'Explore', 'auto', 1235, 'human', null],
  );
  const quiet = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-captured');
  assert.equal(quiet.client, 'claude-code/cli');
  assert.deepEqual([quiet.agentType, quiet.compaction, quiet.contextCompacted, quiet.prevToolMs], [null, null, null, null], 'a request that sent no hint reads each as not sent');
});

test('an attempt window with a reset and no utilization reads as not reported, not as zero', () => {
  const windows = [{ id: '5h', resets_at: 1_000, utilization: null }, { id: '7d', resets_at: 2_000, utilization: 0.5 }];
  const [row] = d.decodeRequestPage({ next_cursor: null, requests: [{ attempts: [{ index: 1, windows }] }] }).requests;
  assert.deepEqual(row.attempts[0].windows.map((w) => [w.id, w.utilization]), [['5h', null], ['7d', 0.5]]);
  const live = d.decodeLiveRequest({ attempts: [{ index: 1, windows }] });
  assert.deepEqual(live.attempts[0].windows.map((w) => w.utilization), [null, 0.5], 'the live feed reads its windows through the same decoder');
});

test('an account hour with no utilization sample carries no peak rather than a zero one', () => {
  const [hour] = d.decodeAccountRollups(fixture('analytics_accounts'));
  const wire = fixture('analytics_accounts').accounts[0].hours[0];
  assert.deepEqual([wire.utilization_5h_max, wire.utilization_7d_max], [0.4, null], 'the fixture samples the 5-hour window alone');
  assert.deepEqual([hour.peak['5h'], hour.peak['7d'], hour.peak['7d_fable']], [0.4, null, null]);
});

test('a captured body carries the bytes the route counted, never the bytes the drawer would measure', () => {
  const whole = d.decodeBodies(fixture('request_bodies'));
  assert.equal(whole.request.model, 'claude-sonnet-5');
  assert.equal(whole.requestBytesStored, whole.requestBytesSeen, 'nothing was cut, so both sides read the same count');
  assert.equal(whole.responseTruncated, false);
  // A stream is stored as its own text: rendering it back through JSON.stringify adds the escapes, and a
  // length measured there would answer less than the stored count rather than the ten bytes that were cut.
  const stream = d.decodeBodies({ request_id: 'r-cut', response: 'event: message_start\ndata: {"type":"message_start"}\n\n', response_bytes_stored: 8_388_608, response_bytes_seen: 8_388_618, response_truncated: true });
  assert.equal(d.bytesDropped(stream.responseBytesSeen, stream.responseBytesStored), 10);
  const multibyte = d.decodeBodies({ request_id: 'r-utf8', request: { text: 'héllo' }, request_bytes_stored: 17, request_bytes_seen: 17 });
  assert.equal(multibyte.requestBytesStored, 17, 'the seventeen bytes the body encodes to, not its sixteen characters');
  assert.equal(d.bytesDropped(multibyte.requestBytesSeen, multibyte.requestBytesStored), 0);
  const cut = d.decodeBodies({ request_id: 'r-json', response: '{"content":[{"type":"text","text":"aaa', response_bytes_stored: 8_388_608, response_bytes_seen: 8_389_632, response_truncated: true });
  assert.equal(cut.responseTruncated, true);
  assert.equal(d.bytesDropped(cut.responseBytesSeen, cut.responseBytesStored), 1_024);
});

test('analytics groups and points keep the histograms and reasons whole', () => {
  const rows = fixture('analytics_breakdown').rows.map(d.decodeAnalyticsGroup);
  const desktop = rows.find((g) => g.key === 'desktop');
  assert.equal(desktop.requests, 4);
  const earlier = rows.find((g) => g.key.startsWith('desktop@'));
  assert.equal(earlier.label, 'desktop (deleted)');
  assert.equal(earlier.requests, 1);
  assert.equal(desktop.histograms.ttfbMs.length, 64);
  assert.equal(typeof desktop.percentiles.ttfbMs.p50, 'number');
  assert.equal(desktop.cacheMissReasons.none, 4);
  assert.equal(desktop.tokens.thinking, 160);
  assert.equal(desktop.savings.wastedProbeReasons.idle_limit, 0);
  assert.equal(desktop.errors.upstream_error, 1);
  const points = fixture('analytics_series').points.map(d.decodeAnalyticsPoint);
  assert.ok(points.length >= 24 && isIso(points[0].at));
  const rollups = d.decodeAccountRollups(fixture('analytics_accounts'));
  assert.ok(rollups.every((r) => isIso(r.hour) && typeof r.peak['5h'] === 'number'));
});

test('the burn series decodes each point with its instant, both token sides, and a plan share only where the route has one', () => {
  const body = fixture('analytics_burn');
  const burn = d.decodeBurn(body);
  assert.equal(burn.bucketSeconds, 3_600);
  assert.equal(burn.points.length, body.points.length);
  // The last point the fit shares a plan window for, so every field it is read against holds a value a misnamed field
  // could not stand in for.
  const at = body.points.findLastIndex((p) => p.plan_percent !== null);
  const [last, wire] = [burn.points[at], body.points[at]];
  assert.ok(wire.meter_tokens > 0 && wire.plan_percent > 0 && wire.cached_tokens > 0 && wire.uncached_tokens > 0, 'the fixture reports a plan point that burned');
  assert.deepEqual(last, { at: new Date(wire.at).toISOString(), cachedTokens: wire.cached_tokens, group: 'business', meterTokens: wire.meter_tokens, planPercent: wire.plan_percent, uncachedTokens: wire.uncached_tokens, usd: wire.usd });
  const direct = burn.points[burn.points.length - 1];
  assert.deepEqual([direct.group, direct.planPercent, direct.usd], ['direct', null, body.points.at(-1).usd], 'an API-key point spends dollars and shares no plan window');
  assert.ok(burn.points.every((p) => isIso(p.at)));
  // Every grouping but the upstream one answers the share as null, which is no fit rather than an idle window.
  const [keyed] = d.decodeBurn({ bucket_seconds: 21_600, points: [{ ...wire, group: 'desktop', plan_percent: null }] }).points;
  assert.equal(keyed.planPercent, null);
  assert.equal(d.decodeBurn({ bucket_seconds: 21_600, points: [] }).bucketSeconds, 21_600);
});

test('the rewrite series decodes with the moves by reason over the range', () => {
  const body = fixture('analytics_rewrites');
  const rewrites = d.decodeRewrites(body);
  assert.equal(rewrites.points.length, body.points.length);
  assert.deepEqual(rewrites.points[0], { at: new Date(body.points[0].at).toISOString(), rewriteTokens: 0 });
  assert.deepEqual(rewrites.moves, { spent: 1 });
  // The reasons are the route's own names, so one the console does not label yet stays a key of its own.
  assert.deepEqual(d.decodeRewrites({ moves: { later_reason: 2, shed: 3 }, points: [] }).moves, { later_reason: 2, shed: 3 });
});

test('a window fit decodes the scale against the prediction and the band pair', () => {
  const [fit] = fixture('analytics_fits').fits.map(d.decodeWindowFit);
  assert.deepEqual(fit, {
    accountId: 'business',
    bandDivergent: false,
    bandFactor: 1.4,
    bandFactorPredicted: 1.45,
    confidence: 0.8,
    corrections: {},
    drift: 0.5,
    fittedAt: new Date(1_000_000).toISOString(),
    intervals: 12,
    predicted: 11_890_000,
    scale: 5_945_000,
    window: 'five_hour',
  });
  // A tier with no row for the window predicts nothing, so it has no drift, and a fit that has not folded both
  // sides of the band carries no band factor.
  const bare = d.decodeWindowFit({ ...fixture('analytics_fits').fits[0], band_factor: null, band_factor_predicted: null, corrections: { 'claude-sonnet-5': 1.08 }, drift: null, predicted: null });
  assert.deepEqual([bare.predicted, bare.drift, bare.bandFactor, bare.bandFactorPredicted, bare.corrections], [null, null, null, null, { 'claude-sonnet-5': 1.08 }]);
});

test('an attempt carries the selection trace with its skipped members as named objects', () => {
  const detail = d.decodeRequest(fixture('request').request);
  assert.equal(detail.attempts[0].selection, null, 'an attempt stored without a trace reads as none');
  // The fixture's trace has not had its cache read folded in, so the member reads as absent.
  assert.deepEqual(detail.attempts[1].selection, { cacheRead: null, moved: null, renewed: false, skipped: [{ accountId: 'direct', obstacle: 'ranked' }], strategy: 'pace', tier: [0, 0, 0] });
  assert.equal(detail.requestClass, null, 'the fixture request sent no class header');
  // No captured request carries a folded cache read, a move, a renewal or a class yet, so this row is built from
  // the backend's serde names: `SelectionTrace` and `Skipped` in the routing crate, `cache_read` from the ledger.
  const row = d.decodeRequest({
    ...fixture('request').request,
    attempts: [{ index: 1, account_id: 'business', selection: { cache_read: 4_200, moved: 'shed', renewed: false, skipped: [{ account_id: 'direct', obstacle: 'above_ceiling' }, { account_id: 'mail', obstacle: 'subscription_ended' }], strategy: 'round_robin', tier: [1, 2, 1] } }, { index: 2, account_id: 'business', selection: { renewed: true, skipped: [], strategy: 'pace', tier: null } }],
    request_class: 'subagent',
  });
  assert.equal(row.requestClass, 'subagent');
  assert.deepEqual(row.attempts[0].selection, { cacheRead: 4_200, moved: 'shed', renewed: false, skipped: [{ accountId: 'direct', obstacle: 'above_ceiling' }, { accountId: 'mail', obstacle: 'subscription_ended' }], strategy: 'round_robin', tier: [1, 2, 1] });
  assert.deepEqual(row.attempts[1].selection, { cacheRead: null, moved: null, renewed: true, skipped: [], strategy: 'pace', tier: null }, 'a renewed pin answers no tier');
  const [odd] = d.decodeRequest({ attempts: [{ index: 1, selection: { renewed: false, skipped: [], strategy: 'pace', tier: [0, 1] } }] }).attempts;
  assert.equal(odd.selection.tier, null, 'a tier that is not three numbers reads as none rather than as a partial one');
});

test('request rows fold into analytics groups with the same sums the routes answer', () => {
  const page = d.decodeRequestPage(fixture('requests'));
  const [all] = d.foldGroups(page.requests, 'all');
  assert.equal(all.requests, 5);
  assert.equal(all.succeeded, 4);
  assert.equal(all.failed, 1);
  assert.equal(all.attempts, 6);
  assert.equal(all.failovers, 1);
  assert.equal(all.tokens.cacheRead, 2500);
  assert.equal(all.histograms.ttfbMs.reduce((a, b) => a + b, 0), 5);
  assert.equal(all.cacheMissReasons.none, 5);
  assert.equal(all.probes.ok, 0);
  const captured = page.requests.find((r) => r.id === 'r-captured');
  const busy = d.foldGroups([{ ...captured, usage: { ...captured.usage, webSearch: 3, webFetch: 1 } }, captured], 'all')[0].tokens;
  assert.equal(busy.webSearch, 3 + captured.usage.webSearch);
  assert.equal(busy.webFetch, 1 + captured.usage.webFetch);
  const byKey = d.foldGroups(page.requests, 'key');
  assert.deepEqual(byKey.map((g) => g.label), ['desktop', 'desktop (deleted)'], 'a reused id is one group per holder, as the breakdown answers it');
  // No captured breakdown groups on effort, so the fold is where the dimension is proven.
  assert.deepEqual(d.foldGroups(page.requests, 'effort').map((g) => g.key), ['high']);
  assert.deepEqual(d.foldGroups([{ ...captured, shape: { ...captured.shape, effort: null } }], 'effort').map((g) => g.key), ['none'], 'a request that asked for no effort files under none, as the rollups file it');
  const oldest = page.requests.find((r) => r.id === 'r-failover');
  const newest = page.requests.find((r) => r.id === 'r-captured');
  const points = d.foldRequests(page.requests, 3_600_000, oldest.at, newest.at);
  assert.ok(points.length >= 1 && points.every((p) => p.groups.length === 1));
});

test('the fold uses the ledger formulas for failovers, throughput, and cache misses', () => {
  const base = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-captured');
  const attempt = base.attempts[0];
  // One attempt pair on the same account is a retry, not a failover.
  const retried = { ...base, attempts: [attempt, { ...attempt, index: 1 }] };
  assert.equal(d.foldGroups([retried], 'all')[0].failovers, 0);
  assert.equal(d.foldGroups([{ ...retried, attempts: [attempt, { ...attempt, accountId: 'other', index: 1 }] }], 'all')[0].failovers, 1);
  // 100 tokens with first content at nine seconds of ten: 100 tokens per second over the last second.
  const slowStart = { ...base, durationMs: 10_000, ttftMs: 9_000, usage: { ...base.usage, output: 100 } };
  const [g] = d.foldGroups([slowStart], 'all');
  assert.ok(g.percentiles.outputTps.p50 >= 90 && g.percentiles.outputTps.p50 <= 110, `p50 ${g.percentiles.outputTps.p50} is the bucket holding 100`);
  assert.equal(d.foldGroups([{ ...slowStart, ttftMs: null }], 'all')[0].histograms.outputTps.reduce((a, b) => a + b, 0), 0, 'no first-content timing, no sample');
  // An API-key request with writes and no diagnostic is a miss by tokens, whatever the reason says.
  const apiKey = { ...base, attempts: [{ ...attempt, cacheMissReason: null }], usage: { ...base.usage, cacheRead: 0, cacheWrite5m: 300 } };
  const [m] = d.foldGroups([apiKey], 'all');
  assert.deepEqual([m.cacheHits, m.cacheMisses, m.uncached], [0, 1, 0]);
  assert.equal(m.cacheMissReasons.none, 0, 'the reason breakdown stays its own count');
});

test('a cache-miss reason the console does not know keeps its own name on the attempt and in every sum', () => {
  const body = fixture('requests');
  const wire = body.requests.find((r) => r.id === 'r-captured');
  const row = d.decodeRequest({ ...wire, attempts: wire.attempts.map((a) => ({ ...a, cache_miss_reason: 'future_reason' })) });
  assert.ok(row.attempts.every((a) => a.cacheMissReason === 'future_reason'), 'the attempt answers the reason as sent rather than reading unavailable');
  assert.equal(d.decodeRequest({ ...wire, attempts: wire.attempts.map((a) => ({ ...a, cache_miss_reason: null })) }).attempts[0].cacheMissReason, null);
  const group = d.decodeAnalyticsGroup({ ...fixture('analytics_breakdown').rows[0], cache_miss_reasons: { future_reason: 3, none: 2 } });
  assert.equal(group.cacheMissReasons.future_reason, 3, 'the breakdown keeps the unknown key under its name');
  assert.equal(group.cacheMissReasons.none, 2);
  assert.equal(group.cacheMissReasons.not_requested, 0, 'every known reason stays present so each column draws');
  const [folded] = d.foldGroups([row], 'all');
  assert.equal(folded.cacheMissReasons.future_reason, 1, 'a session fold counts it under its name too');
  const summed = d.sumGroups([group, group, folded]);
  assert.equal(summed.cacheMissReasons.future_reason, 7);
  assert.equal(summed.cacheMissReasons.none, 4);
  assert.deepEqual(d.missReasonOrder([group, { cacheMissReasons: { a_reason: 1, none: 0 } }]), [...d.CACHE_MISS_REASONS, 'a_reason', 'future_reason'], 'the known eight in reading order, then the rest alphabetically');
  assert.equal(d.missLabel('future_reason'), 'future_reason');
  assert.equal(d.missLabel('messages_changed'), 'History edited');
});

test('a cache-miss reason named like an object member keeps its name as its label and its count as a number in every sum', async () => {
  const names = ['__proto__', 'constructor', 'toString'];
  const own = (o, k) => (Object.prototype.hasOwnProperty.call(o, k) ? o[k] : undefined);
  assert.deepEqual(names.map(d.missLabel), names);
  // JSON.parse keeps `__proto__` as an own key, as the route's body arrives; an object literal would set the prototype.
  const group = d.decodeAnalyticsGroup({ ...fixture('analytics_breakdown').rows[0], cache_miss_reasons: JSON.parse('{"__proto__": 3, "constructor": 4, "toString": 5, "none": 2}') });
  assert.deepEqual(names.map((n) => own(group.cacheMissReasons, n)), [3, 4, 5], 'the decoder keeps each count under its name');
  const wire = fixture('requests').requests.find((r) => r.id === 'r-captured');
  const rows = names.map((name) => d.decodeRequest({ ...wire, attempts: wire.attempts.map((a) => ({ ...a, cache_miss_reason: name })) }));
  assert.deepEqual(rows.map((r) => r.attempts[r.attempts.length - 1].cacheMissReason), names);
  const [folded] = d.foldGroups(rows, 'all');
  assert.deepEqual(names.map((n) => own(folded.cacheMissReasons, n)), [1, 1, 1], 'a session fold counts each under its name');
  const summed = d.sumGroups([group, folded]);
  assert.deepEqual(names.map((n) => own(summed.cacheMissReasons, n)), [4, 5, 6], 'the sum is a number for each');
  assert.deepEqual(d.missReasonOrder([summed]), [...d.CACHE_MISS_REASONS, ...names]);
  const to = new Date();
  const [mocked] = await new d.MockApi().getAnalyticsBreakdown({ from: new Date(to.getTime() - 86_400_000).toISOString(), groupBy: 'key', to: to.toISOString() });
  assert.equal(Object.getPrototypeOf(mocked.cacheMissReasons), null, 'the mock sums into a map with no inherited members too');
});

test('an error code named like an object member keeps its count under its code, and every sum of it is a number', async () => {
  const codes = ['__proto__', 'constructor'];
  const own = (o, k) => (Object.prototype.hasOwnProperty.call(o, k) ? o[k] : undefined);
  // JSON.parse keeps `__proto__` as an own key, as the route's body arrives; an object literal would set the prototype.
  const group = d.decodeAnalyticsGroup({ ...fixture('analytics_breakdown').rows[0], errors: JSON.parse('{"__proto__": 2, "constructor": 3}') });
  assert.deepEqual(codes.map((c) => own(group.errors, c)), [2, 3], 'the decoder keeps each count under its code');
  const wire = fixture('requests').requests.find((r) => r.id === 'r-captured');
  const [folded] = d.foldGroups(codes.map((errorCode) => ({ ...d.decodeRequest(wire), errorCode })), 'all');
  assert.deepEqual(codes.map((c) => own(folded.errors, c)), [1, 1], 'a session fold counts each under its code');
  const summed = d.sumGroups([group, folded]);
  assert.deepEqual(codes.map((c) => own(summed.errors, c)), [3, 4], 'the sum is a number for each');
  const to = new Date();
  const [mocked] = await new d.MockApi().getAnalyticsBreakdown({ from: new Date(to.getTime() - 86_400_000).toISOString(), groupBy: 'key', to: to.toISOString() });
  assert.equal(Object.getPrototypeOf(mocked.errors), null, 'the mock sums into a map with no inherited members too');
});

test('a session fold groups on the client class and narrows to a class pattern the way the rollups do', () => {
  const row = d.decodeRequest(fixture('requests').requests.find((r) => r.id === 'r-captured'));
  const rows = [row, { ...row, id: 'r-second' }, { ...row, client: null, id: 'r-unclassed' }];
  const byClient = d.foldGroups(rows, 'client');
  assert.deepEqual(byClient.map((g) => [g.key, g.label, g.requests]).sort(), [['-', 'unattributed', 1], ['claude-code/cli', 'claude-code/cli', 2]], 'a row with no class files under -, as the rollups file it');
  const narrowed = d.foldGroups(rows, 'client', { clientPatterns: ['claude-code/*'] });
  assert.deepEqual(narrowed.map((g) => [g.key, g.requests]), [['claude-code/cli', 2]], 'a row with no class is matched as -, which the pattern does not name');
  assert.deepEqual(d.foldGroups(rows, 'all', { clientPatterns: ['sdk/*'] }), []);
});

test('the session fold buckets a range the way the analytics routes do', () => {
  const hour = 3_600_000;
  const day = 24 * hour;
  assert.equal(d.bucketSizeFor(48 * hour), hour);
  assert.equal(d.bucketSizeFor(48 * hour + 1), 6 * hour);
  assert.equal(d.bucketSizeFor(20 * day), 6 * hour, 'a twenty-day range buckets at six hours, as the route answers it');
  assert.equal(d.bucketSizeFor(30 * day), 6 * hour);
  assert.equal(d.bucketSizeFor(30 * day + 1), day);
});

test('a session fold marks what request rows cannot supply and a sum carries the gap rather than a number', () => {
  const rows = d.decodeRequestPage(fixture('requests')).requests;
  const [folded] = d.foldGroups(rows, 'all');
  assert.equal(folded.fromRequests, true);
  assert.equal(folded.savings, null);
  assert.equal(folded.keptWarm, null);
  assert.equal(folded.cost.cacheRead, null);
  assert.equal(folded.cost.cacheWrite, null);
  const route = d.decodeAnalyticsGroup(fixture('analytics_breakdown').rows[0]);
  assert.equal(route.fromRequests, undefined, 'a route group carries every figure');
  const mixed = d.sumGroups([route, folded]);
  assert.equal(mixed.cost.cacheRead, null, 'a null source makes the sum unavailable, never NaN');
  assert.equal(mixed.cost.cacheWrite, null);
  assert.equal(mixed.savings, null);
  assert.equal(mixed.keptWarm, null);
  assert.equal(mixed.fromRequests, true);
  assert.equal(mixed.costUsd, route.costUsd + folded.costUsd, 'the figures the rows do carry still sum');
  const routeOnly = d.sumGroups([route, route]);
  assert.equal(routeOnly.cost.cacheRead, route.cost.cacheRead * 2);
  assert.equal(routeOnly.savings.grossUsd, route.savings.grossUsd * 2);
  assert.equal(routeOnly.keptWarm.requests, route.keptWarm.requests * 2);
  assert.equal(routeOnly.fromRequests, undefined);
  const [bucket] = d.foldRequests(rows, 3_600_000, rows[rows.length - 1].at, rows[0].at);
  assert.ok(bucket.groups.every((g) => g.savings === null && g.cost.cacheRead === null), 'every folded bucket marks the same gaps');
});

test('a recipe held because every account of its pool was exhausted keeps that reason', () => {
  const [wire] = fixture('keepalive_recipes').recipes;
  const recipe = d.decodeRecipe({ ...wire, reason: 'pool_exhausted', state: 'held' });
  assert.equal(recipe.reason, 'pool_exhausted');
  assert.equal(recipe.state, 'held');
});

test('the downstream snippet keeps the placeholder and names the inference listener when it is known', () => {
  const known = d.snippetFor('127.0.0.1:8080').split('\n');
  assert.equal(known.length, 4);
  assert.equal(known[0], '# inference is served on 127.0.0.1:8080; use the URL that reaches that listener');
  assert.equal(known[1], `ANTHROPIC_BASE_URL=${d.PLACEHOLDER_URL}`, 'the admin origin is never substituted here');
  const unknown = d.snippetFor(null).split('\n');
  assert.deepEqual(unknown, [`ANTHROPIC_BASE_URL=${d.PLACEHOLDER_URL}`, 'CLAUDE_CODE_OAUTH_TOKEN=<api key>', '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1']);
});

test('a session fold groups on the client each row named', () => {
  const [captured, failed] = d.decodeRequestPage(fixture('requests')).requests;
  assert.equal(captured.userAgent, 'claude-cli/2.1.269 (external, cli)', 'the row carries the agent the route answers');
  const rows = [captured, { ...failed, userAgent: 'phoebis-batch/0.9 (external, ci)' }, { ...failed, id: 'r-none', userAgent: null }];
  const groups = d.foldGroups(rows, 'user_agent');
  assert.deepEqual(groups.map((g) => g.key).sort(), ['-', 'claude-cli/2.1.269 (external, cli)', 'phoebis-batch/0.9 (external, ci)'], 'a row that named no client falls under the marker the rollups file it as');
  assert.equal(groups.find((g) => g.key === '-').label, 'unattributed', 'which every route answers as unattributed');
  assert.deepEqual(groups.map((g) => g.requests), [1, 1, 1]);
  const named = d.foldGroups(rows, 'user_agent', { agentGroups: ['claude-cli/2.1.*', 'phoebis-batch/*'] });
  assert.deepEqual(named.map((g) => g.key).sort(), ['claude-cli/2.1.*', 'other', 'phoebis-batch/*'], 'the row that named no client falls under other, as the breakdown route files it');
  assert.deepEqual(named.map((g) => g.requests), [1, 1, 1]);
  assert.equal(d.foldGroups(rows, 'all', { agentPatterns: ['claude-cli/*'] })[0].requests, 1, 'the pattern keeps only the clients it admits');
});

test('a session fold groups on the request class each row sent, and a classless row where the rollups file it', () => {
  const [captured, failed] = d.decodeRequestPage(fixture('requests')).requests;
  const rows = [{ ...captured, requestClass: 'main' }, { ...failed, requestClass: 'auxiliary' }, { ...failed, id: 'r-main-2', requestClass: 'main' }, { ...failed, id: 'r-none', requestClass: null }];
  const groups = d.foldGroups(rows, 'request_class');
  assert.deepEqual(groups.map((g) => [g.key, g.requests]).sort(), [['-', 1], ['auxiliary', 1], ['main', 2]]);
  assert.equal(groups.find((g) => g.key === '-').label, 'unattributed', 'the label every route answers for the reserved value');
});

test('a folded bucket carries one group per key the page groups on', () => {
  const [captured, failed] = d.decodeRequestPage(fixture('requests')).requests;
  const rows = [captured, { ...failed, keyId: 'laptop' }];
  const [bucket] = d.foldRequests(rows, 3_600_000, failed.at, captured.at, 'key');
  assert.deepEqual(bucket.groups.map((g) => g.label).sort(), ['desktop', 'laptop']);
  assert.deepEqual(d.foldRequests(rows, 3_600_000, failed.at, captured.at)[0].groups.map((g) => g.key), ['all'], 'the ungrouped default is unchanged');
  assert.equal(d.foldRequests(rows, 3_600_000, '1970-01-01T00:00:00.000Z', '1970-01-01T01:00:00.000Z').length, 1, 'a range ending on a bucket start adds no empty trailing bucket');
});

test('a session fold keeps two holders of one key id apart and labels the earlier one deleted', () => {
  const rows = d.decodeRequestPage(fixture('requests')).requests;
  const live = rows.find((r) => r.id === 'r-captured');
  const earlier = rows.find((r) => r.id === 'r-earlier-holder');
  const groups = d.foldGroups(rows, 'key');
  assert.deepEqual(
    groups.map((g) => [g.key, g.label, g.requests]),
    [[`desktop@${live.keyGeneration}`, 'desktop', 4], [`desktop@${earlier.keyGeneration}`, 'desktop (deleted)', 1]],
    'the key a group carries is the lifetime the requests route selects by',
  );
  assert.deepEqual(d.foldGroups([{ ...live, keyGeneration: 0, keyId: null }], 'key').map((g) => [g.key, g.label]), [['-@0', 'unattributed']]);
  const points = d.foldRequests([{ ...earlier, at: '1970-01-01T00:10:00.000Z' }, { ...live, at: '1970-01-01T01:10:00.000Z' }], 3_600_000, '1970-01-01T00:00:00.000Z', '1970-01-01T02:00:00.000Z', 'key');
  assert.deepEqual(points.map((p) => p.groups.map((g) => g.label)), [['desktop (deleted)'], ['desktop']], 'a bucket holding the earlier holder alone still labels it against the whole range');
});

test('a session view folds every page the cursor names', () => {
  const page = d.decodeRequestPage(fixture('requests'));
  const pages = [{ nextCursor: 'c1', requests: page.requests.slice(0, 2) }, { nextCursor: null, requests: page.requests.slice(2) }];
  let calls = 0;
  const load = async (cursor) => { calls += 1; return cursor === undefined ? pages[0] : pages[1]; };
  return d.loadSessionRequests(load, 20).then(({ partial, requests }) => {
    assert.equal(calls, 2);
    assert.equal(requests.length, 5, 'both pages folded');
    assert.equal(partial, false);
    return d.loadSessionRequests(async () => ({ nextCursor: 'more', requests: page.requests }), 2);
  }).then(({ partial, requests }) => {
    assert.equal(partial, true, 'the page bound marks the result partial');
    assert.equal(requests.length, 10);
  });
});

test('the dashboard reads the live pin count the route answers beside the rollups', () => {
  assert.equal(fixture('dashboard').sessions.active_pins, 1);
  assert.equal(d.decodeDashboard(fixture('dashboard')).activePins, 1);
  assert.equal(d.decodeDashboard({ ...fixture('dashboard'), sessions: { active_pins: 3 } }).activePins, 3);
});

test('the dashboard, sessions, recipes, and overview decode', () => {
  const o = d.decodeDashboard(fixture('dashboard'));
  assert.equal(o.hourly.length, 24);
  assert.equal(o.requests24h, 5);
  const sessions = fixture('sessions').sessions.map(d.decodeSession);
  const s1 = sessions.find((s) => s.sessionId === 's1');
  assert.ok(sessions.every((s) => typeof s.keyGeneration === 'number' && typeof s.historical === 'boolean'), 'every session says which generation of its key owns it and whether that holder is gone');
  assert.equal(s1.agentId, null, 'an unpinned row names no agent');
  assert.equal(sessions.find((s) => s.sessionId === 's-fixture').agentId, 'main', 'a pinned row names the pin\'s own agent');
  assert.equal(sessions.find((s) => s.sessionId === 's0').historical, true, 'the row of an earlier holder of the same key id reads historical');
  assert.equal(s1.keyGeneration, fixture('sessions').sessions.find((s) => s.session_id === 's1').key_generation, 'the live holder names the generation its key row carries');
  assert.equal(s1.companion, 'reporting');
  assert.equal(s1.keepalive.state, 'busy');
  assert.equal(s1.requests, 2);
  assert.ok(isIso(s1.lastUsedAt));
  const [recipe] = fixture('keepalive_recipes').recipes.map(d.decodeRecipe);
  assert.equal(recipe.state, 'armed');
  assert.equal(recipe.reason, null);
  const ov = d.decodeKeepaliveOverview(fixture('keepalive_overview'));
  assert.equal(ov.sessionsWithCompanion, 1);
  assert.equal(ov.costToday[0].capUsd, 4);
});

test('a settings answer with no compatibility block reads the defaults the router ships with', () => {
  const bare = { ...fixture('settings') };
  delete bare.compatibility;
  assert.deepEqual(d.decodeSettings(bare).compatibility, { cacheDiagnostics: false, outboundGzip: false, profile: 'claude-code-2.1.269-linux-x64', usageEntrypoint: 'cli' }, 'the profile falls back to the one the about block carries, and the rest to what the router does without the block');
  assert.deepEqual(d.decodeSettings({ ...bare, about: { ...bare.about, profile: 'claude-code-9.9.9-linux-x64' } }).compatibility.profile, 'claude-code-9.9.9-linux-x64');
});

test('settings decode the quota, routing, storage, and usage blocks the route answers', () => {
  const wire = fixture('settings');
  const s = d.decodeSettings(wire);
  assert.deepEqual(s.quota, { pollIntervalSeconds: wire.quota.poll_interval_seconds, sampleRetentionDays: wire.quota.sample_retention_days });
  assert.deepEqual(s.routing, { catalogCacheSeconds: wire.routing.catalog_cache_seconds, catalogRefreshSeconds: wire.routing.catalog_refresh_seconds, cooldownFallbackSeconds: wire.routing.cooldown_fallback_seconds, maxAttempts: wire.routing.max_attempts, modelRejectionSeconds: wire.routing.model_rejection_seconds, startupFetchSeconds: wire.routing.startup_fetch_seconds });
  assert.deepEqual(s.storage, { ledgerPath: wire.storage.ledger_path, path: wire.storage.path, pricingPath: null }, 'no override file is a null path, never an empty one');
  assert.deepEqual(s.usage, { cacheTtlSeconds: wire.usage.cache_ttl_seconds, refreshConcurrency: wire.usage.refresh_concurrency });
  assert.equal(d.decodeSettings({ ...wire, storage: { ...wire.storage, pricing_path: './pricing.yaml' } }).storage.pricingPath, './pricing.yaml');
});

test('settings decode the plan factors', () => {
  const wire = fixture('settings');
  const f = d.decodeSettings(wire).planFactors;
  assert.deepEqual([f.measured, f.reference], [wire.plan_factors.measured, 'claude-opus-5']);
  assert.deepEqual(Object.keys(f.models).sort(), Object.keys(wire.plan_factors.models).sort());
  assert.deepEqual(f.models['claude-fable-5-1'], { cacheRead: 0.026, cacheWrite1h: 1.23, cacheWrite5m: 1.23, factor: 3.61, input: 1, measured: '2026-09-19', output: 4.67, unmeasured: false });
  assert.deepEqual([f.models['claude-fable-5'].measured, f.models['claude-fable-5'].unmeasured], [null, true], 'a row the meter has not measured names no date');
  const bare = { ...wire };
  delete bare.plan_factors;
  assert.deepEqual(d.decodeSettings(bare).planFactors, { measured: '', models: {}, reference: '' }, 'an answer without the table reads as an empty one');
});

test('an account patch encodes the cancelled flag and its end', () => {
  const end = '2026-10-19T09:15:00.000Z';
  assert.deepEqual(d.encodeAccountPatch({ cancelled: true, subscriptionEndsAt: end }), { cancelled: true, subscription_ends_at: Date.parse(end) }, 'the route takes milliseconds');
  assert.deepEqual(d.encodeAccountPatch({ cancelled: false, subscriptionEndsAt: null }), { cancelled: false, subscription_ends_at: null }, 'a cleared end is sent as null, not left out');
});

test('settings, pricing, the import preview, pairing, quota, and usage decode', () => {
  const s = d.decodeSettings(fixture('settings'));
  assert.equal(s.access.viewerConfigured, true);
  assert.equal(s.about.publicUrl, null);
  assert.equal(s.sessions.expireAfterSeconds, 86400);
  assert.equal(s.keepalive.maxWorkers, 4);
  assert.equal(s.listeners.metrics, '127.0.0.1:9464');
  assert.deepEqual(s.compatibility, { cacheDiagnostics: false, outboundGzip: false, profile: 'claude-code-2.1.269-linux-x64', usageEntrypoint: 'cli' });
  assert.equal(s.pricing.models.find((m) => m.model === 'claude-sonnet-5').inputPerM, 2);
  const preview = d.decodePriceImport(fixture('pricing_import'));
  // The imported file carries the one model the router serves, at the five prices the shipped table already
  // holds, so an exact diff would count no change. `diff_against` compares the five fields with `!=`, and the
  // file's 2e-07 per token reads back as 0.19999999999999998 per million against a stored 0.2, so the route
  // answers one. This pins the route's answer, artefact and all; the tolerance belongs to the import, not here.
  assert.equal(preview.inFile, 1);
  assert.equal(preview.changes, 1);
  assert.ok(preview.token.startsWith('imp_'));
  const pairing = d.decodePairingSession(fixture('pairing_started'));
  assert.ok(pairing.id && pairing.state && pairing.authorizeUrl.includes('state=') && isIso(pairing.expiresAt));
  const history = d.decodeQuotaHistory(fixture('account_quota'));
  assert.deepEqual(history.map((h) => h.windowId), ['5h', '7d', '7d_fable']);
  assert.ok(history.every((h) => h.samples.every((x) => isIso(x.at))));
  const snapshot = d.decodeSnapshot('business', fixture('account_usage'));
  assert.equal(snapshot.status, 'success');
  assert.deepEqual(
    d.decodeCatalogModel({ id: 'claude-opus-5', display_name: 'Opus', created_at: '2026-01-01', max_input_tokens: 1_000_000, max_tokens: 128_000 }),
    { id: 'claude-opus-5', displayName: 'Opus', createdAt: '2026-01-01', maxInputTokens: 1_000_000, maxTokens: 128_000 },
    'a catalog row decodes with its two numbers',
  );
  const catalog = d.decodeAccountCatalog(fixture('account_catalog'));
  assert.deepEqual(catalog.models[0], { id: 'claude-sonnet-4-5', displayName: '', createdAt: '', maxInputTokens: null, maxTokens: null }, 'and without them');
  assert.equal(catalog.error, null);
  assert.ok(isIso(catalog.fetchedAt));
});

test('the live snapshot and every delta decode with ISO times and null-safe fields', () => {
  const { snapshot, deltas } = fixture('live_messages');
  const snap = d.decodeLiveSnapshot(snapshot.snapshot);
  assert.ok(isIso(snap.tick.at) && Array.isArray(snap.tick.metrics.ttfbP50.series));
  const types = deltas.map((m) => d.decodeLiveDelta(m.delta)?.type);
  assert.deepEqual(types.filter((t) => t !== 'tick'), ['started', 'selected', 'head', 'progress', 'preview', 'finished']);
  const started = d.decodeLiveDelta(deltas[0].delta);
  assert.equal(started.request.attempts.length, 0);
  assert.ok(isIso(started.request.phases.receivedAt) && started.request.phases.headAt === undefined);
  const finished = d.decodeLiveDelta(deltas.find((m) => m.delta.type === 'finished').delta);
  assert.equal(finished.status, 200);
  assert.ok(isIso(finished.phases.finishedAt) && isIso(finished.phases.firstContentAt));
  assert.deepEqual(finished.attempts.map((a) => [a.index, a.outcome]), [[0, 'succeeded']], 'the request ends with the attempts the feed settled, so a row leaves in flight behind');
  const failed = d.decodeLiveDelta({ ...deltas.find((m) => m.delta.type === 'finished').delta, attempts: [{ account_id: 'business', cooldown_set: null, index: 1, outcome: 'transport_error', started_at: 1000005, upstream_status: null, windows: [] }] });
  assert.deepEqual(failed.attempts.map((a) => a.outcome), ['transport_error']);
  assert.equal(d.decodeLiveDelta({ type: 'nope', cursor: 1 }), null);
});

test('a live request carries the client that sent it', () => {
  const started = fixture('live_messages').deltas.find((m) => m.delta?.type === 'started').delta.request;
  assert.equal(d.decodeLiveRequest({ ...started, user_agent: 'claude-cli/2.1.269 (external, cli)' }).userAgent, 'claude-cli/2.1.269 (external, cli)');
  assert.equal(d.decodeLiveRequest({ ...started, user_agent: null }).userAgent, null);
});

test('the mock feed speaks the router\'s wire and the mock hands the handler what the decoders read from it', async () => {
  const { snapshot, deltas } = fixture('live_messages');
  // The fixture's frames fix the keys a frame carries. A phase is left out until it is observed and a window
  // until a head reports it, so those are held to the router's own names rather than to the fixture's one row.
  const PHASES = ['finished_at', 'first_content_at', 'head_at', 'received_at', 'selected_at', 'stream_end_at', 'written_at'];
  const want = { tick: { cursor: 'number', tick: shapeOf(snapshot.snapshot.tick), type: 'string' } };
  for (const m of deltas) want[m.delta.type] = shapeOf(m.delta);
  const request = want.started.request;
  // The selection's attempt has no answer yet, so its nullable members ask for their key alone.
  const attempt = want.selected.attempt;
  const findings = (shape, frame) => same(shape, frame, '', 0, (path) => path.endsWith('.phases'));

  const frames = [];
  const events = [];
  const stopWire = d.liveFeed().subscribe((frame) => frames.push(frame));
  const stop = new d.MockApi().subscribeLive((event) => events.push(event));
  await new Promise((resolve) => setTimeout(resolve, 2_500));
  stop();
  stopWire();

  const [first, ...rest] = frames;
  assert.equal(first.kind, 'snapshot');
  assert.deepEqual(findings(shapeOf(snapshot.snapshot), first.snapshot), []);
  const rows = [...first.snapshot.in_flight, ...first.snapshot.recent, ...rest.flatMap((f) => (f.delta.type === 'started' ? [f.delta.request] : []))];
  assert.ok(first.snapshot.in_flight.length > 0 && first.snapshot.recent.length > 0, 'the story has rows to carry');
  for (const row of rows) assert.deepEqual(findings(request, row), [], row.id);
  assert.ok(rest.some((f) => f.delta.type === 'tick'), 'a tick lands inside the wait');
  for (const f of rest) {
    assert.equal(f.kind, 'delta');
    // The router leaves `failed` out of a selection that replaced nothing, so the fixture's one row has none.
    const { failed, ...delta } = f.delta;
    assert.deepEqual(findings(want[delta.type], delta), [], delta.type);
    if (failed) assert.deepEqual(findings(attempt, failed), []);
  }
  for (const phases of [...rows.map((r) => r.phases), ...rest.flatMap((f) => (f.delta.phases ? [f.delta.phases] : []))]) {
    assert.deepEqual(Object.keys(phases).filter((k) => !PHASES.includes(k)), []);
    assert.ok(Object.values(phases).every(Number.isInteger), 'phase times are Unix milliseconds');
  }
  const attempts = [...rows.flatMap((r) => r.attempts), ...rest.flatMap((f) => [f.delta.attempt, f.delta.failed, ...(f.delta.attempts ?? [])].filter(Boolean))];
  assert.ok(attempts.length > 0 && attempts.every((a) => a.index >= 1), 'attempts count from one on the wire');
  for (const a of attempts) {
    assert.deepEqual(findings(attempt, a), []);
    assert.ok(a.cooldown_set === null || Number.isInteger(a.cooldown_set.until));
    for (const w of a.windows) assert.deepEqual([Object.keys(w).sort(), d.WINDOW_ID_VALUES.includes(w.id)], [['id', 'resets_at', 'utilization'], true], JSON.stringify(w));
  }
  assert.deepEqual(JSON.parse(JSON.stringify(frames)), frames, 'every frame is what a socket can carry');

  // The second subscriber's snapshot draws its own tick; its rows are the first one's, and every delta after it too.
  assert.deepEqual({ ...events[0].snapshot, tick: null }, { ...d.decodeLiveSnapshot(first.snapshot), tick: null });
  assert.deepEqual(events.slice(1).map((e) => e.delta), rest.map((f) => d.decodeLiveDelta(f.delta)));
  const finished = [...events[0].snapshot.inFlight, ...events[0].snapshot.recent].filter((r) => r.state === 'finished');
  assert.ok(finished.length > 0 && finished.every((r) => r.preview === null), 'a finished row carries no preview');
});

test('alert rules, events, and deliveries decode with the secret reduced to has_secret', () => {
  const rules = fixture('alert_rules').rules.map(d.decodeAlertRule);
  const util = rules.find((r) => r.kind === 'utilization_above');
  assert.equal(util.hasSecret, true);
  assert.equal(util.windowId, '7d_oi');
  assert.equal(util.threshold, 0.9);
  assert.deepEqual(util.scope, { id: 'main', type: 'pool' });
  assert.ok(isIso(util.createdAt) && !('secret' in util));
  assert.equal(rules.find((r) => r.kind === 'client_drift').enabled, false);
  const [event] = fixture('alert_events').events.map(d.decodeAlertEvent);
  assert.equal(event.state, 'open');
  assert.equal(event.detail.trigger, 'hash');
  assert.deepEqual(event.deliveries.map((x) => [x.attempt, x.status, x.error]), [[1, null, 'refused'], [2, 500, null], [3, 200, null]]);
  assert.ok(isIso(event.openedAt) && event.resolvedAt === null);
  assert.equal(fixture('alert_deliveries').deliveries.map(d.decodeDelivery).length, 3);
  const [life] = fixture('alert_lifecycle').deliveries.map(d.decodeLifecycleDelivery);
  assert.equal(life.event, 'session_started');
  assert.equal(life.detail.session, 'e2e-ui');
  assert.equal(life.kind, 'session_active');
  assert.equal(life.ruleId, fixture('alert_lifecycle').deliveries[0].rule_id, 'a lifecycle delivery names the rule that posted it, which no event row holds');
});

test('a question reads as redacted, expired, or missing from the wire detail and the page clock in milliseconds', () => {
  const [wire] = fixture('alert_events').events;
  const detail = { agent: null, key: 'desktop', question: null, question_expires_at: 1_000_000, session: 'sess-1' };
  const asked = d.decodeAlertEvent({ ...wire, detail, kind: 'ask_user_question' }).detail;
  assert.equal(asked.question_expires_at, 1_000_000, 'the deadline passes through as the number the route stores');
  assert.deepEqual(d.questionLine(asked, 1_000_000), { muted: true, text: 'question expired with its captured body' }, 'the route blanks the text at the deadline itself');
  assert.deepEqual(d.questionLine(asked, 999_999), { muted: true, text: 'question not captured' });
  assert.deepEqual(d.questionLine({ ...asked, question_expires_at: null }, 2_000_000), { muted: true, text: 'question not captured' });
  assert.deepEqual(d.questionLine({ ...asked, question_redacted: true }, 2_000_000), { muted: true, text: 'question is for administrators' }, 'a viewer is told who may read it, not that the capture failed');
  assert.deepEqual(d.questionLine({ ...asked, question: 'Which branch?' }, 2_000_000), { muted: false, text: 'asked: Which branch?' });
});

test('a lifecycle delivery whose event name this build does not know still decodes', () => {
  const [wire] = fixture('alert_lifecycle').deliveries;
  const later = d.decodeLifecycleDelivery({ ...wire, event: 'session_hibernated' });
  assert.equal(later.event, 'session_started', 'an unknown event name falls back to the first event of a session rather than throwing the log away');
  assert.equal(d.decodeLifecycleDelivery({ ...wire, status: null, error: 'refused' }).status, null);
});

test('a session lifecycle rule keeps its threshold in minutes and a lifecycle event keeps its detail', () => {
  const [wire] = fixture('alert_rules').rules;
  const idle = d.decodeAlertRule({ ...wire, kind: 'session_idle', threshold: 30, window: null });
  assert.equal(idle.kind, 'session_idle');
  assert.equal(idle.threshold, 30, 'an idle rule counts whole minutes, so the threshold arrives as the number the operator wrote');
  assert.equal(idle.windowId, null);
  const [wireEvent] = fixture('alert_events').events;
  const detail = { agent: 'sub-1', key: 'desktop', parent_agent: 'main', pool: 'main', session: 'e2e-ui' };
  const started = d.decodeAlertEvent({ ...wireEvent, detail, kind: 'subagent_active', subject: 'desktop, session e2e-ui, agent sub-1' });
  assert.equal(started.kind, 'subagent_active');
  assert.deepEqual(started.detail, detail, 'a detail is the free-form object the signal built and is passed through undecoded');
});
