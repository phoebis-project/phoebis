import assert from 'node:assert/strict';
import { test } from 'node:test';
import { fmtPeriodEnd, fmtRelative, INELIGIBLE_LABEL, poolStatusSentence, selectionOrder } from './dist/api.js';

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
// The eligibility verdict rides on the member, because the router computes it against its own live cooldowns and
// overages; a case states it there rather than re-deriving it from the account.
const member = (accountId, priority, eligible = 'eligible') => ({ accountId, eligible, priority });
const pool = (strategy, overagePolicy, members, over = {}) => {
  const merged = {
    activePins: 0,
    affinityTtlSeconds: 600,
    bodyCaptureHours: 0,
    cacheTtl: null,
    cost24h: 0,
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
    shedAheadOfPace: null,
    status: 'active',
    strategy,
    traffic24h: [],
    // The store's defaults, the balanced presets.
    burnWindowSeconds: 600,
    inFlightCap: 16,
    migrateWhenSpent: true,
    movesPerHour: {},
    overageMoveGraceMinutes: 30,
    paceFloor: 0.05,
    retryInPlaceSeconds: 10,
    shedShare: 0.5,
    sourceDwellSeconds: 900,
    warmSessions: 'when_needed',
    windDownMinutes: 30,
    ...over,
  };
  return merged;
};
const accounts = [account('over', { extraUsage: extraUsageOn }), account('plan')];

test('an overage P0 beside a plan-funded P1 follows each overage policy', () => {
  const tiers = [member('over', 0), member('plan', 1)];
  const prefer = selectionOrder(pool('priority', 'prefer_plan', tiers), accounts, [], NOW);
  assert.deepEqual(prefer[0].next, ['over'], 'prefer_plan keeps the overage account eligible and the router lands on it, since no plan-funded sibling shares its tier');
  assert.equal(prefer[0].placements[0].deferred, true);
  const never = selectionOrder(pool('priority', 'never', [member('over', 0, 'overage'), member('plan', 1)]), accounts, [], NOW);
  assert.deepEqual(never[0].next, [], 'never makes overage an obstacle');
  assert.equal(never[0].placements[0].ineligible, 'overage');
  assert.deepEqual(never[1].next, ['plan'], 'so the next fresh session lands one tier down');
  const allow = selectionOrder(pool('priority', 'allow', tiers), accounts, [], NOW);
  assert.deepEqual(allow[0].next, ['over']);
  assert.equal(allow[0].placements[0].deferred, false, 'allow ignores overage');
  const sameTier = selectionOrder(pool('priority', 'prefer_plan', [member('over', 0), member('plan', 0)]), accounts, [], NOW);
  assert.deepEqual(sameTier[0].next, ['plan'], 'inside one tier the plan-funded member ranks ahead of the deferred one');
  assert.deepEqual(
    sameTier[0].placements.map((p) => p.rank),
    [1, 2],
  );
});

test('equal pace scores share a rank, an unsampled account outranks a used one, and every minimum is next', () => {
  const tied = [
    account('a', { windows: window7d(hoursFromNow(24), 0.5) }),
    account('b', { windows: window7d(hoursFromNow(24), 0.5) }),
    account('c', { windows: window7d(hoursFromNow(24), 0.9) }),
  ];
  const trio = tied.map((a) => member(a.id, 0));
  const pace = selectionOrder(pool('pace', 'allow', trio), tied, [], NOW);
  assert.deepEqual(pace[0].next.sort(), ['a', 'b'], 'two members with the same remaining allowance and reset are both next');
  assert.deepEqual(
    pace[0].placements.map((p) => [p.account.id, p.rank]),
    [
      ['a', 1],
      ['b', 1],
      ['c', 2],
    ],
  );
  // Nearly spent over a whole week left is a worse rate than an untouched account's, which the fallback
  // treats as a full week's remaining allowance.
  const untouched = [account('a', { windows: window7d(hoursFromNow(168), 0.95) }), account('b')];
  const ranked = selectionOrder(pool('pace', 'allow', untouched.map((a) => member(a.id, 0))), untouched, [], NOW);
  assert.deepEqual(ranked[0].next, ['b'], 'an unsampled account counts as untouched, ahead of one with a worse remaining-over-hours rate');
  const random = selectionOrder(pool('priority', 'allow', trio), tied, [], NOW);
  assert.deepEqual(random[0].next.sort(), ['a', 'b', 'c'], 'priority makes every eligible member of the first tier next');
});

test("the router's verdict decides eligibility and an obstacle trails its tier", () => {
  const trio = [account('busy'), account('cooled'), account('offline')];
  const members = [member('cooled', 0, 'cooldown'), member('offline', 0, 'unavailable'), member('busy', 0)];
  const tiers = selectionOrder(pool('priority', 'allow', members), trio, [], NOW);
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

test('a pin counts only while the session holds it', () => {
  const pair = [account('held'), account('free')];
  const sessions = [
    { accountId: 'held', pinned: true },
    { accountId: 'held', pinned: false },
    { accountId: 'free', pinned: false },
  ];
  const tiers = selectionOrder(pool('priority', 'allow', [member('free', 0), member('held', 0)]), pair, sessions, NOW);
  assert.deepEqual(
    tiers[0].placements.map((p) => [p.account.id, p.pins]),
    [
      ['free', 0],
      ['held', 1],
    ],
    'an expired session still sits in the list but no longer pins its account',
  );
});

test('the kind order walks plan oauth, overage oauth, then Anthropic keys ahead of priority', () => {
  const trio = [account('key', { kind: 'api_key' }), account('over', { extraUsage: extraUsageOn }), account('plan')];
  const members = [member('key', 0), member('over', 0), member('plan', 1)];
  const tiers = selectionOrder(pool('priority', 'prefer_plan', members, { preferSubscription: true }), trio, [], NOW);
  assert.deepEqual(tiers.map((t) => [t.group, t.priority, t.next]), [[0, 1, ['plan']], [1, 0, []], [2, 0, []]], 'the plan-funded P1 comes first, then the overage P0, then the key');
  const unflagged = selectionOrder(pool('priority', 'prefer_plan', members), trio, [], NOW);
  assert.equal(unflagged[0].group, 0, 'with the flag off every member shares one group whatever the strategy');
  assert.deepEqual(selectionOrder(pool('round_robin', 'allow', [member('a', 0), member('b', 0)]), [account('a'), account('b')], [], NOW)[0].next.sort(), ['a', 'b'], 'round robin marks every member of the tier as next');
});

test('round robin ranks the overage subtier behind the plan-funded one it rotates', () => {
  const members = [member('over', 0), member('plan', 0)];
  const tiers = selectionOrder(pool('round_robin', 'prefer_plan', members), accounts, [], NOW);
  assert.deepEqual(
    tiers[0].placements.map((p) => [p.account.id, p.deferred, p.rank]),
    [
      ['plan', false, 1],
      ['over', true, 2],
    ],
    'the router rotates the plan-funded members alone while they last, so the overage one is not next',
  );
  assert.deepEqual(tiers[0].next, ['plan']);
  const allow = selectionOrder(pool('round_robin', 'allow', members), accounts, [], NOW);
  assert.deepEqual(allow[0].next.sort(), ['over', 'plan'], 'allow rotates them together, so both are next');
  const deferredOnly = selectionOrder(pool('round_robin', 'prefer_plan', [member('over', 0)]), accounts, [], NOW);
  assert.deepEqual(deferredOnly[0].next, ['over'], 'a subtier with nothing ahead of it is still the one the router takes');
});

test('round robin with the subscription preference walks the kind tiers first and rotates inside each', () => {
  const trio = [account('key', { kind: 'api_key' }), account('a'), account('b')];
  const members = [member('key', 0), member('a', 1), member('b', 1)];
  const tiers = selectionOrder(pool('round_robin', 'allow', members, { preferSubscription: true }), trio, [], NOW);
  assert.deepEqual(
    tiers.map((t) => [t.group, t.priority, t.next.sort()]),
    [
      [0, 1, ['a', 'b']],
      [2, 0, []],
    ],
    'the subscription tier leads although its priority is lower, and every member of it is the rotation\'s next',
  );
  assert.deepEqual(tiers[0].placements.map((p) => p.rank), [1, 1], 'the cursor, not a score, picks among them');
  const flat = selectionOrder(pool('round_robin', 'allow', members), trio, [], NOW);
  assert.deepEqual(flat.map((t) => [t.group, t.priority, t.next]), [[0, 0, ['key']], [0, 1, []]], 'without the preference priority alone orders the tiers');
});

test('pace places the floor group and a winding-down member last', () => {
  const subscription = (over) => ({ billingType: 'monthly', derivedEndsAt: null, endsAt: null, organizationType: 'claude_max', plan: 'max', rateLimitTier: 'max_5x', since: null, trialEndsAt: null, ...over });
  const trio = [
    account('fresh', { windows: window7d(hoursFromNow(24), 0.5) }),
    // Its window resets in half an hour, so its rate would lead the tier if the floor did not hold it back.
    account('spent', { windows: window7d(hoursFromNow(0.5), 0.96) }),
    // Cancelled and ten minutes from its end, inside the default half-hour wind-down, with the best rate of all.
    account('ending', { cancelled: true, subscription: subscription({ endsAt: new Date(NOW + 10 * MINUTE).toISOString() }), windows: window7d(hoursFromNow(24), 0.1) }),
    account('other', { windows: window7d(hoursFromNow(24), 0.5) }),
  ];
  const ranked = (tier) => tier.placements.map((p) => [p.account.id, p.lastResort, p.rank]);
  const pace = selectionOrder(pool('pace', 'allow', [member('ending', 0), member('fresh', 0), member('spent', 0)]), trio, [], NOW);
  assert.deepEqual(
    ranked(pace[0]),
    [
      ['fresh', null, 1],
      ['spent', 'floor', 2],
      ['ending', 'winding_down', 3],
    ],
    'the floor group trails the ranked members, and the member winding down trails the floor group',
  );
  assert.deepEqual(pace[0].next, ['fresh']);
  const lower = selectionOrder(pool('pace', 'allow', [member('ending', 0), member('other', 1), member('spent', 0)]), trio, [], NOW);
  assert.deepEqual(
    lower.map((t) => [t.priority, t.next]),
    [
      [0, []],
      [1, ['other']],
    ],
    'a member above the floor one tier down is taken before the last resort of the tier above',
  );
  const lastResort = selectionOrder(pool('pace', 'allow', [member('ending', 0), member('spent', 0)]), trio, [], NOW);
  assert.deepEqual(lastResort[0].next, ['spent'], 'with nothing else left, the floor group places before a member winding down');
  const priority = selectionOrder(pool('priority', 'allow', [member('ending', 0), member('spent', 0)]), trio, [], NOW);
  assert.deepEqual(
    ranked(priority[0]),
    [
      ['spent', null, 1],
      ['ending', 'winding_down', 2],
    ],
    'priority reads no floor, but the wind-down holds under every strategy',
  );
  const unwound = selectionOrder(pool('pace', 'allow', [member('ending', 0), member('fresh', 0)], { windDownMinutes: 5 }), trio, [], NOW);
  assert.deepEqual(unwound[0].next, ['ending'], "outside the pool's wind-down the end only shortens the hours its rate divides by");
  const trial = [account('trial', { subscription: subscription({ trialEndsAt: hoursFromNow(2) }), windows: window7d(hoursFromNow(100), 0.5) }), trio[0]];
  assert.deepEqual(selectionOrder(pool('pace', 'allow', [member('fresh', 0), member('trial', 0)]), trial, [], NOW)[0].next, ['trial'], 'a trial end ahead of the reset is the hours the allowance must last');
  const keyed = [account('key', { kind: 'api_key', windows: [{ id: '5h', label: '5-hour', observedAt: '2026-09-18T00:00:00.000Z', periodHours: 5, resetsAt: hoursFromNow(1), utilization: 0.97 }] }), trio[0]];
  assert.deepEqual(
    ranked(selectionOrder(pool('pace', 'allow', [member('fresh', 0), member('key', 0)]), keyed, [], NOW)[0]),
    [
      ['fresh', null, 1],
      ['key', 'floor', 2],
    ],
    'an Anthropic-key member reads its worst reported window against the floor',
  );
});

test('a member at its budget is skipped with the reason the serving table names', () => {
  const tiers = selectionOrder(pool('priority', 'allow', [member('plan', 0, 'budget'), member('over', 0)]), accounts, [], NOW);
  assert.deepEqual(tiers[0].next, ['over'], 'the router passes over a member whose spend reached its budget');
  const skipped = tiers[0].placements.find((p) => p.account.id === 'plan');
  assert.deepEqual([skipped.ineligible, skipped.rank, INELIGIBLE_LABEL[skipped.ineligible]], ['budget', null, 'at budget']);
});

const labelOf = (id) => accounts.find((a) => a.id === id)?.label ?? id;
const read = (p, accs = accounts, sessions = []) => {
  const s = poolStatusSentence(p, accs, sessions, NOW);
  return `${s.word}: ${s.parts.map((part) => ('text' in part ? part.text : part.label)).join('')}`;
};
const linked = (p, accs = accounts) => poolStatusSentence(p, accs, [], NOW).parts.filter((part) => 'accountId' in part).map((part) => part.accountId);
const at = (p, status) => ({ ...p, status });

test('the status sentence reads one row of the table per pool state', () => {
  assert.equal(read(at(pool('pace', 'allow', [member('plan', 0)]), 'disabled')), 'Disabled: the pool accepts no requests; its keys and members stay.');
  assert.equal(read(at(pool('pace', 'allow', []), 'active')), 'Active: the pool has no members yet.');
  assert.equal(read(at(pool('pace', 'allow', [member('plan', 0, 'cooldown')]), 'degraded')), 'Degraded: no member takes a fresh session right now.');
  assert.equal(read(at(pool('pace', 'allow', [member('plan', 0)]), 'active')), `Active: ${labelOf('plan')} takes every fresh session.`);
  assert.deepEqual(linked(at(pool('pace', 'allow', [member('plan', 0)]), 'active')), ['plan']);
  assert.equal(read(at(pool('priority', 'allow', [member('plan', 0), member('over', 1)]), 'degraded')), `Degraded: all 2 members take fresh sessions; the next one goes to ${labelOf('plan')}.`);
  assert.equal(
    read(at(pool('priority', 'allow', [member('plan', 0), member('over', 0, 'cooldown'), member('key', 1, 'budget'), member('fresh', 1, 'overage')]), 'degraded')),
    `Degraded: 1 of 4 members takes fresh sessions; the next one goes to ${labelOf('plan')}.`,
  );
});

test('the status sentence names several next accounts and the earliest return', () => {
  const two = at(pool('priority', 'allow', [member('plan', 0), member('over', 0)]), 'active');
  const names = [...selectionOrder(two, accounts, [], NOW)[0].next].map(labelOf);
  assert.equal(read(two), `Active: all 2 members take fresh sessions; the next one goes to ${names[0]} or ${names[1]}.`);
  const withFresh = [...accounts, account('fresh')];
  const three = at(pool('priority', 'allow', [member('plan', 0), member('over', 0), member('fresh', 0)]), 'active');
  const trio = [...selectionOrder(three, withFresh, [], NOW)[0].next].map(labelOf);
  assert.equal(trio.length, 3);
  assert.equal(read(three, withFresh), `Active: all 3 members take fresh sessions; the next one goes to ${trio[0]}, ${trio[1]} or ${trio[2]}.`);
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

test('while the accounts are unread the status sentence says only what the verdicts carry', () => {
  const degraded = at(pool('priority', 'allow', [member('plan', 0), member('over', 0, 'cooldown')]), 'degraded');
  const active = at(pool('priority', 'allow', [member('plan', 0), member('over', 1)]), 'active');
  const single = at(pool('pace', 'allow', [member('plan', 0)]), 'active');
  const exhausted = at(pool('pace', 'allow', [member('over', 0, 'cooldown')]), 'exhausted');
  const cooled = accounts.map((a) => (a.id === 'over' ? { ...a, cooldowns: [{ reason: 'rate_limited', scope: 'account', setAt: hoursFromNow(-1), until: hoursFromNow(1), windowId: null }] } : a));
  const unread = [degraded, active, single, exhausted].map((p) => read(p, null));
  assert.deepEqual(unread, ['Degraded: 1 of 2 members takes fresh sessions.', 'Active: all 2 members take fresh sessions.', "Active: the pool's one member takes every fresh session.", 'Exhausted: no member can take a request.']);
  assert.deepEqual(
    [degraded, active, single, exhausted].flatMap((p) => poolStatusSentence(p, null, [], NOW).parts.filter((part) => !('text' in part) || /\b(over|plan)\b/.test(part.text))),
    [],
    'no account part and no raw account id',
  );
  assert.equal(read(degraded), `Degraded: 1 of 2 members takes fresh sessions; the next one goes to ${labelOf('plan')}.`);
  assert.equal(read(single), `Active: ${labelOf('plan')} takes every fresh session.`);
  assert.equal(read(exhausted, cooled), `Exhausted: no member can take a request until ${labelOf('over')}'s cooldown ends ${fmtRelative(hoursFromNow(1), NOW)}.`);
});
