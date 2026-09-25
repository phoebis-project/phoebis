import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;

const pool = d.decodePool(fixture('pools').pools[0]);
const accounts = new Set(pool.members.map((m) => m.accountId));
const input = {
  affinityTtlSeconds: pool.affinityTtlSeconds,
  bodyCaptureHours: pool.bodyCaptureHours,
  cacheDiagnostics: pool.cacheDiagnostics,
  cacheTtl: pool.cacheTtl,
  keepalive: pool.keepalive,
  members: pool.members.map((m) => ({ accountId: m.accountId, priority: m.priority })),
  migrateAboveUtilization: pool.migrateAboveUtilization,
  modelRules: pool.modelRules,
  name: pool.name,
  overagePolicy: pool.overagePolicy,
  preferSubscription: pool.preferSubscription,
  selectBelowUtilization: pool.selectBelowUtilization,
  status: 'active',
  strategy: pool.strategy,
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
