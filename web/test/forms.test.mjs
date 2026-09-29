import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;

const pool = d.decodePool(fixture('pools').pools[0]);
const accounts = new Set(pool.members.map((m) => m.accountId));
// The captured pool holds the store's defaults, the balanced presets, for every engine field.
const input = {
  affinityMode: pool.affinityMode,
  affinityTtlSeconds: pool.affinityTtlSeconds,
  bodyCaptureHours: pool.bodyCaptureHours,
  burnWindowSeconds: pool.burnWindowSeconds,
  cacheDiagnostics: pool.cacheDiagnostics,
  cacheTtl: pool.cacheTtl,
  inFlightCap: pool.inFlightCap,
  keepalive: pool.keepalive,
  members: pool.members.map((m) => ({ accountId: m.accountId, budgetPeriod: m.budgetPeriod, budgetUsd: m.budgetUsd, priority: m.priority })),
  migrateAboveUtilization: pool.migrateAboveUtilization,
  migrateWhenSpent: pool.migrateWhenSpent,
  minSharedPrefixTokens: pool.minSharedPrefixTokens,
  modelRules: pool.modelRules,
  name: pool.name,
  overageMoveGraceMinutes: pool.overageMoveGraceMinutes,
  overagePolicy: pool.overagePolicy,
  paceFloor: pool.paceFloor,
  preferSubscription: pool.preferSubscription,
  quotaHeaders: pool.quotaHeaders,
  retryInPlaceSeconds: pool.retryInPlaceSeconds,
  selectBelowUtilization: pool.selectBelowUtilization,
  sessionPlacement: pool.sessionPlacement,
  shedShare: pool.shedShare,
  sourceDwellSeconds: pool.sourceDwellSeconds,
  status: 'active',
  strategy: pool.strategy,
  subagentPlacement: pool.subagentPlacement,
  warmSessions: pool.warmSessions,
  windDownMinutes: pool.windDownMinutes,
};
const tuned = (patch) => ({ ...input, keepalive: { ...input.keepalive, ...patch } });

test('a pool input the router would take validates', () => {
  assert.equal(d.validatePoolInput(input, accounts), true);
  assert.equal(d.validatePoolInput({ ...input, affinityTtlSeconds: 0, keepalive: { ...input.keepalive, enabled: false } }, accounts), true, '0 turns affinity off');
  assert.equal(d.validatePoolInput({ ...input, affinityTtlSeconds: 60, keepalive: { ...input.keepalive, enabled: false } }, accounts), true);
  assert.equal(d.validatePoolInput({ ...input, selectBelowUtilization: 0.4 }, accounts), true);
});

test('a pool input refuses the affinity the router refuses', () => {
  const off = { ...input.keepalive, enabled: false };
  for (const seconds of [1, 30, 59]) assert.equal(d.validatePoolInput({ ...input, affinityTtlSeconds: seconds, keepalive: off }, accounts), false, `${seconds} s is neither off nor a minute`);
  assert.equal(d.validatePoolInput({ ...input, affinityTtlSeconds: 90.5, keepalive: off }, accounts), false, 'the route takes whole seconds');
  assert.equal(d.validatePoolInput({ ...input, affinityTtlSeconds: 120, keepalive: { ...input.keepalive, enabled: true } }, accounts), false, 'warming needs a pin of five minutes');
});

test('a pool input refuses every keepalive bound the policy check refuses', () => {
  const bounds = {
    coldProbeLimit: [1, 20],
    jitterSeconds1h: [0, 3_599],
    jitterSeconds5m: [0, 299],
    leadSeconds1h: [0, 3_599],
    leadSeconds5m: [0, 299],
    maxIdleSeconds: [0, 31_536_000],
    maxIdleTicks: [0, 1_000_000],
    netNegativeGraceProbes: [0, 100],
    retryMaxSeconds: [1, 604_800],
    retrySeconds: [1, 3_600],
  };
  for (const [key, [min, max]] of Object.entries(bounds)) {
    // The ceiling sits at or above the base, so each is read against the other's widest setting.
    const other = key === 'retrySeconds' ? { retryMaxSeconds: 604_800 } : key === 'retryMaxSeconds' ? { retrySeconds: 1 } : {};
    assert.equal(d.validatePoolInput(tuned({ ...other, [key]: min }), accounts), true, `${key} ${min} is inside`);
    assert.equal(d.validatePoolInput(tuned({ ...other, [key]: max }), accounts), true, `${key} ${max} is inside`);
    assert.equal(d.validatePoolInput(tuned({ ...other, [key]: min - 1 }), accounts), false, `${key} ${min - 1} is below the floor`);
    assert.equal(d.validatePoolInput(tuned({ ...other, [key]: max + 1 }), accounts), false, `${key} ${max + 1} is past the ceiling`);
  }
  assert.equal(d.validatePoolInput(tuned({ coldProbeLimit: 2.5 }), accounts), false, 'counts are whole');
  assert.equal(d.validatePoolInput(tuned({ retryMaxSeconds: 20, retrySeconds: 30 }), accounts), false, 'the ceiling sits at or above the base');
  assert.equal(d.validatePoolInput(tuned({ retryMaxSeconds: 30, retrySeconds: 30 }), accounts), true);
  assert.equal(d.validatePoolInput(tuned({ costCapUsdPerDay: 0 }), accounts), false, 'a cap is positive or absent');
  assert.equal(d.validatePoolInput(tuned({ costCapUsdPerDay: -1 }), accounts), false);
  assert.equal(d.validatePoolInput(tuned({ costCapUsdPerDay: null }), accounts), true);
  assert.equal(d.validatePoolInput(tuned({ costCapUsdPerDay: 0.5 }), accounts), true);
});

test('a pool input refuses members the member write would refuse', () => {
  assert.equal(d.validatePoolInput({ ...input, members: [] }, accounts), false, 'a pool needs a member');
  assert.equal(d.validatePoolInput({ ...input, members: [...input.members, { accountId: 'ghost', priority: 0 }] }, accounts), false, 'an account the console does not know');
  assert.equal(d.validatePoolInput({ ...input, members: [{ accountId: input.members[0].accountId, priority: -1 }] }, accounts), false, 'priorities are unsigned');
  assert.equal(d.validatePoolInput({ ...input, members: [{ accountId: input.members[0].accountId, priority: 1.5 }] }, accounts), false, 'priorities are whole');
  assert.equal(d.validatePoolInput({ ...input, members: [{ accountId: input.members[0].accountId, priority: 7 }] }, accounts), true);
});

test('a pool input refuses thresholds outside their range and an empty name', () => {
  assert.equal(d.validatePoolInput({ ...input, name: '' }, accounts), false);
  assert.equal(d.validatePoolInput({ ...input, selectBelowUtilization: 1.2 }, accounts), false);
  assert.equal(d.validatePoolInput({ ...input, selectBelowUtilization: -0.1 }, accounts), false);
  assert.equal(d.validatePoolInput({ ...input, selectBelowUtilization: 0 }, accounts), true);
  assert.equal(d.validatePoolInput({ ...input, selectBelowUtilization: 1 }, accounts), true);
  assert.equal(d.validatePoolInput({ ...input, migrateAboveUtilization: 0.4 }, accounts), false, 'the form migrates from 50 %');
  assert.equal(d.validatePoolInput({ ...input, migrateAboveUtilization: 0.9 }, accounts), true);
});

test('a pool input with select-below inside the band is refused and every engine bound holds', () => {
  const both = (migrate, select) => d.validatePoolInput({ ...input, migrateAboveUtilization: migrate, selectBelowUtilization: select }, accounts);
  assert.equal(both(0.9, 0.85), true, 'the band\'s lower edge is still under it');
  assert.equal(both(0.7, 0.65), true, 'the edge holds where the subtraction rounds under it');
  assert.equal(both(0.9, 0.86), false, 'one point inside the band');
  assert.equal(both(0.9, 0.9), false);
  assert.equal(both(null, 0.95), true, 'with migration off there is no band');
  assert.equal(both(0.5, null), true);
  assert.equal(d.bandRefusal({ migrateAboveUtilization: 0.9, selectBelowUtilization: 0.86 }), 'Select below must sit at or under migrate above minus 5%, the band a shed session lands under.');
  assert.equal(d.bandRefusal({ migrateAboveUtilization: 0.9, selectBelowUtilization: 0.85 }), null);
  const whole = {
    burnWindowSeconds: [60, null],
    inFlightCap: [1, 4_294_967_295],
    overageMoveGraceMinutes: [0, 300],
    retryInPlaceSeconds: [0, 300],
    sourceDwellSeconds: [60, null],
    windDownMinutes: [0, 10_080],
  };
  for (const [field, [min, max]] of Object.entries(whole)) {
    assert.equal(d.validatePoolInput({ ...input, [field]: min }, accounts), true, `${field} ${min} is inside`);
    assert.equal(d.validatePoolInput({ ...input, [field]: min - 1 }, accounts), false, `${field} ${min - 1} is below the floor`);
    assert.equal(d.validatePoolInput({ ...input, [field]: min + 0.5 }, accounts), false, `${field} is whole`);
    if (max !== null) {
      assert.equal(d.validatePoolInput({ ...input, [field]: max }, accounts), true, `${field} ${max} is inside`);
      assert.equal(d.validatePoolInput({ ...input, [field]: max + 1 }, accounts), false, `${field} ${max + 1} is past the ceiling`);
    }
  }
  for (const field of ['paceFloor', 'shedShare']) {
    for (const v of [0, 0.3, 1]) assert.equal(d.validatePoolInput({ ...input, [field]: v }, accounts), true, `${field} ${v} is a share`);
    for (const v of [-0.01, 1.01, Number.NaN]) assert.equal(d.validatePoolInput({ ...input, [field]: v }, accounts), false, `${field} ${v} is not`);
  }
  for (const mode of ['always', 'never', 'when_needed']) assert.equal(d.validatePoolInput({ ...input, warmSessions: mode }, accounts), true);
  assert.equal(d.validatePoolInput({ ...input, warmSessions: 'sometimes' }, accounts), false);
  assert.equal(d.validatePoolInput({ ...input, migrateWhenSpent: false }, accounts), true);
});

test('a pool input carries the store defaults for the pin, placement and quota fields, and each unknown spelling is refused', () => {
  assert.deepEqual(
    [input.affinityMode, input.subagentPlacement, input.sessionPlacement, input.minSharedPrefixTokens, input.quotaHeaders],
    ['always', 'by_cohort', 'by_strategy', 20_000, 'pass'],
  );
  const spellings = {
    affinityMode: [['always', 'when_cached'], 'sometimes'],
    quotaHeaders: [['hide', 'pass'], 'strip'],
    sessionPlacement: [['by_strategy', 'by_warm_prefix'], 'by_luck'],
    subagentPlacement: [['by_cohort', 'by_strategy', 'with_main'], 'alone'],
  };
  for (const [field, [taken, unknown]] of Object.entries(spellings)) {
    for (const value of taken) assert.equal(d.validatePoolInput({ ...input, [field]: value }, accounts), true, `${field} ${value} is taken`);
    assert.equal(d.validatePoolInput({ ...input, [field]: unknown }, accounts), false, `${field} ${unknown} is refused`);
  }
  for (const tokens of [0, 20_000, 1_000_000]) assert.equal(d.validatePoolInput({ ...input, minSharedPrefixTokens: tokens }, accounts), true, `${tokens} tokens is taken`);
  for (const tokens of [-1, 1.5, Number.NaN]) assert.equal(d.validatePoolInput({ ...input, minSharedPrefixTokens: tokens }, accounts), false, `${tokens} tokens is refused`);
});

test('a member budget is a dollar amount above 0 or none, per day or per month', () => {
  for (const v of ['', '  ', '0.01', '12.5', '50', '1e3']) assert.equal(d.budgetRefusal(v), null, `${JSON.stringify(v)} is taken`);
  for (const v of ['0', '-1', 'x', 'Infinity', '12,50']) assert.equal(d.budgetRefusal(v), 'A budget is a dollar amount above 0; leave it empty for no budget.', `${JSON.stringify(v)} is refused`);
  const [first] = input.members;
  const withBudget = (budget) => d.validatePoolInput({ ...input, members: [{ ...first, ...budget }] }, accounts);
  assert.equal(withBudget({ budgetPeriod: 'day', budgetUsd: 12.5 }), true);
  assert.equal(withBudget({ budgetPeriod: 'month', budgetUsd: null }), true, 'no budget');
  assert.equal(withBudget({ budgetPeriod: 'month', budgetUsd: 0 }), false, 'the member write refuses a budget of 0');
  assert.equal(withBudget({ budgetPeriod: 'month', budgetUsd: Number.NaN }), false, 'a field that read no number');
  assert.equal(withBudget({ budgetPeriod: 'week', budgetUsd: 5 }), false, 'the member write knows a day and a month');
});

test('the prefix threshold is a whole number of tokens at or above 0', () => {
  for (const v of ['0', '20000', ' 512 ']) assert.equal(d.prefixThresholdRefusal(v), null, `${JSON.stringify(v)} is taken`);
  for (const v of ['', '-1', '1.5', 'x', '9007199254740993']) assert.equal(d.prefixThresholdRefusal(v), 'The minimum shared prefix is a whole number of tokens, 0 or more.', `${JSON.stringify(v)} is refused`);
});

const key = d.decodeKey(fixture('keys').keys[0]);
const NOW = Date.parse('2026-09-16T14:30:00Z');

test('a label-only edit of a disabled key sends the key disabled and leaves its expiry alone', () => {
  const disabled = d.decodeKey({ ...fixture('keys').keys[0], enabled: false });
  const form = d.keyFormOf(disabled, 'unused');
  const body = d.encodeKey(d.keyInputOf({ ...form, name: 'renamed' }, NOW));
  assert.equal(body.label, 'renamed');
  assert.equal(body.enabled, false, 'a switch read from the status would have sent true and switched the key back on');
  assert.equal('expires_at' in body, false);
  const lapsed = d.decodeKey({ ...fixture('keys').keys[0], enabled: false, expires_at: 5 });
  assert.equal(d.encodeKey(d.keyInputOf(d.keyFormOf(lapsed, 'unused'), NOW)).enabled, false, 'an expired key keeps the flag it was switched off with');
});

test('the key dialog starts from the stored key and builds the edit from it', () => {
  const form = d.keyFormOf(key, 'unused');
  assert.deepEqual([form.enabled, form.expiry, form.name, form.poolId], [true, 'keep', key.name, key.poolId]);
  const input = d.keyInputOf(form, NOW);
  assert.deepEqual(
    [input.enabled, input.expiresAt, input.modelRules, input.modelRulesMode, input.longContextTwins, input.bodyCaptureHours, input.notes],
    [true, undefined, key.modelRules, key.modelRulesMode, key.longContextTwins, key.bodyCaptureHours, key.notes],
  );
  assert.equal(d.keyInputOf({ ...form, expiry: 'never' }, NOW).expiresAt, null, 'no expiration clears the stored one');
  assert.equal(d.keyInputOf({ ...form, expiry: '30' }, NOW).expiresAt, new Date(NOW + 30 * 86_400_000).toISOString());
  assert.equal(d.keyInputOf({ ...form, enabled: false }, NOW).enabled, false);
  const fresh = d.keyFormOf(null, 'all');
  assert.deepEqual([fresh.enabled, fresh.expiry, fresh.poolId, fresh.captureMode], [true, 'never', 'all', 'inherit'], 'a new key starts on, with no expiry, in the pool it was opened from');
});

test('the key dialog carries the affinity source from the stored key to the body', () => {
  const stored = d.decodeKey({ ...fixture('keys').keys[0], affinity_source: 'session' });
  const form = d.keyFormOf(stored, 'unused');
  assert.deepEqual([form.affinitySource], ['session']);
  const body = d.encodeKey(d.keyInputOf({ ...form, affinitySource: 'user_id' }, NOW));
  assert.deepEqual([body.affinity_source], ['user_id']);
  const fresh = d.keyInputOf(d.keyFormOf(null, 'all'), NOW);
  assert.deepEqual([fresh.affinitySource], ['auto'], 'a new key starts where the store defaults one');
});

test('a key form carries the in-flight cap and null clears it', () => {
  const row = fixture('keys').keys[0];
  const form = d.keyFormOf(d.decodeKey({ ...row, in_flight_cap: 4 }), 'unused');
  assert.equal(form.inFlightCap, '4');
  const body = d.encodeKey(d.keyInputOf(form, NOW));
  assert.equal(body.in_flight_cap, 4);
  assert.equal(d.decodeKey({ ...row, ...body }).inFlightCap, 4, 'the route answers the cap back as it was sent');
  const cleared = d.keyInputOf({ ...form, inFlightCap: '' }, NOW);
  assert.equal(cleared.inFlightCap, null);
  assert.equal(d.encodeKey(cleared).in_flight_cap, null, 'an empty field sends null, which clears the stored cap');
  assert.equal('in_flight_cap' in d.encodeKey({ ...cleared, inFlightCap: undefined }), false, 'a write without the field, a key moved between pools, keeps the stored cap');
  assert.equal(d.keyFormOf(d.decodeKey(row), 'unused').inFlightCap, '', 'no cap opens as an empty field');
  assert.equal(d.keyInputOf(d.keyFormOf(null, 'all'), NOW).inFlightCap, null, 'a new key starts with no cap');
  for (const v of ['', '1', '16', '4294967295']) assert.equal(d.inFlightCapRefusal(v), null, `${JSON.stringify(v)} is taken`);
  for (const v of ['0', '-1', '1.5', '4294967296', 'x']) assert.notEqual(d.inFlightCapRefusal(v), null, `${v} is refused as the route refuses it`);
});

test('ticking cancelled prefills the derived end, unticking clears it, and the form refuses what the route refuses', () => {
  const body = fixture('account');
  const derived = '2026-10-19T09:15:00.000Z';
  const account = d.decodeAccount({ ...body, subscription: { ...body.subscription, derived_ends_at: Date.parse(derived) } });
  const form = d.subscriptionEndFormOf(account);
  assert.deepEqual(form, { cancelled: false, endsAt: '' }, 'an account never cancelled opens unticked with no end');
  const ticked = d.tickCancelled(form, true, account.subscription.derivedEndsAt);
  assert.equal(ticked.cancelled, true);
  assert.equal(new Date(ticked.endsAt).toISOString(), derived, 'the field holds the derived end in local time');
  const corrected = { ...ticked, endsAt: '2026-10-05T12:00' };
  assert.equal(d.tickCancelled({ ...corrected, cancelled: false }, true, derived).endsAt, '2026-10-05T12:00', 'a date already in the field is kept');
  assert.deepEqual(d.tickCancelled(corrected, false, derived), { cancelled: false, endsAt: '' }, 'unticking clears the end with the flag');
  assert.equal(d.subscriptionEndRefusal(ticked), null);
  assert.equal(d.subscriptionEndRefusal({ cancelled: false, endsAt: '' }), null);
  assert.equal(d.subscriptionEndRefusal({ cancelled: true, endsAt: '' }), 'A cancelled plan needs the date it ends.', 'a ticked box whose date was cleared');
  assert.equal(d.subscriptionEndRefusal({ cancelled: false, endsAt: '2026-10-05T12:00' }), 'An end is saved only with Cancelled ticked.');
  assert.deepEqual(d.accountPatchOf(ticked), { cancelled: true, subscriptionEndsAt: derived });
  assert.deepEqual(d.accountPatchOf({ cancelled: false, endsAt: '' }), { cancelled: false, subscriptionEndsAt: null });
  const saved = d.decodeAccount({ ...body, cancelled: true, subscription: { ...body.subscription, ends_at: Date.parse('2026-10-05T12:00:00.000Z'), derived_ends_at: Date.parse(derived) } });
  const reopened = d.subscriptionEndFormOf(saved);
  assert.equal(reopened.cancelled, true);
  assert.equal(new Date(reopened.endsAt).toISOString(), '2026-10-05T12:00:00.000Z', 'a cancelled account opens with the end it was saved with');
});

const rules = (...patterns) => patterns.map((p) => (p.startsWith('NOT ') ? { not: true, pattern: p.slice(4) } : { not: false, pattern: p }));

test('a model rule list is refused in the route\'s order and words, and the refusal holds a pool save', () => {
  assert.equal(d.modelRulesRefusal([]), null);
  assert.equal(d.modelRulesRefusal(rules('claude-haiku-4-5*', 'NOT *')), null);
  assert.equal(d.modelRulesRefusal(rules('claude-*', 'NOT a***', '')), 'model_rules[1]: pattern holds three stars in a row', 'the first refused entry is named');
  assert.equal(d.modelRulesRefusal(rules('')), 'model_rules[0]: pattern is empty');
  assert.equal(d.modelRulesRefusal(rules(...Array(256).fill('*'))), null, 'the bound admits 256 entries');
  assert.equal(d.modelRulesRefusal(rules(...Array(257).fill(''))), 'model_rules holds at most 256 entries', 'the bound is checked before any entry');
  assert.equal(d.validatePoolInput({ ...input, modelRules: rules('claude-*') }, accounts), true);
  assert.equal(d.validatePoolInput({ ...input, modelRules: rules('claude-*', 'a***') }, accounts), false);
});

test("the key dialog's holds agree with its Save condition", () => {
  assert.deepEqual(d.KEY_GROUPS, ['key', 'access', 'models', 'sessions', 'capture']);
  const named = { inFlightCap: '', modelRules: [], name: 'workstation', poolId: 'all' };
  assert.deepEqual(d.keyRefusals(named), [], 'a named key in a pool with no cap and no rules may save');
  assert.deepEqual(d.keyRefusals({ ...named, name: '' }), [{ field: 'Name', group: 'key', text: 'the key needs a name.' }]);
  assert.deepEqual(d.keyRefusals({ ...named, name: '   ' }), [{ field: 'Name', group: 'key', text: 'the key needs a name.' }], 'a name of spaces is saved trimmed, so it is no name');
  assert.deepEqual(d.keyRefusals({ ...named, poolId: '' }), [{ field: 'Pool', group: 'key', text: 'the key needs a pool.' }]);
  assert.deepEqual(d.keyRefusals({ ...named, inFlightCap: '0' }), [{ field: 'Requests in flight', group: 'access', text: d.inFlightCapRefusal('0') }]);
  assert.deepEqual(d.keyRefusals({ ...named, modelRules: rules('a***') }), [{ field: 'Model rules', group: 'models', text: d.modelRulesRefusal(rules('a***')) }]);
  assert.deepEqual(
    d.keyRefusals({ inFlightCap: '0', modelRules: rules('a***'), name: '', poolId: '' }).map((r) => [r.group, r.field]),
    [['key', 'Name'], ['key', 'Pool'], ['access', 'Requests in flight'], ['models', 'Model rules']],
    'every hold at once reads in rail order',
  );
  for (const v of ['', '1', '16', '4294967295']) assert.deepEqual(d.keyRefusals({ ...named, inFlightCap: v }), [], `a cap of ${JSON.stringify(v)} holds nothing`);
  for (const v of ['0', '-1', '1.5', '4294967296', 'x']) assert.deepEqual(d.keyRefusals({ ...named, inFlightCap: v }).map((r) => r.group), ['access'], `a cap of ${v} holds Access`);
  for (const list of [[], rules('claude-haiku-4-5*', 'NOT *'), rules(...Array(256).fill('*'))]) assert.deepEqual(d.keyRefusals({ ...named, modelRules: list }), [], 'a list the route takes holds nothing');
  for (const list of [rules('claude-*', 'NOT a***', ''), rules(''), rules(...Array(257).fill(''))]) assert.deepEqual(d.keyRefusals({ ...named, modelRules: list }), [{ field: 'Model rules', group: 'models', text: d.modelRulesRefusal(list) }]);
});

test('a pool refusal names the group its control sits in', () => {
  assert.deepEqual(d.poolRefusals(input, accounts), [], 'the stored pool is refused nothing');
  const [first] = input.members;
  const refused = (field, group, text) => [{ field, group, text }];
  const cases = [
    [{ name: '' }, refused('Name', 'identity', 'the pool needs a name.')],
    [{ members: [] }, refused('members', 'members', 'the pool needs a member.')],
    [{ members: [...input.members, { accountId: 'ghost', priority: 0 }] }, refused('member:ghost', 'members', 'ghost is not an upstream the console lists.')],
    [{ members: [{ accountId: first.accountId, priority: 1.5 }] }, refused('priority:business', 'members', 'Priority is a whole number of 0 or more.')],
    [{ members: [{ ...first, budgetUsd: 0 }] }, refused('budget:business', 'members', 'A budget is a dollar amount above 0; leave it empty for no budget.')],
    [{ members: [{ ...first, budgetPeriod: 'week', budgetUsd: 5 }] }, refused('period:business', 'members', 'A budget period is a day or a month.')],
    [{ modelRules: rules('a***') }, refused('Model rules', 'models', 'model_rules[0]: pattern holds three stars in a row')],
    [{ subagentPlacement: 'alone' }, refused('Subagent placement', 'placement', 'Subagent placement is one of its listed choices.')],
    [{ sessionPlacement: 'by_luck' }, refused('Session placement', 'placement', 'Session placement is one of its listed choices.')],
    [{ minSharedPrefixTokens: 1.5 }, refused('Minimum shared prefix, tokens', 'placement', 'The minimum shared prefix is a whole number of tokens, 0 or more.')],
    [
      { affinityTtlSeconds: 30 },
      [
        { field: 'Session affinity in seconds', group: 'pinning', text: 'Session affinity is off or at least 60 whole seconds.' },
        { field: 'Keep prompt caches warm', group: 'keepalive', text: 'Keep prompt caches warm needs Session affinity of at least 5m.' },
      ],
    ],
    [{ affinityMode: 'sometimes' }, refused('Pin sessions', 'pinning', 'Pin sessions is one of its listed choices.')],
    [{ migrateAboveUtilization: 0.4 }, refused('Migrate above utilization (%)', 'utilization', 'Migrate above utilization is from 50 to 100.')],
    [{ selectBelowUtilization: 1.2 }, refused('Select below utilization (%)', 'utilization', 'Select below utilization is from 0 to 100.')],
    [
      { migrateAboveUtilization: 0.9, selectBelowUtilization: 0.86 },
      refused('Select below utilization (%)', 'utilization', 'Select below must sit at or under migrate above minus 5%, the band a shed session lands under.'),
    ],
    [{ shedShare: 1.2 }, refused('Share (%)', 'shedding', 'Share (%) is from 0 to 100.')],
    [{ burnWindowSeconds: 59 }, refused('Burn window (s)', 'shedding', 'Burn window (s) is a whole number of at least 60.')],
    [{ sourceDwellSeconds: 59 }, refused('Source dwell (s)', 'shedding', 'Source dwell (s) is a whole number of at least 60.')],
    [{ warmSessions: 'sometimes' }, refused('Warm sessions', 'shedding', 'Warm sessions is one of its listed choices.')],
    [{ paceFloor: 1.2 }, refused('Pace floor (%)', 'failover', 'Pace floor (%) is from 0 to 100.')],
    [{ inFlightCap: 0 }, refused('In flight', 'failover', 'In flight is a whole number from 1 to 4,294,967,295.')],
    [{ retryInPlaceSeconds: 301 }, refused('Retry in place (s)', 'failover', 'Retry in place (s) is a whole number from 0 to 300.')],
    [{ overageMoveGraceMinutes: 301 }, refused('Overage grace (min)', 'overage', 'Overage grace (min) is a whole number from 0 to 300.')],
    [{ windDownMinutes: 10_081 }, refused('Wind down (min)', 'overage', 'Wind down (min) is a whole number from 0 to 10,080.')],
    [
      { keepalive: { ...input.keepalive, netNegativeGraceProbes: 101 } },
      refused('Grace probes after the net turns negative', 'keepalive', 'Grace probes after the net turns negative is a whole number from 0 to 100.'),
    ],
    [{ keepalive: { ...input.keepalive, costCapUsdPerDay: 0 } }, refused('Daily probe cost cap in dollars', 'keepalive', 'Daily probe cost cap ($) is above 0.')],
    [
      { affinityTtlSeconds: 120, keepalive: { ...input.keepalive, enabled: true } },
      refused('Keep prompt caches warm', 'keepalive', 'Keep prompt caches warm needs Session affinity of at least 5m.'),
    ],
    [{ keepalive: { ...input.keepalive, coldProbeLimit: 0 } }, refused('Cold probe limit', 'probeTuning', 'Cold probe limit is a whole number from 1 to 20.')],
    [
      { keepalive: { ...input.keepalive, retryMaxSeconds: 20, retrySeconds: 30 } },
      refused('Retry backoff max (s)', 'probeTuning', 'Retry backoff max (s) is at or above Retry backoff base (s).'),
    ],
    [{ quotaHeaders: 'strip' }, refused('Quota headers', 'responseHeaders', 'Quota headers is one of its listed choices.')],
  ];
  for (const [patch, expected] of cases) assert.deepEqual(d.poolRefusals({ ...input, ...patch }, accounts), expected, JSON.stringify(patch));
});

test('the Probe tuning rows lay out every numeric keepalive bound but the grace count, in the order the refusals read', () => {
  assert.deepEqual(d.PROBE_TUNING_ROWS, [['leadSeconds5m', 'leadSeconds1h'], ['jitterSeconds5m', 'jitterSeconds1h'], ['coldProbeLimit'], ['retrySeconds', 'retryMaxSeconds'], ['maxIdleSeconds', 'maxIdleTicks']]);
  assert.deepEqual(d.PROBE_TUNING_KEYS, d.PROBE_TUNING_ROWS.flat(), 'the refusals walk the fields in the order the group draws them');
  // A numeric keepalive field added later gets a bound from KEEPALIVE_BOUNDS; this keeps it from escaping the refusals.
  assert.deepEqual([...d.PROBE_TUNING_KEYS, 'netNegativeGraceProbes'].sort(), Object.keys(d.KEEPALIVE_BOUNDS).sort());
});

test("refusals come in rail order, a new pool's first two the name and the member", () => {
  const refusals = d.poolRefusals({ ...input, name: '', members: [], modelRules: rules('a***'), quotaHeaders: 'strip' }, accounts);
  assert.deepEqual(
    refusals.map((r) => r.group),
    ['identity', 'members', 'models', 'responseHeaders'],
  );
  assert.deepEqual(
    refusals.slice(0, 2).map((r) => r.text),
    ['the pool needs a name.', 'the pool needs a member.'],
  );
  assert.deepEqual(d.POOL_GROUPS, ['identity', 'members', 'keys', 'models', 'selection', 'placement', 'pinning', 'utilization', 'shedding', 'failover', 'overage', 'cacheTtl', 'keepalive', 'probeTuning', 'responseHeaders', 'bodyCapture', 'cacheDiagnostics']);
});

test('every bounded engine field is refused once it leaves its bound', () => {
  // A bound added to ENGINE_BOUNDS gets its label through the Record type; this keeps it from escaping the refusals.
  const below = Object.fromEntries(Object.entries(d.ENGINE_BOUNDS).map(([key, { min }]) => [key, min - 1]));
  const keyOf = Object.fromEntries(Object.entries(d.ENGINE_FIELD_LABELS).map(([key, label]) => [label, key]));
  const refused = d.poolRefusals({ ...input, ...below }, accounts).map((r) => keyOf[r.field] ?? r.field);
  assert.deepEqual(refused.sort(), Object.keys(d.ENGINE_FIELD_LABELS).sort());
});

test('the key dialog carries the rules, the mode, and the twins switch from the stored key to the body', () => {
  const own = rules('claude-sonnet-5', 'NOT *');
  const stored = d.decodeKey({ ...fixture('keys').keys[0], long_context_twins: false, model_rules: own, model_rules_mode: 'replace' });
  const form = d.keyFormOf(stored, 'unused');
  assert.deepEqual([form.modelRules, form.modelRulesMode, form.longContextTwins], [own, 'replace', false]);
  const body = d.encodeKey(d.keyInputOf(form, NOW));
  assert.deepEqual([body.model_rules, body.model_rules_mode, body.long_context_twins], [own, 'replace', false]);
  const back = d.decodeKey({ ...fixture('keys').keys[0], ...body });
  assert.deepEqual([back.modelRules, back.modelRulesMode, back.longContextTwins], [own, 'replace', false], 'the route answers the body back as it was sent');
  const fresh = d.keyInputOf(d.keyFormOf(null, 'all'), NOW);
  assert.deepEqual([fresh.modelRules, fresh.modelRulesMode, fresh.longContextTwins], [[], 'extend', true], 'a new key starts where the route defaults one');
});
