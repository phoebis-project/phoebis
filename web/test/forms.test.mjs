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
  windDownMinutes: pool.windDownMinutes,
  windowCeiling: pool.windowCeiling,
};
const tuned = (patch) => ({ ...input, keepalive: { ...input.keepalive, ...patch } });

test('a pool input the router would take validates', () => {
  assert.equal(d.validatePoolInput(input, accounts), true);
  assert.equal(d.validatePoolInput({ ...input, affinityTtlSeconds: 0, keepalive: { ...input.keepalive, enabled: false } }, accounts), true, '0 turns affinity off');
  assert.equal(d.validatePoolInput({ ...input, affinityTtlSeconds: 60, keepalive: { ...input.keepalive, enabled: false } }, accounts), true);
  assert.equal(d.validatePoolInput({ ...input, loadShedding: false }, accounts), true, 'the switch is a toggle with no bound');
});

test('a pool input refuses the affinity the router refuses', () => {
  const off = { ...input.keepalive, enabled: false };
  for (const seconds of [1, 30, 59]) assert.equal(d.validatePoolInput({ ...input, affinityTtlSeconds: seconds, keepalive: off }, accounts), false, `${seconds} s is neither off nor a minute`);
  assert.equal(d.validatePoolInput({ ...input, affinityTtlSeconds: 90.5, keepalive: off }, accounts), false, 'the route takes whole seconds');
  assert.equal(d.validatePoolInput({ ...input, affinityTtlSeconds: 120, keepalive: { ...input.keepalive, enabled: true } }, accounts), false, 'warming needs a pin of five minutes');
});

test('a pool input refuses a keepalive idle below 0, a fraction of a second and an empty ceiling, and takes a null one', () => {
  // The captured route probes hold each field's bound pairs, and none sends an idle below its floor of 0.
  assert.equal(d.validatePoolInput(tuned({ maxIdleSeconds: -1 }), accounts), false, 'maxIdleSeconds -1 is below the floor');
  assert.equal(d.validatePoolInput(tuned({ maxIdleSeconds: 2.5 }), accounts), false, 'seconds are whole');
  assert.equal(d.validatePoolInput(tuned({ maxParkedSeconds: null, scheduledHorizonSeconds: null }), accounts), true, 'a null ceiling is the break-even');
  assert.equal(d.validatePoolInput(tuned({ maxParkedSeconds: Number.NaN }), accounts), false, 'a switched-on ceiling left empty is held');
});

test('a pool input refuses members the member write would refuse', () => {
  assert.equal(d.validatePoolInput({ ...input, members: [] }, accounts), false, 'a pool needs a member');
  assert.equal(d.validatePoolInput({ ...input, members: [...input.members, { accountId: 'ghost', priority: 0 }] }, accounts), false, 'an account the console does not know');
  assert.equal(d.validatePoolInput({ ...input, members: [{ accountId: input.members[0].accountId, priority: -1 }] }, accounts), false, 'priorities are unsigned');
  assert.equal(d.validatePoolInput({ ...input, members: [{ accountId: input.members[0].accountId, priority: 1.5 }] }, accounts), false, 'priorities are whole');
  assert.equal(d.validatePoolInput({ ...input, members: [{ accountId: input.members[0].accountId, priority: 7 }] }, accounts), true);
});

test('a members list that did not load holds the save once and names no member', () => {
  const held = { field: 'members', group: 'members', text: 'the account list did not load, so the members cannot be checked.' };
  assert.deepEqual(d.poolRefusals(input, null), [held]);
  assert.equal(d.validatePoolInput(input, null), false);
  const ghost = { accountId: 'ghost', budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 0 };
  assert.deepEqual(d.poolRefusals({ ...input, members: [...input.members, ghost] }, null), [held], 'an id the list may hold is not refused');
});

test('a pool input refuses thresholds outside their range and an empty name', () => {
  assert.equal(d.validatePoolInput({ ...input, name: '' }, accounts), false);
  assert.equal(d.validatePoolInput({ ...input, windowCeiling: 0.5 }, accounts), true, 'a ceiling runs from half of a window');
  assert.equal(d.validatePoolInput({ ...input, windowCeiling: 1 }, accounts), true, 'to the wall itself');
  assert.equal(d.validatePoolInput({ ...input, windowCeiling: 0.49 }, accounts), false);
  assert.equal(d.validatePoolInput({ ...input, windowCeiling: 1.01 }, accounts), false);
  assert.equal(d.validatePoolInput({ ...input, windowCeiling: Number.NaN }, accounts), false, 'a field left empty holds the save');
  assert.equal(d.validatePoolInput({ ...input, shedCap: 0.1 }, accounts), true, 'a cap runs from a tenth of the burn');
  assert.equal(d.validatePoolInput({ ...input, shedCap: 1 }, accounts), true);
  assert.equal(d.validatePoolInput({ ...input, shedCap: 0.09 }, accounts), false);
  assert.equal(d.validatePoolInput({ ...input, shedCap: 1.01 }, accounts), false);
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
  for (const [field, value] of Object.entries(halves)) assert.equal(d.validatePoolInput({ ...input, [field]: value }, accounts), false, `${field} is whole`);
  for (const mode of ['always', 'never', 'when_needed']) assert.equal(d.validatePoolInput({ ...input, warmSessions: mode }, accounts), true);
  assert.equal(d.validatePoolInput({ ...input, warmSessions: 'sometimes' }, accounts), false);
  assert.equal(d.validatePoolInput({ ...input, migrateWhenSpent: false }, accounts), true);
  for (const mode of ['always', 'never', 'when_cold']) assert.equal(d.validatePoolInput({ ...input, returnToPlan: mode }, accounts), true);
  assert.deepEqual(d.poolRefusals({ ...input, returnToPlan: 'sometimes' }, accounts), [{ field: 'Return to plan', group: 'overage', text: 'Return to plan is one of its listed choices.' }]);
  assert.equal(d.validatePoolInput({ ...input, slowDown: true }, accounts), true, 'the slow-down is a toggle with no bound');
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
    d.poolRefusals({ ...input, burnAverageSeconds: 599, shedCap: 1.2, windowCeiling: 0.49 }, accounts).map((r) => [r.group, r.field]),
    [
      ['utilization', 'Window ceiling (%)'],
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
    for (const value of taken) assert.equal(d.validatePoolInput({ ...input, [field]: value }, accounts), true, `${field} ${value} is taken`);
    assert.equal(d.validatePoolInput({ ...input, [field]: unknown }, accounts), false, `${field} ${unknown} is refused`);
  }
  for (const tokens of [0, 20_000, 1_000_000]) assert.equal(d.validatePoolInput({ ...input, minSharedPrefixTokens: tokens }, accounts), true, `${tokens} tokens is taken`);
  for (const tokens of [-1, 1.5, Number.NaN]) assert.equal(d.validatePoolInput({ ...input, minSharedPrefixTokens: tokens }, accounts), false, `${tokens} tokens is refused`);
});

test('a member budget is a dollar amount above 0 or none, a daily and a monthly side by side', () => {
  for (const v of ['', '  ', '0.01', '12.5', '50', '1e3']) assert.equal(d.budgetRefusal(v), null, `${JSON.stringify(v)} is taken`);
  for (const v of ['0', '-1', 'x', 'Infinity', '12,50']) assert.equal(d.budgetRefusal(v), 'A budget is a dollar amount above 0. Leave it empty for no budget.', `${JSON.stringify(v)} is refused`);
  const [first] = input.members;
  const withBudget = (budget) => d.validatePoolInput({ ...input, members: [{ ...first, ...budget }] }, accounts);
  assert.equal(withBudget({ budgetDailyUsd: 12.5, budgetMonthlyUsd: null }), true);
  assert.equal(withBudget({ budgetDailyUsd: 5, budgetMonthlyUsd: 50 }), true, 'both budgets at once');
  assert.equal(withBudget({ budgetDailyUsd: null, budgetMonthlyUsd: null }), true, 'no budget');
  assert.equal(withBudget({ budgetDailyUsd: 0, budgetMonthlyUsd: null }), false, 'the member write refuses a daily budget of 0');
  assert.equal(withBudget({ budgetDailyUsd: null, budgetMonthlyUsd: 0 }), false, 'the member write refuses a monthly budget of 0');
  assert.equal(withBudget({ budgetDailyUsd: 5, budgetMonthlyUsd: Number.NaN }), false, 'a field that read no number');
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
  assert.equal(d.validatePoolInput({ ...input, modelRules: rules('claude-*') }, accounts), true);
  assert.equal(d.validatePoolInput({ ...input, modelRules: rules('claude-*', 'a***') }, accounts), false);
});

test("the key dialog's holds agree with its Save condition", () => {
  assert.deepEqual(d.KEY_GROUPS, ['key', 'access', 'models', 'guardrails', 'sessions', 'capture']);
  const named = { captureHours: '6', captureMode: 'inherit', modelRules: [], name: 'workstation', poolId: 'all' };
  assert.deepEqual(d.keyRefusals(named), [], 'a named key in a pool with no rules may save');
  assert.deepEqual(d.keyRefusals({ ...named, name: '' }), [{ field: 'Name', group: 'key', text: 'the key needs a name.' }]);
  assert.deepEqual(d.keyRefusals({ ...named, name: '   ' }), [{ field: 'Name', group: 'key', text: 'the key needs a name.' }], 'a name of spaces is saved trimmed, so it is no name');
  assert.deepEqual(d.keyRefusals({ ...named, poolId: '' }), [{ field: 'Pool', group: 'key', text: 'the key needs a pool.' }]);
  assert.deepEqual(d.keyRefusals({ ...named, modelRules: rules('a***') }), [{ field: 'Model rules', group: 'models', text: d.modelRulesRefusal(rules('a***')) }]);
  assert.deepEqual(
    d.keyRefusals({ captureHours: '', captureMode: 'override', modelRules: rules('a***'), name: '', poolId: '' }, { maxHours: 72 }).map((r) => [r.group, r.field]),
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
  const form = { captureHours: '6', captureMode: 'override', modelRules: [], name: 'workstation', poolId: 'all' };
  const hours = (captureHours, captureMode = 'override', checks = { maxHours: 72 }) => d.keyRefusals({ ...form, captureHours, captureMode }, checks);
  const refused = (text) => [{ field: 'Capture hours for this key', group: 'capture', text }];
  for (const v of ['', '0', '2.5', '73']) assert.deepEqual(hours(v), refused('Capture hours is a whole number from 1 to 72.'), `${JSON.stringify(v)} is refused`);
  for (const v of ['1', '72']) assert.deepEqual(hours(v), [], `${v} is inside`);
  assert.deepEqual(hours('', 'inherit'), [], 'Inherit never reads the field');
  assert.deepEqual(hours('', 'off'), [], 'Off never reads the field');
  for (const v of ['', '0', '2.5']) assert.deepEqual(hours(v, 'override', {}), refused('Capture hours is a whole number of at least 1.'), `${JSON.stringify(v)} is refused before the ceiling is read`);
  assert.deepEqual(hours('500', 'override', {}), [], 'before the ceiling is read no upper bound applies');
  const row = fixture('keys').keys[0];
  const stored = (hours) => d.keyFormOf(d.decodeKey({ ...row, body_capture_hours: hours }), 'unused');
  assert.deepEqual([stored(12).captureMode, stored(12).captureHours], ['override', '12'], 'an override opens as the text of its hours');
  assert.deepEqual([stored(0).captureMode, stored(0).captureHours], ['off', '6'], 'off opens with the default hours in the field');
  assert.equal(d.keyFormOf(null, 'all').captureHours, '6');
  const sent = (patch) => d.keyInputOf({ ...stored(12), ...patch }, NOW).bodyCaptureHours;
  assert.deepEqual([sent({}), sent({ captureHours: '48' }), sent({ captureMode: 'off' }), sent({ captureMode: 'inherit' })], [12, 48, 0, null]);
});

test('a pool refusal names the group its control sits in', () => {
  assert.deepEqual(d.poolRefusals(input, accounts), [], 'the stored pool is refused nothing');
  const [first] = input.members;
  const refused = (field, group, text) => [{ field, group, text }];
  const cases = [
    [{ name: '' }, refused('Name', 'identity', 'the pool needs a name.')],
    [{ members: [] }, refused('members', 'members', 'the pool needs a member.')],
    [{ members: [...input.members, { accountId: 'ghost', priority: 0 }] }, refused('member:ghost', 'members', 'ghost is not an account the console lists.')],
    [{ members: [{ accountId: first.accountId, priority: 1.5 }] }, refused('priority:business', 'members', 'Priority is a whole number of 0 or more.')],
    [{ members: [{ ...first, budgetDailyUsd: 0 }] }, refused('budget_daily:business', 'members', 'A budget is a dollar amount above 0. Leave it empty for no budget.')],
    [{ members: [{ ...first, budgetMonthlyUsd: -1 }] }, refused('budget_monthly:business', 'members', 'A budget is a dollar amount above 0. Leave it empty for no budget.')],
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
    [{ windowCeiling: 0.49 }, refused('Window ceiling (%)', 'utilization', 'Window ceiling (%) is from 50 to 100.')],
    [{ windowCeiling: Number.NaN }, refused('Window ceiling (%)', 'utilization', 'Window ceiling (%) is from 50 to 100.')],
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
  const retention = (typed, on = true, maxHours = 72) => d.poolRefusals(input, accounts, { capture: { maxHours, on, typed } });
  const refused = (maxHours) => [{ field: 'Capture retention in hours', group: 'bodyCapture', text: `Retention is a whole number of hours from 1 to ${maxHours}.` }];
  for (const typed of ['', '0', '1.5', '-1', '73']) assert.deepEqual(retention(typed), refused(72), `${JSON.stringify(typed)} is refused`);
  for (const typed of ['1', '72']) assert.deepEqual(retention(typed), [], `${typed} is inside`);
  assert.deepEqual(retention('', false), [], 'with capture off the field is not read');
  assert.deepEqual(retention('48', true, 24), refused(24), 'the limit is the one passed in');
  assert.equal(d.validatePoolInput(input, accounts, { capture: { maxHours: 72, on: true, typed: '' } }), false);
  assert.equal(d.validatePoolInput(input, accounts, { capture: { maxHours: 72, on: true, typed: '24' } }), true);
});

test('a retention while the limit is unread checks the floor of 1 alone', () => {
  const retention = (typed) => d.poolRefusals(input, accounts, { capture: { on: true, typed } });
  const floor = [{ field: 'Capture retention in hours', group: 'bodyCapture', text: 'Retention is a whole number of hours of at least 1.' }];
  for (const typed of ['', '0', '1.5', '-1']) assert.deepEqual(retention(typed), floor, `${JSON.stringify(typed)} is refused`);
  for (const typed of ['1', '5', '500']) assert.deepEqual(retention(typed), [], `${typed} is inside, since no upper bound applies before the limit is read`);
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
  const refusals = d.poolRefusals({ ...input, name: '', members: [], modelRules: rules('a***') }, accounts, { capture: { maxHours: 72, on: true, typed: '' } });
  assert.deepEqual(
    refusals.map((r) => r.group),
    ['identity', 'members', 'models', 'bodyCapture'],
  );
  assert.deepEqual(
    refusals.slice(0, 2).map((r) => r.text),
    ['the pool needs a name.', 'the pool needs a member.'],
  );
  assert.deepEqual(d.POOL_GROUPS, ['identity', 'members', 'keys', 'models', 'guardrails', 'autoMode', 'selection', 'placement', 'pinning', 'utilization', 'shedding', 'slowDown', 'failover', 'overage', 'cacheTtl', 'keepalive', 'bodyCapture', 'cacheDiagnostics']);
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
const FRACTION_FIELDS = { shed_cap: 'Shed cap (%)', window_ceiling: 'Window ceiling (%)' };

test('the form admits and refuses the values the routes do', () => {
  const { probes } = fixture('bound_refusals');
  assert.ok(probes.length > 0);
  assert.deepEqual(Object.keys(FRACTION_FIELDS).filter((field) => !probes.some((p) => p.field === field)), [], 'the capture probes both fractions');
  for (const { field, route, status, value, with: fields } of probes) {
    let admitted;
    if (field in FRACTION_FIELDS) {
      const refusals = d.poolRefusals({ ...input, [camel(field)]: value }, accounts);
      assert.deepEqual(refusals.map((r) => r.field).filter((f) => f !== FRACTION_FIELDS[field]), [], `${field} ${value} refuses its own field alone`);
      admitted = refusals.length === 0;
    } else if (field.startsWith('keepalive.')) {
      const key = camel(field.slice('keepalive.'.length));
      assert.ok(key in d.KEEPALIVE_BOUNDS, `${field} is a field the form bounds`);
      admitted = d.validatePoolInput(tuned({ ...keepaliveWith(fields), [key]: value }), accounts);
    } else if (field === 'model_rules') {
      admitted = d.validatePoolInput({ ...input, modelRules: Array.from({ length: value }, () => ({ not: true, pattern: 'claude-haiku-*' })) }, accounts);
    } else {
      const key = camel(field);
      assert.ok(key in d.ENGINE_BOUNDS, `${field} is a field the form bounds`);
      admitted = d.validatePoolInput({ ...input, [key]: value }, accounts);
    }
    assert.equal(admitted, status >= 200 && status < 300, `${route} ${field} ${value}: the route answered ${status}`);
  }
});

test("the warnings mirror reads the pool routes' ten checks on a pool's settings, sorted by code", () => {
  const row = fixture('pools').pools[0];
  assert.deepEqual(d.poolWarnings(d.decodePool(row), 0), d.decodePool(row).warnings, 'the captured pool reads the warnings the route answered');
  const quiet = {
    affinityTtlSeconds: 3_600,
    cacheTtl: { ...input.cacheTtl, enabled: true, injectWhenAbsent: true },
    inFlightCap: 16,
    loadShedding: true,
    migrateWhenSpent: true,
    returnToPlan: 'always',
    returnToPlanDwellSeconds: 900,
    shedCap: 0.5,
    slowDown: false,
    slowDownHoldSeconds: 120,
    warmSessions: 'when_needed',
    windowCeiling: 0.95,
  };
  assert.deepEqual(d.poolWarnings(quiet, 16), [], 'a pool inside every check reads none');
  const codes = (patch, cohort = 0) => d.poolWarnings({ ...quiet, ...patch }, cohort).map((w) => [w.code, w.fields]);
  assert.deepEqual(codes({ affinityTtlSeconds: 600 }), [['affinity_under_cache_ttl', ['affinity_ttl_seconds', 'cache_ttl']]], 'an affinity under the hour the plan main TTL injects');
  assert.deepEqual(codes({ affinityTtlSeconds: 600, cacheTtl: { ...quiet.cacheTtl, injectWhenAbsent: false } }), [], 'a policy that leaves an absent marker alone injects nothing');
  assert.deepEqual(codes({ affinityTtlSeconds: 600, cacheTtl: { ...quiet.cacheTtl, enabled: false } }), [], 'a policy that is off injects nothing');
  assert.deepEqual(codes({}, 17), [['cap_under_cohort', ['in_flight_cap']]]);
  assert.deepEqual(codes({ shedCap: 0.2 }), [['cap_under_quarter', ['shed_cap']]]);
  assert.deepEqual(codes({ windowCeiling: 0.75 }), [['ceiling_low', ['window_ceiling']]]);
  assert.deepEqual(codes({ windowCeiling: 1 }), [['ceiling_without_margin', ['window_ceiling']]]);
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
  const every = d.poolWarnings({ ...quiet, affinityTtlSeconds: 600, loadShedding: false, migrateWhenSpent: false, returnToPlanDwellSeconds: 0, shedCap: 0.2, slowDownHoldSeconds: 300, warmSessions: 'never', windowCeiling: 0.75 }, 17);
  assert.deepEqual(every.map((w) => w.code), ['affinity_under_cache_ttl', 'cap_under_cohort', 'cap_under_quarter', 'ceiling_low', 'hold_over_window', 'return_at_once', 'shedding_off', 'spent_moves_off', 'warm_never'], 'sorted by code');
  assert.equal(every.find((w) => w.code === 'cap_under_cohort').text, 'The in-flight cap is under the largest live cohort of 17 agents, so subagents of one session wait for a slot they share.');
});
