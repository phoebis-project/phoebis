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
  burnAverageSeconds: pool.burnAverageSeconds,
  cacheDiagnostics: pool.cacheDiagnostics,
  cacheTtl: pool.cacheTtl,
  fiveHourCeiling: pool.fiveHourCeiling,
  inFlightCap: pool.inFlightCap,
  keepalive: pool.keepalive,
  loadShedding: pool.loadShedding,
  members: pool.members.map((m) => ({ accountId: m.accountId, budgetDailyUsd: m.budgetDailyUsd, budgetMonthlyUsd: m.budgetMonthlyUsd, priority: m.priority })),
  migrateWhenSpent: pool.migrateWhenSpent,
  minSharedPrefixTokens: pool.minSharedPrefixTokens,
  modelRules: pool.modelRules,
  name: pool.name,
  overageMoveGraceMinutes: pool.overageMoveGraceMinutes,
  overagePolicy: pool.overagePolicy,
  preferSubscription: pool.preferSubscription,
  retryInPlaceSeconds: pool.retryInPlaceSeconds,
  returnToPlan: pool.returnToPlan,
  returnToPlanDwellSeconds: pool.returnToPlanDwellSeconds,
  sessionPlacement: pool.sessionPlacement,
  shedCap: pool.shedCap,
  shedDwellSeconds: pool.shedDwellSeconds,
  slowDown: pool.slowDown,
  slowDownHoldSeconds: pool.slowDownHoldSeconds,
  slowDownStretchSeconds: pool.slowDownStretchSeconds,
  status: 'active',
  strategy: pool.strategy,
  subagentPlacement: pool.subagentPlacement,
  warmSessions: pool.warmSessions,
  weeklyCeiling: pool.weeklyCeiling,
  windDownMinutes: pool.windDownMinutes,
};
const tuned = (patch) => ({ ...input, keepalive: { ...input.keepalive, ...patch } });

test('a pool input the router would take validates', () => {
  assert.deepEqual(d.poolRefusals(input, accounts), []);
  assert.deepEqual(d.poolRefusals({ ...input, affinityTtlSeconds: 0, keepalive: { ...input.keepalive, enabled: false } }, accounts), [], '0 turns affinity off');
  assert.deepEqual(d.poolRefusals({ ...input, affinityTtlSeconds: 60, keepalive: { ...input.keepalive, enabled: false } }, accounts), []);
  assert.deepEqual(d.poolRefusals({ ...input, loadShedding: false }, accounts), [], 'the switch is a toggle with no bound');
});

test('a pool input refuses the affinity the router refuses', () => {
  const off = { ...input.keepalive, enabled: false };
  for (const seconds of [1, 30, 59]) assert.notEqual(d.poolRefusals({ ...input, affinityTtlSeconds: seconds, keepalive: off }, accounts).length, 0, `${seconds} s is neither off nor a minute`);
  assert.notEqual(d.poolRefusals({ ...input, affinityTtlSeconds: 90.5, keepalive: off }, accounts).length, 0, 'the route takes whole seconds');
  assert.notEqual(d.poolRefusals({ ...input, affinityTtlSeconds: 120, keepalive: { ...input.keepalive, enabled: true } }, accounts).length, 0, 'warming needs a pin of five minutes');
});

test('a pool input refuses a keepalive idle below 0, a fraction of a second and an empty ceiling, and takes a null one', () => {
  // The captured route probes hold each field's bound pairs, and none sends an idle below its floor of 0.
  assert.notEqual(d.poolRefusals(tuned({ maxIdleSeconds: -1 }), accounts).length, 0, 'maxIdleSeconds -1 is below the floor');
  assert.notEqual(d.poolRefusals(tuned({ maxIdleSeconds: 2.5 }), accounts).length, 0, 'seconds are whole');
  assert.deepEqual(d.poolRefusals(tuned({ maxParkedSeconds: null, scheduledHorizonSeconds: null }), accounts), [], 'a null ceiling is the break-even');
  assert.notEqual(d.poolRefusals(tuned({ maxParkedSeconds: Number.NaN }), accounts).length, 0, 'a switched-on ceiling left empty is held');
});

test('a pool input refuses members the member write would refuse', () => {
  assert.notEqual(d.poolRefusals({ ...input, members: [] }, accounts).length, 0, 'a pool needs a member');
  assert.notEqual(d.poolRefusals({ ...input, members: [...input.members, { accountId: 'ghost', budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 0 }] }, accounts).length, 0, 'an account the console does not know');
  assert.notEqual(d.poolRefusals({ ...input, members: [{ accountId: input.members[0].accountId, budgetDailyUsd: null, budgetMonthlyUsd: null, priority: -1 }] }, accounts).length, 0, 'priorities are unsigned');
  assert.notEqual(d.poolRefusals({ ...input, members: [{ accountId: input.members[0].accountId, budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 1.5 }] }, accounts).length, 0, 'priorities are whole');
  assert.deepEqual(d.poolRefusals({ ...input, members: [{ accountId: input.members[0].accountId, budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 7 }] }, accounts), []);
});

test('a members list that did not load holds the save once and names no member', () => {
  const held = { field: 'members', group: 'members', text: 'the account list did not load, so the members cannot be checked.' };
  assert.deepEqual(d.poolRefusals(input, null), [held]);
  const ghost = { accountId: 'ghost', budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 0 };
  assert.deepEqual(d.poolRefusals({ ...input, members: [...input.members, ghost] }, null), [held], 'an id the list may hold is not refused');
});

test('a pool input refuses thresholds outside their range and an empty name', () => {
  assert.notEqual(d.poolRefusals({ ...input, name: '' }, accounts).length, 0);
  assert.deepEqual(d.poolRefusals({ ...input, fiveHourCeiling: 0.5 }, accounts), [], 'a ceiling runs from half of a window');
  assert.deepEqual(d.poolRefusals({ ...input, fiveHourCeiling: 1 }, accounts), [], 'to the wall itself');
  assert.notEqual(d.poolRefusals({ ...input, fiveHourCeiling: 0.49 }, accounts).length, 0);
  assert.notEqual(d.poolRefusals({ ...input, fiveHourCeiling: 1.01 }, accounts).length, 0);
  assert.notEqual(d.poolRefusals({ ...input, fiveHourCeiling: Number.NaN }, accounts).length, 0, 'a field left empty holds the save');
  assert.deepEqual(d.poolRefusals({ ...input, shedCap: 0.1 }, accounts), [], 'a cap runs from a tenth of the burn');
  assert.deepEqual(d.poolRefusals({ ...input, shedCap: 1 }, accounts), []);
  assert.notEqual(d.poolRefusals({ ...input, shedCap: 0.09 }, accounts).length, 0);
  assert.notEqual(d.poolRefusals({ ...input, shedCap: 1.01 }, accounts).length, 0);
});

test('a pool input refuses a half step on every whole engine field and a choice outside its list, each refusal in its group', () => {
  // The captured route probes and the test that drops every field below its floor hold the bounds. Half a step past
  // each floor is a value neither sends.
  const halves = {
    burnAverageSeconds: 600.5,
    inFlightCap: 1.5,
    overageMoveGraceMinutes: 0.5,
    retryInPlaceSeconds: 0.5,
    returnToPlanDwellSeconds: 0.5,
    shedDwellSeconds: 300.5,
    slowDownHoldSeconds: 1.5,
    slowDownStretchSeconds: 60.5,
    windDownMinutes: 0.5,
  };
  for (const [field, value] of Object.entries(halves)) assert.notEqual(d.poolRefusals({ ...input, [field]: value }, accounts).length, 0, `${field} is whole`);
  for (const mode of ['always', 'never', 'when_needed']) assert.deepEqual(d.poolRefusals({ ...input, warmSessions: mode }, accounts), []);
  assert.notEqual(d.poolRefusals({ ...input, warmSessions: 'sometimes' }, accounts).length, 0);
  assert.deepEqual(d.poolRefusals({ ...input, migrateWhenSpent: false }, accounts), []);
  for (const mode of ['always', 'never', 'when_cold']) assert.deepEqual(d.poolRefusals({ ...input, returnToPlan: mode }, accounts), []);
  assert.deepEqual(d.poolRefusals({ ...input, returnToPlan: 'sometimes' }, accounts), [{ field: 'Return to plan', group: 'overage', text: 'Return to plan is one of its listed choices.' }]);
  assert.deepEqual(d.poolRefusals({ ...input, slowDown: true }, accounts), [], 'the slow-down is a toggle with no bound');
  assert.deepEqual(
    d.poolRefusals({ ...input, returnToPlanDwellSeconds: 86_401, slowDownHoldSeconds: 0, slowDownStretchSeconds: 59 }, accounts).map((r) => [r.group, r.field, r.text]),
    [
      ['slowDown', 'Stretch (s)', 'Stretch (s) is a whole number from 60 to 18,000.'],
      ['slowDown', 'Hold (s)', 'Hold (s) is a whole number from 1 to 300.'],
      ['overage', 'Return dwell (s)', 'Return dwell (s) is a whole number from 0 to 86,400.'],
    ],
    'the slow-down fields refuse in their own group, the return dwell in the overage group',
  );
  assert.deepEqual(
    d.poolRefusals({ ...input, burnAverageSeconds: 599, fiveHourCeiling: 0.49, shedCap: 1.2 }, accounts).map((r) => [r.group, r.field]),
    [
      ['utilization', '5-hour ceiling (%)'],
      ['utilization', 'Burn average (s)'],
      ['shedding', 'Shed cap (%)'],
    ],
    'the burn average refuses under Windows, after the ceiling and before the shed cap',
  );
});

test('a pool input takes each listed spelling of the pin and placement fields, and each unknown spelling is refused', () => {
  const spellings = {
    affinityMode: [['always', 'when_cached'], 'sometimes'],
    sessionPlacement: [['by_strategy', 'by_warm_prefix'], 'by_luck'],
    subagentPlacement: [['by_cohort', 'by_strategy', 'with_main'], 'alone'],
  };
  for (const [field, [taken, unknown]] of Object.entries(spellings)) {
    for (const value of taken) assert.deepEqual(d.poolRefusals({ ...input, [field]: value }, accounts), [], `${field} ${value} is taken`);
    assert.notEqual(d.poolRefusals({ ...input, [field]: unknown }, accounts).length, 0, `${field} ${unknown} is refused`);
  }
  for (const tokens of [0, 20_000, 1_000_000]) assert.deepEqual(d.poolRefusals({ ...input, minSharedPrefixTokens: tokens }, accounts), [], `${tokens} tokens is taken`);
  for (const tokens of [-1, 1.5, Number.NaN, Number.MAX_SAFE_INTEGER + 1]) assert.notEqual(d.poolRefusals({ ...input, minSharedPrefixTokens: tokens }, accounts).length, 0, `${tokens} tokens is refused`);
});

test('a member budget is a dollar amount above 0 or none, a daily and a monthly side by side', () => {
  const [first] = input.members;
  const withBudget = (budget) => d.poolRefusals({ ...input, members: [{ ...first, ...budget }] }, accounts);
  assert.deepEqual(withBudget({ budgetDailyUsd: 12.5, budgetMonthlyUsd: null }), []);
  assert.deepEqual(withBudget({ budgetDailyUsd: 5, budgetMonthlyUsd: 50 }), [], 'both budgets at once');
  assert.deepEqual(withBudget({ budgetDailyUsd: null, budgetMonthlyUsd: null }), [], 'no budget');
  assert.notEqual(withBudget({ budgetDailyUsd: 0, budgetMonthlyUsd: null }).length, 0, 'the member write refuses a daily budget of 0');
  assert.notEqual(withBudget({ budgetDailyUsd: null, budgetMonthlyUsd: 0 }).length, 0, 'the member write refuses a monthly budget of 0');
  assert.notEqual(withBudget({ budgetDailyUsd: 5, budgetMonthlyUsd: Number.NaN }).length, 0, 'a field that read no number');
  assert.notEqual(withBudget({ budgetDailyUsd: Number.POSITIVE_INFINITY, budgetMonthlyUsd: null }).length, 0, 'a budget without a bound');
});

test('a number field reads empty as none and a field the browser could not parse as NaN', () => {
  assert.equal(d.typedNumber({ bad: false, text: '' }), null);
  assert.equal(d.typedNumber({ bad: true, text: '' }), Number.NaN, 'a field the browser could not parse is not read as empty');
  assert.equal(d.typedNumber({ bad: false, text: ' 512 ' }), 512);
});

test('the discard question reads unparsable text apart from an empty field, and two unparsable entries alike', () => {
  assert.equal(d.sameValues({ daily: Number.NaN }, { daily: null }), false, 'a budget opened as none that holds unparsable text is a change');
  assert.equal(d.sameValues({ daily: Number.NaN }, { daily: Number.NaN }), true);
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
  // A create sends the list too, so a key created here takes the switches' words in place of the server's default.
  assert.deepEqual(d.keyInputOf(fresh, NOW).companionPermissions, ['allowance', 'keepalive'], 'a new key starts with both companion permissions');
});

test('the key form reads its own guardrails in position order, the input leaves them out, and an edit sends the list only when it changed', () => {
  const refs = [
    { enabled: true, id: 'second', mode: 'observe', name: 'Second', position: 3 },
    { enabled: false, id: 'first', mode: 'enforce', name: 'First', position: 1 },
  ];
  const form = d.keyFormOf({ ...key, guardrails: refs }, 'unused');
  assert.deepEqual(form.guardrails, ['first', 'second'], 'by position, which is no index');
  assert.equal('guardrails' in d.keyInputOf(form, NOW), false, 'the input builder leaves the list to the save');
  assert.deepEqual(d.keyFormOf(null, 'all').guardrails, [], 'a new key starts with none');
  assert.deepEqual(d.guardrailsChange(['first', 'second'], ['first', 'second']), {}, 'an unchanged list sends nothing');
  assert.deepEqual(d.guardrailsChange(['second', 'first'], ['first', 'second']), { guardrails: ['second', 'first'] }, 'a reorder sends the whole list');
  assert.deepEqual(d.guardrailsChange([], ['first']), { guardrails: [] }, 'removing the last sends the empty list');
});

test("a key's choice of an account named inherit or none stays that account", () => {
  assert.equal(d.keyFeatureValue({ accountId: 'inherit', mode: 'account' }), 'account:inherit');
  assert.deepEqual(d.keyFeatureChoice('account:inherit'), { accountId: 'inherit', mode: 'account' });
  assert.deepEqual(d.keyFeatureChoice('account:none'), { accountId: 'none', mode: 'account' });
  assert.deepEqual(d.keyFeatureChoice('inherit'), { accountId: null, mode: 'inherit' });
});

test('ticking canceled leaves the end as typed, unticking clears it, and the form refuses what the route refuses', () => {
  const body = fixture('account');
  const account = d.decodeAccount(body);
  const form = d.subscriptionEndFormOf(account);
  assert.deepEqual(form, { canceled: false, endsAt: '', revision: account.revision }, 'an account never canceled opens unticked with no end');
  const ticked = d.tickCanceled(form, true);
  assert.deepEqual(ticked, { canceled: true, endsAt: '', revision: account.revision }, 'the tick copies nothing into the field');
  assert.equal(d.subscriptionEndRefusal(ticked), 'A canceled plan needs the date it ends.', 'so the form asks for the date the billing page shows');
  const typed = { ...ticked, endsAt: '2026-10-05T12:00' };
  assert.equal(d.tickCanceled({ ...typed, canceled: false }, true).endsAt, '2026-10-05T12:00', 'a date already in the field is kept');
  assert.deepEqual(d.tickCanceled(typed, false), { canceled: false, endsAt: '', revision: account.revision }, 'unticking clears the end with the flag');
  assert.equal(d.subscriptionEndRefusal(typed), null);
  assert.equal(d.subscriptionEndRefusal({ canceled: false, endsAt: '' }), null);
  assert.equal(d.subscriptionEndRefusal({ canceled: true, endsAt: '' }), 'A canceled plan needs the date it ends.', 'a ticked box whose date was cleared');
  assert.equal(d.subscriptionEndRefusal({ canceled: false, endsAt: '2026-10-05T12:00' }), 'An end is saved only with Canceled ticked.');
  assert.deepEqual(d.accountPatchOf(typed), { canceled: true, subscriptionEndsAt: new Date('2026-10-05T12:00').toISOString() });
  assert.deepEqual(d.accountPatchOf({ canceled: false, endsAt: '' }), { canceled: false, subscriptionEndsAt: null });
  const saved = d.decodeAccount({ ...body, canceled: true, subscription: { ...body.subscription, ends_at: Date.parse('2026-10-05T12:00:00.000Z') } });
  const reopened = d.subscriptionEndFormOf(saved);
  assert.equal(reopened.canceled, true);
  assert.equal(new Date(reopened.endsAt).toISOString(), '2026-10-05T12:00:00.000Z', 'a canceled account opens with the end it was saved with');
  assert.equal(reopened.revision, saved.revision, 'the reopened editor fences on the revision of the row it opened with');
});

test("the end editor names the zone its field reads in, the browser's own by default", () => {
  assert.equal(d.endZoneLabel('Europe/Berlin'), 'Ends (Europe/Berlin)');
  assert.equal(d.endZoneLabel(), `Ends (${Intl.DateTimeFormat().resolvedOptions().timeZone})`, "the default reads the runtime's zone");
});

const rules = (...patterns) => patterns.map((p) => (p.startsWith('NOT ') ? { not: true, pattern: p.slice(4) } : { not: false, pattern: p }));

test('a model rule list is refused in the route\'s order and words, and the refusal holds a pool save', () => {
  assert.equal(d.modelRulesRefusal([]), null);
  assert.equal(d.modelRulesRefusal(rules('claude-haiku-4-5*', 'NOT *')), null);
  assert.equal(d.modelRulesRefusal(rules('claude-*', 'NOT a***', '')), 'model_rules[1]: pattern holds three stars in a row', 'the first refused entry is named');
  assert.equal(d.modelRulesRefusal(rules('')), 'model_rules[0]: pattern is empty');
  assert.equal(d.modelRulesRefusal(rules(...Array(256).fill('*'))), null, 'the bound admits 256 entries');
  assert.equal(d.modelRulesRefusal(rules(...Array(257).fill(''))), 'model_rules holds at most 256 entries', 'the bound is checked before any entry');
  assert.deepEqual(d.poolRefusals({ ...input, modelRules: rules('claude-*') }, accounts), []);
  assert.notEqual(d.poolRefusals({ ...input, modelRules: rules('claude-*', 'a***') }, accounts).length, 0);
});

test("the key dialog's holds agree with its Save condition", () => {
  const named = { captureHours: 6, captureMode: 'inherit', modelRules: [], name: 'workstation', poolId: 'all' };
  assert.deepEqual(d.keyRefusals(named), [], 'a named key in a pool with no rules may save');
  assert.deepEqual(d.keyRefusals({ ...named, name: '' }), [{ field: 'Name', group: 'key', text: 'the key needs a name.' }]);
  assert.deepEqual(d.keyRefusals({ ...named, name: '   ' }), [{ field: 'Name', group: 'key', text: 'the key needs a name.' }], 'a name of spaces is saved trimmed, so it is no name');
  assert.deepEqual(d.keyRefusals({ ...named, poolId: '' }), [{ field: 'Pool', group: 'key', text: 'the key needs a pool.' }]);
  assert.deepEqual(d.keyRefusals({ ...named, modelRules: rules('a***') }), [{ field: 'Model rules', group: 'models', text: d.modelRulesRefusal(rules('a***')) }]);
  assert.deepEqual(
    d.keyRefusals({ captureHours: null, captureMode: 'override', modelRules: rules('a***'), name: '', poolId: '' }, { maxHours: 72 }).map((r) => [r.group, r.field]),
    [['key', 'Name'], ['key', 'Pool'], ['models', 'Model rules'], ['capture', 'Capture hours for this key']],
    'every hold at once reads in rail order',
  );
  for (const list of [[], rules('claude-haiku-4-5*', 'NOT *'), rules(...Array(256).fill('*'))]) assert.deepEqual(d.keyRefusals({ ...named, modelRules: list }), [], 'a list the route takes holds nothing');
  for (const list of [rules('claude-*', 'NOT a***', ''), rules(''), rules(...Array(257).fill(''))]) assert.deepEqual(d.keyRefusals({ ...named, modelRules: list }), [{ field: 'Model rules', group: 'models', text: d.modelRulesRefusal(list) }]);
});

test("a new key's name is refused when its id is taken, by case or by punctuation, or holds no letter or digit", () => {
  const named = (name, isNew = true, existingIds = new Set(['desktop', 'laptop'])) => d.keyRefusals({ modelRules: [], name, poolId: 'all' }, { identity: { existingIds, isNew } });
  const taken = [{ field: 'Name', group: 'key', text: 'Another key already has the id laptop.' }];
  assert.deepEqual(named('Laptop'), taken, 'by case');
  assert.deepEqual(named('laptop!'), taken, 'by punctuation');
  assert.deepEqual(named('研究'), [], 'letters outside ASCII are a name, which derives the id item');
  assert.deepEqual(named('研究', true, new Set(['item'])), [{ field: 'Name', group: 'key', text: 'Another key already has the id item.' }], 'unless that id is taken');
  assert.deepEqual(named('--'), [{ field: 'Name', group: 'key', text: 'The key needs a name with a letter or a digit.' }], 'no letter or digit in any script');
  assert.deepEqual(named('Laptop 2'), [], 'laptop-2 is free');
  assert.deepEqual(named(''), [{ field: 'Name', group: 'key', text: 'the key needs a name.' }], 'an empty name draws its own sentence alone');
  assert.deepEqual(named('   '), [{ field: 'Name', group: 'key', text: 'the key needs a name.' }], 'a name of spaces is no name, and no id is checked for it');
  assert.deepEqual(named('laptop', false), [], 'an edit keeps its id and is never held for it');
  const long = 'a'.repeat(64);
  assert.deepEqual(named(` ${long}`, true, new Set([long])), [{ field: 'Name', group: 'key', text: `Another key already has the id ${long}.` }], 'the id is derived from the trimmed name, as the create call sends it');
  assert.equal(d.slug('laptop!'), 'laptop', 'the id is the one the create call derives');
});

test("an override's hours must be a whole number inside the ceiling", () => {
  const form = { captureHours: 6, captureMode: 'override', modelRules: [], name: 'workstation', poolId: 'all' };
  const hours = (captureHours, captureMode = 'override', checks = { maxHours: 72 }) => d.keyRefusals({ ...form, captureHours, captureMode }, checks);
  const refused = (text) => [{ field: 'Capture hours for this key', group: 'capture', text }];
  for (const v of [null, 0, 2.5, 73]) assert.deepEqual(hours(v), refused('Capture hours is a whole number from 1 to 72.'), `${JSON.stringify(v)} is refused`);
  for (const v of [1, 72]) assert.deepEqual(hours(v), [], `${v} is inside`);
  assert.deepEqual(hours(null, 'inherit'), [], 'Inherit never reads the field');
  assert.deepEqual(hours(null, 'off'), [], 'Off never reads the field');
  for (const v of [null, 0, 2.5]) assert.deepEqual(hours(v, 'override', {}), refused('Capture hours is a whole number of at least 1.'), `${JSON.stringify(v)} is refused before the ceiling is read`);
  assert.deepEqual(hours(500, 'override', {}), [], 'before the ceiling is read no upper bound applies');
  const row = fixture('keys').keys[0];
  const stored = (hours) => d.keyFormOf(d.decodeKey({ ...row, body_capture_hours: hours }), 'unused');
  assert.deepEqual([stored(12).captureMode, stored(12).captureHours], ['override', 12], 'an override opens as its hours');
  assert.deepEqual([stored(0).captureMode, stored(0).captureHours], ['off', 6], 'off opens with the default hours in the field');
  assert.equal(d.keyFormOf(null, 'all').captureHours, 6);
  const sent = (patch) => d.keyInputOf({ ...stored(12), ...patch }, NOW).bodyCaptureHours;
  assert.deepEqual([sent({}), sent({ captureHours: 48 }), sent({ captureMode: 'off' }), sent({ captureMode: 'inherit' })], [12, 48, 0, null]);
});

test('a pool refusal names the group its control sits in', () => {
  assert.deepEqual(d.poolRefusals(input, accounts), [], 'the stored pool is refused nothing');
  const [first] = input.members;
  const refused = (field, group, text) => [{ field, group, text }];
  const cases = [
    [{ name: '' }, refused('Name', 'identity', 'the pool needs a name.')],
    [{ members: [] }, refused('members', 'members', 'the pool needs a member.')],
    [{ members: [...input.members, { accountId: 'ghost', budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 0 }] }, [{ field: 'member:ghost', group: 'members', member: { accountId: 'ghost', column: 'member' }, text: 'ghost is not an account the console lists.' }]],
    [{ members: [{ accountId: first.accountId, budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 1.5 }] }, [{ field: 'priority:business', group: 'members', member: { accountId: 'business', column: 'priority' }, text: 'Priority is a whole number of 0 or more.' }]],
    [{ members: [{ ...first, budgetDailyUsd: 0 }] }, [{ field: 'budget_daily:business', group: 'members', member: { accountId: 'business', column: 'budget_daily' }, text: 'A budget is a dollar amount above 0. Leave it empty for no budget.' }]],
    [{ members: [{ ...first, budgetMonthlyUsd: -1 }] }, [{ field: 'budget_monthly:business', group: 'members', member: { accountId: 'business', column: 'budget_monthly' }, text: 'A budget is a dollar amount above 0. Leave it empty for no budget.' }]],
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
    [{ fiveHourCeiling: 0.49 }, refused('5-hour ceiling (%)', 'utilization', '5-hour ceiling (%) is from 50 to 100.')],
    [{ fiveHourCeiling: Number.NaN }, refused('5-hour ceiling (%)', 'utilization', '5-hour ceiling (%) is from 50 to 100.')],
    [{ shedCap: 1.2 }, refused('Shed cap (%)', 'shedding', 'Shed cap (%) is from 10 to 100.')],
    [{ burnAverageSeconds: 599 }, refused('Burn average (s)', 'utilization', 'Burn average (s) is a whole number from 600 to 7,200.')],
    [{ shedDwellSeconds: 299 }, refused('Shed dwell (s)', 'shedding', 'Shed dwell (s) is a whole number from 300 to 7,200.')],
    [{ warmSessions: 'sometimes' }, refused('Warm sessions', 'shedding', 'Warm sessions is one of its listed choices.')],
    [{ inFlightCap: 0 }, refused('In flight', 'failover', 'In flight is a whole number from 1 to 4,294,967,295.')],
    [{ retryInPlaceSeconds: 301 }, refused('Retry in place (s)', 'failover', 'Retry in place (s) is a whole number from 0 to 300.')],
    [{ overageMoveGraceMinutes: 301 }, refused('Overage grace (min)', 'overage', 'Overage grace (min) is a whole number from 0 to 300.')],
    [{ windDownMinutes: 10_081 }, refused('Wind down (min)', 'overage', 'Wind down (min) is a whole number from 0 to 10,080.')],
    [
      { affinityTtlSeconds: 120, keepalive: { ...input.keepalive, enabled: true } },
      refused('Keep prompt caches warm', 'keepalive', 'Keep prompt caches warm needs Session affinity of at least 5m.'),
    ],
    [{ keepalive: { ...input.keepalive, maxIdleSeconds: 31_536_001 } }, refused('Max idle (s)', 'keepalive', 'Max idle (s) is a whole number from 0 to 31,536,000.')],
    [{ keepalive: { ...input.keepalive, maxParkedSeconds: 59 } }, refused('Max parked (s)', 'keepalive', 'Max parked (s) is a whole number from 60 to 604,800.')],
    [{ keepalive: { ...input.keepalive, scheduledHorizonSeconds: 604_801 } }, refused('Scheduled horizon (s)', 'keepalive', 'Scheduled horizon (s) is a whole number from 60 to 604,800.')],
  ];
  for (const [patch, expected, checks] of cases) assert.deepEqual(d.poolRefusals({ ...input, ...patch }, accounts, checks), expected, JSON.stringify(patch));
});

test('a retention while capture is on must be a whole number of hours inside the limit', () => {
  const retention = (typed, on = true, maxHours = 72) => d.poolRefusals({ ...input, bodyCaptureHours: typed }, accounts, { capture: { maxHours, on } });
  const refused = (maxHours) => [{ field: 'Capture retention in hours', group: 'bodyCapture', text: `Retention is a whole number of hours from 1 to ${maxHours}.` }];
  for (const typed of [Number.NaN, 0, 1.5, -1, 73]) assert.deepEqual(retention(typed), refused(72), `${typed} is refused`);
  for (const typed of [1, 72]) assert.deepEqual(retention(typed), [], `${typed} is inside`);
  assert.deepEqual(retention(Number.NaN, false), [], 'with capture off the field is not read');
  assert.deepEqual(retention(48, true, 24), refused(24), 'the limit is the one passed in');
});

test('a retention while the limit is unread checks the floor of 1 alone', () => {
  const retention = (typed) => d.poolRefusals({ ...input, bodyCaptureHours: typed }, accounts, { capture: { on: true } });
  const floor = [{ field: 'Capture retention in hours', group: 'bodyCapture', text: 'Retention is a whole number of hours of at least 1.' }];
  for (const typed of [Number.NaN, 0, 1.5, -1]) assert.deepEqual(retention(typed), floor, `${typed} is refused`);
  for (const typed of [1, 5, 500]) assert.deepEqual(retention(typed), [], `${typed} is inside, since no upper bound applies before the limit is read`);
});

test("a new pool's name is refused when its id is taken, by case or by punctuation, or holds no letter or digit", () => {
  const named = (name, isNew = true) => d.poolRefusals({ ...input, name }, accounts, { identity: { isNew, poolIds: new Set(['jobs', 'main']) } });
  const taken = [{ field: 'Name', group: 'identity', text: 'Another pool already has the id jobs.' }];
  assert.deepEqual(named('Jobs'), taken, 'by case');
  assert.deepEqual(named('jobs!'), taken, 'by punctuation');
  assert.deepEqual(named('研究'), [], 'letters outside ASCII are a name, which derives the id item');
  assert.deepEqual(named('--'), [{ field: 'Name', group: 'identity', text: 'The pool needs a name with a letter or a digit.' }], 'no letter or digit in any script');
  assert.deepEqual(named('Jobs 2'), [], 'jobs-2 is free');
  assert.deepEqual(named(''), [{ field: 'Name', group: 'identity', text: 'the pool needs a name.' }], 'an empty name draws its own sentence alone');
  assert.deepEqual(named('Jobs', false), [], 'a rename keeps its id and is never held for it');
  assert.equal(d.slug('jobs!'), 'jobs', 'the id is the one the create call derives');
});

test('a name of letters in any script derives item and is not unnamed', () => {
  assert.deepEqual(d.derivedId('研究'), { id: 'item', unnamed: false });
  assert.deepEqual(d.derivedId('--'), { id: 'item', unnamed: true });
});

test('the Keepalive group refuses its three numbers in the order it draws them', () => {
  // A numeric keepalive field added later gets a bound from KEEPALIVE_BOUNDS; this keeps it from escaping the refusals.
  const below = Object.fromEntries(Object.entries(d.KEEPALIVE_BOUNDS).map(([key, { min }]) => [key, min - 1]));
  const refused = d.poolRefusals(tuned(below), accounts);
  assert.deepEqual(
    refused.map((r) => [r.group, r.field]),
    [['keepalive', 'Max idle (s)'], ['keepalive', 'Max parked (s)'], ['keepalive', 'Scheduled horizon (s)']],
  );
  assert.deepEqual(Object.keys(d.KEEPALIVE_FIELD_LABELS).sort(), Object.keys(d.KEEPALIVE_BOUNDS).sort());
});

test("refusals come in rail order, a new pool's first two the name and the member", () => {
  const refusals = d.poolRefusals({ ...input, bodyCaptureHours: Number.NaN, name: '', members: [], modelRules: rules('a***') }, accounts, { capture: { maxHours: 72, on: true } });
  assert.deepEqual(
    refusals.map((r) => r.group),
    ['identity', 'members', 'models', 'bodyCapture'],
  );
  assert.deepEqual(
    refusals.slice(0, 2).map((r) => r.text),
    ['the pool needs a name.', 'the pool needs a member.'],
  );
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

test("the key dialog maps the server-side auto mode override to inherit, on, or off and back", () => {
  const row = fixture('keys').keys[0];
  const read = (wire) => d.keyFormOf(d.decodeKey({ ...row, auto_mode_server: wire }), 'unused').autoModeServer;
  assert.deepEqual([read(null), read(true), read(false)], ['inherit', 'on', 'off']);
  assert.equal(d.keyFormOf(null, 'all').autoModeServer, 'inherit', 'a new key inherits its pool');
  const form = d.keyFormOf(d.decodeKey(row), 'unused');
  const sent = (choice) => d.keyInputOf({ ...form, autoModeServer: choice }, NOW).autoModeServer;
  assert.deepEqual([sent('inherit'), sent('on'), sent('off')], [null, true, false]);
});

const camel = (s) => s.replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase());
// A probe may send fields beside the probed one, which the capture names under `with` and the replay sends the same
// way.
const keepaliveWith = (fields = {}) => Object.fromEntries(Object.entries(fields).map(([field, value]) => [camel(field.slice('keepalive.'.length)), value]));

/** The fraction probes and the field each one's refusal names in the dialog. */
const FRACTION_FIELDS = { five_hour_ceiling: '5-hour ceiling (%)', shed_cap: 'Shed cap (%)', weekly_ceiling: 'Weekly ceiling (%)' };

test('the form admits and refuses the values the routes do', () => {
  const { probes } = fixture('bound_refusals');
  assert.ok(probes.length > 0);
  assert.deepEqual(Object.keys(FRACTION_FIELDS).filter((field) => !probes.some((p) => p.field === field)), [], 'the capture probes the three fractions');
  for (const { field, route, status, value, with: fields } of probes) {
    let admitted;
    if (field in FRACTION_FIELDS) {
      const refusals = d.poolRefusals({ ...input, [camel(field)]: value }, accounts);
      assert.deepEqual(refusals.map((r) => r.field).filter((f) => f !== FRACTION_FIELDS[field]), [], `${field} ${value} refuses its own field alone`);
      admitted = refusals.length === 0;
    } else if (field.startsWith('keepalive.')) {
      const key = camel(field.slice('keepalive.'.length));
      assert.ok(key in d.KEEPALIVE_BOUNDS, `${field} is a field the form bounds`);
      admitted = d.poolRefusals(tuned({ ...keepaliveWith(fields), [key]: value }), accounts).length === 0;
    } else if (field === 'model_rules') {
      admitted = d.poolRefusals({ ...input, modelRules: Array.from({ length: value }, () => ({ not: true, pattern: 'claude-haiku-*' })) }, accounts).length === 0;
    } else {
      const key = camel(field);
      assert.ok(key in d.ENGINE_BOUNDS, `${field} is a field the form bounds`);
      admitted = d.poolRefusals({ ...input, [key]: value }, accounts).length === 0;
    }
    assert.equal(admitted, status >= 200 && status < 300, `${route} ${field} ${value}: the route answered ${status}`);
  }
});

test("the warnings mirror reads the pool routes' eleven checks on a pool's settings, sorted by code", () => {
  const row = fixture('pools').pools[0];
  assert.deepEqual(d.poolWarnings(d.decodePool(row), 0), d.decodePool(row).warnings, 'the captured pool reads the warnings the route answered');
  const quiet = {
    affinityTtlSeconds: 3_600,
    cacheTtl: { ...input.cacheTtl, enabled: true, injectWhenAbsent: true },
    fiveHourCeiling: 0.95,
    inFlightCap: 16,
    loadShedding: true,
    migrateWhenSpent: true,
    returnToPlan: 'always',
    returnToPlanDwellSeconds: 900,
    shedCap: 0.5,
    slowDown: false,
    slowDownHoldSeconds: 120,
    warmSessions: 'when_needed',
    weeklyCeiling: 1,
  };
  assert.deepEqual(d.poolWarnings(quiet, 16), [], 'a pool inside every check reads none, a weekly ceiling at the wall among them');
  const codes = (patch, cohort = 0) => d.poolWarnings({ ...quiet, ...patch }, cohort).map((w) => [w.code, w.fields]);
  assert.deepEqual(codes({ affinityTtlSeconds: 600 }), [['affinity_under_cache_ttl', ['affinity_ttl_seconds', 'cache_ttl']]], 'an affinity under the hour the plan main TTL injects');
  assert.deepEqual(codes({ affinityTtlSeconds: 600, cacheTtl: { ...quiet.cacheTtl, injectWhenAbsent: false } }), [], 'a policy that leaves an absent marker alone injects nothing');
  assert.deepEqual(codes({ affinityTtlSeconds: 600, cacheTtl: { ...quiet.cacheTtl, enabled: false } }), [], 'a policy that is off injects nothing');
  assert.deepEqual(codes({}, 17), [['cap_under_cohort', ['in_flight_cap']]]);
  assert.deepEqual(codes({ shedCap: 0.2 }), [['cap_under_quarter', ['shed_cap']]]);
  assert.deepEqual(codes({ fiveHourCeiling: 0.75 }), [['five_hour_ceiling_low', ['five_hour_ceiling']]]);
  assert.deepEqual(codes({ fiveHourCeiling: 1 }), [['five_hour_ceiling_without_margin', ['five_hour_ceiling']]]);
  assert.deepEqual(codes({ weeklyCeiling: 0.79 }), [['weekly_ceiling_low', ['weekly_ceiling']]], 'a weekly ceiling under 80 percent');
  assert.deepEqual(codes({ weeklyCeiling: 0.8 }), [], 'a weekly ceiling of 80 percent leaves no fifth unused');
  assert.deepEqual(codes({ loadShedding: false }), [['shedding_off', ['load_shedding', 'slow_down']]], 'the check reads both switches, and the dialog opens the first');
  assert.deepEqual(codes({ migrateWhenSpent: false }), [['spent_moves_off', ['migrate_when_spent']]]);
  assert.deepEqual(codes({ warmSessions: 'never' }), [['warm_never', ['warm_sessions']]]);
  const sentence = (patch) => d.poolWarnings({ ...quiet, ...patch }, 0).map((w) => [w.code, w.fields, w.text]);
  assert.deepEqual(sentence({ slowDownHoldSeconds: 151 }), [
    ['hold_over_window', ['slow_down_hold_seconds'], "A hold over 150 seconds leaves under 30 seconds of the client's first-byte window for the upstream's head, so a slowed request can time out at the client and be sent again."],
  ]);
  assert.deepEqual(codes({ slowDownHoldSeconds: 150 }), [], 'a hold of 150 seconds leaves the window its 30 seconds');
  assert.deepEqual(sentence({ returnToPlanDwellSeconds: 0 }), [
    ['return_at_once', ['return_to_plan_dwell_seconds'], "A return dwell of zero returns every warm session on its first request at the metered member, so the member serves one turn per session and the move's rewrite is paid each time."],
  ]);
  assert.deepEqual(codes({ returnToPlan: 'when_cold', returnToPlanDwellSeconds: 0 }), [], 'a mode that reads no dwell returns no warm session at once');
  assert.deepEqual(codes({ loadShedding: false, slowDown: true }), [], 'with the slow-down on a session over the ceiling is slowed rather than left to spend its window');
  const every = d.poolWarnings({ ...quiet, affinityTtlSeconds: 600, fiveHourCeiling: 0.75, loadShedding: false, migrateWhenSpent: false, returnToPlanDwellSeconds: 0, shedCap: 0.2, slowDownHoldSeconds: 300, warmSessions: 'never', weeklyCeiling: 0.75 }, 17);
  assert.deepEqual(
    every.map((w) => w.code),
    ['affinity_under_cache_ttl', 'cap_under_cohort', 'cap_under_quarter', 'five_hour_ceiling_low', 'hold_over_window', 'return_at_once', 'shedding_off', 'spent_moves_off', 'warm_never', 'weekly_ceiling_low'],
    'sorted by code',
  );
  assert.equal(every.find((w) => w.code === 'cap_under_cohort').text, 'The in-flight cap is under the largest live cohort of 17 agents, so subagents of one session wait for a slot they share.');
});

/** An account with the members the feature options and the standing read, ready, routing and granted both. */
const featureAccount = (over) => ({ canceled: false, featureGrants: ['artifacts', 'connectors'], id: 'alpha', kind: 'oauth', label: 'Alpha', planEndedAt: null, routingEnabled: true, status: 'ready', subscription: null, ...over });

test('the options reach beyond the pool and leave out what cannot be selected', () => {
  // Out of order, and alpha's label sorts apart from its id, so the order read is the labels'.
  const listed = [
    featureAccount({ id: 'charlie', label: 'Charlie' }),
    featureAccount({ featureGrants: null, id: 'delta', kind: 'api_key', label: 'Delta' }),
    featureAccount({ id: 'alpha', label: 'Zulu' }),
    featureAccount({ featureGrants: ['artifacts'], id: 'bravo', label: 'Bravo' }),
  ];
  const poolOf = { alpha: 'Main', bravo: 'Spare', delta: 'Main' };
  const options = d.featureAccountOptions(listed, 'connectors', (a) => poolOf[a.id] ?? null);
  assert.deepEqual(options.map((o) => [o.accountId, o.disabled]), [['bravo', true], ['charlie', false], ['alpha', false]]);
});

test('an account that is paused and signed out stands as signed out', () => {
  const line = (over) => d.featureStandingLine(featureAccount(over), NOW);
  const signedOut = line({ status: 'reauth_required' });
  assert.notEqual(signedOut, null, 'an account signed out alone draws a line');
  assert.equal(line({ routingEnabled: false, status: 'reauth_required' }), signedOut);
  assert.notEqual(line({ routingEnabled: false, status: 'reauth_required' }), line({ routingEnabled: false }), 'the line of an account paused alone');
  const notPaired = line({ status: 'unpaired' });
  assert.notEqual(notPaired, null, 'an account not paired alone draws a line');
  assert.equal(line({ routingEnabled: false, status: 'unpaired' }), notPaired);
});

test('a canceled account past its saved end stands as ended', () => {
  const now = Date.parse('2026-03-01T00:00:00Z');
  const line = (over) => d.featureStandingLine(featureAccount(over), now);
  const planEnded = line({ planEndedAt: '2026-02-01T00:00:00Z' });
  const paused = line({ routingEnabled: false });
  assert.notEqual(planEnded, null, 'an account whose plan ended draws a line');
  assert.notEqual(planEnded, paused);
  const canceled = (endsAt) => line({ canceled: true, routingEnabled: false, subscription: { endsAt } });
  assert.equal(canceled('2026-03-01T00:00:00Z'), planEnded, 'a saved end at the instant of now has passed');
  assert.equal(canceled('2026-03-01T00:00:01Z'), paused, 'a saved end a second ahead has not');
});
