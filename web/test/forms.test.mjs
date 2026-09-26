import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;

const pool = d.decodePool(fixture('pools').pools[0]);
const accounts = new Set(pool.members.map((m) => m.accountId));
// The captured pool holds the store's defaults, the balanced presets, for every engine field.
const input = {
  affinityTtlSeconds: pool.affinityTtlSeconds,
  bodyCaptureHours: pool.bodyCaptureHours,
  burnWindowSeconds: pool.burnWindowSeconds,
  cacheDiagnostics: pool.cacheDiagnostics,
  cacheTtl: pool.cacheTtl,
  inFlightCap: pool.inFlightCap,
  keepalive: pool.keepalive,
  members: pool.members.map((m) => ({ accountId: m.accountId, priority: m.priority })),
  migrateAboveUtilization: pool.migrateAboveUtilization,
  migrateWhenSpent: pool.migrateWhenSpent,
  modelRules: pool.modelRules,
  name: pool.name,
  overageMoveGraceMinutes: pool.overageMoveGraceMinutes,
  overagePolicy: pool.overagePolicy,
  paceFloor: pool.paceFloor,
  preferSubscription: pool.preferSubscription,
  retryInPlaceSeconds: pool.retryInPlaceSeconds,
  selectBelowUtilization: pool.selectBelowUtilization,
  shedShare: pool.shedShare,
  sourceDwellSeconds: pool.sourceDwellSeconds,
  status: 'active',
  strategy: pool.strategy,
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
