import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';
import { cooldownBadge, fmtPeriodEnd, fmtRelative, fmtShortTime, INELIGIBLE_LABEL, LAST_RESORT_LABEL, overageBadge, poolStatusSentence, selectionOrder, spentBadge, windingDownBadge } from './dist/api.js';

const HOUR = 3_600_000;
const MINUTE = 60_000;
/** The instant every order is read at, so two resets built for one hour tie to the millisecond. */
const NOW = Date.parse('2026-09-18T12:00:00Z');
const hoursFromNow = (h) => new Date(NOW + h * HOUR).toISOString();

const account = (id, over = {}) => ({
  cooldowns: [],
  credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, refreshTokenExpiresAt: null },
  email: null,
  extraUsage: null,
  fableWindow: null,
  id,
  kind: 'oauth',
  label: id,
  plan: null,
  pools: [],
  requests: { last24h: 0, last7d: 0, lifetime: 0 },
  routingEnabled: true,
  status: 'ready',
  subscription: null,
  windows: [],
  ...over,
});
const window7d = (resetsAt, utilization = 0.5) => [{ id: '7d', label: '7-day', observedAt: '2026-09-18T00:00:00.000Z', periodHours: 168, resetsAt, utilization }];
const extraUsageOn = { disabledReason: null, enabled: true, inUse: true, monthlyLimitCredits: 100, usedCredits: 10 };
// The eligibility verdict, the Pace score, the fit and its runway ride on the member, because the router computes them
// against its own live state. A case states them there rather than re-deriving them from the account.
const member = (accountId, priority, eligible = 'eligible', paceScore = null) => ({ accountId, eligible, fit: null, paceScore, priority, runwayMs: null });
/** An eligible member whose fit reads `fit` with the runway `runwayMs`, at `priority` with the served `paceScore`. */
const fitted = (accountId, priority, fit, runwayMs, paceScore = null) => ({ ...member(accountId, priority, 'eligible', paceScore), fit, runwayMs });
/** An eligible member at priority 0 whose served Pace score is `paceScore`. */
const scored = (accountId, paceScore) => member(accountId, 0, 'eligible', paceScore);
const pool = (strategy, overagePolicy, members, over = {}) => {
  const merged = {
    activePins: 0,
    affinityTtlSeconds: 600,
    bodyCaptureHours: 0,
    cacheTtl: null,
    day24h: { billedUsd: 0, cost: 0, equivalent: 0 },
    id: 'p',
    keepalive: {
      enabled: false,
      maxIdleSeconds: 3600,
      maxParkedSeconds: null,
      scheduledHorizonSeconds: null,
    },
    members,
    name: 'p',
    overagePolicy,
    preferSubscription: false,
    requests24h: 0,
    status: 'active',
    strategy,
    traffic24h: [],
    warnings: [],
    // The store's defaults, the balanced presets.
    burnAverageSeconds: 1_800,
    fiveHourCeiling: 0.95,
    inFlightCap: 16,
    loadShedding: true,
    migrateWhenSpent: true,
    movesPerHour: {},
    overageMoveGraceMinutes: 30,
    retryInPlaceSeconds: 10,
    shedCap: 0.5,
    shedDwellSeconds: 900,
    warmSessions: 'when_needed',
    weeklyCeiling: 0.95,
    windDownMinutes: 30,
    ...over,
  };
  return merged;
};
const accounts = [account('over', { extraUsage: extraUsageOn }), account('plan')];

test('an overage P0 beside a plan-funded P1 follows each overage policy', () => {
  const tiers = [member('over', 0), member('plan', 1)];
  const prefer = selectionOrder(pool('priority', 'prefer_plan', tiers), accounts, NOW);
  assert.deepEqual(prefer[0].next, ['over'], 'prefer_plan keeps the overage account eligible and the router lands on it, since no plan-funded sibling shares its tier');
  assert.equal(prefer[0].placements[0].deferred, true);
  const never = selectionOrder(pool('priority', 'never', [member('over', 0, 'overage'), member('plan', 1)]), accounts, NOW);
  assert.deepEqual(never[0].next, [], 'never makes overage an obstacle');
  assert.equal(never[0].placements[0].ineligible, 'overage');
  assert.deepEqual(never[1].next, ['plan'], 'so the next fresh session lands one tier down');
  const allow = selectionOrder(pool('priority', 'allow', tiers), accounts, NOW);
  assert.deepEqual(allow[0].next, ['over']);
  assert.equal(allow[0].placements[0].deferred, false, 'allow ignores overage');
  const sameTier = selectionOrder(pool('priority', 'prefer_plan', [member('over', 0), member('plan', 0)]), accounts, NOW);
  assert.deepEqual(sameTier[0].next, ['plan'], 'inside one tier the plan-funded member ranks ahead of the deferred one');
  assert.deepEqual(
    sameTier[0].placements.map((p) => p.rank),
    [1, 2],
  );
});

test('pace ranks by the served score: a heavy burn behind an idle member, a member without one as untouched, and an API-key member by its own', () => {
  // The account rows read the other way round, so the order follows the route's score and no arithmetic of the
  // console's own: busy has more of its week left, but its sessions burn it faster than idle's pace allows.
  const pair = [account('busy', { windows: window7d(hoursFromNow(24), 0.3) }), account('idle', { windows: window7d(hoursFromNow(24), 0.6) })];
  const ranked = (tier) => tier.placements.map((p) => [p.account.id, p.rank]);
  const spare = selectionOrder(pool('pace', 'allow', [scored('busy', -0.004), scored('idle', 0.0167)]), pair, NOW);
  assert.deepEqual(ranked(spare[0]), [['idle', 1], ['busy', 2]], 'the spare rate ranks the idle member at 0.6 ahead of the busy one at 0.3');
  assert.deepEqual(spare[0].next, ['idle']);
  const tied = [...pair, account('third', { windows: window7d(hoursFromNow(24), 0.9) })];
  const even = selectionOrder(pool('pace', 'allow', [scored('busy', 0.01), scored('idle', 0.01), scored('third', 0.002)]), tied, NOW);
  assert.deepEqual(even[0].next.sort(), ['busy', 'idle'], 'two members with the same served score are both next');
  assert.deepEqual(ranked(even[0]), [['busy', 1], ['idle', 1], ['third', 2]]);
  // Nearly spent within the hour by its account row, yet the router has read no score for it.
  const unread = [account('unread', { windows: window7d(hoursFromNow(1), 0.95) }), ...pair];
  const fresh = selectionOrder(pool('pace', 'allow', [scored('busy', -0.004), scored('idle', 0.0167), scored('unread', null)]), unread, NOW);
  assert.deepEqual(ranked(fresh[0]), [['unread', 1], ['idle', 2], ['busy', 3]], 'a member without a served score ranks as untouched, ahead of every scored member');
  // The account rows carry no bucket, so only the served score tells the nearly full bucket from the untouched one.
  const keys = [account('full', { kind: 'api_key' }), account('room', { kind: 'api_key' })];
  const keyed = selectionOrder(pool('pace', 'allow', [scored('full', 0.03), scored('room', 1)]), keys, NOW);
  assert.deepEqual(ranked(keyed[0]), [['room', 1], ['full', 2]], 'an API-key member ranks by its served score, a nearly full bucket behind an untouched one');
  const random = selectionOrder(pool('priority', 'allow', [scored('busy', -0.004), scored('idle', 0.0167), scored('third', 0.002)]), tied, NOW);
  assert.deepEqual(random[0].next.sort(), ['busy', 'idle', 'third'], 'priority makes every eligible member of the first tier next');
});

test("the router's verdict decides eligibility and an obstacle trails its tier", () => {
  const trio = [account('busy'), account('cooled'), account('offline')];
  const members = [member('cooled', 0, 'cooldown'), member('offline', 0, 'unavailable'), member('busy', 0)];
  const tiers = selectionOrder(pool('priority', 'allow', members), trio, NOW);
  assert.deepEqual(tiers[0].next, ['busy'], 'only the member the router calls eligible is next');
  assert.deepEqual(
    tiers[0].placements.map((p) => [p.account.id, p.ineligible, p.rank]),
    [
      ['busy', null, 1],
      ['cooled', 'cooldown', null],
      ['offline', 'unavailable', null],
    ],
    'the obstacles trail the tier in label order, unranked, each carrying the reason the wire gave',
  );
});

test("a member's pins are the engine's count on the member, one per agent and model, whatever the session list holds", () => {
  const pair = [account('held'), account('free')];
  // One pinned session whose subagents run on a second model: the engine counts two pins where the session list holds
  // one pinned row. The order takes no session list, so neither that row nor one whose pin expired counts.
  const members = [
    { ...member('free', 0), activePins: 0 },
    { ...member('held', 0), activePins: 2 },
  ];
  const tiers = selectionOrder(pool('priority', 'allow', members), pair, NOW);
  assert.deepEqual(
    tiers[0].placements.map((p) => [p.account.id, p.pins]),
    [
      ['free', 0],
      ['held', 2],
    ],
  );
});

test('the kind order walks plan oauth, overage oauth, then API keys ahead of priority', () => {
  const trio = [account('key', { kind: 'api_key' }), account('over', { extraUsage: extraUsageOn }), account('plan')];
  const members = [member('key', 0), member('over', 0), member('plan', 1)];
  const tiers = selectionOrder(pool('priority', 'prefer_plan', members, { preferSubscription: true }), trio, NOW);
  assert.deepEqual(tiers.map((t) => [t.group, t.priority, t.next]), [[0, 1, ['plan']], [1, 0, []], [2, 0, []]], 'the plan-funded P1 comes first, then the overage P0, then the key');
  const unflagged = selectionOrder(pool('priority', 'prefer_plan', members), trio, NOW);
  assert.equal(unflagged[0].group, 0, 'with the flag off every member shares one group whatever the strategy');
  assert.deepEqual(selectionOrder(pool('round_robin', 'allow', [member('a', 0), member('b', 0)]), [account('a'), account('b')], NOW)[0].next.sort(), ['a', 'b'], 'round robin marks every member of the tier as next');
});

test('round robin ranks the overage subtier behind the plan-funded one it rotates', () => {
  const members = [member('over', 0), member('plan', 0)];
  const tiers = selectionOrder(pool('round_robin', 'prefer_plan', members), accounts, NOW);
  assert.deepEqual(
    tiers[0].placements.map((p) => [p.account.id, p.deferred, p.rank]),
    [
      ['plan', false, 1],
      ['over', true, 2],
    ],
    'the router rotates the plan-funded members alone while they last, so the overage one is not next',
  );
  assert.deepEqual(tiers[0].next, ['plan']);
  const allow = selectionOrder(pool('round_robin', 'allow', members), accounts, NOW);
  assert.deepEqual(allow[0].next.sort(), ['over', 'plan'], 'allow rotates them together, so both are next');
  const deferredOnly = selectionOrder(pool('round_robin', 'prefer_plan', [member('over', 0)]), accounts, NOW);
  assert.deepEqual(deferredOnly[0].next, ['over'], 'a subtier with nothing ahead of it is still the one the router takes');
});

test('round robin with the subscription preference walks the kind tiers first and rotates inside each', () => {
  const trio = [account('key', { kind: 'api_key' }), account('a'), account('b')];
  const members = [member('key', 0), member('a', 1), member('b', 1)];
  const tiers = selectionOrder(pool('round_robin', 'allow', members, { preferSubscription: true }), trio, NOW);
  assert.deepEqual(
    tiers.map((t) => [t.group, t.priority, t.next.sort()]),
    [
      [0, 1, ['a', 'b']],
      [2, 0, []],
    ],
    'the subscription tier leads although its priority is lower, and every member of it is the rotation\'s next',
  );
  assert.deepEqual(tiers[0].placements.map((p) => p.rank), [1, 1], 'the cursor, not a score, picks among them');
  const flat = selectionOrder(pool('round_robin', 'allow', members), trio, NOW);
  assert.deepEqual(flat.map((t) => [t.group, t.priority, t.next]), [[0, 0, ['key']], [0, 1, []]], 'without the preference priority alone orders the tiers');
});

test('pace places a winding-down member last and knows no floor', () => {
  const subscription = (over) => ({ billingType: 'monthly', endsAt: null, organizationType: 'claude_max', plan: 'max', rateLimitTier: 'max_5x', since: null, trialEndsAt: null, ...over });
  const trio = [
    account('fresh', { windows: window7d(hoursFromNow(24), 0.5) }),
    // Nearly spent, with the lowest served score of the tier: it ranks last among the ranked members and is no last
    // resort of its own.
    account('spent', { windows: window7d(hoursFromNow(0.5), 0.96) }),
    // Canceled and ten minutes from its end, inside the default half-hour wind-down, with the best score of all.
    account('ending', { canceled: true, subscription: subscription({ endsAt: new Date(NOW + 10 * MINUTE).toISOString() }), windows: window7d(hoursFromNow(24), 0.1) }),
    account('other', { windows: window7d(hoursFromNow(24), 0.5) }),
    account('key', { kind: 'api_key' }),
  ];
  const scores = { ending: 0.05, fresh: 0.02, key: 0.03, other: 0.02, spent: 0.0009 };
  const at = (id, priority = 0, fit = null) => ({ ...member(id, priority, 'eligible', scores[id]), fit });
  /** The member whose account ends inside the wind-down, as the route answers its fit. */
  const winding = (priority = 0) => at('ending', priority, 'ending');
  const ranked = (tier) => tier.placements.map((p) => [p.account.id, p.lastResort, p.rank]);
  assert.deepEqual(LAST_RESORT_LABEL, { ending: 'winding down', least_bad: 'least bad', rewrite_only: 'rewrite only', spent: 'window spent' }, 'each word past fits is a last resort with its label');
  const pace = selectionOrder(pool('pace', 'allow', [winding(), at('fresh'), at('spent')]), trio, NOW);
  assert.deepEqual(
    ranked(pace[0]),
    [
      ['fresh', null, 1],
      ['spent', null, 2],
      ['ending', 'ending', 3],
    ],
    'the nearly spent member ranks by its score, and the member winding down trails it',
  );
  assert.deepEqual(pace[0].next, ['fresh']);
  const lower = selectionOrder(pool('pace', 'allow', [winding(), at('other', 1)]), trio, NOW);
  assert.deepEqual(
    lower.map((t) => [t.priority, t.next]),
    [
      [0, []],
      [1, ['other']],
    ],
    'a member one tier down is taken before the member winding down in the tier above',
  );
  const lastResort = selectionOrder(pool('pace', 'allow', [winding(), at('spent')]), trio, NOW);
  assert.deepEqual(lastResort[0].next, ['spent'], 'with nothing else left, the nearly spent member places before a member winding down');
  const priority = selectionOrder(pool('priority', 'allow', [winding(), at('spent')]), trio, NOW);
  assert.deepEqual(
    ranked(priority[0]),
    [
      ['spent', null, 1],
      ['ending', 'ending', 2],
    ],
    'the wind-down holds under every strategy',
  );
  const unwound = selectionOrder(pool('pace', 'allow', [at('ending'), at('fresh')], { windDownMinutes: 5 }), trio, NOW);
  assert.deepEqual(unwound[0].next, ['ending'], "outside the pool's wind-down the member ranks by its score alone");
  assert.deepEqual(
    ranked(selectionOrder(pool('pace', 'allow', [at('fresh'), at('key')]), trio, NOW)[0]),
    [
      ['key', null, 1],
      ['fresh', null, 2],
    ],
    'an API-key member ranks by its served score',
  );
});

test("a winding-down placement carries the end it winds down to, and a member that is not winding down none", () => {
  const subscription = { billingType: 'monthly', endsAt: new Date(NOW + 10 * MINUTE).toISOString(), organizationType: 'claude_max', plan: 'max', rateLimitTier: 'max_5x', since: null, trialEndsAt: null };
  const pair = [account('ending', { canceled: true, subscription, windows: window7d(hoursFromNow(24), 0.1) }), account('fresh', { windows: window7d(hoursFromNow(24), 0.5) })];
  const [tier] = selectionOrder(pool('pace', 'allow', [{ ...member('ending', 0), fit: 'ending' }, member('fresh', 0)]), pair, NOW);
  assert.deepEqual(
    tier.placements.map((p) => [p.account.id, p.lastResort, p.end]),
    [
      ['fresh', null, null],
      ['ending', 'ending', NOW + 10 * MINUTE],
    ],
  );
});

test('the Standing badges read the time left or the reset time, and an overage or a spend cap whose reset is unknown says so on its title', () => {
  const until = new Date(NOW + HOUR + 11 * MINUTE).toISOString();
  assert.equal(cooldownBadge(until, NOW).text, 'cooldown 1h 11m');
  assert.equal(cooldownBadge(new Date(NOW + 25 * MINUTE).toISOString(), NOW).text, 'cooldown 25m', 'under an hour reads the minutes alone');
  const passed = new Date(NOW - MINUTE).toISOString();
  assert.equal(cooldownBadge(passed, NOW).text, 'cooldown', 'an end behind the instant reads no time left');
  assert.deepEqual(cooldownBadge(undefined, NOW), { text: 'cooldown', title: undefined }, 'a cooldown the account names no end for reads the word alone');
  // Built on the run's local calendar, since the badge tells the reset's day apart in the viewer's zone.
  const onLocalDay = (days, hours, minutes) => {
    const at = new Date(NOW);
    at.setDate(at.getDate() + days);
    at.setHours(hours, minutes, 0, 0);
    return at.toISOString();
  };
  const today = onLocalDay(0, 23, 42);
  const overage = overageBadge(today, NOW);
  assert.match(overage.text, /^overage until \d\d:\d\d$/, 'a reset on the same day reads to the minute');
  const tomorrow = onLocalDay(1, 1, 42);
  const later = overageBadge(tomorrow, NOW);
  assert.match(later.text, /^overage until [A-Z][a-z]{2} \d\d, \d\d:\d\d$/, 'a reset on another day names its day, so it never reads as today');
  assert.deepEqual(overageBadge(null, NOW), { text: 'overage', title: 'In overage, reset unknown' });
  assert.deepEqual(spentBadge(null, NOW), { text: 'spend cap reached', title: 'At the spend cap, reset unknown' });
  const end = NOW + 3 * 24 * HOUR;
  assert.equal(windingDownBadge(end, NOW).text, 'winding down 3d');
  assert.equal(windingDownBadge(NOW - MINUTE, NOW).text, 'winding down', 'an end behind the instant reads no time left');
});

/** A slow-down entry read `sinceAgo` before the instant, the stretch and the threshold in minutes. */
const slowDown = (factor, stretchMinutes, thresholdMinutes = 30, sinceAgo = 42 * MINUTE) => ({
  factor,
  holdMs: 120_000,
  ramp: null,
  since: new Date(NOW - sinceAgo).toISOString(),
  speed: factor,
  stretchMs: stretchMinutes * MINUTE,
  thresholdMs: thresholdMinutes * MINUTE,
});

test('a speed word and its hover read the factor, the since, the stretch against the threshold, and nothing on an unslowed account', () => {
  assert.deepEqual([0.5, 0.35, 0.95, 2 / 3, 0.3, 0].map(d.speedWord), ['0.5x', '0.35x', '0.95x', '0.67x', '0.3x', '0x'], 'two decimals with a trailing zero dropped');
  assert.equal(d.speedWord(0.996), '0.99x', 'a factor under 1 never reads 1x, since the chip draws under 1 alone');
  const since = new Date(NOW - 42 * MINUTE).toISOString();
  const tail = 'the sessions on this account would pass the ceiling 42 min before the reset';
  assert.equal(d.speedHover(slowDown(0.5, 42), NOW), `slowed since ${fmtShortTime(since)}: ${tail}, over the 30 min threshold. The pool computed 0.5x. The holds realized 0.5x`);
  assert.equal(
    d.speedHover(slowDown(0.7, 12), NOW),
    `slowed since ${fmtShortTime(since)}: the sessions on this account would pass the ceiling 12 min before the reset, under the 30 min threshold. The pool computed 0.7x. The holds realized 0.7x`,
    'no round fires under the threshold',
  );
  assert.equal(
    d.speedHover(slowDown(0.6, 30), NOW),
    `slowed since ${fmtShortTime(since)}: the sessions on this account would pass the ceiling 30 min before the reset, under the 30 min threshold. The pool computed 0.6x. The holds realized 0.6x`,
    'a stretch at the threshold reads under it, since a round fires only above it',
  );
  assert.equal(d.speedHover(slowDown(0.5, 42, 150), NOW), `slowed since ${fmtShortTime(since)}: ${tail}, under the 2 h 30 min threshold. The pool computed 0.5x. The holds realized 0.5x`, 'a threshold past an hour reads its hours');
  assert.equal(d.speedHover(null, NOW), null, 'nor one the router has not read');
});

/** A 5-hour window read an hour ago that resets at `resetsAt`, the window a slow-down entry is read from. */
const window5h = (resetsAt) => [{ id: '5h', label: '5-hour', observedAt: new Date(NOW - HOUR).toISOString(), periodHours: 5, resetsAt, utilization: 0.86 }];

test('a serving member reads the chip from its account\'s slow-down, from its own speed when the account is not loaded, and none when neither is set', () => {
  const read = slowDown(0.5, 42);
  const studio = account('studio', { slowDown: read, windows: window5h(hoursFromNow(1)) });
  assert.deepEqual(d.memberSpeed({ speed: 0.5 }, studio, NOW), { hover: d.speedHover(read, NOW), speed: 0.5 }, 'a loaded account reads its own slow-down');
  assert.deepEqual(d.memberSpeed({ speed: 0.4 }, studio, NOW), { hover: d.speedHover(read, NOW), speed: 0.5 }, "on a pool with the switch on, the account's entry wins over the member's figure");
  assert.deepEqual(d.memberSpeed({ speed: 0.5 }, account('plain', { slowDown: null }), NOW), { hover: null, speed: null }, 'a loaded account with no slow-down draws none');
  assert.deepEqual(d.memberSpeed({ speed: null }, account('full', { slowDown: slowDown(1, 0), windows: window5h(hoursFromNow(1)) }), NOW), { hover: null, speed: null }, 'an account at full speed draws none');
  assert.deepEqual(d.memberSpeed({ speed: 0.4 }, null, NOW), { hover: null, speed: 0.4 }, "a member whose account is not loaded reads the pool's own figure");
  // The card passes the account only on a pool whose switch is on, so a pool that holds nobody draws no chip for an
  // entry the purge has not yet dropped: the pools route serves its members no speed.
  assert.deepEqual(d.memberSpeed({ speed: null }, null, NOW), { hover: null, speed: null }, 'and none when neither is set, as on a pool with the switch off, where the card passes no account and the member serves no speed');
});

test("a loaded account whose 5-hour window reset after the slow-down's read draws no chip, its entry as stale as the window", () => {
  const read = slowDown(0.5, 42);
  const reset = account('studio', { slowDown: read, windows: window5h(new Date(NOW - 5 * MINUTE).toISOString()) });
  assert.deepEqual(d.memberSpeed({ speed: 0.5 }, reset, NOW), { hover: null, speed: null });
  assert.deepEqual(d.memberSpeed({ speed: 0.5 }, account('studio', { slowDown: read }), NOW), { hover: null, speed: null }, 'nor one with no 5-hour window to read it from');
});

test('a member at its budget is skipped with the reason the serving table names', () => {
  const tiers = selectionOrder(pool('priority', 'allow', [member('plan', 0, 'budget'), member('over', 0)]), accounts, NOW);
  assert.deepEqual(tiers[0].next, ['over'], 'the router passes over a member whose spend reached its budget');
  const skipped = tiers[0].placements.find((p) => p.account.id === 'plan');
  assert.deepEqual([skipped.ineligible, skipped.rank, INELIGIBLE_LABEL[skipped.ineligible]], ['budget', null, 'at budget']);
});

const labelOf = (id) => accounts.find((a) => a.id === id)?.label ?? id;
const read = (p, accs = accounts) => {
  const s = poolStatusSentence(p, accs, NOW);
  return `${s.word}: ${s.parts.map((part) => ('text' in part ? part.text : part.label)).join('')}`;
};
const linked = (p, accs = accounts) => poolStatusSentence(p, accs, NOW).parts.filter((part) => 'accountId' in part).map((part) => part.accountId);
const at = (p, status) => ({ ...p, status });

test('a member that does not fit stands behind a fitting one, whatever its score', () => {
  const pair = [account('roomy'), account('tight')];
  const [tier] = selectionOrder(pool('pace', 'allow', [fitted('roomy', 0, 'fits', null, 0.01), fitted('tight', 0, 'rewrite_only', HOUR, 0.05)]), pair, NOW);
  assert.deepEqual(
    tier.placements.map((p) => [p.account.id, p.lastResort, p.rank]),
    [
      ['roomy', null, 1],
      ['tight', 'rewrite_only', 2],
    ],
    'the fit ranks ahead of the better score, and the word it reads is the last resort',
  );
  assert.deepEqual(tier.next, ['roomy']);
});

test('with no fitting member the better word leads', () => {
  const pair = [account('tight'), account('worst')];
  const [tier] = selectionOrder(pool('pace', 'allow', [fitted('tight', 0, 'rewrite_only', HOUR, 0.01), fitted('worst', 0, 'least_bad', 3 * HOUR, 0.05)]), pair, NOW);
  assert.deepEqual(tier.next, ['tight'], 'the rewrite lands on tight, so its word leads the longer runway and the better score');
});

test('the longest runway leads across tiers', () => {
  const four = [account('p0'), account('p1'), account('q0'), account('q1')];
  const nexts = (members) => selectionOrder(pool('priority', 'allow', members), four, NOW).map((t) => [t.priority, t.next]);
  assert.deepEqual(
    nexts([fitted('p0', 0, 'rewrite_only', HOUR), fitted('p1', 1, 'rewrite_only', 3 * HOUR)]),
    [
      [0, []],
      [1, ['p1']],
    ],
    'the router keeps the longest runway over the pool before it picks a tier',
  );
  assert.deepEqual(
    nexts([fitted('q0', 0, 'rewrite_only', 3 * HOUR), fitted('q1', 1, 'rewrite_only', 3 * HOUR)]),
    [
      [0, ['q0']],
      [1, []],
    ],
    'of the tiers that share the longest runway the first one names next',
  );
});

test('an unbounded runway is the longest', () => {
  const pair = [account('bounded'), account('unbounded')];
  const [tier] = selectionOrder(pool('pace', 'allow', [fitted('bounded', 0, 'least_bad', 3 * HOUR, 0.05), fitted('unbounded', 0, 'least_bad', null, 0.01)]), pair, NOW);
  assert.deepEqual(tier.next, ['unbounded'], 'a null runway is no bound, so it leads three hours and the better score');
});

test('a spent window ranks behind the wind-down, and a pool of spent members still names its next', () => {
  const trio = [account('cooled'), account('ending'), account('spent')];
  const [tier] = selectionOrder(pool('pace', 'allow', [fitted('ending', 0, 'ending', null, 0.01), fitted('spent', 0, 'spent', null, 0.05)]), trio, NOW);
  assert.deepEqual(tier.next, ['ending'], 'the member that only ends takes the session ahead of the spent one with the better score');
  const spent = at(pool('pace', 'allow', [fitted('spent', 0, 'spent', null, 0.05), member('cooled', 0, 'cooldown')]), 'degraded');
  assert.deepEqual(selectionOrder(spent, trio, NOW)[0].next, ['spent'], 'with every eligible member spent the router still lands on one');
  assert.equal(read(spent, trio), 'Degraded: 1 of 2 members takes fresh sessions, and the next one goes to spent.', 'the sentence counts the spent member and names it');
});

test('the status sentence reads one row of the table per pool state', () => {
  assert.equal(read(at(pool('pace', 'allow', [member('plan', 0)]), 'disabled')), 'Disabled: the pool accepts no requests, but its keys and members stay.');
  assert.equal(read(at(pool('pace', 'allow', []), 'disabled')), 'Disabled: the pool accepts no requests, but its keys and members stay.', 'a disabled pool with no member');
  assert.equal(read(at(pool('pace', 'allow', []), 'exhausted')), 'Exhausted: the pool has no members yet.');
  assert.equal(read(at(pool('pace', 'allow', [member('plan', 0, 'cooldown')]), 'degraded')), 'Degraded: no member takes a fresh session right now.');
  assert.equal(read(at(pool('pace', 'allow', [member('plan', 0)]), 'active')), `Active: ${labelOf('plan')} takes every fresh session.`);
  assert.deepEqual(linked(at(pool('pace', 'allow', [member('plan', 0)]), 'active')), ['plan']);
  assert.equal(read(at(pool('priority', 'allow', [member('plan', 0), member('over', 1)]), 'degraded')), `Degraded: all 2 members take fresh sessions, and the next one goes to ${labelOf('plan')}.`);
  assert.equal(
    read(at(pool('priority', 'allow', [member('plan', 0), member('over', 0, 'cooldown'), member('key', 1, 'budget'), member('fresh', 1, 'overage')]), 'degraded')),
    `Degraded: 1 of 4 members takes fresh sessions, and the next one goes to ${labelOf('plan')}.`,
  );
});

test('the status sentence names several next accounts and the earliest return', () => {
  const two = at(pool('priority', 'allow', [member('plan', 0), member('over', 0)]), 'active');
  const names = [...selectionOrder(two, accounts, NOW)[0].next].map(labelOf);
  assert.equal(read(two), `Active: all 2 members take fresh sessions, and the next one goes to ${names[0]} or ${names[1]}.`);
  const withFresh = [...accounts, account('fresh')];
  const three = at(pool('priority', 'allow', [member('plan', 0), member('over', 0), member('fresh', 0)]), 'active');
  const trio = [...selectionOrder(three, withFresh, NOW)[0].next].map(labelOf);
  assert.equal(trio.length, 3);
  assert.equal(read(three, withFresh), `Active: all 3 members take fresh sessions, and the next one goes to ${trio[0]}, ${trio[1]} or ${trio[2]}.`);
  const daily = { ...member('key', 0, 'budget'), budgetUntil: hoursFromNow(5), spends: [{ period: 'day', periodEnd: hoursFromNow(5), spentUsd: 20 }] };
  const monthly = { ...member('plan', 0, 'budget'), budgetUntil: hoursFromNow(9), spends: [{ period: 'month', periodEnd: hoursFromNow(9), spentUsd: 300 }] };
  assert.equal(read(at(pool('pace', 'allow', [monthly, daily]), 'exhausted')), `Exhausted: no member can take a request until ${labelOf('key')}'s daily budget resets at ${fmtPeriodEnd(hoursFromNow(5))}.`);
  assert.equal(read(at(pool('pace', 'allow', [monthly]), 'exhausted')), `Exhausted: no member can take a request until ${labelOf('plan')}'s monthly budget resets at ${fmtPeriodEnd(hoursFromNow(9))}.`);
  const plain = { ...daily, spends: [] };
  assert.equal(read(at(pool('pace', 'allow', [plain]), 'exhausted')), `Exhausted: no member can take a request until ${labelOf('key')}'s budget resets at ${fmtPeriodEnd(hoursFromNow(5))}.`);
  const cooled = accounts.map((a) => (a.id === 'over' ? { ...a, cooldowns: [{ reason: 'rate_limited', scope: 'account', setAt: hoursFromNow(-1), until: hoursFromNow(1), windowId: null }] } : a));
  assert.equal(read(at(pool('pace', 'allow', [member('over', 0, 'cooldown'), daily]), 'exhausted'), cooled), `Exhausted: no member can take a request until ${labelOf('over')}'s cooldown ends ${fmtRelative(hoursFromNow(1), NOW)}.`);
  assert.equal(read(at(pool('pace', 'allow', [member('over', 0, 'cooldown'), member('fresh', 0, 'overage')]), 'exhausted')), 'Exhausted: no member can take a request, and none reports when it returns.');
});

test('the earliest return waits for every blocker a member reports', () => {
  const cooledFor = (h) => accounts.map((a) => (a.id === 'over' ? { ...a, cooldowns: [{ reason: 'rate_limited', scope: 'account', setAt: hoursFromNow(-1), until: hoursFromNow(h), windowId: null }] } : a));
  const both = { ...member('over', 0, 'cooldown'), budgetUntil: hoursFromNow(5), spends: [{ period: 'day', periodEnd: hoursFromNow(5), spentUsd: 20 }] };
  const monthly = { ...member('plan', 0, 'budget'), budgetUntil: hoursFromNow(9), spends: [{ period: 'month', periodEnd: hoursFromNow(9), spentUsd: 300 }] };
  assert.equal(read(at(pool('pace', 'allow', [monthly, both]), 'exhausted'), cooledFor(1)), `Exhausted: no member can take a request until ${labelOf('over')}'s daily budget resets at ${fmtPeriodEnd(hoursFromNow(5))}.`);
  assert.equal(read(at(pool('pace', 'allow', [monthly, both]), 'exhausted'), cooledFor(7)), `Exhausted: no member can take a request until ${labelOf('over')}'s cooldown ends ${fmtRelative(hoursFromNow(7), NOW)}.`);
  assert.equal(read(at(pool('pace', 'allow', [both]), 'exhausted')), 'Exhausted: no member can take a request, and none reports when it returns.');
});

test('the status sentence covers a member without an account and an order without a next', () => {
  assert.equal(read(at(pool('pace', 'allow', [member('ghost', 0)]), 'active')), 'Active: ghost takes every fresh session.');
  assert.deepEqual(linked(at(pool('pace', 'allow', [member('ghost', 0)]), 'active')), []);
  assert.equal(read(at(pool('pace', 'allow', [member('ghost', 0), member('plan', 0, 'cooldown')]), 'degraded')), 'Degraded: no listed member takes fresh sessions.');
});

test('under never an overage window holds a member past its cooldown or budget', () => {
  const overageUntil = (h) => ({ inUse: true, observedAt: hoursFromNow(-1), reset: h === null ? null : hoursFromNow(h), since: hoursFromNow(-2) });
  const cooledFor = (h) => [{ reason: 'rate_limited', scope: 'account', setAt: hoursFromNow(-1), until: hoursFromNow(h), windowId: null }];
  const pair = [account('held', { cooldowns: cooledFor(1), overage: overageUntil(3) }), account('cooled', { cooldowns: cooledFor(2) })];
  const both = [member('held', 0, 'cooldown'), member('cooled', 0, 'cooldown')];
  assert.equal(
    read(at(pool('pace', 'never', both), 'exhausted'), pair),
    `Exhausted: no member can take a request until cooled's cooldown ends ${fmtRelative(hoursFromNow(2), NOW)}.`,
    'the overage window outlasts the cooldown, so the member with a plain cooldown returns first',
  );
  assert.equal(read(at(pool('pace', 'never', [both[0]]), 'exhausted'), pair), `Exhausted: no member can take a request until held's overage window resets ${fmtRelative(hoursFromNow(3), NOW)}.`);
  const unknown = [account('held', { cooldowns: cooledFor(1), overage: overageUntil(null) })];
  assert.equal(read(at(pool('pace', 'never', [both[0]]), 'exhausted'), unknown), 'Exhausted: no member can take a request, and none reports when it returns.', 'an overage window without a reset gives no return');
  const budgeted = { ...member('held', 0, 'budget'), budgetUntil: hoursFromNow(5), spends: [{ period: 'day', periodEnd: hoursFromNow(5), spentUsd: 20 }] };
  assert.equal(read(at(pool('pace', 'never', [budgeted]), 'exhausted'), [account('held', { overage: overageUntil(7) })]), `Exhausted: no member can take a request until held's overage window resets ${fmtRelative(hoursFromNow(7), NOW)}.`);
  assert.equal(read(at(pool('pace', 'allow', both), 'exhausted'), pair), `Exhausted: no member can take a request until held's cooldown ends ${fmtRelative(hoursFromNow(1), NOW)}.`, 'under allow the overage window holds nobody');
});

test("an overage member returns at its overage window's reset", () => {
  const overage = (reset) => ({ inUse: true, observedAt: hoursFromNow(-1), reset, since: hoursFromNow(-2) });
  const over = [member('held', 0, 'overage')];
  assert.equal(read(at(pool('pace', 'never', over), 'exhausted'), [account('held', { overage: overage(hoursFromNow(3)) })]), `Exhausted: no member can take a request until held's overage window resets ${fmtRelative(hoursFromNow(3), NOW)}.`);
  assert.equal(read(at(pool('pace', 'never', over), 'exhausted'), [account('held', { overage: overage(null) })]), 'Exhausted: no member can take a request, and none reports when it returns.', 'an overage window without a reset gives no return');
  const budgeted = { ...member('plan', 0, 'budget'), budgetUntil: hoursFromNow(5), spends: [{ period: 'day', periodEnd: hoursFromNow(5), spentUsd: 20 }] };
  assert.equal(
    read(at(pool('pace', 'never', [...over, budgeted]), 'exhausted'), [account('held', { overage: overage(hoursFromNow(3)) }), account('plan')]),
    `Exhausted: no member can take a request until held's overage window resets ${fmtRelative(hoursFromNow(3), NOW)}.`,
    'the earliest return among the members',
  );
});

test("a spent member returns at its spend cap's reset", () => {
  const seat = account('seat', { spend: { resetsAt: hoursFromNow(4) } });
  assert.equal(read(at(pool('pace', 'allow', [member('seat', 0, 'spent')]), 'exhausted'), [seat]), `Exhausted: no member can take a request until seat's spend cap resets ${fmtRelative(hoursFromNow(4), NOW)}.`);
  assert.equal(read(at(pool('pace', 'allow', [member('seat', 0, 'spent')]), 'exhausted'), [account('seat', { spend: { resetsAt: null } })]), 'Exhausted: no member can take a request, and none reports when it returns.', 'a spend cap without a reset gives no return');
});

test('a usage-based member is a metered source: behind every plan account under the kind order, by priority otherwise, never deferred and never held for overage', () => {
  assert.deepEqual([...d.GROUP_LABEL], ['plan', 'overage', 'metered'], 'the third tier holds the API-key and the usage-based accounts alike');
  // The flags an older router answers on such an account read as overage, so the class has to win over them.
  const flagged = { extraUsage: extraUsageOn, overage: { inUse: true, observedAt: hoursFromNow(-1), reset: hoursFromNow(3), since: hoursFromNow(-2) }, usageBased: true };
  const trio = [account('seat', flagged), account('over', { extraUsage: extraUsageOn }), account('plan')];
  const kind = selectionOrder(pool('priority', 'prefer_plan', [member('seat', 0), member('over', 0), member('plan', 1)], { preferSubscription: true }), trio, NOW);
  assert.deepEqual(kind.map((t) => [t.group, t.priority, t.placements.map((p) => p.account.id)]), [[0, 1, ['plan']], [1, 0, ['over']], [2, 0, ['seat']]], 'the seat files in the third tier, behind the overage one');
  for (const strategy of ['pace', 'priority', 'round_robin']) {
    const flat = selectionOrder(pool(strategy, 'prefer_plan', [member('seat', 0), member('plan', 1)]), trio, NOW);
    assert.deepEqual(flat.map((t) => [t.group, t.priority, t.placements.map((p) => [p.account.id, p.deferred])]), [[0, 0, [['seat', false]]], [0, 1, [['plan', false]]]], `${strategy}: by priority alone, never deferred`);
    assert.deepEqual(flat[0].next, ['seat'], strategy);
  }
  const cooled = [account('seat', { ...flagged, cooldowns: [{ reason: 'rate_limited', scope: 'account', setAt: hoursFromNow(-1), until: hoursFromNow(1), windowId: null }] })];
  assert.equal(read(at(pool('pace', 'never', [member('seat', 0, 'cooldown')]), 'exhausted'), cooled), `Exhausted: no member can take a request until seat's cooldown ends ${fmtRelative(hoursFromNow(1), NOW)}.`, 'the never policy reads no overage end on the seat');
});

test('while the accounts are unread the status sentence says only what the verdicts carry', () => {
  const degraded = at(pool('priority', 'allow', [member('plan', 0), member('over', 0, 'cooldown')]), 'degraded');
  const active = at(pool('priority', 'allow', [member('plan', 0), member('over', 1)]), 'active');
  const single = at(pool('pace', 'allow', [member('plan', 0)]), 'active');
  const exhausted = at(pool('pace', 'allow', [member('over', 0, 'cooldown')]), 'exhausted');
  const cooled = accounts.map((a) => (a.id === 'over' ? { ...a, cooldowns: [{ reason: 'rate_limited', scope: 'account', setAt: hoursFromNow(-1), until: hoursFromNow(1), windowId: null }] } : a));
  const unread = [degraded, active, single, exhausted].map((p) => read(p, null));
  assert.deepEqual(unread, ['Degraded: 1 of 2 members takes fresh sessions.', 'Active: all 2 members take fresh sessions.', "Active: the pool's one member takes every fresh session.", 'Exhausted: no member can take a request.']);
  assert.deepEqual(
    [degraded, active, single, exhausted].flatMap((p) => poolStatusSentence(p, null, NOW).parts.filter((part) => !('text' in part) || /\b(over|plan)\b/.test(part.text))),
    [],
    'no account part and no raw account id',
  );
  assert.equal(read(degraded), `Degraded: 1 of 2 members takes fresh sessions, and the next one goes to ${labelOf('plan')}.`);
  assert.equal(read(single), `Active: ${labelOf('plan')} takes every fresh session.`);
  assert.equal(read(exhausted, cooled), `Exhausted: no member can take a request until ${labelOf('over')}'s cooldown ends ${fmtRelative(hoursFromNow(1), NOW)}.`);
});
