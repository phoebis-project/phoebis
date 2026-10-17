import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;
/** A read under a named zone: a label built in the host's zone is held to one zone so the case holds on any host. */
const inZone = (zone, read) => {
  const was = process.env.TZ;
  process.env.TZ = zone;
  try {
    return read();
  } finally {
    if (was === undefined) delete process.env.TZ;
    else process.env.TZ = was;
  }
};
const isIso = (s) => typeof s === 'string' && !Number.isNaN(Date.parse(s)) && s.endsWith('Z');

test('an account decodes with its windows, plan, pool, and models', () => {
  const body = fixture('account');
  const a = d.decodeAccount(body);
  assert.equal(a.id, 'business');
  assert.equal(a.kind, 'oauth');
  assert.equal(a.plan, 'Max 20x', "the parsed tier names the plan with its multiple, so the badge, the search and the chips read one name");
  assert.equal(d.decodeAccount({ ...body, subscription: { ...body.subscription, plan: null } }).plan, 'Max 20x', "a document without the flags reads the tier's own label");
  assert.equal(a.subscription.rateLimitTier, 'default_claude_max_20x');
  assert.deepEqual(
    Object.keys(a.subscription).sort(),
    ['billingType', 'endsAt', 'organizationType', 'plan', 'rateLimitTier', 'since', 'trialEndsAt'],
    'the route answers the profile and subscription fields and nothing more, so the card draws nothing beside them',
  );
  assert.equal(a.subscription.plan, 'max');
  assert.ok(isIso(a.subscription.since));
  assert.equal(a.subscription.trialEndsAt, null);
  assert.deepEqual(fixture('account').windows.map((w) => w.id), ['five_hour', 'seven_day', 'seven_day_fable', 'seven_day_opus'], 'the route passes the ledger\'s one spelling through, in the ledger order');
  assert.deepEqual(a.windows.map((w) => w.id), ['5h', '7d', 'seven_day_opus'], 'the endpoint keys fold onto the UI ids, a week no row names keeps its key, and the order is the drawn one');
  assert.equal(a.fableWindow.id, '7d_fable');
  assert.ok(isIso(a.windows[0].resetsAt) && isIso(a.windows[0].observedAt));
  assert.equal(a.windows[0].utilization, 0.62, 'the five_hour sample is the 5h window');
  assert.deepEqual(a.pool, { poolId: 'main', priority: 0 });
  assert.equal(d.decodeAccount({ ...body, pool: null }).pool, null, 'an account in no pool answers a null pool');
  assert.equal(d.decodeAccount({ ...body, pool: undefined }).pool, null, 'a row without the member reads as in no pool');
  assert.equal(a.extraUsage.monthlyLimitCredits, 5000);
  assert.equal(a.requests.lifetime, fixture('account').requests.lifetime, 'the lifetime count reads as the route answers it');
  // The fixture seeds one live model-scoped cooldown so the key-models and models-state fixtures have a row.
  assert.equal(a.status, 'cooldown');
  assert.equal(a.email, 'test@example.invalid');
  assert.deepEqual(
    Object.keys(a).sort(),
    ['cancelled', 'cooldowns', 'credential', 'email', 'extraUsage', 'fableWindow', 'fastLane', 'id', 'kind', 'label', 'overage', 'plan', 'pool', 'requests', 'revision', 'routingEnabled', 'slowDown', 'status', 'subscription', 'usage', 'windows'],
    'an account decodes to the fields the console draws and nothing more',
  );
});

test("an account decodes its slow-down and each window's demand, and an api_key account reads null", () => {
  const body = fixture('account');
  const since = 1_791_717_520_000;
  const [five, ...rest] = body.windows;
  const wire = {
    ...body,
    slow_down: { factor: 0.5, hold_ms: 120_000, since, speed: 0.6, stretch_ms: 2_520_000, threshold_ms: 1_800_000 },
    windows: [{ ...five, burn: 0.085, demand: 0.17, horizon: five.resets_at, projection: 0.81 }, ...rest],
  };
  const slowed = d.decodeAccount(wire);
  assert.deepEqual(slowed.slowDown, { factor: 0.5, holdMs: 120_000, since: new Date(since).toISOString(), speed: 0.6, stretchMs: 2_520_000, thresholdMs: 1_800_000 });
  assert.deepEqual(slowed.windows.map((w) => [w.id, w.demand]), [['5h', 0.17], ['7d', null], ['seven_day_opus', null]], 'the demand where the route projects one');
  assert.equal(slowed.fableWindow.demand, null);
  assert.equal(d.decodeAccount({ ...wire, slow_down: { ...wire.slow_down, since: null } }).slowDown.since, null, 'an entry outside a span names no start');
  const captured = d.decodeAccount(body);
  assert.deepEqual([captured.slowDown, captured.windows.map((w) => w.demand)], [null, [null, null, null]], 'the captured account is read by no pinned selection');
  const keyed = fixture('accounts').accounts.find((a) => a.kind === 'api_key');
  assert.equal(d.decodeAccount(keyed).slowDown, null, 'an api_key account has no slow-down');
  const { slow_down: _s, ...bare } = body;
  const { demand: _d, ...unprojected } = five;
  const older = d.decodeAccount({ ...bare, windows: [unprojected] });
  assert.deepEqual([older.slowDown, older.windows[0].demand], [null, null], 'a row without the members reads null');
});

test('an account decodes its cancelled flag and its saved end', () => {
  const body = fixture('account');
  const cancelled = d.decodeAccount({ ...body, cancelled: true, subscription: { ...body.subscription, ends_at: 5_000 } });
  assert.deepEqual([cancelled.cancelled, cancelled.subscription.endsAt], [true, d.iso(5_000)]);
  assert.equal(d.decodeAccount(body).cancelled, false, 'a row the wire answers no flag for reads uncancelled');
});

test("an account's windows read the router's lead over pace, null while the utilization is unknown", () => {
  const body = fixture('account');
  const a = d.decodeAccount(body);
  const wire = (id) => body.windows.find((w) => w.id === id);
  assert.ok(typeof wire('five_hour').lead === 'number', 'the capture answers a lead on a sampled window');
  assert.equal(a.windows[0].lead, wire('five_hour').lead, 'the 5-hour window reads the lead the route answered');
  assert.equal(a.fableWindow.lead, wire('seven_day_fable').lead, 'and so does the Fable week');
  assert.ok(body.windows.every((w) => typeof w.lead === 'number'), 'every captured window carries one');
  const unread = d.decodeAccount({ ...body, windows: [{ ...wire('five_hour'), lead: null, utilization: null }] });
  assert.equal(unread.windows[0].lead, null, 'a window without a utilization reads no lead');
});

test("a group decodes the kept-warm tokens per TTL and the saving's three members, and a sum adds them", () => {
  const row = fixture('analytics_breakdown').rows[0];
  assert.deepEqual(Object.keys(row.kept_warm).sort(), ['requests', 'tokens_1h', 'tokens_5m'], 'the capture answers the two counts and no sum');
  // The capture reads zeros and empty maps, as a missing field decodes, so nonzero copies prove the wire names.
  const wire = { ...row, kept_warm: { requests: 2, tokens_1h: 700, tokens_5m: 300 }, saving_equivalent: { five_hour: 0.02, seven_day: 0.004 }, saving_shares: { five_hour: 0.01 }, saving_usd: 0.5 };
  const group = d.decodeAnalyticsGroup(wire);
  assert.deepEqual(group.keptWarm, { requests: 2, tokens1h: 700, tokens5m: 300 });
  assert.deepEqual([group.savingUsd, group.savingShares, group.savingEquivalent], [0.5, { five_hour: 0.01 }, { five_hour: 0.02, seven_day: 0.004 }]);
  assert.equal(d.decodeAnalyticsGroup({ ...wire, saving_shares: null }).savingShares, null, 'a group that holds no one account answers no share');
  assert.equal(d.keptWarmTokens(group.keptWarm), 1_000, 'the charts and the by-pool table print the two counts summed');
  const sum = d.sumGroups([group, d.decodeAnalyticsGroup({ ...wire, saving_equivalent: { five_hour: 0.25, seven_day: 0.125 }, saving_usd: 0.25 })]);
  assert.deepEqual(sum.keptWarm, { requests: 4, tokens1h: 1_400, tokens5m: 600 }, 'each count sums apart');
  assert.equal(sum.savingUsd, 0.75, 'the dollars sum');
  assert.equal(sum.savingShares, null, 'a sum of groups carries no share of one account');
  assert.ok(Math.abs(sum.savingEquivalent.five_hour - 0.27) < 1e-12 && Math.abs(sum.savingEquivalent.seven_day - 0.129) < 1e-12, 'the equivalent adds by window');
});

test('a sum of groups latches the saving equivalent on the groups that carry a credit, so a probed group with none drops no window', () => {
  const row = fixture('analytics_breakdown').rows[0];
  const plan = { plan_output_tokens: 100, plan_requests: 3 };
  const credited = d.decodeAnalyticsGroup({ ...row, ...plan, kept_warm: { requests: 2, tokens_1h: 700, tokens_5m: 300 }, probes: { ...row.probes, ok: 2 }, saving_equivalent: { five_hour: 0.25, seven_day: 0.125 } });
  const probed = d.decodeAnalyticsGroup({ ...row, ...plan, kept_warm: { requests: 0, tokens_1h: 0, tokens_5m: 0 }, probes: { ...row.probes, ok: 3 }, saving_equivalent: {} });
  assert.deepEqual(d.sumGroups([credited, probed]).savingEquivalent, { five_hour: 0.25, seven_day: 0.125 }, 'a probed group with no credit adds nothing and drops nothing');
  const partial = d.decodeAnalyticsGroup({ ...row, ...plan, kept_warm: { requests: 1, tokens_1h: 0, tokens_5m: 400 }, saving_equivalent: { five_hour: 0.5 } });
  assert.deepEqual(d.sumGroups([credited, partial]).savingEquivalent, { five_hour: 0.75 }, 'a credited group that priced no 7-day saving leaves the window out');
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

test('an account row decodes its usage block and null for an API-key account', () => {
  const rows = fixture('accounts').accounts;
  const [business, direct] = ['business', 'direct'].map((id) => d.decodeAccount(rows.find((r) => r.id === id)));
  const at = d.iso(rows.find((r) => r.id === 'business').usage.poll.last_attempt_at);
  assert.deepEqual(business.usage, {
    accountId: 'business',
    error: null,
    fetchedAt: d.iso(rows.find((r) => r.id === 'business').usage.fetched_at),
    heldUntil: null,
    poll: { backoffSince: null, lastAttemptAt: at, lastCleanAt: at, lastError: null, lastOutcome: 'clean', nextAttemptAt: null, rateLimitedInARow: 0 },
    status: 'success',
  });
  assert.equal(direct.usage, null, 'the poll never reads an API-key account');
  const spare = d.decodeAccount({ ...rows[0], id: 'spare', usage: { error: 'no credential is stored for this account, so pair it to load quota', fetched_at: null, poll: null } }).usage;
  assert.deepEqual([spare.accountId, spare.status, spare.fetchedAt, spare.poll], ['spare', 'error', null, null], 'the sentence standing in for a snapshot reads as an error');
});

test('an account cooling on its fast lane alone reads ready, and one with an account row beside it reads cooldown', () => {
  const fast = { scope: 'fast', until: 1, reason: 'fast_rate_limit', window: 'fast', set_at: 0 };
  const account = { scope: 'account', until: 1, reason: 'rate_limit', window: '5h', set_at: 0 };
  const alone = d.decodeAccount({ ...fixture('account'), cooldowns: [fast] });
  assert.equal(alone.status, 'ready', 'its standard lane still serves');
  assert.deepEqual(alone.cooldowns.map((c) => c.scope), ['fast'], 'the row itself is still decoded');
  assert.equal(d.decodeAccount({ ...fixture('account'), cooldowns: [fast, account] }).status, 'cooldown');
});

test("an account cooling on its fast lane's credits row, alone or beside its rate limit, reads ready", () => {
  const fast = { scope: 'fast', until: 1, reason: 'fast_rate_limit', window: 'fast', set_at: 0 };
  const credits = { scope: 'fast_credits', until: 2, reason: 'fast_credits', window: 'out_of_credits', set_at: 0 };
  assert.equal(d.decodeAccount({ ...fixture('account'), cooldowns: [credits] }).status, 'ready');
  const both = d.decodeAccount({ ...fixture('account'), cooldowns: [fast, credits] });
  assert.equal(both.status, 'ready', 'neither fast row cools its standard lane');
  assert.deepEqual(both.cooldowns.map((c) => c.scope), ['fast', 'fast_credits'], 'each row is decoded');
});

test("an account decodes its fast lane's two readings, a missing one as null, and no pair as null", () => {
  const body = fixture('account');
  const lane = d.decodeAccount(body).fastLane;
  assert.deepEqual(lane, {
    input: { observedAt: d.iso(body.fast_lane.input.observed_at), resetsAt: d.iso(body.fast_lane.input.resets_at), utilization: 0.42 },
    output: { observedAt: d.iso(body.fast_lane.output.observed_at), resetsAt: d.iso(body.fast_lane.output.resets_at), utilization: 0.08 },
  });
  const halved = d.decodeAccount({ ...body, fast_lane: { input: { ...body.fast_lane.input, resets_at: null }, output: null } }).fastLane;
  assert.deepEqual([halved.input.resetsAt, halved.output], [null, null], 'a reading with no reset, and a bucket no answer reported');
  assert.equal(d.decodeAccount({ ...body, fast_lane: null }).fastLane, null, 'no answer to a fast request yet');
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

test('the window key helper names the three known windows and keeps any other key as its own window', () => {
  const keys = ['five_hour', '5h', 'seven_day', '7d', 'seven_day_fable', 'seven_day_overage_included', '7d_oi', 'fable', 'seven_day_opus'];
  assert.deepEqual(keys.map(d.windowId), ['5h', '5h', '7d', '7d', '7d_fable', '7d_fable', '7d_fable', '7d_fable', 'seven_day_opus']);
});

test('an unknown weekly window decodes under its key after the known ones, in the account, its history and its cooldowns', () => {
  const base = fixture('account');
  const opus = { id: 'seven_day_opus', observed_at: 1_000_000, resets_at: 9_000_000, utilization: 0.03 };
  const shared = [{ id: 'five_hour', observed_at: 1_000_000, resets_at: 9, utilization: 0.1 }, { id: 'seven_day', observed_at: 1_000_000, resets_at: 9, utilization: 0.4 }];
  const a = d.decodeAccount({ ...base, windows: [opus, ...shared], cooldowns: [{ scope: 'account', until: 1, reason: 'rate_limit', window: 'seven_day_opus', set_at: 0 }] });
  assert.deepEqual(
    a.windows.map((w) => [w.id, w.label, w.periodHours]),
    [['5h', '5-hour', 5], ['7d', '7-day', 168], ['seven_day_opus', 'seven_day_opus', 168]],
    'kept under its key with a weekly period, drawn after the windows the pages know',
  );
  assert.equal(a.cooldowns[0].windowId, 'seven_day_opus');
  const history = d.decodeQuotaHistory({
    account_id: 'business',
    windows: [
      { id: 'seven_day_opus', samples: [{ at: 1_000_000, utilization: 0.03 }] },
      { id: 'five_hour', samples: [{ at: 1_000_000, utilization: 0.3 }] },
    ],
  });
  assert.deepEqual(history.map((h) => h.windowId), ['5h', 'seven_day_opus']);
  const captured = d.decodeAccount(base);
  assert.deepEqual(captured.windows.map((w) => w.id), ['5h', '7d', 'seven_day_opus'], 'the captured account carries the opus week after the two shared ones; the Fable week sits apart');
  const captive = captured.windows.find((w) => w.id === 'seven_day_opus');
  assert.deepEqual([captive.label, captive.periodHours], ['seven_day_opus', 168]);
});

test('plan labels read the tier', () => {
  assert.equal(d.planLabel('default_claude_max_20x'), 'Max 20x');
  assert.equal(d.planLabel('claude_pro'), 'Pro');
  assert.equal(d.planLabel('default_claude_ai'), 'Pro', 'the tier the profile reports for Pro');
  assert.equal(d.planLabel('default_claude_max_5x'), 'Max 5x');
  assert.equal(d.planLabel('something_else'), 'something_else');
});

test('plan names read the parsed tier, then the flag, and nothing without either', () => {
  const sub = { billingType: '', organizationType: '', plan: null, rateLimitTier: '', since: null, trialEndsAt: null };
  assert.equal(d.planName({ ...sub, plan: 'pro', rateLimitTier: 'default_claude_max_5x' }), 'Max 5x', 'the parsed tier wins over the flag');
  assert.equal(d.planName({ ...sub, plan: 'max', rateLimitTier: 'some_new_tier' }), 'Max', 'a tier the pattern does not parse falls to the flag');
  assert.equal(d.planName({ ...sub, plan: 'pro', rateLimitTier: 'some_new_tier' }), 'Pro', 'and to the Pro flag the same way');
  assert.equal(d.planName({ ...sub, rateLimitTier: 'some_new_tier' }), null, 'a tier the pattern does not parse names no plan without the flag');
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
  assert.equal(k.lifetime.requests, fixture('keys').keys[0].lifetime.requests);
  assert.equal(k.usage7d.tokens > 0, true);
  const wire = fixture('keys').keys[0];
  const read = (u) => ({ billedUsd: u.billed_usd, equivalent: { fiveHour: u.equivalent.five_hour, sevenDay: u.equivalent.seven_day }, estimated: u.estimated });
  for (const [name, decoded, sent] of [['usage7d', k.usage7d, wire.usage_7d], ['lifetime', k.lifetime, wire.lifetime]]) {
    assert.deepEqual({ billedUsd: decoded.billedUsd, equivalent: decoded.equivalent, estimated: decoded.estimated }, read(sent), `the ${name} reads its billed dollars, its 20x equivalents and its estimated flag`);
  }
  assert.ok(k.usage7d.equivalent.fiveHour > 0 && k.usage7d.billedUsd > 0, 'the captured key spent on both sides');
  assert.equal(d.decodeKey({ ...wire, usage_7d: { ...wire.usage_7d, estimated: true } }).usage7d.estimated, true, 'a fallback-priced week reads estimated');
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
  const input = { bodyCaptureHours: null, enabled: false, keepaliveOptOut: false, longContextTwins: true, modelRules: [], modelRulesMode: 'extend', name: 'batch', notes: null, poolId: 'jobs' };
  const kept = d.encodeKey({ ...input, expiresAt: undefined });
  assert.equal(kept.enabled, false);
  assert.equal('expires_at' in kept, false, 'an absent expiry leaves the stored one alone');
  assert.equal(d.encodeKey({ ...input, expiresAt: null }).expires_at, null, 'null clears the expiry');
  assert.equal(d.encodeKey({ ...input, enabled: true, expiresAt: '2026-10-01T00:00:00.000Z' }).expires_at, Date.parse('2026-10-01T00:00:00.000Z'), 'the route takes milliseconds');
  assert.equal(d.encodeKey({ ...input, enabled: true, expiresAt: undefined }).enabled, true);
});


test("a key decodes its server-side auto mode override as on, off, or inheriting, and a write sends it as set", () => {
  const row = fixture('keys').keys[0];
  assert.equal(d.decodeKey(row).autoModeServer, null, 'the captured key inherits its pool');
  assert.equal(d.decodeKey({ ...row, auto_mode_server: true }).autoModeServer, true);
  assert.equal(d.decodeKey({ ...row, auto_mode_server: false }).autoModeServer, false);
  const bare = { ...row };
  delete bare.auto_mode_server;
  assert.equal(d.decodeKey(bare).autoModeServer, null, 'a row without the field inherits');
  const input = { autoModeServer: null, bodyCaptureHours: null, enabled: true, keepaliveOptOut: false, longContextTwins: true, modelRules: [], modelRulesMode: 'extend', name: 'desktop', notes: null, poolId: 'main' };
  assert.equal(d.encodeKey(input).auto_mode_server, null, 'inherit is sent as null so a patch clears an override');
  assert.equal(d.encodeKey({ ...input, autoModeServer: true }).auto_mode_server, true);
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

test('the model state decodes the cooldowns grouped by model and leaves the rejections and permits to the routing log', () => {
  const body = fixture('models_state');
  const state = d.decodeModelsState(body);
  assert.deepEqual(state.cooldowns.map((g) => [g.model, g.rows.length]), [['claude-haiku-4-5-20251001', 1]]);
  assert.equal(state.cooldowns[0].rows[0].accountId, 'business');
  assert.ok(body.rejections.length > 0 && body.permits.length > 0, 'the route still answers both lists');
  assert.deepEqual(Object.keys(state), ['cooldowns'], 'the console reads neither list');
});

test('a pool decodes its policy, status, members, and traffic', () => {
  const [p] = fixture('pools').pools.map(d.decodePool);
  assert.equal(p.name, 'Main');
  assert.equal(p.strategy, 'pace');
  assert.deepEqual(p.keepalive, { enabled: true, maxIdleSeconds: 3_600, maxParkedSeconds: null, scheduledHorizonSeconds: null }, 'the four fields, the two ceilings null as the route answers them');
  assert.equal(p.cacheTtl.enabled, false);
  assert.equal(p.cacheTtl.plan.main, '1h');
  assert.equal(p.cacheDiagnostics, false);
  assert.equal(p.status, 'active');
  assert.equal(p.traffic24h.length, 25);
  assert.ok(isIso(p.traffic24h[0].at));
  const wire = fixture('pools').pools[0];
  assert.deepEqual([p.billed24h, p.equivalent24h, p.estimated24h], [wire.billed_24h, { fiveHour: wire.equivalent_24h.five_hour, sevenDay: wire.equivalent_24h.seven_day }, wire.estimated_24h], "the day's billed dollars, 20x equivalents and estimated flag");
  assert.ok(p.equivalent24h.fiveHour > 0, 'the captured pool served plan traffic');
  const busy = wire.traffic_24h.findIndex((b) => b.requests > 0);
  const bucket = wire.traffic_24h[busy];
  assert.deepEqual(
    [p.traffic24h[busy].billedUsd, p.traffic24h[busy].equivalent, p.traffic24h[busy].estimated],
    [bucket.billed_usd, { fiveHour: bucket.equivalent.five_hour, sevenDay: bucket.equivalent.seven_day }, bucket.estimated],
    "a bucket's billed dollars, 20x equivalents and estimated flag",
  );
  const marked = d.decodePool({ ...wire, estimated_24h: true, traffic_24h: [{ ...bucket, estimated: true }] });
  assert.deepEqual([marked.estimated24h, marked.traffic24h[0].estimated], [true, true], 'a fallback-priced day and bucket read estimated');
  assert.deepEqual(p.members.map((m) => m.accountId), ['business', 'direct']);
  assert.deepEqual(p.members.map((m) => m.eligible), ['eligible', 'eligible'], 'both members are servable in the captured pool');
  assert.equal(d.decodePool({ ...fixture('pools').pools[0], members: [{ account_id: 'business', priority: 0, eligible: 'overage' }] }).members[0].eligible, 'overage');
  assert.equal(d.decodePool({ ...fixture('pools').pools[0], members: [{ account_id: 'business', priority: 0 }] }).members[0].eligible, 'unavailable', 'a member row without the field reads as not servable');
});

test("a pool member reads the pins on its account under the pool's keys, and none where the route answers none", () => {
  const pool = fixture('pools').pools[0];
  const members = [{ account_id: 'business', active_pins: 3, priority: 0 }, { account_id: 'direct', priority: 1 }];
  assert.deepEqual(d.decodePool({ ...pool, members }).members.map((m) => [m.accountId, m.activePins]), [['business', 3], ['direct', 0]]);
});

test('a pool member reads its speed, and none where the route answers none', () => {
  const pool = fixture('pools').pools[0];
  assert.deepEqual(d.decodePool(pool).members.map((m) => m.speed), pool.members.map((m) => m.speed), 'as the route answers it');
  const members = [{ account_id: 'business', priority: 0, speed: 0.5 }, { account_id: 'direct', priority: 1, speed: null }, { account_id: 'other', priority: 2 }];
  assert.deepEqual(d.decodePool({ ...pool, members }).members.map((m) => [m.accountId, m.speed]), [['business', 0.5], ['direct', null], ['other', null]], 'a member row without the figure reads none');
});

test('a pool decodes the ceilings the router resolved for it, and a pool with no member reads null', () => {
  const row = fixture('pools').pools[0];
  const resolved = d.decodePool(row).keepaliveResolved;
  const wire = row.keepalive_resolved;
  assert.deepEqual(
    resolved,
    {
      breakEvenSeconds: { max: wire.break_even_seconds.max, min: wire.break_even_seconds.min },
      horizonSeconds: { max: wire.horizon_seconds.max, min: wire.horizon_seconds.min },
      idleSeconds: { max: wire.idle_seconds.max, min: wire.idle_seconds.min },
      idleShare: { max: wire.idle_share.max, min: wire.idle_share.min },
      parkedSeconds: { max: wire.parked_seconds.max, min: wire.parked_seconds.min },
    },
    'the five ranges as the route answers them',
  );
  assert.ok(resolved.breakEvenSeconds.min < resolved.breakEvenSeconds.max, 'the captured pool mixes a plan member and an API-key member, so its break-even is a range');
  assert.equal(d.decodePool({ ...row, keepalive_resolved: null }).keepaliveResolved, null, 'a pool with no member');
  const bare = { ...row };
  delete bare.keepalive_resolved;
  assert.equal(d.decodePool(bare).keepaliveResolved, null, 'a row without the member resolves nothing');
});

const SHEDDING_FIELDS = ['load_shedding', 'window_ceiling', 'shed_cap', 'burn_average_seconds', 'shed_dwell_seconds'];
const shedding = (p) => [p.loadShedding, p.windowCeiling, p.shedCap, p.burnAverageSeconds, p.shedDwellSeconds];

test('a pool decodes its switch, its ceiling, its cap, its average and its dwell and encodes them back', () => {
  const row = fixture('pools').pools[0];
  assert.deepEqual(shedding(d.decodePool(row)), SHEDDING_FIELDS.map((f) => row[f]), 'the captured pool reads as the route answered it');
  const wire = { ...row, burn_average_seconds: 3_600, load_shedding: false, shed_cap: 0.3, shed_dwell_seconds: 1_800, window_ceiling: 0.8 };
  const p = d.decodePool(wire);
  assert.deepEqual(shedding(p), [false, 0.8, 0.3, 3_600, 1_800], 'a pool with its switch off reads it off');
  const back = d.encodePool({ ...p, name: p.name, status: 'active' });
  assert.deepEqual(Object.fromEntries(SHEDDING_FIELDS.map((f) => [f, back[f]])), Object.fromEntries(SHEDDING_FIELDS.map((f) => [f, wire[f]])), 'each goes back under the name the route reads');
  assert.equal(d.encodePool({ ...p, loadShedding: true, name: p.name, status: 'active' }).load_shedding, true, 'the switch is sent on as it is sent off');
  const bare = { ...row };
  for (const f of SHEDDING_FIELDS) delete bare[f];
  const balanced = d.SHEDDING_PRESETS.balanced;
  assert.deepEqual(shedding(d.decodePool(bare)), [true, 0.95, balanced.shedCap, balanced.burnAverageSeconds, balanced.shedDwellSeconds], 'a row without the fields reads the switch on, the ceiling at 95 % and the balanced preset');
});

const SLOW_FIELDS = ['return_to_plan', 'return_to_plan_dwell_seconds', 'slow_down', 'slow_down_hold_seconds', 'slow_down_stretch_seconds'];
const slow = (p) => [p.returnToPlan, p.returnToPlanDwellSeconds, p.slowDown, p.slowDownHoldSeconds, p.slowDownStretchSeconds];

test('a pool decodes its slow-down and return-to-plan fields and encodes them back', () => {
  const row = fixture('pools').pools[0];
  assert.deepEqual(slow(d.decodePool(row)), SLOW_FIELDS.map((f) => row[f]), 'the captured pool reads as the route answered it');
  const wire = { ...row, return_to_plan: 'when_cold', return_to_plan_dwell_seconds: 1_200, slow_down: true, slow_down_hold_seconds: 90, slow_down_stretch_seconds: 2_400 };
  const p = d.decodePool(wire);
  assert.deepEqual(slow(p), ['when_cold', 1_200, true, 90, 2_400]);
  const back = d.encodePool({ ...p, name: p.name, status: 'active' });
  assert.deepEqual(Object.fromEntries(SLOW_FIELDS.map((f) => [f, back[f]])), Object.fromEntries(SLOW_FIELDS.map((f) => [f, wire[f]])), 'each goes back under the name the route reads');
  assert.equal(d.encodePool({ ...p, name: p.name, slowDown: false, status: 'active' }).slow_down, false, 'the switch is sent off as it is sent on');
  assert.equal(d.decodePool({ ...wire, return_to_plan: 'sometimes' }).returnToPlan, 'always', 'a mode this build does not know reads as always');
  assert.deepEqual([...d.RETURN_TO_PLAN_VALUES], ['always', 'never', 'when_cold']);
  const bare = { ...row };
  for (const f of SLOW_FIELDS) delete bare[f];
  assert.deepEqual(slow(d.decodePool(bare)), ['always', 900, false, 120, 9_000], 'a row without the fields reads the store defaults');
});

test("a pool body carries none of the six names the route refuses as unknown fields", () => {
  const p = d.decodePool(fixture('pools').pools[0]);
  const back = d.encodePool({ ...p, name: p.name, status: 'active' });
  const old = ['burn_window_seconds', 'pace_floor', 'select_below_utilization', 'shed_ahead_of_pace', 'shed_share', 'source_dwell_seconds'];
  assert.deepEqual(old.filter((f) => f in back), []);
});

test("a pool decodes its warnings and a member's pace score and burn", () => {
  const row = fixture('pools').pools[0];
  const p = d.decodePool(row);
  assert.deepEqual(p.warnings, row.warnings.map((w) => ({ code: w.code, fields: w.fields, text: w.text })));
  assert.deepEqual(p.warnings.map((w) => [w.code, w.fields]), [['cap_under_quarter', ['shed_cap']]], 'the captured pool sets its cap under a quarter');
  const bare = { ...row };
  delete bare.warnings;
  assert.deepEqual(d.decodePool(bare).warnings, [], 'a row without the member reads none');
  assert.deepEqual(d.decodePool({ ...row, warnings: [{ code: 'warm_never', fields: ['warm_sessions'] }, 'stray'] }).warnings, [{ code: 'warm_never', fields: ['warm_sessions'], text: '' }], 'an entry that is no object is left out');
  assert.deepEqual(p.members.map((m) => [m.accountId, m.paceScore, m.burn]), row.members.map((m) => [m.account_id, m.pace_score, m.burn]));
  const keyed = p.members.find((m) => m.kind === 'api_key');
  assert.deepEqual([typeof keyed.paceScore, keyed.burn], ['number', null], "an API-key member's score is its remaining allowance, with no burn");
  const spending = d.decodePool({ ...row, members: [{ ...row.members[0], burn: 0.085, pace_score: -0.02 }] }).members[0];
  assert.deepEqual([spending.paceScore, spending.burn], [-0.02, 0.085], 'a spare rate under the burn reads negative');
  const unread = d.decodePool({ ...row, members: [{ account_id: 'business', priority: 0 }] }).members[0];
  assert.deepEqual([unread.paceScore, unread.burn], [null, null], 'a member row without the figures reads none');
});

test('a window decodes its projection, horizon and burn and nulls without them', () => {
  const body = fixture('account');
  const [five] = body.windows;
  assert.equal(five.id, 'five_hour');
  const wire = { ...body, windows: [{ ...five, burn: 0.085, horizon: five.resets_at, projection: 0.81 }, ...body.windows.slice(1)] };
  const a = d.decodeAccount(wire);
  const read = (w) => [w.projection, w.horizon, w.burn];
  assert.deepEqual(read(a.windows.find((w) => w.id === '5h')), [0.81, new Date(five.resets_at).toISOString(), 0.085]);
  assert.deepEqual(read(a.windows.find((w) => w.id === '7d')), [null, null, null], 'the capture reads no burn, so nothing is projected');
  assert.deepEqual(read(a.fableWindow), [null, null, null]);
  const { burn: _b, horizon: _h, projection: _p, ...unprojected } = five;
  assert.deepEqual(read(d.decodeAccount({ ...body, windows: [unprojected] }).windows[0]), [null, null, null], 'a window without the members reads null');
});

test('a session decodes its speed and its cycle, and a row without them reads null', () => {
  const row = fixture('sessions').sessions[0];
  const read = (wire) => {
    const s = d.decodeSession(wire);
    return [s.speed, s.cycleMs];
  };
  assert.deepEqual(read(row), [row.speed, row.cycle_ms], 'as the route answers it');
  assert.deepEqual(read({ ...row, cycle_ms: 20_400, speed: 0.5 }), [0.5, 20_400], 'a slowed pin with a cycle');
  const { cycle_ms: _c, speed: _s, ...bare } = row;
  assert.deepEqual(read(bare), [null, null], 'a row without the members reads null');
});

test('a session decodes its runway and whether it is bounded', () => {
  const row = fixture('sessions').sessions[0];
  const read = (wire) => {
    const s = d.decodeSession(wire);
    return [s.runwayMs, s.runwayBounded];
  };
  assert.deepEqual(read(row), [row.runway_ms, row.runway_bounded], 'as the route answers it');
  assert.deepEqual(read({ ...row, runway_bounded: true, runway_ms: 7_800_000 }), [7_800_000, true], 'a bounded runway');
  assert.deepEqual(read({ ...row, runway_bounded: false, runway_ms: null }), [null, false], 'an unbounded runway');
  assert.deepEqual(read({ ...row, runway_bounded: null, runway_ms: null }), [null, null], 'no projection to run to');
  const { runway_bounded: _rb, runway_ms: _rm, ...bare } = row;
  assert.deepEqual(read(bare), [null, null], 'a row without the members reads no projection');
});

test("both record versions decode their settings, a version 1 record's three removed ones under legacy", () => {
  const body = fixture('decision');
  const v2 = d.decodeDecisionRecord(body);
  const s = body.settings;
  assert.equal(v2.version, 2);
  assert.equal(Object.keys(s).length, 25, 'the route answers twenty-five settings');
  assert.deepEqual(Object.keys(v2.settings).filter((k) => k !== 'legacy').length, 25);
  assert.deepEqual(shedding(v2.settings), SHEDDING_FIELDS.map((f) => s[f]), 'a version 2 record reads the new names');
  assert.equal(v2.settings.legacy, null);
  assert.equal(d.decodeDecisionRecord({ ...body, settings: { ...s, load_shedding: false } }).settings.loadShedding, false, 'the switch reads off');
  assert.equal(d.decodeDecisionRecord({ ...body, settings: { ...s, shed_share: 0.9 } }).settings.shedCap, s.shed_cap, 'a version 2 record reads no old name');
  const { burn_average_seconds: _a, load_shedding: _l, shed_cap: _c, shed_dwell_seconds: _d, window_ceiling: _w, ...shared } = s;
  const old = { ...shared, burn_window_seconds: 600, migrate_band: 0.05, pace_floor: 0.05, select_below_utilization: 0.8, shed_ahead_of_pace: 0.2, shed_share: 0.5, source_dwell_seconds: 900 };
  const v1 = d.decodeDecisionRecord({ ...body, settings: old, version: 1 });
  assert.equal(v1.version, 1);
  assert.deepEqual(shedding(v1.settings), [null, null, 0.5, 600, 900], 'the three renamed settings read the old names, the switch and the ceiling none');
  assert.deepEqual(v1.settings.legacy, { paceFloor: 0.05, selectBelowUtilization: 0.8, shedAheadOfPace: 0.2 });
  assert.equal(v1.settings.strategy, s.strategy, 'every setting both versions name reads as today');
  const off = d.decodeDecisionRecord({ ...body, settings: { ...old, select_below_utilization: null, shed_ahead_of_pace: null }, version: 1 });
  assert.deepEqual(off.settings.legacy, { paceFloor: 0.05, selectBelowUtilization: null, shedAheadOfPace: null }, 'a setting the pool left off reads null');
});

test("a record decodes the five settings, the hold's figures, the cause, the factor, the demand and the pin's cycle, and a record without them reads null", () => {
  const body = fixture('decision');
  const until = 1_791_724_600_000;
  const [chosen, ...others] = body.candidates;
  const slowed = d.decodeDecisionRecord({
    ...body,
    candidates: [{ ...chosen, factor: 0.5, windows: { ...chosen.windows, five_hour: { ...Object.values(chosen.windows)[0], demand: 0.17 } } }, ...others],
    outcome: {
      ...body.outcome,
      cause: { reason: 'rate_limit', scope: 'account', window: 'five_hour' },
      hold: { account_id: 'business', bound_ms: 120_000, cohort: null, cycle_ms: 20_000, factor: 0.5, reason: 'slow_down', speed: 0.6, stretch_ms: 2_520_000, threshold_ms: 1_800_000, until },
      kind: 'held',
    },
    pin: { ...body.pin, cycle_ms: 20_000, slow_down_until: until },
    settings: { ...body.settings, return_to_plan: 'when_cold', return_to_plan_dwell_seconds: 1_200, slow_down: true, slow_down_hold_seconds: 90, slow_down_stretch_seconds: 2_400 },
  });
  const settings = (s) => [s.returnToPlan, s.returnToPlanDwellSeconds, s.slowDown, s.slowDownHoldSeconds, s.slowDownStretchSeconds];
  assert.deepEqual(settings(slowed.settings), ['when_cold', 1_200, true, 90, 2_400]);
  assert.deepEqual(slowed.outcome.hold, { accountId: 'business', boundMs: 120_000, cohort: null, cycleMs: 20_000, factor: 0.5, reason: 'slow_down', speed: 0.6, stretchMs: 2_520_000, thresholdMs: 1_800_000, until: new Date(until).toISOString() });
  assert.deepEqual(slowed.outcome.cause, { cause: null, reason: 'rate_limit', scope: 'account', state: null, window: 'five_hour' }, "a cooldown's cause names no state");
  assert.deepEqual([slowed.candidates[0].factor, slowed.candidates[0].windows.five_hour.demand], [0.5, 0.17]);
  assert.deepEqual([slowed.pin.cycleMs, slowed.pin.slowDownUntil], [20_000, new Date(until).toISOString()]);
  const marked = d.decodeDecisionRecord({ ...body, outcome: { ...body.outcome, cause: { cause: 'refresh_failed', state: 'reauth_required' } } }).outcome.cause;
  assert.deepEqual(marked, { cause: 'refresh_failed', reason: null, scope: null, state: 'reauth_required', window: null }, "a mark's cause names no scope");

  // A record written before these members carries none of their keys.
  const older = structuredClone(body);
  for (const key of ['return_to_plan', 'return_to_plan_dwell_seconds', 'slow_down', 'slow_down_hold_seconds', 'slow_down_stretch_seconds']) delete older.settings[key];
  for (const c of older.candidates) {
    delete c.factor;
    for (const w of Object.values(c.windows)) delete w.demand;
  }
  delete older.outcome.cause;
  delete older.pin.cycle_ms;
  delete older.pin.slow_down_until;
  older.outcome.hold = { account_id: 'business', bound_ms: 60_000, cohort: null, reason: 'cap', until: null };
  const before = d.decodeDecisionRecord(older);
  assert.deepEqual(settings(before.settings), [null, null, null, null, null], 'the five settings read as not recorded');
  assert.deepEqual(before.outcome.hold, { accountId: 'business', boundMs: 60_000, cohort: null, cycleMs: null, factor: null, reason: 'cap', speed: null, stretchMs: null, thresholdMs: null, until: null });
  assert.equal(before.outcome.cause, null);
  assert.ok(before.candidates.every((c) => c.factor === null && Object.values(c.windows).every((w) => w.demand === null)), 'no factor and no demand');
  assert.deepEqual([before.pin.cycleMs, before.pin.slowDownUntil], [null, null]);
  assert.equal(before.version, 2, 'the record stays version 2');
});

test('a request row reads its waits, and a row without them reads an empty list', () => {
  const wire = fixture('requests').requests.find((r) => r.held !== null);
  assert.deepEqual(d.decodeRequest(wire).held.waits, wire.held.waits.map((w) => ({ heldMs: w.held_ms, reason: w.reason })), 'as the route answers it');
  const both = { cohort: null, held_ms: 45_000, waited_on: ['business'], waits: [{ held_ms: 42_000, reason: 'slow_down' }, { held_ms: 3_000, reason: 'cap' }] };
  assert.deepEqual(d.decodeRequest({ ...wire, held: both }).held.waits, [{ heldMs: 42_000, reason: 'slow_down' }, { heldMs: 3_000, reason: 'cap' }], 'oldest first, as the router writes them');
  const { waits: _w, ...before } = wire.held;
  assert.deepEqual(d.decodeRequest({ ...wire, held: before }).held.waits, [], 'a row from before the member reads none');
});

test('the rewrites answer decodes its held points, and an answer without them reads none', () => {
  const body = fixture('analytics_rewrites');
  const r = d.decodeRewrites(body);
  assert.deepEqual(r.held, body.held.map((p) => ({ at: new Date(p.at).toISOString(), heldMs: p.held_ms, requests: p.requests })), 'as the route answers it');
  assert.deepEqual(r.held.map((p) => p.at), r.points.map((p) => p.at), 'one point per bucket, as the rewrite points');
  const at = 1_791_633_600_000;
  assert.deepEqual(d.decodeRewrites({ ...body, held: [{ at, held_ms: 84_000, requests: 2 }] }).held, [{ at: new Date(at).toISOString(), heldMs: 84_000, requests: 2 }]);
  const { held: _h, ...before } = body;
  assert.deepEqual(d.decodeRewrites(before).held, [], 'an answer from before the member reads none');
});

test('a candidate decodes its fit figures and an outcome its reservation and its hold\'s reason', () => {
  const body = fixture('decision');
  const [wire] = body.candidates;
  const [chosen] = d.decodeDecisionRecord(body).candidates;
  assert.deepEqual([chosen.binding, chosen.burn, chosen.fit, chosen.runwayMs, chosen.pace.burn], [wire.binding, wire.burn, wire.fit, wire.runway_ms, wire.pace.burn], 'as the route answers it');
  assert.deepEqual(chosen.windows.fast_input, { ...chosen.windows.fast_input, horizon: null, projection: null, reserved: 0 });
  const reset = 1_791_724_600_000;
  const projected = {
    ...wire,
    binding: 'five_hour',
    burn: { five_hour: 0.073, seven_day: 0.0085 },
    fit: 'rewrite_only',
    pace: { ...wire.pace, burn: 0.0085 },
    runway_ms: 7_800_000,
    windows: { five_hour: { age_ms: 4_000, ahead: 0.1, horizon: reset, observed_at: reset - 3_600_000, projection: 0.993, reserved: 0.104, reset, source: 'head', utilization: 0.96 } },
  };
  const [c] = d.decodeDecisionRecord({ ...body, candidates: [projected] }).candidates;
  assert.deepEqual([c.binding, c.burn, c.fit, c.runwayMs, c.pace.burn], ['five_hour', { five_hour: 0.073, seven_day: 0.0085 }, 'rewrite_only', 7_800_000, 0.0085]);
  assert.deepEqual([c.windows.five_hour.projection, c.windows.five_hour.horizon, c.windows.five_hour.reserved], [0.993, new Date(reset).toISOString(), 0.104]);
  const { binding: _b, burn: _u, fit: _f, runway_ms: _r, ...older } = wire;
  const unbooked = Object.fromEntries(Object.entries(wire.windows).map(([name, { reserved: _x, ...w }]) => [name, w]));
  const [v1] = d.decodeDecisionRecord({ ...body, candidates: [{ ...older, pace: { ...wire.pace, burn: undefined }, windows: unbooked }], version: 1 }).candidates;
  assert.deepEqual([v1.binding, v1.burn, v1.fit, v1.runwayMs, v1.pace.burn], [null, {}, null, null, null], 'a version 1 candidate shows no fit, no runway and no burn');
  assert.ok(Object.keys(v1.windows).length > 0 && Object.values(v1.windows).every((w) => w.reserved === null), 'a version 1 window reads no reservation');
  // The pick writes a burn fact after each score fact, which the walk decodes as it decodes every fact.
  const facts = [
    { account_id: null, key: 'strategy', unit: null, value: 'pace' },
    { account_id: 'business', key: 'score', unit: 'percent_per_hour', value: -0.0021 },
    { account_id: 'business', key: 'burn', unit: 'percent_per_hour', value: 0.0085 },
  ];
  const picked = d.decodeDecisionRecord({ ...body, walk: [{ facts, step: 'pick', verdict: 'chosen' }] }).walk[0];
  assert.deepEqual(picked.facts.map((f) => [f.key, f.accountId, f.unit, f.value]), facts.map((f) => [f.key, f.account_id, f.unit, f.value]));

  const outcome = d.decodeDecisionRecord(body).outcome;
  assert.deepEqual([outcome.hold, outcome.markCleared, outcome.reservation], [null, null, null], "the fixture's placement held nothing, cleared no mark and booked nothing");
  const held = d.decodeDecisionRecord({ ...body, outcome: { ...body.outcome, hold: { account_id: 'business', bound_ms: 60_000, cohort: null, reason: 'reset', until: reset }, kind: 'held' } }).outcome;
  assert.deepEqual(held.hold, { accountId: 'business', boundMs: 60_000, cohort: null, cycleMs: null, factor: null, reason: 'reset', speed: null, stretchMs: null, thresholdMs: null, until: new Date(reset).toISOString() });
  const booked = d.decodeDecisionRecord({
    ...body,
    outcome: {
      ...body.outcome,
      mark_cleared: { marked_at: reset - 600_000, marked_on: 'business', marked_window: 'five_hour', projection: 0.91, reason: 'healed' },
      reservation: { account_id: 'business', burn: { five_hour: 0.073 }, expires_at: reset, rewrite: { five_hour: 0.104, seven_day: 0.0121 } },
    },
  }).outcome;
  assert.deepEqual(booked.reservation, { accountId: 'business', burn: { five_hour: 0.073 }, expiresAt: new Date(reset).toISOString(), rewrite: { five_hour: 0.104, seven_day: 0.0121 } });
  assert.deepEqual(booked.markCleared, { markedAt: new Date(reset - 600_000).toISOString(), markedOn: 'business', markedWindow: 'five_hour', projection: 0.91, reason: 'healed' });
  const capped = d.decodeDecisionRecord({ ...body, outcome: { ...body.outcome, hold: { account_id: 'business', bound_ms: 60_000, cohort: null }, kind: 'held' } }).outcome;
  assert.deepEqual([capped.hold.reason, capped.hold.until], ['cap', null], "a hold written before its reason waits for a slot");
});

test("a record's pin decodes its shed mark and its request the body's estimated tokens", () => {
  const body = fixture('decision');
  const r = d.decodeDecisionRecord(body);
  assert.deepEqual([r.pin.markedAt, r.pin.markedOn, r.pin.markedWindow], [body.pin.marked_at, body.pin.marked_on, body.pin.marked_window]);
  assert.equal(r.request.bodyTokens, body.request.body_tokens);
  const at = 1_791_717_520_000;
  const marked = d.decodeDecisionRecord({ ...body, pin: { ...body.pin, marked: true, marked_at: at, marked_on: 'business', marked_window: 'seven_day' }, request: { ...body.request, body_tokens: 870_174 } });
  assert.deepEqual([marked.pin.markedAt, marked.pin.markedOn, marked.pin.markedWindow, marked.request.bodyTokens], [new Date(at).toISOString(), 'business', 'seven_day', 870_174]);
  const { body_tokens: _t, ...refused } = body.request;
  assert.equal(d.decodeDecisionRecord({ ...body, request: refused }).request.bodyTokens, null, 'a refusal carries no estimate');
});

test('the rewrites answer decodes its moves by reason and fit', () => {
  const body = fixture('analytics_rewrites');
  assert.deepEqual(d.decodeRewrites(body).movesByReasonAndFit, body.moves_by_reason_and_fit);
  const two = d.decodeRewrites({ moves: { shed: 3, spent: 2 }, moves_by_reason_and_fit: { shed: { fits: 2, least_bad: 1 }, spent: { fits: 1, rewrite_only: 1 } }, points: [] });
  assert.deepEqual(two.movesByReasonAndFit, { shed: { fits: 2, least_bad: 1 }, spent: { fits: 1, rewrite_only: 1 } }, 'two reasons keep their own fits');
  assert.deepEqual(d.decodeRewrites({ moves: { spent: 1 }, moves_by_reason_and_fit: { spent: { fits: 'x', unknown: 1 } }, points: [] }).movesByReasonAndFit, { spent: { unknown: 1 } }, 'a count that is not a number is left out');
  assert.deepEqual(d.decodeRewrites({ moves: {}, points: [] }).movesByReasonAndFit, {}, 'a missing member reads an empty map');
});

test('a pool and a key decode the stamp of their last write', () => {
  const pool = fixture('pools').pools[0];
  const key = fixture('keys').keys[0];
  const [p, k] = [d.decodePool(pool), d.decodeKey(key)];
  assert.ok(isIso(p.updatedAt) && isIso(k.updatedAt));
  assert.deepEqual([p.updatedAt, k.updatedAt], [new Date(pool.updated_at).toISOString(), new Date(key.updated_at).toISOString()]);
});

test('a pool and a key decode the revision of their row, which an edit sends back', () => {
  const pool = fixture('pools').pools[0];
  const key = fixture('keys').keys[0];
  const created = fixture('key_created');
  for (const row of [pool, key, created]) {
    assert.ok(Number.isSafeInteger(row.revision) && row.revision > 2 ** 42, "the capture carries the row's token");
  }
  const decoded = [d.decodePool(pool), d.decodeKey(key), d.decodeKey(created)];
  assert.deepEqual(decoded.map((row) => row.revision), [pool.revision, key.revision, created.revision]);
  // The token packs the incarnation above the stamp's 42 bits, so it reads back to the row's own stamp.
  assert.deepEqual(decoded.map((row) => row.revision % 2 ** 42), [pool.updated_at, key.updated_at, created.updated_at]);
  assert.deepEqual(
    [d.decodePool({ ...pool, revision: undefined }).revision, d.decodeKey({ ...key, revision: undefined }).revision],
    [0, 0],
    'a row whose answer carried none reads 0, which the route refuses as stale',
  );
});

test('an account decodes the revision of its row, which an edit sends back', () => {
  const body = fixture('account');
  assert.ok(Number.isInteger(body.revision) && body.revision > 0, 'the capture carries the row\'s counter');
  assert.equal(d.decodeAccount(body).revision, body.revision);
  for (const row of fixture('accounts').accounts) {
    const { revision } = d.decodeAccount(row);
    assert.ok(Number.isInteger(revision) && revision > 0, row.id);
  }
  assert.equal(d.decodeAccount({ ...body, revision: undefined }).revision, 0, 'a row whose answer carried none reads 0, which the route refuses as stale');
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
      plan: { main: '5m', subagent: '1h', models: [{ model: 'claude-haiku-4-5-20251001', main: '5m', subagent: '5m' }] },
      metered: { main: '5m', subagent: '5m', models: [] },
    },
    cache_diagnostics: true,
    keepalive: { ...row.keepalive, max_parked_seconds: 7_200 },
  };
  const p = d.decodePool(wire);
  assert.deepEqual([p.cacheTtl.enabled, p.cacheTtl.injectWhenAbsent, p.cacheTtl.plan.main, p.cacheTtl.plan.subagent], [true, true, '5m', '1h']);
  assert.deepEqual(p.cacheTtl.plan.models, [{ main: '5m', model: 'claude-haiku-4-5-20251001', subagent: '5m' }]);
  assert.deepEqual([p.cacheTtl.metered.main, p.cacheTtl.metered.subagent], ['5m', '5m']);
  assert.equal(p.cacheDiagnostics, true);
  assert.equal(p.keepalive.maxParkedSeconds, 7_200);
  const back = d.encodePool({ ...p, name: p.name, status: 'active' });
  assert.equal(back.cache_ttl.enabled, true);
  assert.equal(back.cache_ttl.inject_when_absent, true);
  assert.equal(back.cache_ttl.plan.models[0].model, 'claude-haiku-4-5-20251001');
  assert.equal(back.cache_ttl.metered.main, '5m');
  assert.equal(back.cache_diagnostics, true);
  assert.deepEqual(back.keepalive, { enabled: true, max_idle_seconds: 3_600, max_parked_seconds: 7_200, scheduled_horizon_seconds: null }, 'the four fields go back under the names the route reads, a null ceiling as null');
  assert.equal(d.decodePool({ ...row, cache_diagnostics: false }).cacheDiagnostics, false);
  assert.equal(d.encodePool({ ...p, name: p.name, cacheDiagnostics: false, status: 'active' }).cache_diagnostics, false, 'the switch is sent off as it is sent on');
});

test('a pool decodes its server-side auto mode switch, on when the row carries none, and encodes it back', () => {
  const row = fixture('pools').pools[0];
  const p = d.decodePool(row);
  assert.equal(p.autoModeServer, true, 'the captured pool is on');
  assert.equal(d.decodePool({ ...row, auto_mode_server: false }).autoModeServer, false);
  const bare = { ...row };
  delete bare.auto_mode_server;
  assert.equal(d.decodePool(bare).autoModeServer, true, 'a row without the field reads the store default');
  assert.equal(d.encodePool({ ...p, name: p.name, status: 'active' }).auto_mode_server, true);
  assert.equal(d.encodePool({ ...p, autoModeServer: false, name: p.name, status: 'active' }).auto_mode_server, false);
});

test('a pool decodes the engine fields and encodes them back', () => {
  const row = fixture('pools').pools[0];
  const p = d.decodePool(row);
  assert.deepEqual(
    [p.shedCap, p.burnAverageSeconds, p.shedDwellSeconds, p.migrateWhenSpent, p.warmSessions],
    [row.shed_cap, 1_800, 900, true, 'when_needed'],
    'the captured pool holds the balanced average and dwell beside its own cap',
  );
  assert.deepEqual([p.retryInPlaceSeconds, p.inFlightCap, p.overageMoveGraceMinutes, p.windDownMinutes], [60, 16, 30, 30], 'and the balanced failover preset');
  assert.deepEqual(p.movesPerHour, { spent: 1 });
  const wire = {
    ...row,
    shed_cap: 0.3,
    burn_average_seconds: 900,
    shed_dwell_seconds: 1800,
    migrate_when_spent: false,
    warm_sessions: 'never',
    retry_in_place_seconds: 20,
    in_flight_cap: 8,
    overage_move_grace_minutes: 45,
    wind_down_minutes: 120,
    moves_per_hour: { shed: 2, spent: 0.5 },
  };
  const q = d.decodePool(wire);
  assert.deepEqual(q.movesPerHour, { shed: 2, spent: 0.5 });
  const back = d.encodePool({ ...q, name: q.name, status: 'active' });
  const fields = ['shed_cap', 'burn_average_seconds', 'shed_dwell_seconds', 'migrate_when_spent', 'warm_sessions', 'retry_in_place_seconds', 'in_flight_cap', 'overage_move_grace_minutes', 'wind_down_minutes'];
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
    metered: { main: '5m', models: [], subagent: '5m' },
    plan: { main: '1h', models: [], subagent: '1h' },
  });
  const partial = d.decodePool({ ...wire, cache_ttl: { enabled: true, plan: { main: '5m' } } });
  assert.deepEqual(
    [partial.cacheTtl.enabled, partial.cacheTtl.plan.main, partial.cacheTtl.plan.subagent, partial.cacheTtl.metered.main],
    [true, '5m', '1h', '5m'],
    'a field the route leaves out falls back one field at a time, each set against its own default',
  );
  const keepalive = { ...wire.keepalive };
  delete keepalive.max_parked_seconds;
  delete keepalive.scheduled_horizon_seconds;
  const bare = d.decodePool({ ...wire, keepalive }).keepalive;
  assert.deepEqual([bare.maxParkedSeconds, bare.scheduledHorizonSeconds], [null, null], 'a keepalive block without the two ceilings reads them as the break-even');
});

test('a pool decodes the pin and placement fields and its members\' budgets, and encodes them back', () => {
  const row = fixture('pools').pools[0];
  const p = d.decodePool(row);
  assert.deepEqual(
    [p.affinityMode, p.subagentPlacement, p.sessionPlacement, p.minSharedPrefixTokens],
    ['always', 'by_strategy', 'by_strategy', 20_000],
    'the captured pool holds the store defaults',
  );
  const direct = p.members.find((m) => m.accountId === 'direct');
  const [report] = row.members[1].spends;
  assert.deepEqual(
    [direct.kind, direct.budgetDailyUsd, direct.budgetMonthlyUsd, direct.budgetUntil, direct.spends],
    ['api_key', null, 50, null, [{ period: 'month', periodEnd: new Date(report.period_end).toISOString(), spentUsd: report.spent_usd }]],
    'the budgeted API-key member reads its budgets and the spend the ledger reported against each',
  );
  const business = p.members.find((m) => m.accountId === 'business');
  const [metered] = row.members[0].spends;
  assert.deepEqual(
    [business.kind, business.budgetDailyUsd, business.budgetMonthlyUsd, business.spends],
    ['oauth', null, 30, [{ period: 'month', periodEnd: new Date(metered.period_end).toISOString(), spentUsd: metered.spent_usd }]],
    'the budgeted subscription member reads its budget and the metered spend the ledger reported against it',
  );
  assert.deepEqual(p.keys, [{ enabled: true, id: 'desktop', label: 'Desktop', prefix: row.keys[0].prefix }]);

  const wire = {
    ...row,
    affinity_mode: 'when_cached',
    subagent_placement: 'with_main',
    session_placement: 'by_warm_prefix',
    min_shared_prefix_tokens: 30_000,
    members: [{ ...row.members[1], budget_daily_usd: 12.5, budget_monthly_usd: 50, budget_until: 86_400_000, eligible: 'budget' }],
    keys: [{ ...row.keys[0], enabled: false }],
  };
  const q = d.decodePool(wire);
  assert.deepEqual([q.affinityMode, q.subagentPlacement, q.sessionPlacement, q.minSharedPrefixTokens], ['when_cached', 'with_main', 'by_warm_prefix', 30_000]);
  assert.deepEqual(
    [q.members[0].budgetDailyUsd, q.members[0].budgetMonthlyUsd, q.members[0].budgetUntil, q.members[0].eligible],
    [12.5, 50, new Date(86_400_000).toISOString(), 'budget'],
    'a member at its budget reads the verdict and the end the route names',
  );
  assert.equal(q.keys[0].enabled, false);
  const back = d.encodePool({ ...q, name: q.name, status: 'active' });
  assert.deepEqual(
    [back.affinity_mode, back.subagent_placement, back.session_placement, back.min_shared_prefix_tokens],
    ['when_cached', 'with_main', 'by_warm_prefix', 30_000],
  );
  assert.deepEqual(back.members, [{ account_id: 'direct', budget_daily_usd: 12.5, budget_monthly_usd: 50, priority: 1 }], 'a member goes back with its budgets and nothing the route reports');
  assert.equal('keys' in back, false, 'the keys are the registry\'s, never a pool setting');

  const bare = { ...row, members: [{ account_id: 'direct', priority: 0, eligible: 'eligible' }], keys: [{ id: 'k', label: 'K', prefix: 'p' }] };
  for (const field of ['affinity_mode', 'subagent_placement', 'session_placement', 'min_shared_prefix_tokens']) delete bare[field];
  const defaults = d.decodePool(bare);
  assert.deepEqual(
    [defaults.affinityMode, defaults.subagentPlacement, defaults.sessionPlacement, defaults.minSharedPrefixTokens],
    ['always', 'by_strategy', 'by_strategy', 20_000],
    'a field the route leaves out reads the store default',
  );
  assert.deepEqual(
    [defaults.members[0].budgetDailyUsd, defaults.members[0].budgetMonthlyUsd, defaults.members[0].budgetUntil, defaults.members[0].spends, defaults.members[0].kind],
    [null, null, null, [], 'oauth'],
    'a member row without the fields reads no budget, no spend and no API key',
  );
  assert.equal(defaults.keys[0].enabled, true, 'a key row without the fields reads the store defaults');
  const odd = d.decodePool({
    ...wire,
    affinity_mode: 'sometimes',
    members: [{ ...wire.members[0], eligible: 'maybe', kind: 'robot', spends: [{ period: 'week', period_end: null, spent_usd: 1 }] }],
    session_placement: 'by_luck',
    subagent_placement: 'alone',
  });
  assert.deepEqual(
    [odd.affinityMode, odd.sessionPlacement, odd.subagentPlacement, odd.members[0].spends[0].period, odd.members[0].eligible, odd.members[0].kind],
    ['always', 'by_strategy', 'by_strategy', 'month', 'unavailable', 'oauth'],
    'every unknown spelling reads its default',
  );
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
  assert.equal(failover.autoMode, 'server');
  assert.deepEqual(failover.attempts[1].response, { clearedInputTokens: null, inferenceGeo: 'us', refusalCategory: null, refusalRecommendedModel: null, servedModel: null, speed: 'fast', stopReason: 'tool_use', stopSequence: null });
  assert.deepEqual([failover.attempts[1].accountKind, failover.attempts[1].overageInUse], ['oauth', null], "the serving attempt's account kind and its unread overage flag");
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
  assert.equal(d.decodeRequest(fixture('request').request).canonicalModel, 'claude-sonnet-5-5');
});

test("a request page carries the ledger's newest write instant, which a poll passes back", () => {
  const wire = fixture('requests');
  assert.equal(typeof wire.landed_through, 'number', 'the route answers the member');
  assert.equal(d.decodeRequestPage(wire).landedThrough, wire.landed_through);
  assert.equal(d.decodeRequestPage({ landed_through: 0, next_cursor: null, requests: [] }).landedThrough, 0, 'an empty ledger answers 0');
});

test('a request carries its seven phases as ISO instants and null where the ledger holds none', () => {
  const failover = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-failover');
  const { phases } = failover;
  assert.deepEqual(Object.keys(phases).sort(), ['finishedAt', 'firstContentAt', 'headAt', 'receivedAt', 'selectedAt', 'streamEndAt', 'writtenAt']);
  assert.ok(Object.values(phases).every(isIso), 'a served stream passed every phase');
  assert.equal(phases.receivedAt, failover.at);
  assert.equal(phases.selectedAt, failover.attempts[0].startedAt, "the first attempt's start");
  const ms = (s) => Date.parse(s);
  assert.equal(ms(phases.writtenAt) - ms(failover.attempts[1].startedAt), 30, "the serving attempt's write");
  assert.deepEqual(
    [phases.headAt, phases.firstContentAt, phases.streamEndAt, phases.finishedAt].map((s) => ms(s) - ms(phases.receivedAt)),
    [520, 700, 850, 900],
    'the head, the first content, the stream end and the finish follow the receipt',
  );
  const bare = d.decodeRequest({ at: 5, phases: { received_at: 5 } }).phases;
  assert.equal(bare.receivedAt, new Date(5).toISOString());
  assert.deepEqual(
    [bare.finishedAt, bare.firstContentAt, bare.headAt, bare.selectedAt, bare.streamEndAt, bare.writtenAt],
    [null, null, null, null, null, null],
    'a phase the ledger holds no instant for reads null',
  );
});

test("a request's cost shares decode keyed by window, null when billed in dollars, empty for an unknown account and null on an unpriced window", () => {
  const page = d.decodeRequestPage(fixture('requests'));
  const plan = page.requests.find((r) => r.id === 'r-captured');
  assert.deepEqual(Object.keys(plan.costShares), ['five_hour', 'seven_day'], 'a Sonnet request a subscription window paid reads its two shared windows and no model week');
  assert.ok(Object.values(plan.costShares).every((s) => typeof s === 'number' && s > 0), 'each a priced fraction');
  assert.equal(page.requests.find((r) => r.id === 'r-failover').costShares, null, 'a fast answer is billed in dollars');
  assert.deepEqual(d.decodeRequest({ cost_shares: {} }).costShares, {}, "an account the route's tiers lack");
  assert.deepEqual(d.decodeRequest({ cost_shares: { five_hour: 0.01, seven_day_fable: null } }).costShares, { five_hour: 0.01, seven_day_fable: null }, 'a window the cost model could not price keeps its null');
  assert.equal(d.decodeRequest({}).costShares, null, 'a row without the member reads null');
});

test("a request's 20x equivalents decode keyed by window and null when billed, and the row and its attempts read their estimated flag and tier", () => {
  const wire = fixture('requests').requests;
  const page = d.decodeRequestPage(fixture('requests'));
  const planWire = wire.find((r) => r.id === 'r-captured');
  const plan = page.requests.find((r) => r.id === 'r-captured');
  assert.deepEqual(plan.costEquivalent, planWire.cost_equivalent, 'a plan request reads its equivalents under the route');
  assert.ok(Object.values(plan.costEquivalent).every((e) => typeof e === 'number' && e > 0), 'each a priced fraction of a 20x window');
  assert.deepEqual([plan.estimated, plan.tier], [planWire.estimated, planWire.tier]);
  assert.equal(typeof plan.tier, 'string', 'a subscription request names the tier it was served on');
  const failoverWire = wire.find((r) => r.id === 'r-failover');
  const failover = page.requests.find((r) => r.id === 'r-failover');
  assert.equal(failover.costEquivalent, null, 'a billed request reads no equivalent');
  assert.deepEqual(failover.attempts.map((a) => a.tier), failoverWire.attempts.map((a) => a.tier), 'each attempt reads its own tier');
  assert.ok(failover.attempts.some((a) => a.tier === null) && failover.attempts.some((a) => a.tier !== null), 'an API key attempt reads none beside the subscription attempt that served');
  assert.deepEqual(d.decodeRequest({ cost_equivalent: { five_hour: 0.01, seven_day: null } }).costEquivalent, { five_hour: 0.01, seven_day: null }, 'a window the model has no weight on keeps its key with null');
  assert.equal(d.decodeRequest({ ...planWire, estimated: true }).estimated, true, 'a fallback-priced request reads estimated');
});

test('an answer from before the 20x equivalents reads them empty, zero, null or false, and a cost model without its model tiers reads the problem', () => {
  const zero = { fiveHour: 0, sevenDay: 0 };
  const group = d.decodeAnalyticsGroup({});
  assert.deepEqual(
    [group.billed, group.billedUsd, group.cacheEquivalent, group.costEquivalent, group.equivalentSplit, group.estimated, group.probeEquivalent],
    [{ cacheRead: 0, cacheWrite: 0, input: 0, output: 0 }, 0, {}, {}, null, false, {}],
    'a group',
  );
  const point = d.decodeBurnPoint({});
  assert.deepEqual([point.estimated, point.fiveHourEquivalent, point.sevenDayEquivalent, point.modelWeekEquivalent, point.modelWeekShare], [false, null, null, null, null], 'a burn point');
  const day = d.decodeDashboard({ hourly: [{ at: 0 }] });
  assert.deepEqual([day.billed24h, day.equivalent24h, day.estimated24h, day.hourly[0].billed, day.hourly[0].equivalent], [0, zero, false, 0, zero], 'the dashboard');
  const key = d.decodeKey({});
  assert.deepEqual([key.lifetime.billedUsd, key.lifetime.equivalent, key.lifetime.estimated, key.usage7d.billedUsd, key.usage7d.equivalent, key.usage7d.estimated], [0, zero, false, 0, zero, false], 'a key');
  const pool = d.decodePool({ traffic_24h: [{ at: 0 }] });
  assert.deepEqual([pool.billed24h, pool.equivalent24h, pool.estimated24h, pool.traffic24h[0].billedUsd, pool.traffic24h[0].equivalent, pool.traffic24h[0].estimated], [0, zero, false, 0, zero, false], 'a pool');
  const session = d.decodeSession({});
  assert.deepEqual([session.burnEquivalent, session.estimated], [null, false], 'a session');
  const request = d.decodeRequest({ attempts: [{ index: 1 }] });
  assert.deepEqual([request.costEquivalent, request.estimated, request.tier, request.attempts[0].tier], [null, false, null, null], 'a request');
  const wire = fixture('settings');
  const { revisions: _prices, ...pricesBefore } = wire.api_prices;
  const { revisions: _costs, ...costsBefore } = wire.subscription_costs;
  const { model_tier: _tier, tier_order: _order, ...contentBefore } = costsBefore.content;
  const before = d.decodeSettings({ ...wire, api_prices: pricesBefore, subscription_costs: { ...costsBefore, content: contentBefore } });
  assert.deepEqual(before.apiPrices.revisions, [], 'a price table read leniently lists no revision');
  assert.equal(before.apiPrices.models.length, Object.keys(pricesBefore.content.models).length, 'and keeps its rows');
  assert.deepEqual([before.subscriptionCosts.problem, before.subscriptionCosts.tierOrder, before.subscriptionCosts.revisions], ["The cost model's answer could not be read.", [], []], 'a cost model read strictly cannot price without its model tiers');
});

test("an attempt reads the upstream's request id, and its message id only on the answer that served", () => {
  const failover = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-failover');
  assert.deepEqual(
    failover.attempts.map((a) => [a.upstreamStatus, a.upstreamRequestId, a.upstreamMessageId]),
    [[429, 'req_r-failover_1', null], [200, 'req_r-failover_2', 'msg_r-failover_2']],
  );
  const missing = d.decodeRequest({ attempts: [{ index: 1 }] }).attempts[0];
  assert.deepEqual([missing.upstreamRequestId, missing.upstreamMessageId], [null, null], 'an attempt with no head names neither');
});

test("an attempt reads the answer's service tier, and none where the answer carried none", () => {
  const wire = fixture('requests').requests[0].attempts[0];
  assert.equal(wire.service_tier, null, 'the captured answer carried no tier');
  assert.equal(d.decodeRequest({ attempts: [wire] }).attempts[0].serviceTier, null);
  assert.equal(d.decodeRequest({ attempts: [{ ...wire, service_tier: 'priority' }] }).attempts[0].serviceTier, 'priority');
  assert.equal(d.decodeRequest({ attempts: [{ index: 1 }] }).attempts[0].serviceTier, null, 'an attempt without the field reads none');
});

test('a stream that ended before its end decodes as incomplete and a group reads its own counter', () => {
  const cut = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-incomplete');
  assert.deepEqual([cut.outcome, cut.status, cut.errorCode, cut.transport], ['incomplete', 200, 'stream_incomplete', 'sse'], 'the sixth outcome reads as itself, not the failed fallback');
  assert.equal(cut.attempts.at(-1).outcome, 'upstream_error', 'the serving attempt reads as failed');
  assert.equal(d.decodeRequest({ ...fixture('request').request, outcome: 'incomplete' }).outcome, 'incomplete');
  assert.equal(d.decodeAnalyticsGroup({ incomplete: 3 }).incomplete, 3);
  const desktop = fixture('analytics_breakdown').rows.map(d.decodeAnalyticsGroup).find((g) => g.key === 'desktop');
  assert.equal(desktop.incomplete, 1, 'the route counts the row in the group\'s own column');
});

test('a request reads its auto-mode standing, and a spelling the console does not know reads as none', () => {
  const captured = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-captured');
  assert.equal(captured.autoMode, null, 'a request that asked for nothing');
  assert.deepEqual(['body_shape', 'server', 'stripped'].map((m) => d.decodeRequest({ attempts: [], auto_mode: m }).autoMode), ['body_shape', 'server', 'stripped']);
  assert.equal(d.decodeRequest({ attempts: [], auto_mode: 'unknown' }).autoMode, null);
});

test('a request reads who put the diagnostics member on its body, and any other value as none', () => {
  const rows = d.decodeRequestPage(fixture('requests')).requests;
  assert.deepEqual(
    rows.map((r) => [r.id, r.diagnosticsSource]),
    fixture('requests').requests.map((r) => [r.id, r.diagnostics_source]),
    'the list reads each row as the route answered it',
  );
  assert.deepEqual([rows.find((r) => r.id === 'r-captured').diagnosticsSource, rows.find((r) => r.id === 'r-failover').diagnosticsSource], ['client', 'router'], 'the fixture holds both words');
  assert.equal(d.decodeRequest(fixture('request').request).diagnosticsSource, 'router', 'the detail route answers the same row');
  assert.deepEqual(
    [{ diagnostics_source: null }, {}, { diagnostics_source: 'elsewhere' }].map((r) => d.decodeRequest({ attempts: [], ...r }).diagnosticsSource),
    [null, null, null],
    'null, an absent member and a word the console does not know read as none',
  );
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
});

test('an account hour with no utilization sample carries no peak rather than a zero one', () => {
  const [hour] = d.decodeAccountRollups(fixture('analytics_accounts'));
  const wire = fixture('analytics_accounts').accounts[0].hours[0];
  assert.deepEqual([wire.utilization_5h_max, wire.utilization_7d_max], [0.4, null], 'the fixture samples the 5-hour window alone');
  assert.deepEqual([hour.peak['5h'], hour.peak['7d'], hour.peak['7d_fable']], [0.4, null, null]);
});

test("an account rollup carries its hour's window samples keyed by the ledger's ids with the lead and the sampled instant, and a reset-only sample decodes a null lead", () => {
  const wireAccount = fixture('analytics_accounts').accounts.find((a) => a.account_id === 'business');
  const wireHour = wireAccount.hours.find((h) => h.windows.five_hour !== null);
  const rollups = d.decodeAccountRollups(fixture('analytics_accounts'));
  const sampled = rollups.find((r) => r.accountId === 'business' && r.hour === new Date(wireHour.hour).toISOString());
  assert.deepEqual(Object.keys(sampled.windows).sort(), Object.keys(wireHour.windows).filter((k) => wireHour.windows[k] !== null).sort(), "the keys are the ledger's ids as the route spells them, the null windows absent");
  assert.ok('seven_day_fable' in sampled.windows, 'the model week keeps its own key rather than folding onto the overage-included one');
  const five = wireHour.windows.five_hour;
  assert.deepEqual(sampled.windows.five_hour, { lead: five.lead, resetsAt: new Date(five.reset_at).toISOString(), sampledAt: new Date(five.sampled_at).toISOString(), utilization: five.utilization });
  assert.equal(typeof sampled.windows.five_hour.lead, 'number', 'a sampled utilization carries its lead');
  const wireIdle = wireAccount.hours.find((h) => Object.values(h.windows).every((w) => w === null));
  assert.ok(wireIdle, 'the capture holds a business hour without a sample');
  assert.deepEqual(rollups.find((r) => r.accountId === 'business' && r.hour === new Date(wireIdle.hour).toISOString()).windows, {}, 'an hour whose windows all read null decodes none');
  const [hand] = d.decodeAccountRollups({ accounts: [{ account_id: 'a', hours: [{ hour: 3_600_000, windows: { five_hour: null, seven_day: { utilization: null, reset_at: 7_200_000, lead: null, sampled_at: 5_400_000 }, seven_day_fable: null } }] }] });
  assert.deepEqual(hand.windows, { seven_day: { lead: null, resetsAt: '1970-01-01T02:00:00.000Z', sampledAt: '1970-01-01T01:30:00.000Z', utilization: null } }, 'a reset-only sample keeps its null lead rather than reading as on pace');
  const [bare] = d.decodeAccountRollups({ accounts: [{ account_id: 'a', hours: [{ hour: 3_600_000, windows: { five_hour: { utilization: 0.5, reset_at: null, lead: 0.5 } } }] }] });
  assert.equal(bare.windows.five_hour.sampledAt, '1970-01-01T01:00:00.000Z', "a sample without an instant reads the hour's own");
  assert.equal(bare.windows.five_hour.resetsAt, null);
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
  // An earlier holder's label is built at decode time in the host's zone, so the rows are read under a pinned one.
  const rows = inZone('UTC', () => fixture('analytics_breakdown').rows.map(d.decodeAnalyticsGroup));
  const desktop = rows.find((g) => g.key === 'desktop');
  assert.equal(desktop.requests, fixture('analytics_breakdown').rows.find((g) => g.key === 'desktop').requests);
  const earlier = rows.find((g) => g.key.startsWith('desktop@'));
  assert.equal(earlier.label, 'desktop (deleted, created Oct 11, 2026, 12:15)');
  assert.equal(earlier.requests, 1);
  // A nonzero generation proves the wire names, as the held-probe copy below does for its field.
  assert.equal(earlier.keyId, 'desktop');
  assert.equal(earlier.keyCreatedAt, 1_791_720_940_000);
  assert.equal(desktop.keyId, 'desktop');
  assert.equal(desktop.keyCreatedAt, null, 'the live key carries no creation instant');
  assert.equal(desktop.histograms.ttfbMs.length, 64);
  assert.equal(typeof desktop.percentiles.ttfbMs.p50, 'number');
  const desktopWire = fixture('analytics_breakdown').rows.find((g) => g.key === 'desktop');
  assert.equal(desktop.cacheMissReasons.none, desktopWire.cache_miss_reasons.none);
  assert.equal(desktop.tokens.thinking, desktopWire.tokens.thinking);
  assert.equal(desktop.wastedProbeReasons.idle_limit, 0);
  assert.equal('savings' in desktop, false, 'the group carries the probe figures and no savings member');
  assert.equal(desktop.errors.upstream_error, 1);
  assert.equal(desktop.probes.held, 0, 'the captured group counts no held probe');
  // The capture reads 0, which a missing field decodes to as well, so a nonzero copy proves the wire name.
  const row = fixture('analytics_breakdown').rows[0];
  assert.equal(d.decodeAnalyticsGroup({ ...row, probes: { ...row.probes, held: 3 } }).probes.held, 3, 'a group reads its held probes off the route');
  const points = fixture('analytics_series').points.map(d.decodeAnalyticsPoint);
  assert.ok(points.length >= 24 && isIso(points[0].at));
  // The 20x equivalents, the billed side and the plan counts read under the key grouping, where no share does.
  const wire = fixture('analytics_breakdown').rows.find((r) => r.key === 'desktop');
  const split = (c) => ({ cacheRead: c.cache_read, cacheWrite: c.cache_write, input: c.input, output: c.output });
  assert.equal(wire.cost_shares, null, 'the key grouping answers no share');
  assert.deepEqual(desktop.costEquivalent, wire.cost_equivalent, 'the key grouping reads the 20x equivalent by window');
  assert.ok(Object.keys(desktop.costEquivalent).length > 0, 'the captured key served plan traffic');
  assert.deepEqual(desktop.cacheEquivalent, wire.cache_equivalent);
  assert.deepEqual(desktop.probeEquivalent, wire.probe_equivalent);
  assert.deepEqual([desktop.billedUsd, desktop.billed], [wire.billed_usd, split(wire.billed)], 'the billed rows alone');
  assert.deepEqual(desktop.equivalentSplit, { cacheRead: wire.equivalent_split.cache_read, cacheWrite: wire.equivalent_split.cache_write, input: wire.equivalent_split.input, output: wire.equivalent_split.output });
  assert.equal(desktop.estimated, wire.estimated);
  assert.deepEqual([desktop.planRequests, desktop.planOutputTokens], [wire.plan_requests, wire.plan_output_tokens], 'the plan counts read under the key grouping');
  assert.ok(typeof desktop.planRequests === 'number' && typeof desktop.planOutputTokens === 'number', 'as numbers');
  const point = points.find((p) => p.groups.length > 0);
  const pointWire = fixture('analytics_series').points.find((p) => p.groups.length > 0).groups[0];
  assert.deepEqual([point.groups[0].costEquivalent, point.groups[0].billedUsd], [pointWire.cost_equivalent, pointWire.billed_usd], 'a series point carries the same members');
  const rollups = d.decodeAccountRollups(fixture('analytics_accounts'));
  assert.ok(rollups.every((r) => isIso(r.hour) && typeof r.peak['5h'] === 'number'));
});

test('an account-grouped breakdown row carries its shares keyed by window and a key-grouped one null', () => {
  const business = fixture('analytics_breakdown_accounts').rows.map(d.decodeAnalyticsGroup).find((g) => g.key === 'business');
  assert.equal(typeof business.costShares.five_hour, 'number');
  assert.equal(typeof business.costShares.seven_day, 'number');
  assert.equal(typeof business.cacheShares.five_hour.read, 'number');
  assert.equal(typeof business.cacheShares.five_hour.write, 'number');
  assert.equal(typeof business.planRequests, 'number');
  assert.equal(typeof business.planOutputTokens, 'number');
  // The capture seeds no probe on the account, so the map is answered with no window in it.
  assert.deepEqual(business.probeShares, {});
  const probed = d.decodeAnalyticsGroup({ probe_shares: { five_hour: { cost: 0.25, wasted: null }, seven_day: { cost: 0.05, wasted: 0.0125 } } });
  assert.deepEqual(probed.probeShares, { five_hour: { cost: 0.25, wasted: null }, seven_day: { cost: 0.05, wasted: 0.0125 } }, 'a wasted null reads through');
  const businessWire = fixture('analytics_breakdown_accounts').rows.find((r) => r.key === 'business');
  assert.deepEqual([business.costEquivalent, business.cacheEquivalent], [businessWire.cost_equivalent, businessWire.cache_equivalent], 'the account grouping reads the equivalents beside the shares');
  const directWire = fixture('analytics_breakdown_accounts').rows.find((r) => r.key === 'direct');
  const direct = fixture('analytics_breakdown_accounts').rows.map(d.decodeAnalyticsGroup).find((g) => g.key === 'direct');
  assert.deepEqual([direct.cacheShares, direct.costShares, direct.probeShares], [null, null, null], 'an api_key group');
  assert.deepEqual([direct.planOutputTokens, direct.planRequests], [0, 0], 'an api_key group counts no plan-funded request');
  assert.deepEqual([direct.planOutputTokens, direct.planRequests, direct.costEquivalent, direct.billedUsd], [directWire.plan_output_tokens, directWire.plan_requests, {}, directWire.billed_usd], 'an api_key group counts no plan request and reads its dollars billed');
  for (const [i, row] of fixture('analytics_breakdown').rows.map(d.decodeAnalyticsGroup).entries()) {
    const sent = fixture('analytics_breakdown').rows[i];
    assert.deepEqual([row.cacheShares, row.costShares, row.probeShares], [null, null, null], `a key-grouped row: ${row.key}`);
    assert.deepEqual([row.planOutputTokens, row.planRequests], [sent.plan_output_tokens, sent.plan_requests], `a key-grouped row counts its plan requests: ${row.key}`);
    assert.ok(typeof row.planRequests === 'number' && typeof row.planOutputTokens === 'number', `the plan-funded counts answer under every grouping: ${row.key}`);
  }
  const point = fixture('analytics_series_accounts').points.map(d.decodeAnalyticsPoint).find((p) => p.groups.some((g) => g.key === 'business'));
  assert.equal(typeof point.groups.find((g) => g.key === 'business').costShares.five_hour, 'number', 'a series point carries the same shares');
});

test('the burn series decodes each point with its instant, both token sides, and both window shares only where the route has them', () => {
  const body = fixture('analytics_burn');
  const burn = d.decodeBurn(body);
  assert.equal(burn.bucketSeconds, 3_600);
  assert.equal(burn.points.length, body.points.length);
  // The last point that burned a subscription's windows, so every field it is read against holds a value a misnamed
  // field could not stand in for.
  const at = body.points.findLastIndex((p) => p.five_hour_share !== null);
  const [last, wire] = [burn.points[at], body.points[at]];
  assert.ok(
    wire.five_hour_share > 0 && wire.seven_day_share > 0 && wire.cached_tokens > 0 && wire.uncached_tokens > 0 && wire.tokens > wire.cached_tokens + wire.uncached_tokens,
    'the fixture reports a subscription point with both window shares priced, output tokens among its count',
  );
  assert.ok(wire.five_hour_equivalent > 0 && wire.seven_day_equivalent > 0, 'the fixture prices the point in the 20x equivalent too');
  assert.deepEqual(last, {
    at: new Date(wire.at).toISOString(),
    cachedTokens: wire.cached_tokens,
    estimated: wire.estimated,
    fiveHourEquivalent: wire.five_hour_equivalent,
    fiveHourShare: wire.five_hour_share,
    group: 'business',
    modelWeekEquivalent: wire.model_week_equivalent,
    modelWeekShare: wire.model_week_share,
    sevenDayEquivalent: wire.seven_day_equivalent,
    sevenDayShare: wire.seven_day_share,
    tokens: wire.tokens,
    uncachedTokens: wire.uncached_tokens,
    unit: 'window_share',
    usd: wire.usd,
  });
  assert.ok(body.points.every((p) => 'model_week_share' in p && 'model_week_equivalent' in p), 'every point carries the model week beside the two shared windows');
  const week = d.decodeBurnPoint({ ...wire, model_week_equivalent: 0.25, model_week_share: 0.125 });
  assert.deepEqual([week.modelWeekEquivalent, week.modelWeekShare], [0.25, 0.125], "a bucket on a model with a week of its own reads that week's share and equivalent");
  const direct = burn.points[burn.points.length - 1];
  assert.deepEqual(
    [direct.group, direct.fiveHourShare, direct.sevenDayShare, direct.fiveHourEquivalent, direct.sevenDayEquivalent, direct.unit, direct.usd],
    ['direct', null, null, 0, 0, 'usd', body.points.at(-1).usd],
    'an API-key point spends dollars, reads no window share and a zero 20x equivalent',
  );
  assert.ok(burn.points.every((p) => isIso(p.at)));
  // Every grouping but the account one answers the shares and the unit as null, and the equivalents under every one.
  const [keyed] = d.decodeBurn({ bucket_seconds: 21_600, points: [{ ...wire, five_hour_share: null, group: 'desktop', seven_day_share: null, unit: null }] }).points;
  assert.deepEqual([keyed.fiveHourShare, keyed.sevenDayShare, keyed.unit], [null, null, null]);
  assert.deepEqual([keyed.fiveHourEquivalent, keyed.sevenDayEquivalent], [wire.five_hour_equivalent, wire.seven_day_equivalent]);
  assert.equal(d.decodeBurnPoint({ ...wire, estimated: true }).estimated, true, 'a fallback-priced bucket reads estimated');
  const { tokens: _, ...untold } = wire;
  assert.equal(d.decodeBurn({ bucket_seconds: 3_600, points: [untold] }).points[0].tokens, 0, 'a point without the count decodes it as zero');
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

test('an attempt carries the selection trace with its skipped members as named objects', () => {
  const detail = d.decodeRequest(fixture('request').request);
  const [bare] = d.decodeRequest({ attempts: [{ index: 1, selection: null }] }).attempts;
  assert.equal(bare.selection, null, 'an attempt stored without a trace reads as none');
  // The fixture's trace has not had its cache read folded in, so the member reads as absent.
  assert.deepEqual(detail.attempts[1].selection, { cacheRead: null, cohortHold: null, moved: null, placement: null, renewed: false, skipped: [{ accountId: 'direct', obstacle: 'ranked' }], strategy: 'pace', tier: [0, 0, 0] });
  assert.equal(detail.requestClass, null, 'the fixture request sent no class header');
  // No captured request carries a folded cache read, a move, a renewal or a class yet, so this row is built from
  // the backend's serde names: `SelectionTrace` and `Skipped` in the routing crate, `cache_read` from the ledger.
  const row = d.decodeRequest({
    ...fixture('request').request,
    attempts: [{ index: 1, account_id: 'business', selection: { cache_read: 4_200, moved: 'shed', renewed: false, skipped: [{ account_id: 'direct', obstacle: 'above_ceiling' }, { account_id: 'mail', obstacle: 'subscription_ended' }], strategy: 'round_robin', tier: [1, 2, 1] } }, { index: 2, account_id: 'business', selection: { renewed: true, skipped: [], strategy: 'pace', tier: null } }],
    request_class: 'subagent',
  });
  assert.equal(row.requestClass, 'subagent');
  assert.deepEqual(row.attempts[0].selection, { cacheRead: 4_200, cohortHold: null, moved: 'shed', placement: null, renewed: false, skipped: [{ accountId: 'direct', obstacle: 'above_ceiling' }, { accountId: 'mail', obstacle: 'subscription_ended' }], strategy: 'round_robin', tier: [1, 2, 1] });
  assert.deepEqual(row.attempts[1].selection, { cacheRead: null, cohortHold: null, moved: null, placement: null, renewed: true, skipped: [], strategy: 'pace', tier: null }, 'a renewed pin answers no tier');
  const [odd] = d.decodeRequest({ attempts: [{ index: 1, selection: { renewed: false, skipped: [], strategy: 'pace', tier: [0, 1] } }] }).attempts;
  assert.equal(odd.selection.tier, null, 'a tier that is not three numbers reads as none rather than as a partial one');
});

test('a selection trace decodes its placement', () => {
  // The fixture's first attempt was placed through the router's own selection, after a hold on its cohort.
  const wire = fixture('request').request.attempts[0].selection;
  const placed = d.decodeRequest(fixture('request').request).attempts[0].selection;
  assert.ok(wire?.placement && wire?.cohort_hold, 'the captured trace carries a placement and a hold');
  assert.deepEqual(placed.placement, { depth: wire.placement.depth, prefixTokens: wire.placement.prefix_tokens, rung: wire.placement.rung });
  assert.deepEqual(placed.cohortHold, { cohort: wire.cohort_hold.cohort, heldMs: wire.cohort_hold.held_ms, waitedOn: wire.cohort_hold.waited_on, waits: wire.cohort_hold.waits.map((w) => ({ heldMs: w.held_ms, reason: w.reason })) });
  assert.equal(placed.cohortHold.heldMs, 1250, 'the captured hold names its wait');
  assert.deepEqual([placed.placement.rung, placed.placement.depth, placed.placement.prefixTokens > 0], ['content', 2, true]);
  const trace = (placement, cohort_hold) => d.decodeSelection({ cohort_hold, placement, renewed: false, skipped: [], strategy: 'pace', tier: null });
  for (const rung of ['content', 'static', 'with_main']) assert.equal(trace({ depth: 1, prefix_tokens: 10, rung }, null).placement.rung, rung);
  assert.deepEqual([...d.PLACEMENT_RUNG_VALUES], ['content', 'static', 'with_main']);
  assert.equal(trace({ depth: 1, prefix_tokens: 10, rung: 'guess' }, null).placement, null, 'a rung the console cannot word reads as no placement');
  assert.equal(trace(null, null).placement, null);
  assert.deepEqual(trace(null, { cohort: null, held_ms: 400, waited_on: ['business', 'direct'], waits: [{ held_ms: 400, reason: 'cap' }] }).cohortHold, { cohort: null, heldMs: 400, waitedOn: ['business', 'direct'], waits: [{ heldMs: 400, reason: 'cap' }] }, "a hold on the strategy's choice names no cohort");
  const older = d.decodeSelection({ renewed: true, skipped: [], strategy: 'pace', tier: null });
  assert.deepEqual([older.placement, older.cohortHold], [null, null], 'a trace without either member reads as neither');
});

test('a request decodes the holds it waited through, whether or not an attempt followed', () => {
  const wire = fixture('request').request;
  assert.deepEqual(d.decodeRequest(wire).held, { cohort: wire.held.cohort, heldMs: 1250, waitedOn: wire.held.waited_on, waits: [{ heldMs: 1250, reason: 'cap' }] }, 'the row carries its own summary');
  assert.deepEqual(d.decodeRequest(wire).held, d.decodeRequest(wire).attempts[0].selection.cohortHold, 'and it is the trace the attempt after the hold names');
  assert.deepEqual(d.decodeRequest({ ...wire, attempts: [], held: { cohort: null, held_ms: 10_000, waited_on: ['business'], waits: [{ held_ms: 10_000, reason: 'cap' }] } }).held, { cohort: null, heldMs: 10_000, waitedOn: ['business'], waits: [{ heldMs: 10_000, reason: 'cap' }] }, 'a hold no attempt followed');
  assert.equal(d.decodeRequest({ ...wire, held: null }).held, null, 'a request never held');
});

test('a request decodes its fork', () => {
  const wire = fixture('request').request;
  const detail = d.decodeRequest(wire);
  assert.deepEqual([detail.forkedFrom, detail.forkDepth], [wire.forked_from, wire.fork_depth]);
  assert.deepEqual([detail.sessionId, detail.forkedFrom, detail.forkDepth], ['s1', 's0', 2], 'the captured session branched from another at depth 2');
  const plain = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-failed');
  assert.deepEqual([plain.forkedFrom, plain.forkDepth], [null, null], 'a request that branched from no other reads no fork');
});

test('a budget reads as a skip obstacle and as a pin release reason', () => {
  assert.ok(d.SKIP_OBSTACLE_VALUES.includes('budget'), 'the trace labels a member passed over at its budget');
  assert.deepEqual(d.decodeSelection({ renewed: false, skipped: [{ account_id: 'direct', obstacle: 'budget' }], strategy: 'pace', tier: [0, 0, 0] }).skipped, [{ accountId: 'direct', obstacle: 'budget' }]);
  assert.ok(d.PIN_RELEASE_REASON_VALUES?.includes('budget'), 'the drawer words a pin released for a member at its budget');
  const released = d.decodeRequestEvent({ at: 1_000, detail: { agent_id: 'main', reasons: ['budget', 'spent'] }, kind: 'pin_expired' });
  assert.deepEqual(released.detail.reasons, ['budget', 'spent']);
});

test('a member the fit gate passed over reads no_fit, and the two partitions that left are no obstacle', () => {
  assert.deepEqual([...d.SKIP_OBSTACLE_VALUES], ['at_cap', 'attempted', 'budget', 'budget_unknown', 'cooldown', 'credits', 'no_fit', 'operator', 'overage', 'ranked', 'spent', 'subscription_ended', 'winding_down']);
});

test('the routing log page decodes both row shapes from the fixture', () => {
  const body = fixture('routing_log');
  const page = d.decodeRoutingLogPage(body);
  assert.deepEqual(page.rows.map((r) => r.type), body.rows.map((r) => r.type), 'every fixture row decodes, in the route order, under its own type');
  assert.equal(page.counts, null, 'a page with no request filter answers no counts');
  assert.equal(page.nextCursor, null);
  const placed = page.rows.find((r) => r.type === 'decision' && r.requestId === 'r-static');
  assert.ok(isIso(placed.at), 'a decision time decodes to ISO');
  assert.deepEqual(placed.fact, [{ entity: null, key: 'prefix', unit: 'tokens', value: 31200 }, { entity: null, key: 'depth', unit: 'count', value: 1 }]);
  assert.deepEqual(placed.rung, { depth: 1, prefixTokens: 31200, rung: 'static' });
  assert.deepEqual(placed.cache, { hit: true, inputTokens: 1000, promisedTokens: 31200, readTokens: 500 });
  const refused = page.rows.find((r) => r.type === 'decision' && r.kind === 'refused');
  assert.deepEqual(refused.fact, [{ entity: { id: 'retired', kind: 'key' }, key: 'key', unit: null, value: 'retired' }], 'a fact names its entity');
  assert.deepEqual(refused.refusal, { code: 'unknown', kind: 'key', reason: 'the key is not in the registry' }, "a refusal reads the router's code beside its reason");
  assert.equal(d.decodeDecisionSummary({ ...body.rows.find((r) => r.kind === 'refused'), refusal: { kind: 'key', reason: 'the key is not in the registry' } }).refusal.code, '', 'a refusal written before its code reads an empty one');
  assert.equal(d.decodeDecisionSummary({ ...body.rows.find((r) => r.kind === 'refused'), refusal: { code: 'revoked', kind: 'key', reason: 'the key was revoked' } }).refusal.code, 'revoked');
  const exhausted = page.rows.find((r) => r.type === 'decision' && r.kind === 'exhausted');
  assert.deepEqual(exhausted.by, { kind: 'exhaustion', tier: null, value: 'timed' });
  const wireDeadline = body.rows.find((r) => r.type === 'decision' && r.kind === 'exhausted').exhaustion.deadline;
  assert.deepEqual(exhausted.exhaustion, { deadline: new Date(wireDeadline).toISOString(), kind: 'timed', obstacle: null, reason: null, window: 'five_hour' });
  const declined = d.decodeDecisionSummary({ ...body.rows.find((r) => r.kind === 'exhausted'), exhaustion: { deadline: null, kind: 'credits', obstacle: 'credits', reason: 'declined', window: null } });
  assert.equal(declined.exhaustion.reason, 'declined', "an exhaustion's reason is kept, the Fact a credits exhaustion draws");
  const held = page.rows.find((r) => r.type === 'decision' && r.kind === 'held');
  assert.deepEqual([held.hold, held.score.unit], [{ accountId: 'business', boundMs: 60_000, cohort: null, cycleMs: null, factor: null, reason: 'cap', speed: null, stretchMs: null, thresholdMs: null, until: null }, 'percent_per_hour']);
  const round = page.rows.find((r) => r.type === 'event' && r.kind === 'shedding_round');
  assert.ok(isIso(round.at));
  assert.deepEqual([round.accountId, round.agentId, round.poolId, round.requestId, round.sessionId], ['business', 'main', 'main', 'r-failover', 's1']);
  assert.equal(round.detail.persisted, true, "the kind's own fields pass through for the console to read by name");
  assert.deepEqual(page.rows.find((r) => r.type === 'event' && r.kind === 'overage_started').detail, {}, 'a null detail reads as no fields');
  assert.deepEqual(d.decodeRoutingLogPage({ ...body, counts: { decisions: 3, events: 4 } }).counts, { decisions: 3, events: 4 });
});

test('a decision record decodes its six blocks and the completion', () => {
  const body = fixture('decision');
  const r = d.decodeDecisionRecord(body);
  assert.deepEqual([r.version, r.requestId, r.seq], [2, 'r-static', 1]);
  assert.equal(r.raw, body, 'the record keeps the route answer whole for the card to copy');
  assert.ok(isIso(r.request.at));
  assert.deepEqual([r.request.keyId, r.request.poolId, r.request.canonicalModel], ['desktop', 'main', 'claude-sonnet-5']);
  assert.deepEqual(r.request.lane, { effort: '-', speed: 'standard', thinking: 'none' });
  assert.equal(r.request.flags.marker, true);
  assert.deepEqual(r.request.chain, { boundaries: 0, staticBoundaries: 1, staticHash: false });
  assert.deepEqual([r.pin.found, r.pin.accountId, r.pin.released], ['none', null, []]);
  assert.equal(r.pinReason, null, 'a record with a pin block names no reason');
  assert.ok('window_ceiling' in body.settings, 'the route names the ceiling as the pool row does');
  assert.deepEqual([r.settings.strategy, r.settings.inFlightCap, r.settings.windowCeiling], ['pace', 16, body.settings.window_ceiling]);
  assert.ok(isIso(r.settings.poolUpdatedAt));
  const [chosen, spare] = r.candidates;
  assert.match(chosen.prefix.static.hash, /^[0-9a-f]{64}$/, 'a prefix lookup carries the boundary hash whole');
  assert.equal(chosen.prefix.staticMatch.ttlMs, 3_600_000);
  assert.ok(isIso(chosen.prefix.static.writtenAt));
  assert.deepEqual([chosen.prefix.content, chosen.prefix.contentMatch, chosen.prefix.withMain], [null, null, false]);
  assert.deepEqual(chosen.tier, [0, 0, 0]);
  assert.deepEqual([chosen.windows.fast_input.source, chosen.windows.fast_input.ageMs, chosen.windows.fast_input.ahead], ['head', 5_000, null]);
  assert.ok(isIso(chosen.windows.fast_input.observedAt));
  assert.deepEqual(chosen.cooldowns.map((c) => [c.scope, c.blocking]), [['model:claude-haiku-4-5-20251001', false]]);
  assert.deepEqual([chosen.verdict, spare.verdict, spare.kind, spare.pace.hours], ['chosen', 'not_needed', 'api_key', null]);
  assert.equal(spare.budget[0].spentUsd, 0.0036);
  assert.deepEqual(r.walk.map((s) => s.step), ['pin', 'eligible', 'partitions', 'rung:content', 'rung:with_main', 'rung:static', 'reserve']);
  assert.deepEqual(r.walk[5].facts[0], { accountId: null, key: 'depth', unit: 'count', value: 1 });
  assert.deepEqual(r.walk[1].facts[0], { accountId: 'business', key: 'standing', unit: null, value: 'eligible' });
  assert.deepEqual([r.outcome.kind, r.outcome.accountId, r.outcome.rung], ['placed', 'business', { depth: 1, prefixTokens: 31200, rung: 'static' }]);
  assert.deepEqual(r.outcome.pinWritten, { action: 'created', epoch: 3, hashesStamped: false, lane: 'claude-sonnet-5|standard|none|-', refreshStamped: true });
  assert.deepEqual(r.outcome.pinEvents, [{ kind: 'pin_created', reason: null }]);
  assert.equal(r.outcome.completion.hit, true);
  assert.deepEqual(r.outcome.completion.cacheMapWritten, [{ accountId: 'business', depth: 1, prefixTokens: 31200, tier: 'message', ttlMs: 300_000 }]);
  assert.ok(isIso(r.outcome.completion.completedAt));
  // The fixture's record waited on no hold and refused nothing, so the run-out bit and the refusal code ride on a copy.
  const held = (h) => ({ account_id: 'business', attempt: 1, cohort: null, held_ms: 60_000, ...h });
  const ranOut = d.decodeDecisionRecord({ ...body, request: { ...body.request, holds: [held({}), held({ ran_out: true })] } });
  assert.deepEqual(ranOut.request.holds.map((h) => h.ranOut), [false, true], 'a hold without the bit reads as one the relay did not answer at');
  const revoked = d.decodeDecisionRecord({ ...body, outcome: { ...body.outcome, refusal: { code: 'revoked', kind: 'key', reason: 'the key was revoked' } } });
  assert.deepEqual(revoked.outcome.refusal, { code: 'revoked', kind: 'key', reason: 'the key was revoked' });
});

test('a row with null optionals decodes to nulls', () => {
  const s = d.decodeDecisionSummary({
    account_id: null, agent_id: null, at: 1_000, attempt: 1, attempt_count: null, by: { kind: 'none', tier: null, value: '' }, cache: null, exhaustion: null, fact: [{ entity: null, key: 'pick', unit: null, value: true }, { entity: null, key: 'idle', unit: 'ms', value: 5 }],
    fast: false, from_account_id: null, hold: null, key_id: null, kind: 'renewed', model: null, pool_id: null, probe: false, refusal: null, request_id: 'r', rule: null, rung: null, runner_up: null, score: null, seq: 1, session_id: null, skipped: 0, status: null, strategy: 'pace', tier: null, type: 'decision', warmth: null,
  });
  assert.deepEqual(
    [s.accountId, s.agentId, s.attemptCount, s.cache, s.exhaustion, s.fromAccountId, s.hold, s.keyId, s.model, s.poolId, s.refusal, s.rule, s.rung, s.runnerUp, s.score, s.sessionId, s.status, s.tier, s.warmth, s.by.tier],
    Array(20).fill(null),
  );
  assert.deepEqual(s.fact, [{ entity: null, key: 'idle', unit: 'ms', value: 5 }], 'a fact whose value is neither a number nor a string is dropped');
  const e = d.decodeRoutingEvent({ account_id: null, actor: null, agent_id: null, at: 1_000, detail: null, id: 3, key_id: null, kind: 'router_started', model: null, pool_id: null, request_id: null, session_id: null, type: 'event' });
  assert.deepEqual([e.accountId, e.actor, e.agentId, e.keyId, e.model, e.poolId, e.requestId, e.sessionId, e.detail], [null, null, null, null, null, null, null, null, {}]);
  const page = d.decodeRoutingLogPage({ next_cursor: '1000:0:3', rows: [{ type: 'summary' }, { account_id: null, at: 1_000, detail: null, id: 3, kind: 'router_started', type: 'event' }, { at: 1_000, kind: 'placed', type: 'decision' }] });
  assert.deepEqual(page.rows.map((r) => r.type), ['event'], 'a row of another type, or a decision naming no request, is dropped');
  assert.equal(page.nextCursor, '1000:0:3');
  const body = fixture('decision');
  const bare = d.decodeDecisionRecord({ ...body, outcome: { ...body.outcome, completion: null, hold: null, pin_written: null }, pin: null, pin_reason: 'pinning_off', settings: null });
  assert.deepEqual([bare.pin, bare.pinReason, bare.settings, bare.outcome.completion, bare.outcome.pinWritten], [null, 'pinning_off', null, null, null]);
});

test("a refusal's record decodes its pin reason as not looked up", () => {
  const body = fixture('decision');
  const refused = { ...body, pin: null, pin_reason: 'not_looked_up', outcome: { ...body.outcome, account_id: null, completion: null, kind: 'refused', pin_events: [], pin_written: null, refusal: { kind: 'client_drift', reason: 'hash' }, rung: null } };
  const r = d.decodeDecisionRecord(refused);
  assert.deepEqual([r.pin, r.pinReason, r.outcome.kind, r.outcome.refusal], [null, 'not_looked_up', 'refused', { code: '', kind: 'client_drift', reason: 'hash' }]);
  assert.equal(d.decodeDecisionRecord({ ...refused, pin_reason: 'looked_away' }).pinReason, null, 'a word outside the list reads as none, which the raw answer still carries');
  assert.equal(d.decodeDecisionRecord(body).pinReason, null, "the fixture's placement reads its pin, so it names no reason");
  assert.deepEqual([...d.PIN_REASON_VALUES], ['not_looked_up', 'pinning_off']);
  assert.ok(d.SKIP_OBSTACLE_VALUES.includes('budget_unknown'), 'a trace names a budgeted member passed over while the ledger is behind');
});

test('request rows fold into analytics groups with the same sums the routes answer', () => {
  const page = d.decodeRequestPage(fixture('requests'));
  const [all] = d.foldGroups(page.requests, 'all');
  assert.equal(all.requests, 6);
  assert.equal(all.succeeded, 4);
  assert.equal(all.failed, 1, 'a stream that ended short is not a failure');
  assert.equal(all.incomplete, 1);
  assert.equal(all.attempts, 7);
  assert.equal(all.failovers, 1);
  assert.equal(all.tokens.cacheRead, 3000);
  assert.equal(all.histograms.ttfbMs.reduce((a, b) => a + b, 0), 6);
  assert.equal(all.cacheMissReasons.none, 5);
  assert.equal(all.cacheMissReasons.pending, 1, 'a comparison still running counts under its own word');
  assert.equal(all.probes.ok, 0);
  assert.equal(all.probes.held, 0, 'a group folded from request rows counts no held probe');
  const captured = page.requests.find((r) => r.id === 'r-captured');
  const busy = d.foldGroups([{ ...captured, usage: { ...captured.usage, webSearch: 3, webFetch: 1 } }, captured], 'all')[0].tokens;
  assert.equal(busy.webSearch, 3 + captured.usage.webSearch);
  assert.equal(busy.webFetch, 1 + captured.usage.webFetch);
  const byKey = inZone('UTC', () => d.foldGroups(page.requests, 'key'));
  assert.deepEqual(byKey.map((g) => g.label), ['desktop', 'desktop (deleted, created Oct 11, 2026, 12:15)'], 'a reused id is one group per holder, as the breakdown answers it');
  // No captured breakdown groups on effort, so the fold is where the dimension is proven.
  assert.deepEqual(d.foldGroups(page.requests, 'effort').map((g) => g.key), ['high']);
  assert.deepEqual(d.foldGroups([{ ...captured, shape: { ...captured.shape, effort: null } }], 'effort').map((g) => g.key), ['none'], 'a request that asked for no effort files under none, as the rollups file it');
  const oldest = page.requests.find((r) => r.id === 'r-failover');
  const newest = page.requests.find((r) => r.id === 'r-captured');
  const points = d.foldRequests(page.requests, 3_600_000, oldest.at, newest.at);
  assert.ok(points.length >= 1 && points.every((p) => p.groups.length === 1));
});

test('a session fold under a funding side keeps the rows that side paid for', () => {
  const plan = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-captured');
  const attempt = plan.attempts[plan.attempts.length - 1];
  const rows = [
    { ...plan, attempts: [{ ...attempt, overageInUse: true }], costShares: null, id: 'overage' },
    { ...plan, attempts: [{ ...attempt, response: { ...attempt.response, speed: 'fast' } }], costShares: null, id: 'fast' },
    { ...plan, attempts: [{ ...attempt, accountKind: 'api_key' }], costShares: null, id: 'api-key' },
    { ...plan, id: 'plan-1' },
    { ...plan, id: 'plan-2' },
  ];
  const requestsUnder = (funding) => d.foldGroups(rows, 'all', { funding }).reduce((n, g) => n + g.requests, 0);
  assert.equal(requestsUnder(undefined), 5, 'no side keeps every row');
  assert.equal(requestsUnder('billed'), 3, 'the overage, fast and API key rows are billed in dollars');
  assert.equal(requestsUnder('plan'), 2, 'the plan rows alone');
  const hourAfter = new Date(Date.parse(plan.at) + 3_600_000).toISOString();
  assert.equal(d.foldRequests(rows, 3_600_000, plan.at, hourAfter, 'all', { funding: 'plan' })[0].groups[0].requests, 2, 'the bucketed fold keeps the same side');
  const unserved = [{ ...plan, attempts: [], costShares: null, id: 'unserved' }];
  assert.deepEqual([d.foldGroups(unserved, 'all', { funding: 'plan' }).map((g) => g.requests), d.foldGroups(unserved, 'all', { funding: 'billed' }).length], [[1], 0], 'a request no attempt served falls on the plan side, as the ledger files it');
});

test("a session fold under the account grouping sums the rows' shares, leaves a window no row's model applies to absent, nulls one a row could not price and nulls the map where no share applies", () => {
  const plan = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-captured');
  const attempt = plan.attempts[plan.attempts.length - 1];
  const row = (id, over) => ({ ...plan, id, ...over });
  const fable = (id, costShares) => row(id, { canonicalModel: 'claude-fable-5-1', costShares, model: 'claude-fable-5-1' });
  const haiku = (id, costShares) => row(id, { canonicalModel: 'claude-haiku-4-5', costShares, model: 'claude-haiku-4-5' });
  const fableShares = { five_hour: 0.25, seven_day: 0.125, seven_day_fable: 0.03125 };
  const haikuShares = { five_hour: 0.5, seven_day: 0.0625 };
  const [mixed] = d.foldGroups([fable('f', fableShares), haiku('h', haikuShares)], 'account');
  assert.deepEqual(mixed.costShares, { five_hour: 0.75, seven_day: 0.1875, seven_day_fable: 0.03125 }, "the shared windows sum and the week is the Fable row's alone");
  assert.deepEqual([mixed.planRequests, mixed.planOutputTokens], [2, 2 * plan.usage.output], 'the plan rows and their output tokens');
  assert.deepEqual([mixed.cacheShares, mixed.probeShares], [null, null], 'a fold prices no cache or probe share');
  const [reversed] = d.foldGroups([haiku('h', haikuShares), fable('f', fableShares)], 'account');
  assert.deepEqual(reversed.costShares, mixed.costShares, 'in either row order');
  const [haikuOnly] = d.foldGroups([haiku('h1', haikuShares), haiku('h2', { five_hour: 0.25, seven_day: 0.0625 })], 'account');
  assert.equal('seven_day_fable' in haikuOnly.costShares, false, "a window no row's model applies to is no key at all");
  assert.deepEqual(haikuOnly.costShares, { five_hour: 0.75, seven_day: 0.125 });
  const [unpriced] = d.foldGroups([fable('f1', { ...fableShares, seven_day_fable: null }), fable('f2', fableShares)], 'account');
  assert.ok('seven_day_fable' in unpriced.costShares, 'a window the model applies to stays a key');
  assert.deepEqual(unpriced.costShares, { five_hour: 0.5, seven_day: 0.25, seven_day_fable: null }, 'with null where one row could not be priced on it, whatever the other rows priced');
  const [unknown] = d.foldGroups([fable('f', fableShares), row('u', { costShares: {} })], 'account');
  assert.equal(unknown.costShares, null, "a row whose account the route could not price nulls the group's map, as the route answers such an account");
  assert.deepEqual([unknown.cacheShares, unknown.probeShares], [null, null], 'with the other maps');
  assert.deepEqual([unknown.planRequests, unknown.planOutputTokens], [2, 2 * plan.usage.output], 'while the plan rows count, as the route counts them beside a map it cannot price');
  assert.equal(unknown.requests, 2, 'the rows still count as requests');
  const [byModel] = d.foldGroups([fable('f', fableShares)], 'model');
  assert.equal(byModel.costShares, null, "another grouping mixes accounts, whose windows' shares do not add");
  assert.deepEqual([byModel.planRequests, byModel.planOutputTokens], [1, plan.usage.output], 'and counts its plan rows, as the route counts them under every grouping');
  const [keyed] = d.foldGroups([row('k', { attempts: [{ ...attempt, accountKind: 'api_key' }], costShares: null })], 'account');
  assert.deepEqual([keyed.costShares, keyed.planRequests, keyed.planOutputTokens], [null, 0, 0], "an API key account's rows pay no window and count no plan request");
  const [overage] = d.foldGroups([fable('f', fableShares), row('o', { attempts: [{ ...attempt, overageInUse: true }], costShares: null })], 'account');
  assert.deepEqual([overage.costShares, overage.planRequests, overage.requests], [fableShares, 1, 2], 'an overage row adds to no share and to no plan count');
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
  assert.deepEqual(d.missReasonOrder([group, { cacheMissReasons: { a_reason: 1, none: 0 } }]), [...d.CACHE_MISS_REASONS, 'a_reason', 'future_reason'], 'the known nine in reading order, then the rest alphabetically');
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

test('a session fold reads a probe the router held while its ledger caught up under its held count alone', () => {
  const served = d.decodeRequest(fixture('requests').requests.find((r) => r.id === 'r-captured'));
  const held = { ...served, accountId: null, attempts: [], costUsd: 0, errorCode: 'ledger_behind', kind: 'probe', outcome: 'rejected', status: 503 };
  const [both] = d.foldGroups([served, held], 'all');
  assert.deepEqual([both.requests, both.rejected, both.attempts, both.uncached + both.cacheHits + both.cacheMisses, Object.keys(both.errors), both.probes.held], [1, 0, served.attempts.length, 1, [], 1]);
  assert.equal(both.histograms.durationMs.reduce((a, b) => a + b, 0), 1, 'nor does it join a timing');
  const [alone] = d.foldGroups([held], 'all');
  assert.deepEqual([alone.requests, alone.rejected, Object.keys(alone.errors), alone.probes.held], [0, 0, [], 1], 'a group of held probes alone holds no request, as its rollup row does');
  const [refused] = d.foldGroups([{ ...held, errorCode: 'accounts_unavailable' }], 'all');
  assert.deepEqual([refused.requests, refused.rejected, Object.keys(refused.errors), refused.probes.held], [1, 1, ['accounts_unavailable'], 0], 'a probe refused for another reason stays a request');
  assert.equal(d.foldRequests([served, held], 3_600_000, served.at, served.at)[0].groups[0].probes.held, 1, 'the bucketed fold reads it the same');
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
  assert.equal(folded.keptWarm, null);
  assert.equal(folded.cost.cacheRead, null);
  assert.equal(folded.cost.cacheWrite, null);
  const route = d.decodeAnalyticsGroup({ ...fixture('analytics_breakdown').rows[0], probe_cost_usd: 0.25, wasted_probe_usd: 0.125, wasted_probes: 1 });
  assert.equal(route.fromRequests, undefined, 'a route group carries every figure');
  const mixed = d.sumGroups([route, folded]);
  assert.equal(mixed.cost.cacheRead, null, 'a null source makes the sum unavailable, never NaN');
  assert.equal(mixed.cost.cacheWrite, null);
  assert.equal(mixed.keptWarm, null);
  assert.equal(mixed.fromRequests, true);
  assert.equal(mixed.costUsd, route.costUsd + folded.costUsd, 'the figures the rows do carry still sum');
  const routeOnly = d.sumGroups([route, route]);
  assert.equal(routeOnly.cost.cacheRead, route.cost.cacheRead * 2);
  assert.deepEqual([routeOnly.probeCostUsd, routeOnly.wastedProbeUsd, routeOnly.wastedProbes], [0.5, 0.25, 2], 'the probe figures sum');
  assert.equal(routeOnly.keptWarm.requests, route.keptWarm.requests * 2);
  assert.equal(routeOnly.fromRequests, undefined);
  const [bucket] = d.foldRequests(rows, 3_600_000, rows[rows.length - 1].at, rows[0].at);
  assert.ok(bucket.groups.every((g) => g.fromRequests === true && g.cost.cacheRead === null), 'every folded bucket marks the same gaps');
});

test('a sum of groups adds the 20x equivalents per window, the billed side and the plan counts, and carries a fold\'s gaps', () => {
  const rows = d.decodeRequestPage(fixture('requests')).requests;
  const [folded] = d.foldGroups(rows, 'all');
  const [first, second] = fixture('analytics_breakdown').rows;
  const a = d.decodeAnalyticsGroup({ ...first, estimated: false, probe_equivalent: { five_hour: { cost: 0.25, wasted: 0.125 } } });
  const b = d.decodeAnalyticsGroup({ ...second, estimated: true, probe_equivalent: { five_hour: { cost: 0.5, wasted: 0 }, seven_day: { cost: 0.05, wasted: 0 } } });
  const keys = (...maps) => [...new Set(maps.flatMap((m) => Object.keys(m)))];
  const close = (got, want, what) => assert.ok(typeof got === 'number' && Math.abs(got - want) <= Math.abs(want) * 1e-12, `${what}: ${got} against ${want}`);
  const routeOnly = d.sumGroups([a, b]);
  for (const w of keys(a.costEquivalent, b.costEquivalent)) close(routeOnly.costEquivalent[w], (a.costEquivalent[w] ?? 0) + (b.costEquivalent[w] ?? 0), `costEquivalent.${w}`);
  for (const w of keys(a.cacheEquivalent, b.cacheEquivalent)) {
    for (const half of ['read', 'write']) close(routeOnly.cacheEquivalent[w][half], (a.cacheEquivalent[w]?.[half] ?? 0) + (b.cacheEquivalent[w]?.[half] ?? 0), `cacheEquivalent.${w}.${half}`);
  }
  assert.deepEqual(routeOnly.probeEquivalent, { five_hour: { cost: 0.75, wasted: 0.125 }, seven_day: { cost: 0.05, wasted: 0 } }, 'a window any group carries, its halves added');
  close(routeOnly.billedUsd, a.billedUsd + b.billedUsd, 'billedUsd');
  for (const k of ['cacheRead', 'cacheWrite', 'input', 'output']) close(routeOnly.billed[k], a.billed[k] + b.billed[k], `billed.${k}`);
  for (const k of ['cacheRead', 'cacheWrite', 'input', 'output']) close(routeOnly.equivalentSplit[k], a.equivalentSplit[k] + b.equivalentSplit[k], `equivalentSplit.${k}`);
  assert.equal(routeOnly.estimated, true, 'one estimated group marks the sum');
  assert.equal(d.sumGroups([a, a]).estimated, false);
  assert.deepEqual([routeOnly.planRequests, routeOnly.planOutputTokens], [a.planRequests + b.planRequests, a.planOutputTokens + b.planOutputTokens], 'the plan counts sum');
  const mixed = d.sumGroups([a, b, folded]);
  assert.deepEqual([mixed.billed.cacheRead, mixed.billed.cacheWrite], [null, null], "a fold's unknown cache halves leave the billed halves unknown");
  close(mixed.billed.input, a.billed.input + b.billed.input + folded.billed.input, 'billed.input with the fold');
  assert.equal(mixed.equivalentSplit, null, 'a fold carries one total, so the split by kind is unknown');
  close(mixed.billedUsd, a.billedUsd + b.billedUsd + folded.billedUsd, 'billedUsd with the fold');
  for (const w of keys(a.costEquivalent, b.costEquivalent, folded.costEquivalent)) close(mixed.costEquivalent[w], (a.costEquivalent[w] ?? 0) + (b.costEquivalent[w] ?? 0) + (folded.costEquivalent[w] ?? 0), `costEquivalent.${w} with the fold`);
  const planRows = rows.filter((r) => d.costUnitOf(r, 'native').kind !== 'dollars');
  assert.deepEqual([folded.planRequests, folded.planOutputTokens], [planRows.length, planRows.reduce((n, r) => n + r.usage.output, 0)], 'a fold counts its plan rows under every grouping, as the route counts them');
  assert.deepEqual([mixed.planRequests, mixed.planOutputTokens], [routeOnly.planRequests + folded.planRequests, routeOnly.planOutputTokens + folded.planOutputTokens], 'and the counts sum');
  // A group the route answered no count for is skipped while another carries one.
  const uncounted = d.decodeAnalyticsGroup({ ...first, plan_output_tokens: null, plan_requests: null });
  assert.deepEqual([d.sumGroups([a, uncounted]).planRequests, d.sumGroups([a, uncounted]).planOutputTokens], [a.planRequests, a.planOutputTokens], 'a null count is skipped while another group carries one');
  assert.deepEqual([d.sumGroups([uncounted]).planRequests, d.sumGroups([uncounted]).planOutputTokens], [null, null], 'and null when every group reads null');
});

test('a sum of groups leaves out a shared window a group with plan rows lacks, so no partial sum reads as the whole', () => {
  // The ledger leaves a window out of a group's equivalents when it cannot price one of the group's plan rows there.
  const own = (o, k) => Object.prototype.hasOwnProperty.call(o, k);
  const [first, second] = fixture('analytics_breakdown').rows;
  const priced = d.decodeAnalyticsGroup(first);
  const lacking = { ...second, cache_equivalent: { five_hour: second.cache_equivalent.five_hour }, cost_equivalent: { five_hour: second.cost_equivalent.five_hour } };
  const unpriced = d.decodeAnalyticsGroup(lacking);
  assert.ok(priced.planRequests > 0 && unpriced.planRequests > 0 && unpriced.planOutputTokens > 0, 'both captured groups hold plan rows with tokens');
  const summed = d.sumGroups([priced, unpriced]);
  assert.deepEqual([own(summed.costEquivalent, 'seven_day'), own(summed.cacheEquivalent, 'seven_day')], [false, false], 'the 7-day window one plan group lacks stays out of the sum');
  assert.equal(summed.costEquivalent.five_hour, priced.costEquivalent.five_hour + unpriced.costEquivalent.five_hour, 'a window both carry still sums');
  assert.equal(d.sidesOf(summed, 'seven_day').equivalent, null, 'and the plan side reads it unpriced');
  // A group with no plan row carries no window, nor does one whose plan rows wrote no token, which the ledger prices
  // nothing on, so the sum keeps the other group's windows whole.
  for (const [what, over] of [['no plan row', { plan_output_tokens: 0, plan_requests: 0 }], ['plan rows with no token', { plan_output_tokens: 0 }]]) {
    const without = d.decodeAnalyticsGroup({ ...second, cache_equivalent: {}, cost_equivalent: {}, ...over });
    assert.deepEqual([d.sumGroups([priced, without]).costEquivalent, d.sumGroups([priced, without]).cacheEquivalent], [priced.costEquivalent, priced.cacheEquivalent], what);
  }
});

test('a session fold leaves out a shared window one of its plan rows lacks and, counting its plan rows, reads the window unpriced', () => {
  const own = (o, k) => Object.prototype.hasOwnProperty.call(o, k);
  const plan = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-captured');
  const attempt = plan.attempts[plan.attempts.length - 1];
  const [folded] = d.foldGroups([plan, { ...plan, costEquivalent: { ...plan.costEquivalent, seven_day: null }, id: 'r-lacking' }], 'key');
  assert.equal(own(folded.costEquivalent, 'seven_day'), false, 'the window one plan row could not be priced on leaves the group');
  assert.equal(folded.costEquivalent.five_hour, 2 * plan.costEquivalent.five_hour, 'a window both rows carry still sums');
  assert.deepEqual([folded.planRequests, d.sidesOf(folded, 'seven_day').equivalent], [2, null], 'and the group reads it unpriced');
  // A model of no known tier prices neither shared window: the group still counts its plan rows, so it reads the
  // window unpriced rather than as a group with no plan row.
  const [unknown] = d.foldGroups([plan, { ...plan, costEquivalent: { five_hour: null, seven_day: null }, id: 'r-unknown' }], 'key');
  assert.deepEqual([unknown.costEquivalent, d.sidesOf(unknown, 'seven_day').equivalent, d.costHeadOf('native', unknown).meta], [{}, null, '- in range']);
  // A billed row carries no plan window to lack, and a plan row with no token prices nothing, so neither leaves the fold.
  const billed = { ...plan, attempts: [{ ...attempt, accountKind: 'api_key' }], costEquivalent: null, id: 'r-billed' };
  const tokenless = { ...plan, costEquivalent: { five_hour: null, seven_day: null }, id: 'r-tokenless', usage: { ...plan.usage, cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 } };
  for (const [what, row] of [['a billed row', billed], ['a plan row with no token', tokenless]]) assert.deepEqual(d.foldGroups([plan, row], 'key')[0].costEquivalent, plan.costEquivalent, what);
});

test('a recipe evicted at the byte cap or lapsed past its entry keeps that cause, and the wasted probes their labels', () => {
  const [wire] = fixture('keepalive_recipes').recipes;
  for (const cause of ['evicted', 'expired', 'expired_at_restart']) {
    const recipe = d.decodeRecipe({ ...wire, cause, state: 'ended' });
    assert.deepEqual([recipe.cause, recipe.state], [cause, 'ended']);
  }
  const row = fixture('analytics_breakdown').rows[0];
  const group = d.decodeAnalyticsGroup({ ...row, wasted_probe_reasons: { cold_return: 3, evicted: 2, expired: 1 }, wasted_probes: 6 });
  assert.deepEqual([group.wastedProbeReasons.cold_return, group.wastedProbeReasons.evicted, group.wastedProbeReasons.expired], [3, 2, 1]);
});

test('a recipe paused because every account of its pool was exhausted keeps that cause', () => {
  const [wire] = fixture('keepalive_recipes').recipes;
  const recipe = d.decodeRecipe({ ...wire, cause: 'probe_pool_exhausted', state: 'paused' });
  assert.equal(recipe.cause, 'probe_pool_exhausted');
  assert.equal(recipe.state, 'paused');
});

test('a recipe decodes every cause word the engine serializes, its five states, the park and the day counts', () => {
  const [wire] = fixture('keepalive_recipes').recipes;
  assert.equal(d.RECIPE_CAUSE_VALUES.length, 62);
  assert.deepEqual([...d.RECIPE_CAUSE_VALUES], [...d.RECIPE_CAUSE_VALUES].sort(), 'the causes read alphabetically');
  for (const cause of d.RECIPE_CAUSE_VALUES) assert.equal(d.decodeRecipe({ ...wire, cause }).cause, cause);
  assert.deepEqual([...d.RECIPE_STATE_VALUES], ['ended', 'held', 'not_retained', 'paused', 'retained']);
  for (const state of d.RECIPE_STATE_VALUES) assert.equal(d.decodeRecipe({ ...wire, state }).state, state);
  assert.deepEqual([...d.AGENT_STATE_VALUES], ['busy', 'idle', 'parked', 'released', 'unknown'], 'the lifecycle word admits every state the engine serializes, unknown among them');
  const parked = d.decodeRecipe({ ...wire, agent_state: 'parked', park_kind: 'both', deadline: 7_200_000, wake_at: 10_800_000, last_probe_at: 3_600_000, probes_today: 4, credited_today: 2 });
  assert.deepEqual(
    [parked.agentState, parked.parkKind, parked.deadline, parked.wakeAt, parked.lastProbeAt, parked.probesToday, parked.creditedToday],
    ['parked', 'both', new Date(7_200_000).toISOString(), new Date(10_800_000).toISOString(), new Date(3_600_000).toISOString(), 4, 2],
  );
  assert.equal(d.decodeRecipe({ ...wire, agent_state: 'unknown' }).agentState, 'unknown');
  assert.equal(d.decodeRecipe({ ...wire, park_kind: 'nap' }).parkKind, null, 'a park kind outside the three reads as none');
});

test('a session row carries the presence age and the cause of its main recipe', () => {
  const row = fixture('sessions').sessions.find((s) => s.session_id === 's1');
  assert.equal(d.decodeSession({ ...row, presence_age_seconds: 12 }).presenceAgeSeconds, 12);
  assert.equal(d.decodeSession({ ...row, presence_age_seconds: null }).presenceAgeSeconds, null);
  assert.equal(d.decodeSession({ ...row, keepalive: { ...row.keepalive, cause: 'cooled' } }).keepalive.cause, 'cooled');
  assert.equal(d.decodeSession({ ...row, keepalive: { ...row.keepalive, cause: null } }).keepalive.cause, null);
});

test('a recipe held under the horizon or ended at the parked limit keeps that cause, and the wasted probes their labels', () => {
  const [wire] = fixture('keepalive_recipes').recipes;
  assert.deepEqual([d.decodeRecipe({ ...wire, cause: 'horizon', state: 'held' }).cause, d.decodeRecipe({ ...wire, cause: 'parked_limit', state: 'ended' }).cause], ['horizon', 'parked_limit']);
  const row = fixture('analytics_breakdown').rows[0];
  const group = d.decodeAnalyticsGroup({ ...row, wasted_probe_reasons: { horizon: 2, parked_limit: 1 }, wasted_probes: 3 });
  assert.deepEqual([group.wastedProbeReasons.horizon, group.wastedProbeReasons.parked_limit], [2, 1]);
});

test("the downstream snippet is the seven recommended lines, its base URL the proxy listener's scheme and port on a placeholder host", () => {
  const lines = (listeners) => d.snippetFor(listeners).split('\n');
  const rest = ['CLAUDE_CODE_OAUTH_TOKEN=<key>', 'CLAUDE_CODE_SUBSCRIPTION_TYPE=max', '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1', 'CLAUDE_CODE_GATEWAY_HINT_HEADERS=1', 'DISABLE_TELEMETRY=1', 'CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1'];
  assert.deepEqual(lines({ proxy: '127.0.0.1:8080', proxyTls: false }), ['ANTHROPIC_BASE_URL=http://router.example.com:8080', ...rest]);
  assert.equal(lines({ proxy: '0.0.0.0:8443', proxyTls: true })[0], 'ANTHROPIC_BASE_URL=https://router.example.com:8443', 'a listener that terminates TLS reads https');
  assert.equal(lines({ proxy: '[::]:8080', proxyTls: false })[0], 'ANTHROPIC_BASE_URL=http://router.example.com:8080', 'the port follows the last colon, so an IPv6 bind reads its own');
  assert.equal(lines({ proxy: '', proxyTls: false })[0], 'ANTHROPIC_BASE_URL=http://router.example.com', 'a bind with no colon names no port');
  assert.equal(lines({ proxy: '[::1]', proxyTls: true })[0], 'ANTHROPIC_BASE_URL=https://router.example.com', 'nor does a bracketed address without one');
  assert.deepEqual(lines(null), ['ANTHROPIC_BASE_URL=https://router.example.com', ...rest], 'without a settings answer the block reads the placeholder host with no port');
});

test('a session fold groups on the client each row named', () => {
  const [captured, failed] = d.decodeRequestPage(fixture('requests')).requests;
  assert.equal(captured.userAgent, 'claude-cli/2.1.269 (external, cli)', 'the row carries the agent the route answers');
  const rows = [captured, { ...failed, userAgent: 'phoebis-batch/0.9 (external, ci)' }, { ...failed, id: 'r-none', userAgent: null }];
  const groups = d.foldGroups(rows, 'user_agent');
  assert.deepEqual(groups.map((g) => g.key).sort(), ['-', 'claude-cli/2.1.269', 'phoebis-batch/0.9'], 'a row that named no client falls under the marker the rollups file it as');
  assert.equal(groups.find((g) => g.key === '-').label, 'unattributed', 'which every route answers as unattributed');
  assert.deepEqual(groups.map((g) => g.requests), [1, 1, 1]);
  const named = d.foldGroups(rows, 'user_agent', { agentGroups: ['claude-cli/2.1.*', 'phoebis-batch/*'] });
  assert.deepEqual(named.map((g) => g.key).sort(), ['claude-cli/2.1.*', 'other', 'phoebis-batch/*'], 'the row that named no client falls under other, as the breakdown route files it');
  assert.deepEqual(named.map((g) => g.requests), [1, 1, 1]);
  assert.equal(d.foldGroups(rows, 'all', { agentPatterns: ['claude-cli/*'] })[0].requests, 1, 'the pattern keeps only the clients it admits');
  assert.equal(
    d.foldGroups(rows, 'all', { agentPatterns: ['claude-cli/2.1.269'] })[0]?.requests,
    1,
    'an exact token selects the row it folds from, as the unscoped page matches the rollups',
  );
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

test('a fold assigns rows on bucket edges and outside the range by index', () => {
  const [row] = d.decodeRequestPage(fixture('requests')).requests;
  const hour = 3_600_000;
  const iso = (ms) => new Date(ms).toISOString();
  // Each row carries its own key, so a bucket's groups name the rows it holds.
  const at = (ms, keyId) => ({ ...row, at: iso(ms), keyGeneration: 1, keyId });
  const rows = [
    at(-1, 'before-first-bucket'),
    at(10 * 60_000, 'before-from'),
    at(hour, 'bucket-start'),
    at(2 * hour - 1, 'before-next-bucket'),
    at(2.5 * hour, 'at-to'),
    at(2.75 * hour, 'past-to'),
    at(3 * hour, 'after-last-bucket'),
  ];
  const points = d.foldRequests(rows, hour, iso(30 * 60_000), iso(2.5 * hour), 'key');
  assert.deepEqual(
    points.map((p) => [p.at, p.groups.map((g) => g.label).sort()]),
    [
      [iso(0), ['before-from']],
      [iso(hour), ['before-next-bucket', 'bucket-start']],
      [iso(2 * hour), ['at-to', 'past-to']],
    ],
    'the first bucket starts on the boundary below from and the last runs to its own end, as the series route buckets them',
  );
});

test('a session fold keeps two holders of one key id apart and labels the earlier one deleted', () => {
  const rows = d.decodeRequestPage(fixture('requests')).requests;
  const live = rows.find((r) => r.id === 'r-captured');
  const earlier = rows.find((r) => r.id === 'r-earlier-holder');
  const groups = inZone('UTC', () => d.foldGroups(rows, 'key'));
  assert.deepEqual(
    groups.map((g) => [g.key, g.label, g.requests]),
    [[`desktop@${live.keyGeneration}`, 'desktop', 5], [`desktop@${earlier.keyGeneration}`, 'desktop (deleted, created Oct 11, 2026, 12:15)', 1]],
    'the key a group carries is the lifetime the requests route selects by',
  );
  assert.deepEqual(d.foldGroups([{ ...live, keyGeneration: 0, keyId: null }], 'key').map((g) => [g.key, g.label]), [['-@0', 'unattributed']]);
  const points = inZone('UTC', () => d.foldRequests([{ ...earlier, at: '1970-01-01T00:10:00.000Z' }, { ...live, at: '1970-01-01T01:10:00.000Z' }], 3_600_000, '1970-01-01T00:00:00.000Z', '1970-01-01T02:00:00.000Z', 'key'));
  assert.deepEqual(points.map((p) => p.groups.map((g) => g.label)), [['desktop (deleted, created Oct 11, 2026, 12:15)'], ['desktop']], 'a bucket holding the earlier holder alone still labels it against the whole range');
});

test('a session fold labels an earlier holder at generation 0 deleted, with no creation time', () => {
  const rows = d.decodeRequestPage(fixture('requests')).requests;
  const live = rows.find((r) => r.id === 'r-captured');
  const earlier = rows.find((r) => r.id === 'r-earlier-holder');
  assert.deepEqual(
    d.foldGroups([live, { ...earlier, keyGeneration: 0 }], 'key').map((g) => [g.key, g.label]),
    [['desktop@0', 'desktop (deleted)'], [`desktop@${live.keyGeneration}`, 'desktop']],
    'a row filed before the generation column has no creation time to show',
  );
});

test('a session view folds every page the cursor names', () => {
  const page = d.decodeRequestPage(fixture('requests'));
  const pages = [{ nextCursor: 'c1', requests: page.requests.slice(0, 2) }, { nextCursor: null, requests: page.requests.slice(2) }];
  let calls = 0;
  const load = async (cursor) => { calls += 1; return cursor === undefined ? pages[0] : pages[1]; };
  return d.loadSessionRequests(load, 20).then(({ partial, requests }) => {
    assert.equal(calls, 2);
    assert.equal(requests.length, 6, 'both pages folded');
    assert.equal(partial, false);
    return d.loadSessionRequests(async () => ({ nextCursor: 'more', requests: page.requests }), 2);
  }).then(({ partial, requests }) => {
    assert.equal(partial, true, 'the page bound marks the result partial');
    assert.equal(requests.length, 12);
  });
});

test('the dashboard reads the live pin count the route answers beside the rollups', () => {
  assert.equal(fixture('dashboard').sessions.active_pins, 1);
  assert.equal(d.decodeDashboard(fixture('dashboard')).activePins, 1);
  assert.equal(d.decodeDashboard({ ...fixture('dashboard'), sessions: { active_pins: 3 } }).activePins, 3);
});

test('the dashboard reads its billed dollars, its 20x equivalents and its estimated flag for the day and per hour', () => {
  const wire = fixture('dashboard');
  const o = d.decodeDashboard(wire);
  const shared = (e) => ({ fiveHour: e.five_hour, sevenDay: e.seven_day });
  assert.deepEqual([o.billed24h, o.equivalent24h, o.estimated24h], [wire.billed_24h, shared(wire.equivalent_24h), wire.estimated_24h]);
  assert.ok(o.billed24h > 0 && o.equivalent24h.fiveHour > 0, 'the captured day spent on both sides');
  const busy = wire.hourly.findIndex((h) => h.requests > 0);
  assert.deepEqual([o.hourly[busy].billed, o.hourly[busy].equivalent], [wire.hourly[busy].billed, shared(wire.hourly[busy].equivalent)], "an hour's billed dollars and equivalents");
  assert.equal(d.decodeDashboard({ ...wire, estimated_24h: true }).estimated24h, true, 'a fallback-priced day reads estimated');
});

test('the dashboard, sessions, recipes, and overview decode', () => {
  const o = d.decodeDashboard(fixture('dashboard'));
  assert.equal(o.hourly.length, 24);
  assert.equal(o.requests24h, fixture('dashboard').requests_24h);
  const sessions = fixture('sessions').sessions.map(d.decodeSession);
  const s1 = sessions.find((s) => s.sessionId === 's1');
  assert.ok(sessions.every((s) => typeof s.keyGeneration === 'number' && typeof s.historical === 'boolean'), 'every session says which generation of its key owns it and whether that holder is gone');
  assert.equal(s1.agentId, null, 'an unpinned row names no agent');
  assert.equal(sessions.find((s) => s.sessionId === 's-fixture').agentId, 'main', 'a pinned row names the pin\'s own agent');
  assert.equal(sessions.find((s) => s.sessionId === 's0').historical, true, 'the row of an earlier holder of the same key id reads historical');
  assert.equal(s1.keyGeneration, fixture('sessions').sessions.find((s) => s.session_id === 's1').key_generation, 'the live holder names the generation its key row carries');
  assert.equal(s1.companion, 'reporting');
  assert.equal(s1.keepalive.state, 'busy');
  assert.equal(s1.keepalive.cause, 'scheduled');
  assert.equal(s1.presenceAgeSeconds, null, 'the captured mod reported no presence');
  assert.equal(s1.requests, 3);
  assert.ok(isIso(s1.lastUsedAt));
  const [recipe] = fixture('keepalive_recipes').recipes.map(d.decodeRecipe);
  assert.deepEqual([recipe.state, recipe.cause, recipe.agentState, recipe.parkKind, recipe.probesToday, recipe.creditedToday], ['retained', 'scheduled', 'busy', null, 0, 0]);
  const ov = d.decodeKeepaliveOverview(fixture('keepalive_overview'));
  assert.deepEqual(Object.keys(ov).sort(), ['agents', 'busGaps', 'expiredBetweenTurns24h', 'limits', 'pools', 'probes24h']);
  assert.equal(ov.busGaps, 0, 'the captured router saw no gap');
  assert.deepEqual(ov.limits, { agents: { count: 1, max: 1024 }, retainedBytes: { count: fixture('keepalive_overview').limits.retained_bytes.count, max: 268_435_456 }, sessions: { count: 1, max: 128 } }, 'each count beside the cap the bootstrap file sets');
  const [pool] = ov.pools;
  assert.deepEqual(
    [ov.pools.length, pool.poolId, pool.credited24h, pool.probes24h, pool.wasted24h, Object.keys(fixture('keepalive_overview').pools[0].wasted_reasons_24h).sort()],
    [1, 'main', 0, 0, 0, [...d.WASTED_PROBE_REASONS].sort()],
    'the captured pool has the idle window it opened and no ledger rows yet, and the route names every wasted reason',
  );
  assert.deepEqual(
    pool.idleReturns.map((b) => [b.fromSeconds, b.toSeconds, b.reached, b.returned]),
    [[0, 300, 1, 0], [300, 900, 0, 0], [900, 1_800, 0, 0], [1_800, 3_600, 0, 0], [3_600, 7_200, 0, 0], [7_200, 14_400, 0, 0], [14_400, 28_800, 0, 0], [28_800, 57_600, 0, 0], [57_600, 115_200, 0, 0], [115_200, null, 0, 0]],
    'ten buckets, the last open-ended',
  );
  const wire = { ...fixture('keepalive_overview'), pools: [{ ...fixture('keepalive_overview').pools[0], credited_24h: 2, wasted_reasons_24h: { horizon: 1 } }] };
  const [read] = d.decodeKeepaliveOverview(wire).pools;
  assert.deepEqual([read.credited24h, read.wastedReasons24h.horizon, read.wastedReasons24h.idle_limit], [2, 1, 0], 'a reason the answer leaves out reads zero');
  assert.deepEqual(Object.keys(ov.probes24h).sort(), ['failed', 'held', 'migrated', 'ok', 'skipped'], 'the five probe outcomes the tile draws');
  const captured = fixture('keepalive_overview');
  assert.equal(d.decodeKeepaliveOverview({ ...captured, probes_24h: { ...captured.probes_24h, held: 2 } }).probes24h.held, 2, 'the held count reads off the route');
  // The capture reads 0, which a missing field decodes to as well, so a nonzero copy proves the wire name.
  assert.equal(d.decodeKeepaliveOverview({ ...fixture('keepalive_overview'), bus_gaps: 3 }).busGaps, 3, 'the engine\'s gap count reads off the route');
});

test('a session decodes its fork', () => {
  const rows = fixture('sessions').sessions;
  const sessions = rows.map(d.decodeSession);
  const by = (id) => sessions.find((s) => s.sessionId === id);
  assert.deepEqual(
    [by('s1').forkedFrom, by('s1').forkDepth],
    ['s0', 2],
    'a session that branched from another names its base and the depth where the two part',
  );
  assert.deepEqual([by('s2').forkedFrom, by('s2').forkDepth], [null, null], 'a session that continued no other reads no fork');
  assert.deepEqual([by('s-fixture').forkedFrom, by('s-fixture').forkDepth], [null, null], 'a session the ledger holds no request for reads no fork');
});

test('a placement row and a member budget keep the figures the routes leave null as null', () => {
  const [row] = fixture('analytics_placements').rows;
  assert.equal(d.decodePlacementRow({ ...row, hit_share: null }).hitShare, null, 'a rung that placed nothing has no share, not a zero one');
  assert.equal(d.decodePlacementRow({ ...row, rung: 'by_moonlight' }).rung, 'by_moonlight', 'a rung this build does not know reads as the route sent it');
  const [member] = fixture('analytics_budgets').members;
  const unreported = d.decodeMemberBudget({ ...member, period_end: null, spent_usd: null });
  assert.deepEqual([unreported.spentUsd, unreported.periodEnd], [null, null], 'a member the ledger has not reported on has no spend yet');
  assert.equal(d.decodeMemberBudget({ ...member, budget_period: 'week' }).budgetPeriod, 'month', 'a period outside the two reads as the store default');
  assert.deepEqual(
    fixture('analytics_budgets').members.map((m) => d.decodeMemberBudget(m).kind),
    ['oauth', 'api_key'],
    "each row names its member's account kind, so the card can say a subscription member's spend is metered",
  );
  assert.equal(d.decodeMemberBudget({ ...member, kind: 'robot' }).kind, 'oauth', "a kind outside the two reads as the pool member's decoder reads it");
});

test("decodeReconciliations reads the fixture's rows and an empty list", () => {
  const body = fixture('analytics_reconciliations');
  const answer = d.decodeReconciliations(body);
  const rows = answer.reconciliations;
  assert.ok(rows.length >= 1, 'the capture holds a completed window');
  for (const [i, row] of rows.entries()) {
    assert.deepEqual(Object.keys(row).sort(), ['accountId', 'computedAt', 'estimated', 'firstReadingAt', 'lastReadingAt', 'observedRise', 'predictedShare', 'ratio', 'requests', 'window', 'windowEnd']);
    assert.deepEqual([typeof row.accountId, typeof row.window], ['string', 'string']);
    for (const key of ['estimated', 'observedRise', 'predictedShare', 'ratio', 'requests']) assert.equal(typeof row[key], 'number', key);
    for (const key of ['computedAt', 'firstReadingAt', 'lastReadingAt', 'windowEnd']) assert.ok(isIso(row[key]), `${key} reads ${row[key]}, an ISO instant`);
    assert.equal(Date.parse(row.windowEnd), body.reconciliations[i].window_end, 'the instant is the route\'s milliseconds');
  }
  assert.equal(typeof answer.requestRetentionDays, 'number');
  assert.deepEqual(d.decodeReconciliations({ reconciliations: [], request_retention_days: 30 }), { reconciliations: [], requestRetentionDays: 30 }, 'an empty list decodes to none');
});

test('settings decode the members the route answers and nothing more', () => {
  const wire = fixture('settings');
  const s = d.decodeSettings(wire);
  assert.deepEqual(
    Object.keys(s).sort(),
    ['about', 'access', 'apiPrices', 'listeners', 'retention', 'sessions', 'storage', 'subscriptionCosts'],
    'the decoder reads the members the route answers',
  );
  const members = Object.fromEntries(Object.entries(s).map(([name, body]) => [name, Object.keys(body).sort()]));
  assert.deepEqual(
    members,
    {
      about: ['profile', 'version'],
      access: ['adminConfigured'],
      apiPrices: ['effective', 'effectiveDate', 'models', 'revisions', 'source'],
      listeners: ['metrics', 'proxy', 'proxyTls'],
      retention: ['bodyCaptureMaxHours', 'retentionDays', 'rollupRetentionDays'],
      sessions: ['expireAfterSeconds'],
      storage: ['apiPricesPath', 'ledgerPath', 'path', 'subscriptionCostsPath'],
      subscriptionCosts: ['capacity', 'effective', 'modelTier', 'modelWeight', 'modelWindows', 'overrides', 'peak', 'problem', 'regimes', 'revisions', 'source', 'tierOrder', 'typeMultiplier', 'unit'],
    },
    'each member carries what the pages read and no other key',
  );
  assert.deepEqual(s.listeners, { metrics: wire.listeners.metrics, proxy: wire.listeners.proxy, proxyTls: wire.listeners.proxy_tls });
  assert.equal(d.decodeSettings({ ...wire, listeners: { metrics: wire.listeners.metrics, proxy: wire.listeners.proxy } }).listeners.proxyTls, false, 'an answer without proxy_tls reads a listener that terminates no TLS');
  assert.equal(d.decodeSettings({ ...wire, listeners: { ...wire.listeners, proxy_tls: true } }).listeners.proxyTls, true);
  assert.deepEqual(s.retention, { bodyCaptureMaxHours: wire.retention.body_capture_max_hours, retentionDays: wire.retention.retention_days, rollupRetentionDays: wire.retention.rollup_retention_days });
  assert.deepEqual(
    d.decodeSettings({ ...wire, retention: { body_capture_max_hours: wire.retention.body_capture_max_hours } }).retention,
    { bodyCaptureMaxHours: wire.retention.body_capture_max_hours, retentionDays: 0, rollupRetentionDays: 0 },
    'an answer without the two day counts reads 0 on each',
  );
  assert.deepEqual(s.storage, { apiPricesPath: null, ledgerPath: wire.storage.ledger_path, path: wire.storage.path, subscriptionCostsPath: null }, 'no override file is a null path, never an empty one');
  const configured = d.decodeSettings({ ...wire, storage: { ...wire.storage, api_prices_path: './api_prices.yaml', subscription_costs_path: './subscription_costs.yaml' } }).storage;
  assert.deepEqual([configured.apiPricesPath, configured.subscriptionCostsPath], ['./api_prices.yaml', './subscription_costs.yaml']);
});

test('a settings member the answer leaves out decodes to its default', () => {
  const wire = fixture('settings');
  const without = (name) => {
    const bare = { ...wire };
    delete bare[name];
    return d.decodeSettings(bare)[name];
  };
  assert.deepEqual(without('about'), { profile: '', version: '' });
  assert.deepEqual(without('access'), { adminConfigured: true }, 'no session reaches the console without the admin secret');
  assert.deepEqual(without('listeners'), { metrics: '', proxy: '', proxyTls: false });
  assert.deepEqual(without('retention'), { bodyCaptureMaxHours: 0, retentionDays: 0, rollupRetentionDays: 0 });
  assert.deepEqual(without('sessions'), { expireAfterSeconds: 0 });
  assert.deepEqual(without('storage'), { apiPricesPath: null, ledgerPath: '', path: '', subscriptionCostsPath: null });
  const bare = { ...wire };
  delete bare.subscription_costs;
  assert.deepEqual(
    d.decodeSettings(bare).subscriptionCosts,
    {
      capacity: {},
      effective: '',
      modelTier: {},
      modelWeight: {},
      modelWindows: {},
      overrides: {},
      peak: { capacityFactor: { fiveHour: 1, sevenDay: 1 }, fromHour: 0, toHour: 0, weekdays: false, zone: '' },
      problem: null,
      regimes: {},
      revisions: [],
      source: '',
      tierOrder: [],
      typeMultiplier: { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 },
      unit: { definedBy: { band: '', tier: '', window: '' }, unitsPerWindow: 0 },
    },
    'an answer without the cost model reads as an empty one, with no capacity, no weight, no model tier and no problem',
  );
});

test('settings decode the subscription cost model from its envelope', () => {
  const wire = fixture('settings');
  const s = d.decodeSettings(wire);
  const costs = s.subscriptionCosts;
  const content = wire.subscription_costs.content;
  assert.equal(costs.source, wire.subscription_costs.source);
  assert.equal(costs.effective, content.effective);
  assert.deepEqual(costs.unit, { definedBy: { band: content.unit.defined_by.band, tier: content.unit.defined_by.tier, window: content.unit.defined_by.window }, unitsPerWindow: content.unit.units_per_window });
  assert.equal(costs.capacity.default_claude_ai.fiveHour, content.capacity.default_claude_ai.five_hour, "Pro's 5-hour capacity under the key the profile reports");
  assert.deepEqual(Object.keys(costs.capacity), Object.keys(content.capacity), "the tiers in the file's order");
  for (const [tier, row] of Object.entries(content.capacity)) assert.deepEqual(costs.capacity[tier], { fiveHour: row.five_hour, sevenDay: row.seven_day }, tier);
  assert.deepEqual(Object.keys(costs.modelWeight), Object.keys(content.model_weight), "the weights in the file's order");
  assert.equal(costs.modelWeight['claude-opus-5-5'], content.model_weight['claude-opus-5-5']);
  const peak = content.peak;
  assert.deepEqual(costs.peak, { capacityFactor: { fiveHour: peak.capacity_factor.five_hour, sevenDay: peak.capacity_factor.seven_day }, fromHour: peak.from_hour, toHour: peak.to_hour, weekdays: peak.weekdays, zone: peak.zone });
  const m = content.type_multiplier;
  assert.deepEqual(costs.typeMultiplier, { cacheRead: m.cache_read, cacheWrite1h: m.cache_write_1h, cacheWrite5m: m.cache_write_5m, input: m.input, output: m.output });
  const fable = content.model_windows.seven_day_fable;
  assert.deepEqual(costs.modelWindows.seven_day_fable, { capacityFactor: fable.capacity_factor, of: fable.of });
  assert.deepEqual(Object.keys(costs.overrides), Object.keys(content.overrides));
  assert.deepEqual(costs.tierOrder, content.tier_order, 'the model tiers in display order');
  assert.deepEqual(costs.modelTier, content.model_tier, "each model's tier");
  assert.ok(costs.tierOrder.length > 0 && Object.keys(costs.modelTier).length > 0, 'the shipped model names its tiers');
  assert.deepEqual(costs.revisions.map((r) => r.effective), wire.subscription_costs.revisions.map((r) => r.effective), 'every revision of the cost model, in the order sent');
  assert.deepEqual(costs.revisions.map((r) => r.content.modelWeight), wire.subscription_costs.revisions.map((r) => r.content.model_weight), "each revision's own content");
  assert.deepEqual(s.apiPrices.models.map((p) => p.model), Object.keys(wire.api_prices.content.models), "the price rows in the file's order");
  assert.deepEqual([s.apiPrices.effective, s.apiPrices.source], [wire.api_prices.content.effective, wire.api_prices.source]);
  assert.deepEqual(s.apiPrices.revisions.map((r) => r.effective), wire.api_prices.revisions.map((r) => r.effective), 'every revision of the price table, in the order sent');
  assert.deepEqual(s.apiPrices.revisions.map((r) => r.models.map((m) => m.model)), wire.api_prices.revisions.map((r) => Object.keys(r.content.models)), "each revision's own rows");
  const bare = { ...wire };
  delete bare.subscription_costs;
  assert.deepEqual(Object.keys(d.decodeSettings(bare).subscriptionCosts.capacity), [], 'an answer without the cost model sizes no tier');
});

test('the cost model decodes an override with the members it names alone, and a null weight and capacity as null', () => {
  const content = JSON.parse(readFileSync(new URL('./fixtures/test_tables.json', import.meta.url), 'utf8')).body.subscription_costs.content;
  const costs = d.decodeSubscriptionCosts(content);
  const haiku = content.overrides['claude-haiku-4-5'];
  assert.deepEqual(costs.overrides['claude-haiku-4-5'], { typeMultiplier: { cacheRead: haiku.type_multiplier.cache_read }, weightByWindow: haiku.weight_by_window });
  assert.deepEqual(d.decodeSubscriptionCosts({ ...content, overrides: { 'claude-haiku-4-5': { weight_by_window: haiku.weight_by_window } } }).overrides['claude-haiku-4-5'], { weightByWindow: haiku.weight_by_window }, 'an override without multipliers carries none');
  assert.equal(costs.modelWeight['claude-opus-5'], content.model_weight['claude-opus-5']);
  assert.equal(costs.modelWeight['claude-opus-5'], null);
  assert.equal(costs.capacity.default_claude_team.sevenDay, null);
});

test('a cost model answer with a member missing or of the wrong type reads as the empty model and says so', () => {
  const wire = fixture('settings');
  const envelope = JSON.parse(readFileSync(new URL('./fixtures/test_tables.json', import.meta.url), 'utf8')).body.subscription_costs;
  const decoded = (edit) => {
    const content = structuredClone(envelope.content);
    return d.decodeSettings({ ...wire, subscription_costs: { ...envelope, content: edit(content) ?? content } }).subscriptionCosts;
  };
  const unreadable = {
    capacity: {},
    effective: '',
    modelTier: {},
    modelWeight: {},
    modelWindows: {},
    overrides: {},
    peak: { capacityFactor: { fiveHour: 1, sevenDay: 1 }, fromHour: 0, toHour: 0, weekdays: false, zone: '' },
    problem: "The cost model's answer could not be read.",
    regimes: {},
    revisions: [],
    source: envelope.source,
    tierOrder: [],
    typeMultiplier: { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 },
    unit: { definedBy: { band: '', tier: '', window: '' }, unitsPerWindow: 0 },
  };
  // The input multiplier would price at 0, and the band's factor at 1, a share half the one the router charges.
  assert.deepEqual(decoded((c) => { c.type_multiplier.input = 'bad'; }), unreadable, 'a multiplier that is not a number prices nothing');
  assert.deepEqual(decoded((c) => { c.peak.capacity_factor.five_hour = '0.5'; }), unreadable, 'a factor sent as a string prices nothing');
  const malformed = {
    'a capacity left out': (c) => { delete c.capacity.default_claude_ai.five_hour; },
    'a model window factor left out': (c) => { delete c.model_windows.seven_day_fable.capacity_factor; },
    "a regime's band factor left out": (c) => { delete c.regimes.desk.peak.capacity_factor; },
    "a regime's entrypoints left out": (c) => { delete c.regimes.desk.entrypoints; },
    'a weekday flag sent as a string': (c) => { c.peak.weekdays = 'true'; },
    'a weight map sent as a list': (c) => { c.model_weight = []; },
    'a weight that is neither a number nor null': (c) => { c.model_weight['claude-opus-5-5'] = '2'; },
    'a window weight that is null': (c) => { c.overrides['claude-haiku-4-5'].weight_by_window.seven_day = null; },
    'a zone that is not a string': (c) => { c.peak.zone = 5; },
    'an override multiplier sent as a string': (c) => { c.overrides['claude-haiku-4-5'].type_multiplier.cache_read = '0.2'; },
    'a model tier map with a tier that is a number': (c) => { c.model_tier['claude-opus-5-5'] = 3; },
    'no model tier order': (c) => { delete c.tier_order; },
  };
  for (const [what, edit] of Object.entries(malformed)) assert.deepEqual(decoded(edit), unreadable, what);
  const revision = (edit) => {
    const sent = structuredClone(envelope);
    edit(sent.revisions[1].content);
    return d.decodeSettings({ ...wire, subscription_costs: sent }).subscriptionCosts;
  };
  assert.deepEqual(revision((c) => { delete c.tier_order; }), unreadable, 'one unreadable revision makes the envelope unreadable');
  assert.deepEqual(revision(() => undefined).revisions.map((r) => r.effective), envelope.revisions.map((r) => r.effective), 'every revision reads when each does');
  assert.deepEqual(d.decodeSettings({ ...wire, subscription_costs: { ...envelope, content: undefined } }).subscriptionCosts, unreadable, 'an envelope without its content');
  assert.deepEqual(d.decodeSettings({ ...wire, subscription_costs: null }).subscriptionCosts, { ...unreadable, source: '' }, 'an envelope that is not an object');
  const read = decoded(() => undefined);
  assert.equal(read.problem, null, 'the test tables read with no problem');
  assert.equal(read.modelWeight['claude-opus-5'], null, 'a null weight still decodes');
  const weightByWindow = envelope.content.overrides['claude-haiku-4-5'].weight_by_window;
  assert.deepEqual(decoded((c) => { c.overrides['claude-haiku-4-5'].type_multiplier.cache_read = null; }).overrides['claude-haiku-4-5'], { typeMultiplier: {}, weightByWindow }, 'a null override multiplier keeps the shared one');
  assert.deepEqual(decoded((c) => { c.overrides['claude-haiku-4-5'].type_multiplier = null; }).overrides['claude-haiku-4-5'], { weightByWindow }, 'a null override block names nothing');
});

test('the cost model decodes its regimes strictly and the empty model carries none', () => {
  const content = fixture('pricing').subscription_costs.content;
  const costs = d.decodeSubscriptionCosts(content);
  assert.ok(Object.keys(content.regimes).length > 0, 'the fixture carries a regime');
  assert.deepEqual(Object.keys(costs.regimes), Object.keys(content.regimes), "the regimes in the file's order");
  for (const [name, regime] of Object.entries(content.regimes)) {
    const factor = regime.peak.capacity_factor;
    assert.deepEqual(
      costs.regimes[name],
      {
        capacity: Object.fromEntries(Object.entries(regime.capacity).map(([tier, row]) => [tier, { fiveHour: row.five_hour, sevenDay: row.seven_day }])),
        entrypoints: regime.entrypoints,
        peakCapacityFactor: { fiveHour: factor.five_hour, sevenDay: factor.seven_day },
      },
      name,
    );
    assert.deepEqual(Object.keys(costs.regimes[name].capacity), Object.keys(regime.capacity), `${name}'s tiers in the file's order`);
  }
  const empty = d.decodeSubscriptionCosts(undefined);
  assert.deepEqual(empty.regimes, {}, 'the empty model carries no regime');
  const name = Object.keys(content.regimes)[0];
  const refused = (edit) => {
    const c = structuredClone(content);
    edit(c.regimes[name]);
    return d.decodeSubscriptionCosts(c);
  };
  assert.deepEqual(refused((r) => { delete r.entrypoints; }), empty, 'a regime without its entrypoints reads as the empty model');
  assert.deepEqual(refused((r) => { r.entrypoints = [...r.entrypoints, 5]; }), empty, 'an entrypoint that is not a string');
  assert.deepEqual(refused((r) => { delete r.peak.capacity_factor; }), empty, 'a regime without its band factors');
  assert.deepEqual(refused((r) => { r.capacity = []; }), empty, 'a capacity map sent as a list');
});

test('a pricing effective date that is not a date decodes as the string sent with no parsed date', () => {
  const wire = fixture('settings');
  const pricingOf = (effective) => d.decodeSettings({ ...wire, api_prices: { ...wire.api_prices, content: { ...wire.api_prices.content, effective } } }).apiPrices;
  // V8 reads some free text as a date ("version 3" as March 2001), so only text that opens with an ISO date counts.
  for (const effective of ['v3', '', 'Q3 2026', 'version 3', 'rev 2']) {
    const pricing = pricingOf(effective);
    assert.deepEqual([pricing.effective, pricing.effectiveDate], [effective, null], `${JSON.stringify(effective)} is kept as sent`);
    assert.equal(pricing.models.length, Object.keys(wire.api_prices.content.models).length, 'the table beside it still decodes');
  }
  const dated = pricingOf('2026-10-01');
  assert.deepEqual([dated.effective, dated.effectiveDate], ['2026-10-01', '2026-10-01T00:00:00.000Z'], 'a date keeps its text and parses');
  // The revisions read as leniently as the entry in force: free text is kept, and the rest of the list still reads.
  const [first, ...rest] = wire.api_prices.revisions;
  const listed = d.decodeSettings({ ...wire, api_prices: { ...wire.api_prices, revisions: [{ ...first, effective: 'v3', content: { ...first.content, effective: 'v3' } }, ...rest] } }).apiPrices.revisions;
  assert.deepEqual([listed[0].effective, listed[0].effectiveDate], ['v3', null], "a revision's free-text date is kept as sent");
  assert.equal(listed[0].models.length, Object.keys(first.content.models).length, 'its rows still decode');
  assert.deepEqual(listed.slice(1).map((r) => r.effectiveDate), rest.map((r) => `${r.effective}T00:00:00.000Z`), 'the dated revisions parse');
  assert.deepEqual(d.decodeSettings({ ...wire, api_prices: { ...wire.api_prices, revisions: 'none' } }).apiPrices.revisions, [], 'a list the answer garbles reads as none');
});

test("a key group's label is built from the holder's id and creation instant, in the viewer's zone", () => {
  const row = fixture('analytics_breakdown').rows.find((r) => r.key === 'desktop@1791720940000');
  const earlier = inZone('America/Los_Angeles', () => d.decodeAnalyticsGroup(row));
  assert.equal(earlier.label, 'desktop (deleted, created Oct 11, 2026, 05:15)', "the route's label is UTC and the console prints the viewer's zone");
  assert.equal(earlier.keyId, 'desktop');
  assert.equal(earlier.keyCreatedAt, 1_791_720_940_000);
  const unstamped = d.decodeAnalyticsGroup({ ...row, key: 'desktop@0', key_created_at: 0, label: 'desktop (deleted)' });
  assert.equal(unstamped.label, 'desktop (deleted)', 'generation 0 carries no creation time');
  assert.equal(unstamped.keyCreatedAt, 0);
  const { key_created_at: _generation, ...liveWire } = row;
  const live = d.decodeAnalyticsGroup({ ...liveWire, key: 'desktop', label: 'desktop' });
  assert.equal(live.label, 'desktop', 'a live key reads its id');
  assert.equal(live.keyId, 'desktop');
  assert.equal(live.keyCreatedAt, null);
  const reserved = d.decodeAnalyticsGroup({ ...liveWire, key: '-', key_id: '-', label: 'unattributed' });
  assert.equal(reserved.label, 'unattributed', "the reserved id keeps the route's label, which no id would build");
  assert.equal(reserved.keyId, '-');
  assert.equal(reserved.keyCreatedAt, null);
  const { key_id: _id, ...other } = liveWire;
  const model = d.decodeAnalyticsGroup({ ...other, key: 'claude-opus-4-1', label: 'claude-opus-4-1' });
  assert.equal(model.label, 'claude-opus-4-1', 'another dimension keeps its label');
  assert.equal(model.keyId, null);
  assert.equal(model.keyCreatedAt, null);
});

test("a table's effective date reads as the calendar date the file names in every zone, and free text as sent", () => {
  // A date-only effective parses to midnight UTC, which a zone west of UTC reads as the evening before; the case sets
  // such a zone itself, so it holds on a host in any zone.
  const zone = process.env.TZ;
  process.env.TZ = 'America/Los_Angeles';
  try {
    assert.equal(d.effectiveLabel('2026-10-01'), 'Oct 01, 2026');
    assert.equal(d.effectiveLabel('2026-10-01T23:00'), 'Oct 01, 2026', "a date with a time reads in the browser's zone");
    assert.equal(d.effectiveLabel('v3'), 'v3');
    // Each revision's date is its UTC midnight, so the label reads the calendar date the file names here too.
    const { apiPrices, subscriptionCosts } = d.decodeSettings(fixture('settings'));
    for (const r of [...apiPrices.revisions, ...subscriptionCosts.revisions]) {
      assert.equal(r.effectiveDate, `${r.effective}T00:00:00.000Z`, `${r.effective} parses to its midnight`);
      assert.equal(d.effectiveLabel(r.effective), new Date(r.effectiveDate).toLocaleDateString('en-US', { day: '2-digit', month: 'short', timeZone: 'UTC', year: 'numeric' }), `${r.effective} reads its calendar date`);
    }
  } finally {
    if (zone === undefined) delete process.env.TZ;
    else process.env.TZ = zone;
  }
});

test('an account patch encodes the cancelled flag and its end', () => {
  const end = '2026-10-19T09:15:00.000Z';
  assert.deepEqual(d.encodeAccountPatch({ cancelled: true, subscriptionEndsAt: end }), { cancelled: true, subscription_ends_at: Date.parse(end) }, 'the route takes milliseconds');
  assert.deepEqual(d.encodeAccountPatch({ cancelled: false, subscriptionEndsAt: null }), { cancelled: false, subscription_ends_at: null }, 'a cleared end is sent as null, not left out');
});

test('settings, pricing, pairing, quota, and usage decode', () => {
  const wire = fixture('settings');
  const s = d.decodeSettings(wire);
  assert.equal(s.sessions.expireAfterSeconds, 86400);
  assert.equal(s.listeners.metrics, '127.0.0.1:9464');
  const rows = wire.api_prices.content.models;
  assert.equal(s.apiPrices.models.find((m) => m.model === 'claude-sonnet-5').inputPerM, rows['claude-sonnet-5'].input);
  const opus = s.apiPrices.models.find((m) => m.model === 'claude-opus-5-5');
  assert.deepEqual([opus.cacheWrite1hPerM, opus.fast.outputPerM], [rows['claude-opus-5-5'].cache_write_1h, rows['claude-opus-5-5'].fast.output], "a cell and a fast cell read under the file's names");
  const plainRow = { input: 1, cache_write_5m: 1.25, cache_write_1h: 2, cache_read: 0.1, output: 5 };
  const longContextRow = { ...plainRow, long_context: { over: 1000, input: 5, cache_write_5m: 6.25, cache_write_1h: 10, cache_read: 0.5, output: 25 } };
  const withLongContext = d.decodeSettings({ ...wire, api_prices: { ...wire.api_prices, content: { ...wire.api_prices.content, models: { ...rows, 'claude-haiku-5-5': longContextRow, 'claude-test-plain': plainRow } } } });
  const haiku = withLongContext.apiPrices.models.find((m) => m.model === 'claude-haiku-5-5');
  assert.deepEqual(haiku.longContext, { cacheReadPerM: 0.5, cacheWrite1hPerM: 10, cacheWrite5mPerM: 6.25, inputPerM: 5, outputPerM: 25, over: 1000 }, "a long-context set reads its threshold and its cells under the file's names");
  assert.equal(haiku.fast, null);
  assert.equal(withLongContext.apiPrices.models.find((m) => m.model === 'claude-test-plain').longContext, null, 'a row without the block reads null');
  const pairing = d.decodePairingSession(fixture('pairing_started'));
  assert.ok(pairing.id && pairing.state && pairing.authorizeUrl.includes('state=') && isIso(pairing.expiresAt));
  const history = d.decodeQuotaHistory(fixture('account_quota'));
  assert.deepEqual(history.map((h) => h.windowId), ['5h', '7d', '7d_fable', 'seven_day_opus']);
  assert.ok(history.every((h) => h.samples.every((x) => isIso(x.at))));
  const snapshot = d.decodeSnapshot('business', fixture('account_usage'));
  assert.equal(snapshot.status, 'success');
  const at = new Date(fixture('account_usage').poll.last_attempt_at).toISOString();
  assert.deepEqual(snapshot.poll, { backoffSince: null, lastAttemptAt: at, lastCleanAt: at, lastError: null, lastOutcome: 'clean', nextAttemptAt: null, rateLimitedInARow: 0 }, 'a clean poll');
  const hold = 'the usage endpoint rate-limited the account';
  const held = d.decodeSnapshot('mail', { error: 'usage request rate-limited', fetched_at: 1_000, held_until: 240_000, poll: { backoff_since: 60_000, last_attempt_at: 120_000, last_clean_at: 1_000, last_error: hold, last_outcome: 'rate_limited', next_attempt_at: 240_000, rate_limited_in_a_row: 2 } });
  assert.deepEqual(
    [held.status, held.heldUntil, held.poll],
    ['error', '1970-01-01T00:04:00.000Z', { backoffSince: '1970-01-01T00:01:00.000Z', lastAttemptAt: '1970-01-01T00:02:00.000Z', lastCleanAt: '1970-01-01T00:00:01.000Z', lastError: hold, lastOutcome: 'rate_limited', nextAttemptAt: '1970-01-01T00:04:00.000Z', rateLimitedInARow: 2 }],
    'a poll backing off keeps the hold deadline, each instant, the count and the last error',
  );
  assert.equal(d.decodeSnapshot('spare', { error: 'the poll has not read this account yet', fetched_at: null, poll: null }).poll, null, 'no poll before the first ask');
  assert.equal(d.decodeSnapshot('a', { poll: { last_outcome: 'somewhere_new' } }).poll.lastOutcome, 'failed', 'an outcome this build does not know reads as a failure');
  assert.deepEqual(
    d.decodeCatalogModel({ id: 'claude-opus-5', display_name: 'Opus', created_at: '2026-01-01', max_input_tokens: 1_000_000, max_tokens: 128_000, effort_levels: ['low', 'high', 7] }),
    { id: 'claude-opus-5', displayName: 'Opus', createdAt: '2026-01-01', effortLevels: ['low', 'high'], maxInputTokens: 1_000_000, maxTokens: 128_000 },
    'a catalog row decodes with its two numbers and its effort levels, an entry that is not a name dropped',
  );
  assert.deepEqual(d.decodeCatalogModel({ id: 'claude-haiku-4-5' }).effortLevels, [], 'a row without effort levels reads an empty list');
  const catalog = d.decodeAccountCatalog(fixture('account_catalog'));
  assert.deepEqual(catalog.models[0], { id: 'claude-sonnet-4-5', displayName: '', createdAt: '', effortLevels: ['low', 'medium', 'high'], maxInputTokens: null, maxTokens: null }, 'and without the numbers');
  assert.equal(catalog.error, null);
  assert.ok(isIso(catalog.fetchedAt));
});

test("a pairing's confirm decodes the account, the status and whether its first poll is in flight", () => {
  assert.deepEqual(d.decodePairingConfirmed({ account_id: 'business', first_poll_in_flight: true, status: 'confirmed' }), { accountId: 'business', firstPollInFlight: true, status: 'confirmed' });
  assert.equal(d.decodePairingConfirmed({ account_id: 'business', first_poll_in_flight: false, status: 'confirmed' }).firstPollInFlight, false, 'the member is read, never assumed');
});

test('a session decodes its burn unit, and a pin the cost model cannot price reads no burn', () => {
  const rows = fixture('sessions').sessions;
  const wire = rows.find((s) => s.session_id === 's-fixture');
  assert.equal(wire.model, 'claude-sonnet-5', 'the fixture pins Sonnet 5');
  const pinned = d.decodeSession(wire);
  assert.deepEqual([pinned.burnUnit, pinned.burn], ['window_share', 0], 'a pin never renewed with usage burns nothing, whatever its model');
  assert.deepEqual([d.decodeSession({ ...wire, burn: null }).burnUnit, d.decodeSession({ ...wire, burn: null }).burn], ['window_share', null], 'a burn the cost model cannot price reads null beside its unit');
  assert.deepEqual([pinned.burnEquivalent, pinned.estimated], [wire.burn_equivalent, wire.estimated], "the pin's burn as a 20x equivalent and its estimated flag");
  assert.equal(typeof pinned.burnEquivalent, 'number', 'a plan pin carries one');
  assert.deepEqual([d.decodeSession({ ...wire, burn_equivalent: 0.25, estimated: true }).burnEquivalent, d.decodeSession({ ...wire, burn_equivalent: 0.25, estimated: true }).estimated], [0.25, true]);
  const unpinned = rows.find((s) => s.burn === null);
  assert.equal(d.decodeSession(unpinned).burnEquivalent, null, 'a session with no pin burns nothing');
  assert.equal(d.decodeSession({ ...rows[0], burn_unit: 'tokens' }).burnUnit, null, 'a unit outside the two reads as none');
  assert.deepEqual(Object.keys(d.decodeSession(rows[0]).keepalive).sort(), ['cause', 'lastGuard', 'nextProbeAt', 'state'], 'the keepalive summary carries no balance');
  assert.equal(Object.keys(d.decodeRecipe(fixture('keepalive_recipes').recipes[0])).some((k) => k.startsWith('net')), false, 'nor does a recipe');
});

test("an attention item decodes the route's shape, its start an instant and its detail passed through", () => {
  const [raw] = fixture('attention').items;
  assert.deepEqual(d.decodeAttention(raw), {
    detail: { key: 'desktop', pool: 'main' },
    kind: 'pool_exhausted',
    link: { id: 'main', page: 'pools' },
    pools: ['main'],
    since: new Date(raw.since).toISOString(),
    subject: { id: 'main', label: 'Main', type: 'pool' },
  });
  const drift = d.decodeAttention({ ...raw, detail: { client_version: '2.1.271', key: null, trigger: 'hash' }, kind: 'client_drift', link: null, pools: [], subject: { id: '2.1.271', label: '2.1.271', type: 'client' } });
  assert.deepEqual([drift.kind, drift.link, drift.pools, drift.subject.type], ['client_drift', null, [], 'client'], 'a drift refusal that carried no key links nowhere');
});

test("an attention item of a state writer or a parked record decodes under its own kind, and a router subject as itself", () => {
  const parked = fixture('attention').items.find((i) => i.kind === 'ledger_record_parked');
  assert.deepEqual(d.decodeAttention(parked), {
    detail: { accounts: ['business'], count: 1, pool: 'main' },
    kind: 'ledger_record_parked',
    link: { id: 'main', page: 'pools' },
    pools: ['main'],
    since: new Date(parked.since).toISOString(),
    subject: { id: 'main', label: 'Main', type: 'pool' },
  });
  const writer = d.decodeAttention({ detail: { component: 'routing' }, kind: 'writer_degraded', link: null, pools: [], since: 1_000_000, subject: { id: 'routing', label: 'routing', type: 'router' } });
  assert.deepEqual([writer.kind, writer.link, writer.pools, writer.subject], ['writer_degraded', null, [], { id: 'routing', label: 'routing', type: 'router' }], "the router's own component opens nothing and belongs to no pool");
});

test("an unweighted model's attention item decodes under its own kind, a model subject opening the Models page, read off the route's capture", () => {
  const raw = fixture('attention').items.find((i) => i.kind === 'model_unweighted');
  assert.ok(raw, 'the capture holds the unweighted model item');
  assert.deepEqual(d.decodeAttention(raw), {
    detail: raw.detail,
    kind: 'model_unweighted',
    link: { id: raw.link.id, page: 'models' },
    pools: [],
    since: new Date(raw.since).toISOString(),
    subject: { id: raw.subject.id, label: raw.subject.label, type: 'model' },
  });
  assert.deepEqual(Object.keys(raw.detail).sort(), ['model', 'requests', 'tier', 'weight_of'], 'the detail names the model, its plan requests, its model tier and the model whose weight prices it');
});

test('an attention item with a spelling the console does not know reads the worst kind, a pool subject and no link', () => {
  const odd = d.decodeAttention({ detail: 'text', kind: 'cache_cold', link: { id: 'r1', page: 'settings' }, pools: ['all', 3], since: 'yesterday', subject: { id: 'x', label: 'x', type: 'key' } });
  assert.deepEqual(
    [odd.kind, odd.subject.type, odd.link, odd.pools, odd.since, odd.detail],
    ['pool_exhausted', 'pool', null, ['all'], '1970-01-01T00:00:00.000Z', {}],
  );
  assert.equal(d.decodeAttention({ ...fixture('attention').items[0], link: { id: 'main' } }).link, null, 'a link that names no page opens nothing');
});
