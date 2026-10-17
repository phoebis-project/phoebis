import assert from 'node:assert/strict';
import { test } from 'node:test';

// The mock skips its draws by the local hour and every instant below is chosen in UTC, so the file runs there whatever
// the host's zone; `node --test` gives each file a process of its own.
process.env.TZ = 'UTC';

const { accountSlots, slowedLongOf } = await import('./dist/api.js');

const HOUR = 3_600_000;
const DAY = 24 * HOUR;
const startOfHour = (ms) => Math.floor(ms / HOUR) * HOUR;
const iso = (ms) => new Date(ms).toISOString();

/** A rollup row's requests, as the analytics routes count them. */
const requestsOf = (r) => r.requests.ok + r.requests.incomplete + r.requests.rate_limited + r.requests.overloaded + r.requests.upstream_error + r.requests.client_gone;
const tokensOf = (r) => r.tokens.uncachedInput + r.tokens.cacheRead + r.tokens.cacheWrite5m + r.tokens.cacheWrite1h + r.tokens.output;
const sum = (list, read) => list.reduce((n, x) => n + read(x), 0);
const near = (a, b) => Math.abs(a - b) < 1e-6;

/** The engine's pin key, so a session's subagents on another model, or on another account, hold pins of their own. */
const pinOf = (r) => `${r.keyId}|${r.sessionId}|${r.agentId}|${r.model}`;
/** The pins the story's routed requests wrote, each under the session the router routed it by. */
const routedPinsOf = (s) => new Set(s.requests.filter((r) => r.kind !== 'probe' && r.sessionId !== null).map((r) => `${r.keyId}|${r.sessionId}|${r.agentId}|${r.canonicalModel}`));
/**
 * The recipes whose pins the router holds: every live one, and a released one whose routed request the ledger holds,
 * since the mod's release ends the keepalive record and leaves the pin that request wrote.
 */
const pinnedRecipesOf = (s, recipes) => {
  const routed = routedPinsOf(s);
  return recipes.filter((r) => r.agentState !== 'released' || routed.has(pinOf(r)));
};

/** The names the mock's story used before it read as one operator's, and the admin harness's own. */
const RETIRED = ['business', 'codex', 'Console workspace', 'console', 'direct', 'evals', 'Evals workspace', 'mail', 'personal', 'team'];

/**
 * The story as every view reads it, loaded at `at`: the mock draws its story from the clock it loads at, so each
 * instant loads a copy of the bundle of its own and every window below is measured from that same instant.
 */
async function storyAt(at) {
  const wall = Date.now;
  Date.now = () => Date.parse(at);
  let d;
  try {
    d = await import(`./dist/api.js?consistency=${at}`);
  } finally {
    Date.now = wall;
  }
  const api = new d.MockApi();
  await api.login('admin');
  const [accounts, pools, keys, sessions, attention, overview] = await Promise.all([api.listAccounts(), api.listPools(), api.listKeys(), api.listSessions(), api.getAttention(), api.getOverview()]);
  // The probes too, which the list leaves out unless asked, since the story's checks read every row it holds.
  const { requests } = await api.listRequests({ includeProbes: true, limit: 100_000 });
  const events = (await Promise.all(requests.map((r) => api.getRequest(r.id)))).flatMap((detail) => detail?.events ?? []);
  const now = Date.parse(at);
  // The day the dashboard draws: 24 hourly buckets, the current hour last. An account's or a pool's day total, and a
  // pool's 25 hourly buckets, start an hour earlier, on the hour a full day back, and the week on the hour seven days
  // back, as the routes total their rollups.
  const dayFrom = startOfHour(now) - 23 * HOUR;
  const totalsFrom = startOfHour(now) - 24 * HOUR;
  const weekFrom = startOfHour(now) - 7 * DAY;
  const counted = requests.filter((r) => r.kind !== 'probe' && Date.parse(r.at) >= dayFrom);
  const totalled = requests.filter((r) => r.kind !== 'probe' && Date.parse(r.at) >= totalsFrom);
  const rollups = d.ALL_ROLLUPS.filter((r) => r.kind !== 'probe');
  const settings = await api.getSettings();
  return { accounts, allRollups: d.ALL_ROLLUPS, api, attention, counted, dayFrom, events, keys, leadOf: d.leadOf, lib: d, now, overview, pools, requests, rollups, sessions, settings, totalled, totalledFrom: totalsFrom, weekFrom };
}

/** Every decision record of the story, read as the routing log answers them, newest first. */
async function decisionsOf(s) {
  const from = iso(Math.min(...s.requests.map((r) => Date.parse(r.at))) - HOUR);
  const records = [];
  let cursor;
  do {
    const page = await s.api.listRoutingLog({ cursor, from, limit: 200, to: iso(s.now) });
    // Each mock answer waits out its latency, so a page's records are read together rather than one by one.
    records.push(...(await Promise.all(page.rows.filter((row) => row.type === 'decision').map((row) => s.api.getDecision(row.requestId, row.seq)))));
    cursor = page.nextCursor ?? undefined;
  } while (cursor !== undefined);
  return records.filter((r) => r !== null);
}

/**
 * The members a record's partitions kept, the set the engine's pick orders by tier and then by score: a member stays
 * through a partition that dropped others only by passing it alone.
 */
function keptOf(d, r) {
  const facts = r.walk.find((w) => w.step === 'partitions')?.facts ?? [];
  return narrowedSetOf(r).filter((c) => {
    const alone = d.partitionsOf([{ ...c }], d.poolById.get(r.request.poolId), d.requestById.get(r.requestId), moving(r));
    return facts.every((f, i) => typeof f.value !== 'number' || alone[i].value === 0);
  });
}

/** Every row of the story's routing log under the route's default kinds, a decision as its summary, newest first. */
async function logOf(s) {
  const from = iso(Math.min(...s.requests.map((r) => Date.parse(r.at))) - 7 * DAY);
  const rows = [];
  let cursor;
  do {
    const page = await s.api.listRoutingLog({ cursor, from, limit: 200, to: iso(s.now + DAY) });
    rows.push(...page.rows);
    cursor = page.nextCursor ?? undefined;
  } while (cursor !== undefined);
  return rows;
}

/** The members the store's `pool_diff` names, whose changed ones a pool edit carries as pairs. */
const POOL_DIFF_MEMBERS = [
  'affinity_mode', 'affinity_ttl_seconds', 'auto_mode_server', 'body_capture_hours', 'burn_average_seconds', 'cache_diagnostics', 'cache_ttl', 'enabled', 'in_flight_cap', 'label', 'load_shedding',
  'migrate_when_spent', 'min_shared_prefix_tokens', 'model_rules', 'overage_move_grace_minutes', 'overage_policy', 'prefer_subscription', 'retry_in_place_seconds', 'return_to_plan',
  'return_to_plan_dwell_seconds', 'session_placement', 'shed_cap', 'shed_dwell_seconds', 'slow_down', 'slow_down_hold_seconds', 'slow_down_stretch_seconds', 'strategy', 'subagent_placement',
  'warm_sessions', 'wind_down_minutes', 'window_ceiling',
];
/** The fields an account edit carries as pairs, beside the flag a staged secret sets. */
const ACCOUNT_DIFF_MEMBERS = ['cancelled', 'enabled', 'label', 'state', 'subscription_ends_at'];

/** Whether one tier ranks before another, compared member by member as the engine orders them. */
const tierBefore = (x, y) => {
  const i = x.findIndex((n, j) => n !== y[j]);
  return i >= 0 && x[i] < y[i];
};
const sameTier = (x, y) => x.tier.join('/') === y.tier.join('/');

/** Whether a record's partitions narrow a move's destinations, which the no-fit partition keeps as its gate left them. */
const moving = (r) => r.walk.some((w) => w.step === 'cap_filter');

/**
 * The set a record's partitions narrow: a move's destinations (the cap filter's facts name them), else the eligible
 * members.
 */
function narrowedSetOf(r) {
  const capFilter = r.walk.find((w) => w.step === 'cap_filter');
  const among = capFilter ? new Set(capFilter.facts.map((f) => f.accountId)) : null;
  return r.candidates.filter((c) => (among ? among.has(c.accountId) : c.obstacle === null));
}

/**
 * The members a placement's fit reads: the set the spent and cap partitions left, or the whole set the cap holds for
 * a slot when every member is at it. A Fable request's spent member drops unless the pool funds it.
 */
function fitReadOf(set, pool, r) {
  const spent = (c) => ['five_hour', 'seven_day', 'seven_day_fable'].some((w) => (c.windows[w]?.utilization ?? 0) >= 1);
  const funded = (c) => pool.overagePolicy === 'allow' && c.credits !== null && c.credits.blocked === null;
  const left = r.request.canonicalModel.includes('fable') ? set.filter((c) => !spent(c) || funded(c)) : set;
  const free = left.filter((c) => r.request.flags.count || c.inFlight < c.inFlightCap);
  return new Set(free.length > 0 ? free : left);
}

/**
 * A fresh session's fit on a member as a placement reads it: an account inside the pool's wind-down before its end
 * never fits, an API-key member fits while its worst bucket has room, and on every plan window the model draws the
 * body's rewrite must land under the ceiling and the account's burn with the fresh prior must not carry it there
 * before the horizon. The story's hand-written placements price the prior, a drawn one none.
 */
function freshFitOf(d, c, r, pool) {
  const at = Date.parse(r.request.at);
  const end = c.term?.endsAt ? Date.parse(c.term.endsAt) : null;
  if (end !== null && end - at <= pool.windDownMinutes * 60_000) return 'ending';
  if (c.kind === 'api_key') return (c.windows.worst_bucket?.utilization ?? 0) < 1 ? 'fits' : 'least_bad';
  const prior = d.STORY_IDS.has(r.requestId) ? d.PRIOR_BURN : {};
  let lands = true;
  let every = true;
  for (const [w, share] of Object.entries(d.rewriteOf(r.request.bodyTokens ?? 0, r.request.canonicalModel))) {
    const x = c.windows[w];
    if (!x || x.projection === null || x.horizon === null) continue;
    const room = pool.windowCeiling - x.utilization - (x.reserved ?? 0) - share;
    const burn = (c.burn[w] ?? 0) + (prior[w] ?? 0);
    if (room <= 0) {
      lands = false;
      every = false;
    } else if (burn > 0 && (room / burn) * HOUR < Date.parse(x.horizon) - at) every = false;
  }
  return every ? 'fits' : lands ? 'rewrite_only' : 'least_bad';
}

test("the mock's slowed-account fold reads its bar from the start rows and the spans standing at the day's cutoff", () => {
  const now = Date.parse('2026-09-16T14:30:00Z');
  const cutoff = now - DAY;
  const MINUTE = 60_000;
  // A pool whose threshold is 40 minutes, above the 30 whose bar the floor of an hour would also give.
  const started = (at, factor) => ({ accountId: 'studio', at: iso(at), detail: { factor, threshold_ms: 40 * MINUTE }, kind: 'slow_down_started', poolId: 'wide' });
  const ended = (at, since, durationMs, factorMin) => ({ accountId: 'studio', at: iso(at), detail: { duration_ms: durationMs, factor_min: factorMin, since }, kind: 'slow_down_ended', poolId: 'wide' });
  const rows = [
    // A span standing at the cutoff, 50 of its 80 minutes inside the day, and an hour's span later in the day.
    started(cutoff - 30 * MINUTE, 0.6),
    ended(cutoff + 52 * MINUTE, cutoff - 30 * MINUTE, 80 * MINUTE, 0.5),
    started(now - 2 * HOUR, 0.4),
    ended(now - HOUR + 2 * MINUTE, now - 2 * HOUR, HOUR, 0.4),
  ];
  assert.deepEqual(slowedLongOf(rows, 'studio', now), {
    detail: { bar_ms: 80 * MINUTE, factor_min: 0.4, open: false, setting: ['slow_down_stretch_seconds'], slowed_ms: 110 * MINUTE, spans: 2 },
    since: iso(cutoff - 30 * MINUTE),
  });
  // Without the span from before the cutoff the hour stays under the bar of twice 40 minutes.
  assert.equal(slowedLongOf(rows.slice(2), 'studio', now), null);
});

// One load at a time, since each pins the clock its bundle reads while it loads.
let loading = Promise.resolve();

// The instant the smoke run and the browser pass pin, whose views the site's screenshots show, and a Sunday night,
// where the draws skip the overnight hours and the weekend damps the synthesized days.
for (const at of ['2026-09-16T14:30:00Z', '2026-09-20T03:40:00Z']) {
  const story = (loading = loading.then(() => storyAt(at)));

  test(`every id a row names is in the mock's own lists, under the one label the list gives it, at ${at}`, async () => {
    const s = await story;
    const accountLabel = new Map(s.accounts.map((a) => [a.id, a.label]));
    const poolName = new Map(s.pools.map((p) => [p.id, p.name]));
    const keyName = new Map(s.keys.map((k) => [k.id, k.name]));
    const named = (map, id, where) => assert.ok(map.has(id), `${where} names ${id}, which the list lacks`);
    for (const a of s.accounts) if (a.pool) named(poolName, a.pool.poolId, `account ${a.id}`);
    for (const p of s.pools) {
      for (const m of p.members) named(accountLabel, m.accountId, `pool ${p.id}`);
      for (const k of p.keys) {
        named(keyName, k.id, `pool ${p.id}`);
        assert.equal(k.label, keyName.get(k.id), `pool ${p.id} labels key ${k.id}`);
      }
    }
    for (const k of s.keys) named(poolName, k.poolId, `key ${k.id}`);
    const attempts = s.requests.flatMap((r) => r.attempts);
    for (const r of s.requests) {
      if (r.accountId !== null) named(accountLabel, r.accountId, `request ${r.id}`);
      named(keyName, r.keyId, `request ${r.id}`);
      named(poolName, r.poolId, `request ${r.id}`);
      for (const id of r.held?.waitedOn ?? []) named(accountLabel, id, `request ${r.id}'s hold`);
    }
    for (const a of attempts) {
      named(accountLabel, a.accountId, 'an attempt');
      for (const skip of a.selection?.skipped ?? []) named(accountLabel, skip.accountId, 'a trace');
    }
    for (const e of s.events) {
      if (e.accountId !== null) named(accountLabel, e.accountId, `a ${e.kind} event`);
      if (typeof e.detail?.from_account === 'string') named(accountLabel, e.detail.from_account, `a ${e.kind} event`);
      for (const id of e.detail?.rejected_by ?? []) named(accountLabel, id, `a ${e.kind} event`);
    }
    for (const session of s.sessions) {
      named(keyName, session.keyId, `session ${session.sessionId}`);
      if (session.accountId !== null) named(accountLabel, session.accountId, `session ${session.sessionId}`);
    }
    for (const item of s.attention) {
      for (const id of item.pools) named(poolName, id, `the ${item.kind} item`);
      if (item.subject.type === 'account') {
        named(accountLabel, item.subject.id, `the ${item.kind} item`);
        assert.equal(item.subject.label, accountLabel.get(item.subject.id), `the ${item.kind} item labels its account`);
      }
      if (item.subject.type === 'pool') {
        named(poolName, item.subject.id, `the ${item.kind} item`);
        assert.equal(item.subject.label, poolName.get(item.subject.id), `the ${item.kind} item labels its pool`);
      }
      if (typeof item.detail.account === 'string') named(accountLabel, item.detail.account, `the ${item.kind} item`);
      if (typeof item.detail.label === 'string') assert.equal(item.detail.label, accountLabel.get(item.detail.account), `the ${item.kind} item's label`);
      for (const id of item.detail.accounts ?? []) named(accountLabel, id, `the ${item.kind} item`);
      if (typeof item.detail.pool === 'string') named(poolName, item.detail.pool, `the ${item.kind} item`);
      if (typeof item.detail.key === 'string') named(keyName, item.detail.key, `the ${item.kind} item`);
    }
    const month = { from: iso(s.now - 30 * DAY), to: iso(s.now) };
    const desktop = s.keys.find((k) => k.id === 'desktop');
    for (const [groupBy, labels] of [['account', accountLabel], ['key', keyName], ['pool', poolName]]) {
      for (const g of await s.api.getAnalyticsBreakdown({ ...month, groupBy })) {
        // What the router never attributed, a request answered at the cap before any account served it, is one group.
        if (g.key === '-') {
          assert.equal(g.label, 'unattributed');
          continue;
        }
        const [id, generation] = g.key.split('@');
        named(labels, id, `the ${groupBy} grouping`);
        // The route labels a group by its key in every dimension, and a key group by the key's name, which is its id;
        // an earlier holder of a reused key id is its own group, labeled with the id and its creation time.
        if (generation === undefined) assert.equal(g.label, groupBy === 'key' ? labels.get(id) : id, `the ${groupBy} grouping labels ${id}`);
        else assert.ok(id === desktop.id && g.label.startsWith(`${id} (deleted, created `), `the ${groupBy} grouping labels ${g.key} as an earlier holder`);
      }
    }
  });

  test(`an account belongs to one pool, and its pool is the one whose members name it, at ${at}`, async () => {
    const s = await story;
    const holder = new Map();
    for (const p of s.pools) {
      for (const m of p.members) {
        assert.ok(!holder.has(m.accountId), `${m.accountId} is a member of ${holder.get(m.accountId)?.poolId} and ${p.id}`);
        holder.set(m.accountId, { poolId: p.id, priority: m.priority });
      }
    }
    for (const a of s.accounts) assert.deepEqual(a.pool, holder.get(a.id) ?? null, `account ${a.id} answers the pool whose members name it`);
  });

  test(`each pool's warnings are what the eight checks give on its own settings and its largest live cohort, at ${at}`, async () => {
    const s = await story;
    const poolOfKey = new Map(s.keys.map((k) => [k.id, k.poolId]));
    const cohortOf = (poolId) => Math.max(0, ...s.sessions.filter((x) => x.pinned && poolOfKey.get(x.keyId) === poolId).map((x) => x.agents));
    for (const p of s.pools) assert.deepEqual(p.warnings, s.lib.poolWarnings(p, cohortOf(p.id)), p.id);
    const codes = new Map(s.pools.map((p) => [p.id, p.warnings.map((w) => w.code)]));
    assert.deepEqual([codes.get('all'), codes.get('jobs'), codes.get('work')], [['affinity_under_cache_ttl', 'ceiling_low'], ['spent_moves_off', 'warm_never'], []]);
  });

  test(`no account's rollup row predates the plan it was served under, at ${at}`, async () => {
    const s = await story;
    const facts = await s.api.listAccountRollups(iso(s.now - 31 * DAY), iso(s.now));
    // A row covers its whole hour, so one whose hour ends after the plan started can hold the plan's first requests.
    for (const a of s.accounts.filter((x) => x.subscription?.since)) {
      const since = Date.parse(a.subscription.since);
      const early = [...s.allRollups, ...facts].filter((r) => r.accountId === a.id && Date.parse(r.hour) + HOUR <= since);
      assert.equal(early.length, 0, `${a.id}'s plan started at ${a.subscription.since}, after ${early.length} of its rollup rows from ${early[0]?.hour}`);
    }
  });

  test(`no account fact outlasts the account's newest window reading, at ${at}`, async () => {
    const s = await story;
    const facts = await s.api.listAccountRollups(iso(s.now - 31 * DAY), iso(s.now));
    // A fact's peaks are readings, so an hour that starts after the account's newest window reading has none behind it.
    for (const a of s.accounts) {
      const readings = [...a.windows, ...(a.fableWindow ? [a.fableWindow] : [])].map((w) => Date.parse(w.observedAt));
      const late = facts.filter((r) => r.accountId === a.id && Date.parse(r.hour) > Math.max(...readings));
      assert.equal(late.length, 0, `${a.id}'s newest reading is ${readings.length ? iso(Math.max(...readings)) : 'none'}, before ${late.length} of its account facts from ${late[0]?.hour}`);
    }
  });

  test(`no retired story name and no harness name reaches a row, at ${at}`, async () => {
    const s = await story;
    const names = [
      ...s.accounts.flatMap((a) => [a.id, a.label, a.pool?.poolId]),
      ...s.pools.flatMap((p) => [p.id, p.name, ...p.members.map((m) => m.accountId), ...p.keys.flatMap((k) => [k.id, k.label])]),
      ...s.keys.flatMap((k) => [k.id, k.name, k.poolId]),
      ...s.sessions.flatMap((x) => [x.keyId, x.accountId]),
      ...s.requests.flatMap((r) => [r.accountId, r.keyId, r.poolId, ...r.attempts.map((a) => a.accountId)]),
      ...s.events.flatMap((e) => [e.accountId, e.detail?.from_account, ...(e.detail?.rejected_by ?? [])]),
      ...s.attention.flatMap((item) => [item.subject.id, item.subject.label, item.link?.id, ...item.pools, item.detail.account, item.detail.label, item.detail.pool, item.detail.key, ...(item.detail.accounts ?? [])]),
    ];
    const reached = [...new Set(names.filter((n) => RETIRED.includes(n)))];
    assert.deepEqual(reached, [], 'a retired name reaches a row');
  });

  test(`every account of the story draws in a color of its own, at ${at}`, async () => {
    const s = await story;
    const slotOf = accountSlots(s.accounts.map((a) => a.id));
    const slots = s.accounts.map((a) => slotOf(a.id));
    assert.equal(new Set(slots).size, Math.min(8, s.accounts.length), 'the eight series colors give every account its own until a ninth cycles');
    assert.ok(slots.every((slot) => slot !== null), 'every listed account has a slot');
  });

  test(`the accounts read as one operator's, every email an alias of one mailbox and no plan a Team plan, at ${at}`, async () => {
    const s = await story;
    for (const a of s.accounts) {
      if (a.email !== null) assert.match(a.email, /^operator(\+[a-z]+)?@example\.com$/, `${a.id}'s email`);
      assert.notEqual(a.plan, 'Team', `${a.id}'s plan`);
      assert.ok(!`${a.subscription?.organizationType} ${a.subscription?.rateLimitTier}`.includes('claude_team'), `${a.id}'s subscription`);
    }
    assert.deepEqual(
      s.accounts.filter((a) => a.kind === 'oauth').map((a) => a.email),
      ['operator@example.com', 'operator+second@example.com', 'operator+nightly@example.com', 'operator+studio@example.com', 'operator+old@example.com', 'operator+spare@example.com'],
      'every subscription keeps the identity it last paired with',
    );
    const pairing = await s.api.startPairing({ accountId: 'legacy' });
    const preview = await s.api.completePairing(pairing.id, { code: 'code', state: pairing.state });
    assert.deepEqual([preview.email, preview.organization], ['operator+old@example.com', 'Personal']);
  });

  test(`no account bills overage beside the provider's reason for no longer funding it, at ${at}`, async () => {
    const s = await story;
    for (const a of s.accounts) {
      const billing = a.extraUsage?.inUse === true || a.overage?.inUse === true;
      assert.ok(!billing || !a.extraUsage?.disabledReason, `${a.id} reads its overage in use beside "${a.extraUsage?.disabledReason}"`);
    }
  });

  test(`each account's request counts are its rows' from the hour a day back and the rollups' over the week, at ${at}`, async () => {
    const s = await story;
    for (const a of s.accounts) {
      const mine = s.rollups.filter((r) => r.accountId === a.id);
      const week = sum(mine.filter((r) => Date.parse(r.hour) >= s.weekFrom), requestsOf);
      assert.equal(a.requests.last24h, s.totalled.filter((r) => r.accountId === a.id).length, `${a.id}'s day`);
      assert.equal(a.requests.last7d, week, `${a.id}'s week`);
      assert.ok(a.requests.lifetime >= sum(mine, requestsOf) && a.requests.lifetime >= a.requests.last7d, `${a.id}'s lifetime reads ${a.requests.lifetime}, under what the rollups hold`);
    }
  });

  test(`each key's week is its live generation's rollups and its lifetime never under them, at ${at}`, async () => {
    const s = await story;
    const monthFrom = Math.min(...s.rollups.map((r) => Date.parse(r.hour)));
    for (const k of s.keys) {
      const mine = s.rollups.filter((r) => r.keyId === k.id && r.keyGeneration === Date.parse(k.createdAt));
      const week = mine.filter((r) => Date.parse(r.hour) >= s.weekFrom);
      assert.equal(k.usage7d.requests, sum(week, requestsOf), `${k.id}'s week`);
      assert.equal(k.usage7d.tokens, sum(week, tokensOf), `${k.id}'s week's tokens`);
      assert.ok(near(k.usage7d.cost, sum(week, (r) => r.costUsd)), `${k.id}'s week's cost`);
      assert.ok(k.lifetime.requests >= sum(mine, requestsOf) && k.lifetime.cost >= sum(mine, (r) => r.costUsd) - 1e-6, `${k.id}'s lifetime under its rollups`);
      // A key created inside the month the rollups hold has every request of its life among them.
      if (Date.parse(k.createdAt) >= monthFrom) assert.equal(k.lifetime.requests, sum(mine, requestsOf), `${k.id}'s lifetime`);
    }
  });

  test(`each pool's day and its 25 hourly buckets total its rows from the hour a day back, at ${at}`, async () => {
    const s = await story;
    for (const p of s.pools) {
      const mine = s.totalled.filter((r) => r.poolId === p.id);
      assert.equal(p.requests24h, mine.length, `${p.id}'s day`);
      assert.ok(near(p.cost24h, sum(mine, (r) => r.costUsd)), `${p.id}'s cost`);
      assert.equal(p.traffic24h.length, 25, `${p.id}'s buckets`);
      assert.equal(Date.parse(p.traffic24h[0].at), s.totalledFrom, `${p.id}'s first bucket`);
      assert.equal(sum(p.traffic24h, (b) => b.requests), mine.length, `${p.id}'s buckets`);
    }
  });

  test(`the dashboard's day and the analytics' day add up from the rows and the rollups, at ${at}`, async () => {
    const s = await story;
    assert.equal(s.overview.requests24h, s.counted.length);
    assert.equal(sum(s.overview.hourly, (h) => h.requests), s.overview.requests24h, 'the hourly buckets');
    assert.equal(sum(s.pools, (p) => p.requests24h), s.totalled.length, "the pools' days");
    const range = { from: iso(s.now - DAY), to: iso(s.now) };
    const points = await s.api.getAnalyticsSeries({ ...range, groupBy: 'all' });
    const day = s.rollups.filter((r) => Date.parse(r.hour) >= Date.parse(range.from) && Date.parse(r.hour) <= Date.parse(range.to));
    assert.equal(sum(points, (pt) => sum(pt.groups, (g) => g.requests)), sum(day, requestsOf));
  });

  test(`the live desktop key starts on the hour its earlier holder ended, inside the last day, at ${at}`, async () => {
    const s = await story;
    const desktop = s.keys.find((k) => k.id === 'desktop');
    const created = Date.parse(desktop.createdAt);
    assert.equal(created % HOUR, 0, 'on the hour');
    assert.equal(Math.floor(desktop.revision / 2 ** 42), 2, "the live key is its id's second incarnation");
    assert.ok(created >= s.now - 22 * HOUR && created <= s.now - 18 * HOUR, `created ${new Date(created).toISOString()}, not 18 to 22 hours back`);
    const rows = s.allRollups.filter((r) => r.keyId === 'desktop');
    const holder = rows.filter((r) => Date.parse(r.hour) < created);
    const earlier = new Set(holder.map((r) => r.keyGeneration));
    assert.ok(holder.some((r) => r.kind === 'inference') && holder.some((r) => r.kind === 'probe'), 'the earlier holder has rows, probes among them');
    assert.equal(created, Math.max(...holder.map((r) => Date.parse(r.hour))) + HOUR, "the live key starts the hour after the holder's last rollup hour");
    assert.equal(earlier.size, 1, 'one earlier generation');
    const [generation] = earlier;
    assert.ok(generation < created, 'the earlier generation is the older one');
    assert.ok(rows.filter((r) => Date.parse(r.hour) >= created).every((r) => r.keyGeneration === created), 'the live key holds every hour from its creation');
    assert.ok(s.requests.filter((r) => r.keyId === 'desktop').every((r) => r.keyGeneration === (Date.parse(r.at) < created ? generation : created)), 'each request row carries the holder it ran under');
  });

  test(`the earlier holder's session row reads its own requests and stays listed inside the session expiry, at ${at}`, async () => {
    const s = await story;
    const created = Date.parse(s.keys.find((k) => k.id === 'desktop').createdAt);
    const historical = s.sessions.filter((x) => x.historical);
    assert.deepEqual(historical.map((x) => x.keyId), ['desktop'], 'one earlier holder is listed');
    const [held] = historical;
    // The rows run newest first.
    const holders = s.requests.filter((r) => r.kind !== 'probe' && r.keyId === 'desktop' && r.keyGeneration === held.keyGeneration);
    const own = holders.filter((r) => r.sessionId === held.sessionId);
    assert.ok(held.keyGeneration < created && own.length > 0, 'the row is the earlier generation, with rows of its own');
    assert.deepEqual([held.requests, held.lastRequestAt], [own.length, own[0].at], "the row's figures are its own rows'");
    assert.equal(held.lastRequestAt, holders[0].at, "the holder's last request ran on this session");
    assert.ok(Date.parse(held.lastRequestAt) < created, 'before the live key');
    assert.deepEqual([held.lastUsedAt, held.pinned, held.agents], [null, false, 0], 'no pin and no activity of its own past the ledger');
    assert.ok(Date.parse(held.lastRequestAt) + s.settings.sessions.expireAfterSeconds * 1000 > s.now, 'still inside the session expiry');
    assert.ok(s.sessions.some((x) => !x.historical && x.keyId === 'desktop' && x.sessionId === held.sessionId), 'the live key holds the same session id');
  });

  test(`a live session row counts the non-probe requests of its own key, generation and session, at ${at}`, async () => {
    const s = await story;
    for (const x of s.sessions.filter((row) => !row.historical)) {
      const own = s.requests.filter((r) => r.kind !== 'probe' && r.keyId === x.keyId && r.keyGeneration === x.keyGeneration && r.sessionId === x.sessionId);
      const newest = own.reduce((last, r) => (last === null || Date.parse(r.at) > Date.parse(last) ? r.at : last), null);
      assert.deepEqual([x.requests, x.lastRequestAt], [own.length, newest], `${x.keyId}'s ${x.sessionId}`);
    }
  });

  test(`a switched-off key was last used before its last write, and a pool's one member holds what its keys sent, at ${at}`, async () => {
    const s = await story;
    for (const k of s.keys.filter((x) => x.status === 'disabled' || x.status === 'revoked')) {
      assert.ok(k.lastUsedAt === null || Date.parse(k.lastUsedAt) <= Date.parse(k.updatedAt), `${k.id} was used at ${k.lastUsedAt}, after it was switched off at ${k.updatedAt}`);
    }
    for (const p of s.pools.filter((x) => x.members.length === 1)) {
      const member = s.accounts.find((a) => a.id === p.members[0].accountId);
      const sent = sum(s.keys.filter((k) => k.poolId === p.id), (k) => k.lifetime.requests);
      assert.ok(member.requests.lifetime >= sent, `${member.id}'s lifetime reads ${member.requests.lifetime}, under the ${sent} its pool's keys sent`);
    }
  });

  test(`the ci key's last use is its newest admitted request, its revocation follows, and its pool's exhaustion starts at the first request the spent member could not take, at ${at}`, async () => {
    const s = await story;
    const ci = s.keys.find((k) => k.id === 'ci');
    // The rows run newest first. A send on the key after its revocation is refused as an unknown key and uses nothing.
    const rows = s.requests.filter((r) => r.keyId === 'ci');
    const admitted = rows.filter((r) => r.errorCode !== 'auth_unknown');
    const revokedAt = Date.parse(ci.updatedAt);
    assert.equal(ci.status, 'revoked');
    assert.ok(admitted.length > 0, 'the key has requests');
    assert.ok(Date.parse(ci.lastUsedAt) >= Date.parse(admitted[0].at), `last used at ${ci.lastUsedAt}, before its request of ${admitted[0].at}`);
    assert.ok(revokedAt > Date.parse(admitted[0].at), `revoked at ${ci.updatedAt}, before its request of ${admitted[0].at}`);
    for (const r of rows.filter((x) => Date.parse(x.at) > revokedAt)) assert.deepEqual([r.status, r.errorCode], [401, 'auth_unknown'], `the send of ${r.at} after the revocation`);
    // The attention reader dates an exhaustion from the first exhausted answer after the pool's last success, under
    // that request's key, and a success clears it.
    const pooled = s.requests.filter((r) => r.poolId === ci.poolId);
    const lastSuccess = pooled.find((r) => r.outcome === 'succeeded');
    const first = pooled.filter((r) => r.outcome === 'exhausted' && (lastSuccess === undefined || Date.parse(r.at) > Date.parse(lastSuccess.at))).at(-1);
    const item = s.attention.find((i) => i.kind === 'pool_exhausted' && i.subject.id === ci.poolId);
    assert.ok(first !== undefined && item !== undefined, `the ${ci.poolId} pool holds an exhausted answer after its last success and an exhaustion item`);
    assert.deepEqual([item.since, item.detail.key], [iso(Date.parse(first.at) + (first.durationMs ?? 0)), first.keyId], `the ${ci.poolId} pool's exhaustion`);
  });

  test(`the runner's sessions on ci read their last use off their own newest request, at ${at}`, async () => {
    const s = await story;
    const runner = s.sessions.filter((x) => x.keyId === 'ci' && !x.historical);
    assert.ok(runner.length > 0, 'the story lists sessions of the ci key');
    for (const x of runner) assert.equal(x.lastUsedAt, x.lastRequestAt, `${x.sessionId}'s last use`);
  });

  test(`each pool's active pins count one pin per key, session, agent and model under its keys, its members' sum to it, and the dashboard's are their sum, at ${at}`, async () => {
    const s = await story;
    const poolOf = new Map(s.keys.map((k) => [k.id, k.poolId]));
    const live = pinnedRecipesOf(s, await s.api.listKeepaliveRecipes());
    // A pinned session no recipe warms holds one pin per agent all the same.
    const unwarmed = (poolId, accountId) => sum(s.sessions.filter((x) => x.pinned && x.keepalive.state === null && poolOf.get(x.keyId) === poolId && (accountId === undefined || x.accountId === accountId)), (x) => x.agents);
    const pins = (poolId, accountId) => new Set(live.filter((r) => poolOf.get(r.keyId) === poolId && (accountId === undefined || r.accountId === accountId)).map(pinOf)).size + unwarmed(poolId, accountId);
    for (const p of s.pools) {
      assert.equal(p.activePins, pins(p.id), `${p.id}'s active pins`);
      for (const m of p.members) assert.equal(m.activePins, pins(p.id, m.accountId), `${p.id}'s ${m.accountId}`);
      assert.equal(sum(p.members, (m) => m.activePins), p.activePins, `${p.id}'s members sum to its figure`);
    }
    assert.ok(s.pools.some((p) => p.activePins > s.sessions.filter((x) => x.pinned && poolOf.get(x.keyId) === p.id).length), 'a pool holds more pins than pinned sessions');
    assert.equal(s.overview.activePins, sum(s.pools, (p) => p.activePins), "the dashboard's active pins");
  });

  test(`two sessions on one account and one model hold two pins, at ${at}`, async () => {
    const s = await story;
    const poolOf = new Map(s.keys.map((k) => [k.id, k.poolId]));
    const live = pinnedRecipesOf(s, await s.api.listKeepaliveRecipes()).filter((r) => poolOf.get(r.keyId) === 'all' && r.accountId === 'primary');
    // The desktop sessions on primary whose main runs Fable share the account, the agent id and the model.
    const pair = new Set(live.filter((r) => r.keyId === 'desktop' && r.agentId === 'main' && r.model === 'claude-fable-5-1').map((r) => r.sessionId));
    assert.ok(pair.size >= 2, `${pair.size} such sessions`);
    const pool = s.pools.find((p) => p.id === 'all');
    const member = pool.members.find((m) => m.accountId === 'primary');
    const perSession = new Set(live.map(pinOf)).size;
    const merged = new Set(live.map((r) => `${r.keyId}|${r.agentId}|${r.model}`)).size;
    assert.ok(merged <= perSession - (pair.size - 1), 'merged across sessions the count would lose a pin per session past the first');
    assert.equal(member.activePins, perSession, "primary's figure holds one pin per session of them");
    assert.ok(pool.activePins >= member.activePins, "and the pool's figure holds the member's");
  });

  test(`a key created inside the rollups' month reads its own rows as its life and its week, at ${at}`, async () => {
    const s = await story;
    const notebook = s.keys.find((k) => k.id === 'notebook');
    const rows = s.requests.filter((r) => r.keyId === 'notebook' && r.kind !== 'probe').length;
    assert.ok(rows > 1, `notebook's rows: ${rows}`);
    assert.deepEqual([notebook.lifetime.requests, notebook.usage7d.requests], [rows, rows]);
  });

  test(`the written session holds an agent in each lifecycle word, three released records and a seventeen-character id, listed out of the view's order, at ${at}`, async () => {
    const s = await story;
    const recipes = await s.api.listKeepaliveRecipes();
    const bySession = new Map();
    for (const r of recipes) bySession.set(r.sessionId, [...(bySession.get(r.sessionId) ?? []), r]);
    const full = [...bySession.values()].filter((rows) => ['busy', 'idle', 'parked', 'released'].every((word) => rows.some((r) => r.agentState === word)));
    assert.equal(full.length, 1, 'one session holds every lifecycle word');
    const rows = full[0];
    const released = rows.filter((r) => r.agentState === 'released');
    assert.deepEqual(released.map((r) => r.cause).sort(), ['agent_released', 'agent_released', 'session_ended'], 'two agents the mod released and one ended with the session');
    for (const r of released) assert.deepEqual([r.state, r.nextProbeAt], ['ended', null], `${r.agentId} reads as the route answers a released agent`);
    assert.equal(recipes.filter((r) => r.agentId.length === 17).length, 1, 'one id of the length Claude Code gives a subagent');
    assert.equal(s.sessions.find((x) => x.sessionId === rows[0].sessionId).agents, rows.length, 'the session row counts every record, the released ones too');
    // The list's order is not the view's: the parked row precedes the idle one, whose last request is newer than the
    // busy one's.
    const words = rows.filter((r) => r.agentState !== 'released').map((r) => (r.agentId === 'main' ? 'main' : r.agentState));
    assert.deepEqual(words, ['main', 'busy', 'parked', 'idle']);
    const lastRequestOf = (word) => Date.parse(rows.find((r) => r.agentId !== 'main' && r.agentState === word).lastRequestAt);
    assert.ok(lastRequestOf('idle') > lastRequestOf('busy'), 'the idle row is the newer request');
    const overview = await s.api.getKeepaliveOverview();
    const count = (word) => recipes.filter((r) => r.agentState === word).length;
    assert.deepEqual([overview.agents.busy, overview.agents.idle, overview.agents.parked], [count('busy'), count('idle'), count('parked')], 'the tiles count the recipe rows by lifecycle word, released ones left out');
    assert.equal(overview.limits.agents.count, s.sessions.filter((x) => x.companion !== 'none' || x.keepalive.state !== null).reduce((n, x) => n + x.agents, 0), 'the cap counts every record');
  });

  test(`the all-released session lists every record released, keeps its row and the pin its one request wrote, at ${at}`, async () => {
    const s = await story;
    const recipes = await s.api.listKeepaliveRecipes();
    const bySession = new Map();
    for (const r of recipes) bySession.set(`${r.keyId}|${r.sessionId}`, [...(bySession.get(`${r.keyId}|${r.sessionId}`) ?? []), r]);
    const whole = [...bySession.values()].filter((rows) => rows.every((r) => r.agentState === 'released'));
    assert.equal(whole.length, 1, 'one session the mod released whole');
    const [rows] = whole;
    const main = rows.find((r) => r.agentId === 'main');
    assert.ok(main !== undefined, 'main among them');
    for (const r of rows) assert.deepEqual([r.cause, r.state, r.nextProbeAt, r.deadline, r.wakeAt], ['agent_released', 'ended', null, null, null], `${r.agentId} reads as a released agent`);
    const row = s.sessions.find((x) => !x.historical && x.keyId === rows[0].keyId && x.sessionId === rows[0].sessionId);
    assert.ok(row !== undefined, 'the session keeps its row');
    assert.equal(row.agents, rows.length, 'the row counts every record');
    // The engine's session view takes the state off main's record, which the release moved to released.
    assert.deepEqual([row.keepalive.state, row.keepalive.cause], ['released', 'agent_released'], "the row reads main's lifecycle word and cause");
    const own = s.requests.filter((r) => r.kind !== 'probe' && r.keyId === row.keyId && r.sessionId === row.sessionId);
    assert.equal(own.length, 1, 'the ledger holds the one request the session routed');
    assert.deepEqual([row.pinned, row.requests, row.lastRequestAt], [true, 1, own[0].at], 'the row reads that request as its activity');
    assert.ok(routedPinsOf(s).has(pinOf(main)), "main's pin is the one its request wrote");
    const poolId = s.keys.find((k) => k.id === row.keyId).poolId;
    const counted = new Set(pinnedRecipesOf(s, recipes).filter((r) => s.keys.find((k) => k.id === r.keyId)?.poolId === poolId && r.accountId === main.accountId).map(pinOf));
    const member = s.pools.find((p) => p.id === poolId).members.find((m) => m.accountId === main.accountId);
    assert.ok(counted.has(pinOf(main)), 'the release left the pin in place');
    assert.equal(member.activePins, counted.size, `${main.accountId}'s figure counts it`);
  });

  test(`no sample of legacy is newer than its last clean poll, at ${at}`, async () => {
    const s = await story;
    const clean = Date.parse(s.accounts.find((a) => a.id === 'legacy').usage.poll.lastCleanAt);
    const samples = (await s.api.getQuotaHistory('legacy')).flatMap((h) => h.samples);
    assert.ok(samples.length > 0, 'legacy has samples');
    const late = samples.filter((x) => Date.parse(x.at) > clean);
    assert.deepEqual(late.map((x) => x.at), [], `samples past the last clean poll of ${iso(clean)}`);
  });

  test(`a Fable request's member without the model week's reading scores untouched on the model week, at ${at}`, async () => {
    const s = await story;
    const records = (await decisionsOf(s)).filter((r) => r.request.canonicalModel.includes('fable') && r.candidates.some((c) => c.pace !== null));
    const unread = records.flatMap((r) => r.candidates.filter((c) => c.kind === 'oauth' && c.pace !== null && c.windows.seven_day_fable === undefined).map((c) => [r, c]));
    assert.ok(records.length > 0 && unread.length > 0, 'the story runs a Fable request under Pace past a member that reports no model week');
    for (const [r, c] of unread) {
      assert.deepEqual([c.pace.remaining, c.pace.reset, c.pace.window], [1, null, 'seven_day_fable'], `${r.requestId}: ${c.accountId}`);
      assert.ok(s.lib.candidateCells(c, r, s.now).paceTitle.startsWith('100% of the Fable week left'), `${r.requestId}: ${c.accountId}`);
    }
    for (const r of records) for (const c of r.candidates) if (c.kind === 'oauth' && c.pace !== null) assert.equal(c.pace.window, 'seven_day_fable', r.requestId);
  });

  test(`every Pace placement's winner scores highest in its tier among the eligible members, at ${at}`, async () => {
    const s = await story;
    let seen = 0;
    for (const r of await decisionsOf(s)) {
      if (!['placed', 'moved'].includes(r.outcome.kind) || r.settings?.strategy !== 'pace' || r.outcome.rung !== null) continue;
      const id = `${r.requestId}/${r.seq}`;
      const winner = r.candidates.find((c) => c.accountId === r.outcome.accountId);
      // A move picks among its destinations and a placement among the eligible members, each past the partitions.
      const tier = keptOf(s.lib, r).filter((c) => c !== winner && c.accountId !== r.outcome.fromAccountId && sameTier(c, winner));
      assert.equal(winner.score, winner.pace.score, `${id}: ${winner.accountId}'s score is the one its reading gives`);
      for (const c of tier) {
        assert.ok(winner.pace.score >= c.pace.score, `${id}: ${winner.accountId} scores ${winner.pace.score} under ${c.accountId}'s ${c.pace.score}`);
      }
      seen += 1;
    }
    assert.ok(seen > 0, 'the story places by Pace');
  });

  test(`the two run-outs read the exhaustion their members' obstacles name, at ${at}`, async () => {
    const s = await story;
    // One run-out waits beside a member that refused the model and one beside a member whose credits the fast lane
    // could not draw, each at the clock that draws it.
    const NAMED = { 'req_151f3e2e-d99': 'fast_credits', 'req_48db1b37-495': 'model_rejected' };
    const runOuts = (await decisionsOf(s)).filter((r) => r.outcome.kind === 'exhausted' && s.lib.requestById.get(r.requestId).errorCode === 'accounts_at_cap');
    assert.ok(runOuts.some((r) => r.requestId in NAMED), `the story's run-outs: ${runOuts.map((r) => r.requestId).join(', ')}`);
    for (const r of runOuts) {
      const id = `${r.requestId}/${r.seq}`;
      const named = r.candidates.map((c) => `${c.accountId} ${c.obstacle?.name}`).join(', ');
      if (r.requestId in NAMED) assert.equal(r.outcome.exhaustion?.kind, NAMED[r.requestId], `${id}: ${named}`);
      // An obstacle a wait or a refusal names keeps the answer from reading untimed, and with every member tried or
      // off by the operator nothing else is left to name.
      const blocked = r.candidates.some((c) => !['attempted', 'operator', 'subscription_ended'].includes(c.obstacle?.name));
      if (blocked) assert.notEqual(r.outcome.exhaustion?.kind, 'untimed', `${id}: ${named}`);
      else assert.equal(r.outcome.exhaustion?.kind, 'untimed', `${id}: ${named}`);
    }
  });

  test(`a run-out writes one held record per account its hold waited on, and its exhausted record carries the held and answer facts, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    const from = iso(Math.min(...s.requests.map((r) => Date.parse(r.at))) - HOUR);
    const runOuts = s.requests.filter((r) => r.errorCode === 'accounts_at_cap');
    assert.ok(runOuts.length > 0, 'the story holds a run-out');
    for (const req of runOuts) {
      const own = records.filter((r) => r.requestId === req.id).sort((x, y) => x.seq - y.seq);
      const summaries = (await s.api.listRoutingLog({ from, limit: 200, request: req.id, to: iso(s.now + DAY) })).rows.filter((x) => x.type === 'decision');
      const summary = (r) => summaries.find((x) => x.seq === r.seq);
      const held = own.filter((r) => r.outcome.kind === 'held');
      assert.deepEqual(held.map((r) => r.outcome.accountId), req.held.waitedOn, `${req.id}'s held records against its hold's waits`);
      for (const r of held) assert.ok(summary(r)?.fact.some((f) => f.key === 'in_flight'), `${req.id}/${r.seq} reads its in-flight slots: ${JSON.stringify(summary(r)?.fact)}`);
      const exhausted = own[own.length - 1];
      assert.equal(exhausted?.outcome.kind, 'exhausted', `${req.id} ends on its answer`);
      const holds = exhausted.request.holds;
      assert.deepEqual(holds.map((h) => h.ranOut), holds.map((_, i) => i === holds.length - 1), `${req.id}/${exhausted.seq}: the last hold ran out`);
      const bound = exhausted.settings.retryInPlaceSeconds * 1_000;
      const facts = summary(exhausted)?.fact.map((f) => [f.key, f.value, f.unit]);
      assert.deepEqual(facts, [['held', bound, 'ms'], ['answer', 429, 'count']], `${req.id}/${exhausted.seq}'s facts`);
    }
  });

  test(`a pick under a round-robin pool carries the turn fact and the others keep rank, at ${at}`, async () => {
    const s = await story;
    const records = new Map((await decisionsOf(s)).map((r) => [`${r.requestId}/${r.seq}`, r]));
    const pairs = (row) => row.fact.map((f) => [f.key, f.value]);
    const keys = (row) => row.fact.map((f) => f.key);
    const seen = new Set();
    for (const row of (await logOf(s)).filter((x) => x.type === 'decision' && x.kind === 'placed' && x.by.kind === 'strategy' && x.rung === null)) {
      const r = records.get(`${row.requestId}/${row.seq}`);
      if (!r) continue;
      const id = `${row.requestId}/${row.seq}`;
      const winner = r.candidates.find((c) => c.accountId === row.accountId);
      const tier = keptOf(s.lib, r).filter((c) => sameTier(c, winner));
      const others = r.candidates.filter((c) => c !== winner);
      // The branches in the order the summary writer takes them.
      if (others.length > 0 && others.every((c) => r.request.attempted.includes(c.accountId))) {
        seen.add('last account');
        assert.deepEqual(pairs(row), [['pick', 'last account']], id);
      } else if (tier.length <= 1) {
        seen.add('only candidate');
        assert.deepEqual(pairs(row), [['pick', 'only candidate']], id);
      } else if (row.strategy === 'pace') {
        seen.add('gap');
        assert.deepEqual(keys(row), ['gap', 'over'], id);
        assert.equal(row.fact[1].value, row.runnerUp?.accountId, `${id} names its runner-up`);
      } else {
        seen.add(row.strategy);
        assert.deepEqual(pairs(row), [[row.strategy === 'round_robin' ? 'turn' : 'rank', `1 of ${tier.length}`]], id);
      }
      assert.ok(row.strategy !== 'pace' || !keys(row).some((k) => k === 'rank' || k === 'turn'), `${id} reads ${keys(row).join(', ')}`);
    }
    assert.ok(seen.has('gap') && seen.has('only candidate'), [...seen].join(', '));
    // No pool of the story contests a tier under round robin or priority, so a tier of two built here drives the
    // writer's last branch under each.
    const tier = ['a', 'b'].map((accountId) => ({ accountId, tier: [0, 0, 0] }));
    const built = { candidates: tier, outcome: { accountId: 'a', rung: null }, request: { attempted: [] } };
    for (const [strategy, key] of [['priority', 'rank'], ['round_robin', 'turn']]) {
      const facts = s.lib.placementFactOf(built, { runnerUp: null, score: null, tierSize: tier.length }, strategy);
      assert.deepEqual(facts.map((f) => [f.key, f.value]), [[key, '1 of 2']], strategy);
    }
  });

  test(`every model rejection sets a cooldown row and every crossing a spent row, at ${at}`, async () => {
    const s = await story;
    const events = (await logOf(s)).filter((x) => x.type === 'event');
    const rejections = s.requests.flatMap((r) => r.attempts.filter((a) => a.cooldownSet?.reason === 'model_rejected').map((a) => [r, a]));
    assert.ok(rejections.length > 0, 'the story holds a model rejection');
    for (const [r, a] of rejections) {
      const answered = iso(Date.parse(a.startedAt) + (a.durationMs ?? 0));
      const rows = events.filter((e) => e.kind === 'cooldown_set' && e.accountId === a.accountId && e.requestId === r.id && e.detail.reason === 'model_rejected');
      assert.deepEqual(rows.map((e) => [e.at, e.detail.scope, e.detail.window]), [[answered, `model:${r.canonicalModel}`, 'model_rejection']], `${r.id}'s rejection on ${a.accountId}`);
    }
    // A window reads its limit from its crossing until it resets, so each reset a record reads at 1 is one crossing.
    const crossings = new Map();
    const add = (map, key, reset) => map.set(key, new Set([...(map.get(key) ?? []), reset]));
    const records = await decisionsOf(s);
    for (const r of records) {
      for (const c of r.candidates) {
        for (const w of ['five_hour', 'seven_day']) if (c.windows[w]?.utilization === 1) add(crossings, `${c.accountId} ${w}`, Date.parse(c.windows[w].reset));
      }
    }
    const spent = new Map();
    for (const e of events.filter((x) => x.kind === 'window_spent')) add(spent, `${e.accountId} ${e.detail.window}`, e.detail.reset);
    assert.ok(crossings.size > 0, 'a record reads a window at its limit');
    // The comparison runs where a record reads the account, since a record reads the members of its pool alone.
    const read = new Set(records.flatMap((r) => r.candidates.map((c) => c.accountId)));
    for (const key of new Set([...crossings.keys(), ...spent.keys()].filter((key) => read.has(key.split(' ')[0])))) {
      assert.deepEqual([...(spent.get(key) ?? [])].sort(), [...(crossings.get(key) ?? [])].sort(), `${key}'s crossings`);
    }
    assert.equal(events.filter((x) => x.kind === 'window_spent').length, [...spent.values()].reduce((n, x) => n + x.size, 0), 'one spent row per crossing');
  });

  test(`a cleared window follows its spent row under the same reset, before the reset, with no reading at the limit after it, at ${at}`, async () => {
    const s = await story;
    const events = (await logOf(s)).filter((x) => x.type === 'event');
    const cleared = events.filter((e) => e.kind === 'window_cleared');
    assert.ok(cleared.length > 0, 'the story holds a cleared window');
    const WINDOW = { '5h': 'five_hour', '7d': 'seven_day', '7d_fable': 'seven_day_fable' };
    const records = await decisionsOf(s);
    for (const e of cleared) {
      const d = e.detail;
      const id = `${e.accountId} ${d.window} at ${e.at}`;
      const after = (ms) => ms > Date.parse(e.at);
      assert.deepEqual(Object.keys(d).sort(), ['reset', 'source', 'source_request_id', 'utilization', 'utilization_before', 'window'], `${id} carries the readings and the reset alone`);
      assert.ok(d.utilization_before >= 1 && d.utilization < 1 && Date.parse(e.at) < d.reset, `${id} reads ${d.utilization_before} then ${d.utilization} before its reset`);
      assert.equal(e.requestId, d.source_request_id, `${id} names its head's request where a head read it`);
      const spent = events.filter((x) => x.kind === 'window_spent' && x.accountId === e.accountId && x.detail.window === d.window && x.detail.reset === d.reset);
      assert.equal(spent.length, 1, `${id} follows one spent row under its reset`);
      assert.ok(Date.parse(spent[0].at) < Date.parse(e.at), `${id} follows its spent row at ${spent[0].at}`);
      // A reading back at the limit under the same reset would be spent again, which the story writes no row for, and
      // the account's standing reading is its newest, so under that reset it reads under the limit too.
      const limits = [
        ...records.filter((r) => after(Date.parse(r.request.at))).flatMap((r) => r.candidates.filter((c) => c.accountId === e.accountId).map((c) => c.windows[d.window])),
        ...s.requests.flatMap((r) => r.attempts.filter((a) => a.accountId === e.accountId && after(Date.parse(a.startedAt))).flatMap((a) => a.windows.filter((w) => WINDOW[w.id] === d.window))).map((w) => ({ reset: w.resetsAt, utilization: w.utilization })),
        ...s.accounts.filter((a) => a.id === e.accountId).flatMap((a) => a.windows.filter((w) => WINDOW[w.id] === d.window)).map((w) => ({ reset: w.resetsAt, utilization: w.utilization })),
      ].filter((w) => w !== undefined && w.reset !== null && Date.parse(w.reset) === d.reset && (w.utilization ?? 0) >= 1);
      assert.deepEqual(limits, [], `${id} has no reading at the limit after it under its reset`);
    }
  });

  test(`no event reads the retired spellings, at ${at}`, async () => {
    const s = await story;
    const events = (await logOf(s)).filter((x) => x.type === 'event');
    assert.deepEqual(events.filter((e) => e.detail.reason === 'rate_limited').map((e) => `${e.kind} ${e.id}`), [], 'a 429 cooldown reads rate_limit');
    assert.deepEqual(events.filter((e) => e.kind === 'reauth_required').map((e) => e.id), [], 'the state change stands for the re-pair');
    const cleared = events.filter((e) => e.kind === 'cooldown_cleared');
    assert.ok(cleared.length > 0 && cleared.every((e) => ['admin', 'expired'].includes(e.detail.cause)), cleared.map((e) => e.detail.cause).join(', '));
    for (const r of await decisionsOf(s)) {
      for (const c of r.candidates) {
        const id = `${r.requestId}/${r.seq}: ${c.accountId}`;
        assert.ok(c.cooldowns.every((x) => x.reason !== 'rate_limited') && c.obstacle?.reason !== 'rate_limited', id);
        assert.ok(Object.values(c.windows).every((w) => ['head', 'poll'].includes(w.source)), id);
      }
    }
  });

  test(`the refused records carry their codes, at ${at}`, async () => {
    const s = await story;
    const summaries = (await logOf(s)).filter((x) => x.type === 'decision' && x.kind === 'refused');
    const refused = (await decisionsOf(s)).filter((r) => r.outcome.kind === 'refused');
    assert.deepEqual(refused.map((r) => r.outcome.refusal.kind).sort(), ['client', 'key']);
    for (const r of refused) {
      const id = `${r.requestId}/${r.seq}`;
      assert.equal(r.outcome.refusal.code, r.outcome.refusal.kind === 'key' ? 'revoked' : 'drift', id);
      assert.equal(summaries.find((x) => x.requestId === r.requestId && x.seq === r.seq)?.refusal?.code, r.outcome.refusal.code, `${id}'s row`);
    }
  });

  test(`every registry change carries the diff its writer stores, at ${at}`, async () => {
    const s = await story;
    const changes = (await logOf(s)).filter((x) => x.type === 'event' && x.kind === 'registry_changed');
    const isChange = (v) => v !== null && typeof v === 'object' && !Array.isArray(v) && Object.keys(v).sort().join() === 'from,to';
    // A pair names a member its writer stores and a value that moved.
    const pairsOf = (e, members) => {
      const pairs = Object.entries(e.detail.diff).filter(([field]) => field !== 'secret_replaced');
      assert.ok(pairs.length > 0, `${e.id}: the edit names the fields it changed`);
      for (const [field, change] of pairs) {
        assert.ok(members.includes(field), `${e.id}: ${field} is a field the store names`);
        assert.ok(isChange(change) && JSON.stringify(change.from) !== JSON.stringify(change.to), `${e.id}: ${field} reads its value before and after`);
      }
      return Object.fromEntries(pairs);
    };
    for (const e of changes) {
      if (e.detail.kind === 'key_revoked') assert.deepEqual(e.detail.diff, {}, `${e.id}: a revocation stores no diff`);
      if (e.detail.kind === 'pool_updated') {
        const pairs = pairsOf(e, POOL_DIFF_MEMBERS);
        assert.ok(!('secret_replaced' in e.detail.diff), `${e.id}: a pool holds no secret`);
        // The story's one edit of the pool is its last, so each new value is the one the pool holds, its label as the
        // pool's name.
        const pool = s.pools.find((x) => x.id === e.detail.target);
        assert.ok(pool, `${e.id}: the edit names a pool of the story`);
        const member = (field) => (field === 'label' ? 'name' : field.replace(/_([a-z])/g, (_, c) => c.toUpperCase()));
        for (const [field, change] of Object.entries(pairs)) if (member(field) in pool) assert.deepEqual(change.to, pool[member(field)], `${e.id}: ${field} reads the pool's value`);
      }
      if (e.detail.kind === 'account_updated') {
        const pairs = pairsOf(e, ACCOUNT_DIFF_MEMBERS);
        assert.equal(e.detail.diff.secret_replaced, true, `${e.id}: the edit staged a secret, which reads as the flag alone`);
        const account = s.accounts.find((x) => x.id === e.detail.target);
        assert.equal(account?.kind, 'api_key', `${e.id}: only an API-key account holds a secret`);
        assert.ok(!('cancelled' in pairs) && !('subscription_ends_at' in pairs), `${e.id}: an API-key account has no end`);
        // The story's one edit of the account is its last, so each new value is the one the account holds.
        const held = { enabled: account.routingEnabled, label: account.label };
        for (const [field, change] of Object.entries(pairs)) if (field in held) assert.deepEqual(change.to, held[field], `${e.id}: ${field} reads the account's value`);
      }
    }
    assert.ok(['account_updated', 'key_revoked', 'pool_updated'].every((kind) => changes.some((e) => e.detail.kind === kind)), changes.map((e) => e.detail.kind).join(', '));
  });

  test(`every cache write a record dates falls on a request of its hour, at ${at}`, async () => {
    const s = await story;
    // The instants a session's requests ran at: each arrival and each attempt's start, where a decision is made.
    const instants = new Map();
    for (const r of s.requests) {
      if (r.sessionId === null) continue;
      const own = instants.get(r.sessionId) ?? new Set();
      for (const at of [r.at, ...r.attempts.map((a) => a.startedAt)]) own.add(Date.parse(at));
      instants.set(r.sessionId, own);
    }
    const written = (session, at) => instants.get(session)?.has(Date.parse(at)) === true;
    let seen = 0;
    for (const r of await decisionsOf(s)) {
      if (r.pin?.found !== 'live') continue;
      seen += 1;
    }
    assert.ok(seen > 0, 'the story reads a live pin');
    for (const e of (await logOf(s)).filter((x) => x.type === 'event' && ['pin_created', 'pin_migrated'].includes(x.kind))) {
      assert.ok(written(e.sessionId, e.at), `the ${e.kind} event ${e.id} at ${e.at} has no request of session ${e.sessionId} at it`);
    }
  });

  test(`a scripted pick comes from the highest tier that holds an eligible member, and every record's instant follows its request's, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    const events = (await logOf(s)).filter((x) => x.type === 'event' && x.kind === 'cooldown_set');
    const accounts = new Map(s.accounts.map((a) => [a.id, a]));
    for (const r of records) {
      const id = `${r.requestId}/${r.seq}`;
      const at = Date.parse(r.request.at);
      if (['placed', 'moved'].includes(r.outcome.kind) && r.outcome.rung === null) {
        const winner = r.candidates.find((c) => c.accountId === r.outcome.accountId);
        const higher = keptOf(s.lib, r).filter((c) => c.accountId !== r.outcome.fromAccountId && tierBefore(c.tier, winner.tier));
        assert.deepEqual(higher.map((c) => `${c.accountId} ${c.tier.join('/')}`), [], `${id}: ${winner.accountId} sits in tier ${winner.tier.join('/')}`);
      }
      const req = s.lib.requestById.get(r.requestId);
      const attempt = req.attempts[r.request.attempt - 1];
      const end = attempt ? Date.parse(attempt.startedAt) + (attempt.durationMs ?? 0) : Date.parse(req.at) + (req.durationMs ?? 0);
      assert.ok(at >= Date.parse(req.at) && at <= end, `${id} at ${r.request.at} against its request's ${req.at} to ${iso(end)}`);
      for (const c of r.candidates) {
        for (const cd of c.cooldowns) {
          const until = Date.parse(cd.until);
          const setAt = [
            ...events.filter((e) => e.accountId === c.accountId && e.detail.scope === cd.scope && e.detail.until === until).map((e) => Date.parse(e.at)),
            ...(accounts.get(c.accountId)?.cooldowns ?? []).filter((x) => x.scope === cd.scope && Date.parse(x.until) === until).map((x) => Date.parse(x.setAt)),
          ];
          assert.ok(until > at && setAt.some((x) => x <= at), `${id}: ${c.accountId}'s ${cd.scope} cooldown to ${cd.until} set at ${setAt.map(iso).join(', ') || 'no instant'}`);
        }
      }
    }
    for (const r of records.filter((x) => x.outcome.kind === 'held')) {
      const next = records.find((x) => x.requestId === r.requestId && x.seq === r.seq + 1);
      assert.ok(next !== undefined && Date.parse(next.request.at) >= Date.parse(r.request.at), `${r.requestId}/${r.seq}: the hold precedes the decision after it`);
    }
  });

  test(`every record that reaches the rungs carries one partitions step before them, a hold its pick and an exhaustion the walk the engine writes, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    assert.ok(records.length > 20);
    const seen = { exhausted: 0, held: 0, ranOut: 0, reset: 0, slowDown: 0 };
    for (const r of records) {
      const id = `${r.requestId}/${r.seq}`;
      const steps = r.walk.map((w) => w.step);
      const walk = `${id}: ${steps.join(', ')}`;
      const firstRung = steps.findIndex((x) => x.startsWith('rung:'));
      const partitions = steps.flatMap((x, i) => (x === 'partitions' ? [i] : []));
      const pick = r.walk.find((w) => w.step === 'pick');
      if (!['exhausted', 'held', 'moved', 'placed'].includes(r.outcome.kind)) continue;
      // A hold for a spent window's reset ends the walk at the spent rule, before any placement.
      if (r.outcome.hold?.reason === 'reset') {
        seen.reset += 1;
        assert.deepEqual([steps.indexOf('move:spent') >= 0 && r.walk[steps.indexOf('move:spent')].verdict, steps.includes('partitions'), steps[steps.length - 1]], ['held', false, 'reserve'], walk);
        continue;
      }
      // A slow-down hold waits on the live pin the move rules left in place, before its renewal: no partitions, no rung
      // and no pick.
      if (r.outcome.hold?.reason === 'slow_down') {
        seen.slowDown += 1;
        assert.deepEqual(
          [r.pin?.found, steps.indexOf('move:shed') > steps.indexOf('pin'), steps.includes('partitions'), firstRung, steps.includes('pick'), steps[steps.length - 1]],
          ['live', true, false, -1, false, 'reserve'],
          walk,
        );
        continue;
      }
      assert.ok(firstRung >= 0, walk);
      assert.equal(partitions.length, 1, walk);
      assert.ok(partitions[0] < firstRung, `${walk}: partitions after the rungs`);
      const slot = steps.findIndex((x) => x === 'hold' || x === 'exhausted');
      if (r.outcome.kind === 'held') {
        seen.held += 1;
        assert.equal(pick?.verdict, 'chosen', walk);
        assert.ok(steps.indexOf('pick') < slot, `${walk}: no pick before the hold`);
        assert.equal(r.candidates.find((c) => c.accountId === r.outcome.accountId)?.rank, 1, `${id}: the held-on member's rank`);
      } else if (r.outcome.kind === 'exhausted') {
        seen.exhausted += 1;
        assert.deepEqual([r.walk[partitions[0]].verdict, pick?.verdict], ['none', 'none'], walk);
        assert.ok(steps.indexOf('pick') < slot, `${walk}: no pick before the exhaustion`);
        assert.ok(!steps.includes('bound'), walk);
        // Past a hold's bound the relay passes the account it waited on over and selects again, so the answer is an
        // empty eligible set with every account the request waited on attempted.
        if (s.lib.requestById.get(r.requestId).errorCode === 'accounts_at_cap') {
          seen.ranOut += 1;
          assert.equal(r.walk.find((w) => w.step === 'eligible')?.verdict, 'none', walk);
          assert.ok(r.candidates.every((c) => c.obstacle !== null), `${id}: a member left eligible`);
          for (const h of r.request.holds) assert.equal(r.candidates.find((c) => c.accountId === h.accountId)?.obstacle?.name, 'attempted', `${id}: ${h.accountId}`);
          // The exhaustion is named after the members' obstacles, so only a set of attempted members reads untimed.
          assert.ok(r.candidates.every((c) => c.obstacle.name === 'attempted') || r.outcome.exhaustion.kind !== 'untimed', `${id}: ${r.candidates.map((c) => `${c.accountId} ${c.obstacle.name}`).join(', ')}`);
        }
      } else {
        // A rung places the request itself, and the strategy picks only where no rung matched.
        assert.equal(pick?.verdict, r.outcome.rung === null ? 'chosen' : undefined, walk);
      }
    }
    assert.ok(seen.exhausted > 0 && seen.held > 0 && seen.ranOut > 0 && seen.reset > 0 && seen.slowDown > 0, JSON.stringify(seen));
    assert.ok(records.some((r) => r.requestId === 'req_51d0e2b8-7a0' && r.walk.some((w) => w.step === 'partitions')), 'the empty-eligible exhaustion walks the partitions');
  });

  test(`every partitions step reads the four facts the derivation gives over the record's own set, none past the set, at ${at}`, async () => {
    const s = await story;
    const d = s.lib;
    const records = await decisionsOf(s);
    let seen = 0;
    let fitted = 0;
    let unfit = 0;
    for (const r of records) {
      const steps = r.walk.map((w) => w.step);
      if (!steps.some((x) => x.startsWith('rung:'))) continue;
      const set = narrowedSetOf(r);
      const pool = d.poolById.get(r.request.poolId);
      // A move's destinations carry its gate's fit. A placement's fit is computed here over the members the spent and
      // cap partitions left, and the members they dropped read none, so the no-fit count follows the computed fits.
      let members = set.map((c) => ({ ...c }));
      // A drawn token count and a drawn hold read no fit: the generator draws their account without one, and
      // neither takes a slot the cap could keep that choice by.
      const unread = !d.STORY_IDS.has(r.requestId) && (r.request.flags.count || r.outcome.kind === 'held');
      if (unread) assert.ok(set.every((c) => c.fit === null), `${r.requestId}/${r.seq} reads a fit`);
      else if (!moving(r)) {
        const read = fitReadOf(set, pool, r);
        members = set.map((c) => ({ ...c, fit: read.has(c) ? freshFitOf(d, c, r, pool) : null }));
        for (const [i, c] of set.entries()) {
          assert.equal(c.fit, members[i].fit, `${r.requestId}/${r.seq}: ${c.accountId} reads fit ${c.fit}`);
          if (read.has(c)) fitted += 1;
          if (read.has(c) && c.fit !== 'fits' && !d.STORY_IDS.has(r.requestId)) unfit += 1;
        }
      }
      const expected = d.partitionsOf(members, pool, d.requestById.get(r.requestId), moving(r));
      for (const w of r.walk.filter((x) => x.step === 'partitions')) {
        seen += 1;
        assert.deepEqual(w.facts.map((f) => [f.key, f.value]), expected.map((f) => [f.key, f.value]), `${r.requestId}/${r.seq}`);
        assert.deepEqual(w.facts.map((f) => f.key), ['spent', 'at_cap', 'no_fit', 'winding_down']);
        // The engine writes a partition's count bare, as it writes its words.
        assert.ok(w.facts.every((f) => f.unit === null), `${r.requestId}/${r.seq}: ${w.facts.map((f) => f.unit).join(', ')}`);
        for (const f of w.facts) {
          if (typeof f.value === 'number') assert.ok(f.value >= 0 && f.value <= set.length, `${r.requestId}: ${f.key} ${f.value} over ${set.length}`);
          else assert.ok(['fell_through', 'held', 'off'].includes(String(f.value)), `${r.requestId}: ${f.key} ${f.value}`);
        }
      }
    }
    assert.ok(seen > 10);
    assert.ok(fitted > 10 && unfit > 0, `${fitted} members read a fit, ${unfit} of the drawn placements' not fitting`);

    const read = (id, keys) => {
      const w = records.find((r) => `${r.requestId}/${r.seq}` === id)?.walk.find((x) => x.step === 'partitions');
      return [w?.verdict, (w?.facts ?? []).filter((f) => keys.includes(f.key)).map((f) => [f.key, f.value])];
    };
    // The cohort hold waits with every member it could take at its cap, under a non-Fable model, and with neither
    // fitting under the ceiling the set stays whole.
    assert.deepEqual(read('req_69e30548-32e/1', ['spent', 'at_cap', 'no_fit']), ['match', [['spent', 'off'], ['at_cap', 'held'], ['no_fit', 'fell_through']]]);
    // The spare-rate placement passes over secondary, which falls short of the ceiling.
    assert.deepEqual(read('req_2c7d9f15-b63/1', ['no_fit']), ['match', [['no_fit', 1]]]);
    // Nothing is left to narrow, so the step reads none with every count 0 or off.
    assert.deepEqual(read('req_51d0e2b8-7a0/1', ['spent', 'at_cap', 'no_fit', 'winding_down']), ['none', [['spent', 'off'], ['at_cap', 0], ['no_fit', 0], ['winding_down', 0]]]);

    const pairs = (facts) => facts.map((f) => [f.key, f.value]);
    const member = (accountId, o = {}) => ({ accountId, credits: null, fit: null, inFlight: 0, inFlightCap: 4, kind: 'oauth', pace: { remaining: 0.5 }, term: { windingDown: false }, windows: {}, ...o });
    const haiku = { canonicalModel: 'claude-haiku-4-5-20251001', kind: 'inference' };
    const fable = { canonicalModel: 'claude-fable-5-1', kind: 'inference' };
    // The cap drops the member at it, and the wind-down then falls through rather than empty the set.
    assert.deepEqual(pairs(d.partitionsOf([member('a', { inFlight: 4 }), member('b', { term: { windingDown: true } })], d.poolById.get('jobs'), haiku)), [['spent', 'off'], ['at_cap', 1], ['no_fit', 0], ['winding_down', 'fell_through']]);
    // A Fable request draws on the shared windows too, and a spent member drops outright, even when none is left.
    const spentOn = (name) => ({ windows: { [name]: { utilization: 1 } } });
    assert.deepEqual(pairs(d.partitionsOf([member('a', spentOn('five_hour')), member('b')], d.poolById.get('all'), fable)), [['spent', 1], ['at_cap', 0], ['no_fit', 0], ['winding_down', 0]]);
    assert.deepEqual(pairs(d.partitionsOf([member('a', spentOn('seven_day')), member('b', spentOn('seven_day_fable'))], d.poolById.get('all'), fable)), [['spent', 2], ['at_cap', 0], ['no_fit', 0], ['winding_down', 0]]);
    // A member the request does not fit drops while one it fits is left, and with none fitting the set stays whole.
    const all = d.poolById.get('all');
    assert.deepEqual(pairs(d.partitionsOf([member('a'), member('b', { fit: 'least_bad' })], all, haiku)), [['spent', 'off'], ['at_cap', 0], ['no_fit', 1], ['winding_down', 0]]);
    assert.deepEqual(pairs(d.partitionsOf([member('a', { fit: 'rewrite_only' }), member('b', { fit: 'least_bad' })], all, haiku)), [['spent', 'off'], ['at_cap', 0], ['no_fit', 'fell_through'], ['winding_down', 0]]);
    // A token count takes no slot, so the cap keeps every member, even with each at it.
    assert.deepEqual(pairs(d.partitionsOf([member('a', { inFlight: 4 }), member('b', { inFlight: 4 })], d.poolById.get('jobs'), { ...haiku, kind: 'count_tokens' })), [['spent', 'off'], ['at_cap', 0], ['no_fit', 0], ['winding_down', 0]]);
    // With every member at its cap, a Fable request's spent member drops before the cap holds the rest, so it is never
    // the account the request waits on or passes over.
    assert.deepEqual(pairs(d.partitionsOf([member('a', { inFlight: 4, ...spentOn('five_hour') }), member('b', { inFlight: 4 })], all, fable)), [['spent', 1], ['at_cap', 'held'], ['no_fit', 0], ['winding_down', 0]]);
  });

  test(`the member a placement, a move or a hold names is one the partitions kept, at ${at}`, async () => {
    const s = await story;
    const d = s.lib;
    const records = await decisionsOf(s);
    let seen = 0;
    for (const r of records) {
      const partitions = r.walk.find((w) => w.step === 'partitions');
      if (!partitions || !['held', 'moved', 'placed'].includes(r.outcome.kind)) continue;
      const id = `${r.requestId}/${r.seq}`;
      const named = r.candidates.find((c) => c.accountId === r.outcome.accountId);
      assert.ok(named !== undefined, `${id}: ${r.outcome.accountId} is no candidate`);
      seen += 1;
      assert.equal(keptOf(d, r).includes(named), true, `${id}: ${r.outcome.kind} on ${named.accountId} over ${partitions.facts.map((f) => `${f.key} ${f.value}`).join(', ')}`);
    }
    assert.ok(seen > 10);
  });

  test(`the story lists three forks, one on its base's account, one on another and one without a pin, at ${at}`, async () => {
    const s = await story;
    // The mock lists its live sessions in the order it seeds them, one per slot of its session ids, so a row's place
    // among them is the slot the story pinned its fork on.
    const listed = s.sessions.filter((x) => !x.historical);
    const forks = s.sessions.filter((x) => x.forkedFrom !== null);
    assert.equal(forks.length, 3, `${forks.length} forks are listed`);
    const pairs = forks.map((fork) => {
      const base = s.sessions.find((x) => x.sessionId === fork.forkedFrom && x.keyId === fork.keyId);
      assert.ok(base, `${fork.sessionId}'s base ${fork.forkedFrom} is listed under ${fork.keyId}`);
      return { base, fork };
    });
    // An unpinned drawn session's first turn takes the third fork, which slot depending on whether the clock left
    // that turn a token count, and its base is a session of its own key.
    const pinless = pairs.filter((p) => !p.fork.pinned);
    assert.equal(pinless.length, 1, 'one fork holds no pin');
    const slot = listed.indexOf(pinless[0].fork);
    assert.deepEqual([slot, pinless[0].base.sessionId, pinless[0].fork.forkDepth], slot === 12 ? [12, listed[7].sessionId, 2] : [13, listed[1].sessionId, 2], 'the pinless fork and its base');
    const pinned = pairs.filter((p) => p.fork.pinned);
    const same = pinned.filter((p) => p.fork.accountId === p.base.accountId);
    const other = pinned.filter((p) => p.fork.accountId !== p.base.accountId);
    assert.equal(same.length, 1, "one fork sits on its base's account");
    assert.equal(other.length, 1, 'one fork sits on another account than its base');
    assert.deepEqual([same[0].fork.sessionId, same[0].base.sessionId], [listed[5].sessionId, listed[3].sessionId], 'the same-account fork and its base');
    assert.deepEqual([other[0].fork.sessionId, other[0].base.sessionId], [listed[2].sessionId, listed[1].sessionId], 'the other-account fork and its base');
  });

  test(`one window of the story reads ahead of pace and one behind at ${at}`, async () => {
    const s = await story;
    const lead = (accountId, windowId) => {
      const account = s.accounts.find((a) => a.id === accountId);
      const w = account.windows.find((x) => x.id === windowId);
      return s.leadOf(w, w.periodHours * HOUR, s.now, s.lib.endOf(account, s.now));
    };
    assert.ok(lead('primary', '5h') > 0.05, `primary's 5h window reads ${lead('primary', '5h')}, not ahead of pace`);
    assert.ok(lead('primary', '7d') < -0.05, `primary's 7d window reads ${lead('primary', '7d')}, not behind pace`);
    assert.ok(lead('secondary', '5h') > 0 && lead('secondary', '7d') > 0, `secondary's windows read ${lead('secondary', '5h')} and ${lead('secondary', '7d')}, not both ahead of pace`);
  });

  test(`every account window answers the lead the router's arithmetic gives its own reading, at ${at}`, async () => {
    const s = await story;
    let read = 0;
    for (const a of s.accounts) {
      const end = s.lib.endOf(a, s.now);
      for (const w of [...a.windows, ...(a.fableWindow === null ? [] : [a.fableWindow])]) {
        const want = s.leadOf(w, w.periodHours * HOUR, s.now, end);
        assert.ok(typeof w.lead === 'number' && near(w.lead, want), `${a.id}'s ${w.id} leads ${w.lead}, not ${want}`);
        read += 1;
      }
    }
    assert.ok(read > 0, 'the story holds windows');
  });

  test(`the default range's kept-warm saving nets a figure per pool, a saving where the pool keeps caches warm and a loss on one whose probes cost more than they saved, at ${at}`, async () => {
    const s = await story;
    const groups = s.lib.groupsOf(defaultRange(s), { groupBy: 'pool', includeProbes: false });
    const nets = groups.map((g) => ({ kept: g.keptWarm.tokens1h + g.keptWarm.tokens5m, net: g.savingUsd - g.probeCostUsd, pool: g.key }));
    assert.ok(nets.every((n) => Number.isFinite(n.net)), `every pool nets a figure: ${JSON.stringify(nets)}`);
    assert.ok(nets.some((n) => n.kept > 0 && n.net > 0), `a pool that keeps caches warm nets a saving: ${JSON.stringify(nets)}`);
    assert.equal(nets.filter((n) => n.net < 0).length, 1, `one pool's probes cost more than they saved: ${JSON.stringify(nets)}`);
    assert.ok(nets.filter((n) => n.net >= 0).every((n) => n.kept > 0 || n.net === 0), `every other pool that kept nothing warm nets nothing: ${JSON.stringify(nets)}`);
    const warmed = groups.filter((g) => g.keptWarm.tokens1h > 0 && g.keptWarm.tokens5m > 0);
    assert.ok(warmed.length > 0, 'the kept tokens split between the two TTLs');
  });

  test(`every decision record reads a plan window's lead and projects it to its horizon, at ${at}`, async () => {
    const s = await story;
    let capped = 0;
    let burning = 0;
    for (const r of await decisionsOf(s)) {
      const t = Date.parse(r.request.at);
      for (const c of r.candidates) {
        const account = s.accounts.find((a) => a.id === c.accountId);
        if (account?.kind !== 'oauth') continue;
        const end = s.lib.endOf(account, t);
        for (const [name, hours] of [['five_hour', 5], ['seven_day', 168]]) {
          const w = c.windows[name];
          if (!w || w.ahead === null) continue;
          const want = Math.round(s.leadOf({ resetsAt: w.reset, utilization: w.utilization }, hours * HOUR, t, end) * 1000) / 1000;
          assert.ok(near(w.ahead, want), `${r.requestId}: ${c.accountId}'s ${name} reads ${w.ahead}, not ${want}`);
        }
        // The 5-hour window runs to its reset and a weekly one five hours out at most, each to the end where it comes
        // first, and the projection adds the account's burn over the span.
        for (const [name, w] of Object.entries(c.windows)) {
          const reset = w.reset === null ? null : Date.parse(w.reset);
          const bounds = name === 'five_hour' ? [reset ?? t + 5 * HOUR, end] : [t + 5 * HOUR, reset, end];
          const horizon = Math.max(t, Math.min(...bounds.filter((x) => x !== null)));
          assert.equal(w.horizon, iso(horizon), `${r.requestId}/${r.seq}: ${c.accountId}'s ${name} horizon`);
          const projected = Math.round((w.utilization + ((c.burn[name] ?? 0) * (horizon - t)) / HOUR) * 1000) / 1000;
          assert.ok(near(w.projection, projected), `${r.requestId}/${r.seq}: ${c.accountId}'s ${name} projects ${w.projection}, not ${projected}`);
          if (name !== 'five_hour' && reset !== null && horizon < reset) capped += 1;
          if ((c.burn[name] ?? 0) > 0) burning += 1;
        }
      }
    }
    assert.ok(capped > 0 && burning > 0, `a weekly horizon five hours out (${capped}) and a burning window (${burning})`);
  });

  test(`the rewrites answer counts each reason's moves over its fits, two reasons by different fits, at ${at}`, async () => {
    const s = await story;
    const { moves, movesByReasonAndFit } = await s.api.getAnalyticsRewrites({ from: iso(s.now - 7 * DAY), to: iso(s.now) });
    assert.deepEqual(Object.keys(movesByReasonAndFit).sort(), Object.keys(moves).sort());
    for (const [reason, fits] of Object.entries(movesByReasonAndFit)) assert.equal(sum(Object.values(fits), (n) => n), moves[reason], reason);
    const shapes = new Set(Object.values(movesByReasonAndFit).map((fits) => JSON.stringify(Object.keys(fits).sort())));
    assert.ok(shapes.size > 1, `the reasons read ${[...shapes].join(', ')}`);
  });

  test(`a fired rule's destinations are its gate's answer in the set's order, the shed rule's fitting set whole for the strategy to rank with the runway breaking its tie, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    const destinationsOf = (r) => r.walk.find((w) => w.step.startsWith('move:') && w.verdict === 'fired')?.facts.find((f) => f.key === 'destinations')?.value;
    const runway = (c) => c.runwayMs ?? Number.POSITIVE_INFINITY;
    const longest = (list) => list.filter((c) => runway(c) === Math.max(...list.map(runway)));
    const free = (c) => c.inFlight < c.inFlightCap;
    const seen = { other: 0, shed: 0 };
    for (const r of records) {
      const line = r.walk.find((w) => w.step.startsWith('move:') && w.verdict === 'fired');
      if (!line) continue;
      const readIds = new Set(r.walk.find((w) => w.step === 'fit').facts.filter((f) => f.key === 'verdict').map((f) => f.accountId));
      const read = r.candidates.filter((c) => readIds.has(c.accountId));
      const fits = read.filter((c) => c.fit === 'fits' && free(c));
      const rewriteOnly = read.filter((c) => c.fit === 'rewrite_only' && free(c));
      const shed = line.step === 'move:shed';
      const want = fits.length > 0 ? fits : shed ? [] : rewriteOnly.length > 0 ? longest(rewriteOnly) : read.filter((c) => c.fit !== 'ending');
      assert.deepEqual(destinationsOf(r), want.map((c) => c.accountId), `${r.requestId}/${r.seq}`);
      seen[shed ? 'shed' : 'other'] += 1;
    }
    assert.ok(seen.shed > 0 && seen.other > 0, JSON.stringify(seen));
    // The Overflow workspace fits with no end to its runway and sits at its cap through the round and the move, so the
    // gate answers primary alone.
    const move = records.find((r) => r.requestId === 'req_868264bc-611');
    const overflow = move.candidates.find((c) => c.accountId === 'overflow');
    assert.deepEqual([destinationsOf(move), overflow.fit, overflow.runwayMs, overflow.inFlight === overflow.inFlightCap], [['primary'], 'fits', null, true]);
    const round = (await logOf(s)).find((row) => row.type === 'event' && row.kind === 'shedding_round' && row.requestId === 'req_1eaa9bc1-a18');
    const planner = records.find((r) => r.requestId === 'req_1eaa9bc1-a18');
    const cap = planner.candidates.find((c) => c.accountId === 'overflow');
    assert.deepEqual([round.detail.pins.filter((p) => p.marked).map((p) => p.planned), cap.inFlight === cap.inFlightCap], [['primary', 'primary'], true]);
  });

  test(`every shedding round's pin reads its burn per token of its last prompt to seven digits, at ${at}`, async () => {
    const s = await story;
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round');
    assert.ok(rounds.length > 0);
    for (const e of rounds) {
      for (const p of e.detail.pins.filter((x) => x.burn !== null)) {
        const want = p.burn / p.last_input_tokens;
        assert.ok(p.per_rewrite > 0 && Math.abs(p.per_rewrite - want) <= want * 1e-6, `${e.requestId}: ${p.session_id} reads ${p.per_rewrite}, not ${want}`);
      }
    }
  });

  test(`no 5-hour shedding round of the story fires inside the hold of its pool, and a held shed line names the window's age and hold beside its mark, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    // The window's start as the router reads it off the source's 5-hour reading: the reset less five hours where the
    // reset lies ahead, the passed reset itself, and none where the reading names no reset.
    const startOf = (r, accountId) => {
      const reset = r.candidates.find((c) => c.accountId === accountId)?.windows.five_hour?.reset ?? null;
      if (reset === null) return null;
      const at = Date.parse(reset);
      return at > Date.parse(r.request.at) ? at - 5 * HOUR : at;
    };
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round' && row.detail.window === 'five_hour');
    assert.ok(rounds.length > 0);
    for (const e of rounds) {
      const r = records.find((x) => x.requestId === e.requestId && x.request.at === e.at);
      const start = startOf(r, e.accountId);
      const age = start === null ? null : Date.parse(e.at) - start;
      assert.ok(age !== null && age >= 2 * e.detail.burn_average_seconds * 1_000, `${e.requestId}: the round fires ${age === null ? 'with no start' : `${age / 60_000} min`} into its window`);
    }
    // Every shed line that read the trigger names the pair exactly where its source's 5-hour window is under the hold,
    // and a held line goes on to the mark, as a line inside a wait does.
    let held = 0;
    for (const r of records) {
      const line = r.walk.find((w) => w.step === 'move:shed');
      const value = (key) => line?.facts.find((f) => f.key === key);
      if (!value('ceiling') || r.pin?.accountId === null || r.pin?.accountId === undefined) continue;
      const at = Date.parse(r.request.at);
      const start = startOf(r, r.pin.accountId);
      const hold = 2 * r.settings.burnAverageSeconds * 1_000;
      const where = `${r.requestId}/${r.seq}`;
      if (start !== null && start < at && at - start < hold) {
        held += 1;
        assert.deepEqual([value('age_five_hour'), value('hold_five_hour')].map((f) => [f?.value, f?.unit]), [[at - start, 'ms'], [hold, 'ms']], where);
        assert.ok(value('marked') !== undefined || value('cooldown')?.value === true, `${where}: a held line reads no mark`);
      } else assert.deepEqual([value('age_five_hour'), value('hold_five_hour')], [undefined, undefined], where);
    }
    assert.ok(held > 0, 'a held shed line');
  });

  test(`every round's pin the pick read prices its rewrite as the story's 20x equivalent of its weekly window with the window named, at ${at}`, async () => {
    const s = await story;
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round');
    // The model's own week where it draws one, else the 7-day window.
    const weekOf = (model) => (model.includes('fable') ? 'seven_day_fable' : 'seven_day');
    const read = { api_key: 0, oauth: 0 };
    for (const e of rounds) {
      for (const p of e.detail.pins) {
        const where = `${e.requestId}: ${p.session_id}`;
        if (p.planned === null) {
          assert.deepEqual([p.rewrite, p.rewrite_window], [null, null], where);
          continue;
        }
        assert.equal(p.rewrite_window, weekOf(p.model), where);
        assert.equal(p.rewrite, s.lib.rewriteOf(p.last_input_tokens, p.model)[p.rewrite_window], where);
        read[s.accounts.find((a) => a.id === p.planned).kind] += 1;
      }
    }
    assert.ok(read.api_key > 0 && read.oauth > 0, `the pins the pick read by destination kind: ${JSON.stringify(read)}`);
  });

  test(`the Fable-week round lists its Opus pin off the window after the Fable pin and marks nothing for the last pin, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    const r = records.find((x) => x.requestId === 'req_3e7a5c19-f84' && x.seq === 1);
    assert.ok(r, 'the renewal on nightly at 08:30');
    assert.deepEqual([r.outcome.kind, r.outcome.accountId], ['renewed', 'nightly']);
    // The walk reaches the shed rule before the round is read: no rule before it fired, and its line ran the round.
    const at = r.walk.findIndex((w) => w.step === 'move:shed');
    const line = r.walk[at];
    const value = (key) => line?.facts.find((f) => f.key === key);
    assert.ok(at >= 0 && line.verdict !== 'not_run', `the shed line reads ${line?.verdict}`);
    assert.deepEqual(r.walk.slice(0, at).filter((w) => w.step.startsWith('move:') && w.verdict === 'fired').map((w) => w.step), []);
    assert.deepEqual([value('trigger')?.value, value('round')?.value], ['seven_day_fable', 'ran']);
    // A cooldown over nightly would end the line at `cooldown` before its mark, so the line reads the mark alone.
    assert.deepEqual([value('cooldown'), value('marked')?.value], [undefined, false]);
    const e = (await logOf(s)).find((row) => row.type === 'event' && row.kind === 'shedding_round' && row.requestId === r.requestId);
    assert.ok(e, "the round's event");
    const [fable, opus] = e.detail.pins;
    assert.deepEqual(
      [e.accountId, e.detail.window, e.detail.pins.map((p) => p.model), fable.skip, opus.skip, [opus.burn, opus.per_rewrite, opus.rewrite, opus.planned], e.detail.marks, e.detail.outcome, e.detail.reason],
      ['nightly', 'seven_day_fable', ['claude-fable-5-1', 'claude-opus-5'], 'last_pin', 'off_window', [null, null, null, null], [], 'nothing_to_mark', 'last_pin'],
    );
    // The 5-hour window opened 19 minutes before, under the hold of twice all's half-hour average.
    assert.deepEqual([value('age_five_hour'), value('hold_five_hour')].map((f) => [f?.value, f?.unit]), [[1_140_000, 'ms'], [3_600_000, 'ms']]);
  });

  test(`a booking a request ended without usage expires once at the request's end, with the cause its end carries, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    const expired = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'reservation_expired');
    // A later placement or move, a failover's among them, replaces the request's booking, and a renewal leaves it.
    const booked = new Map();
    for (const r of [...records].sort((x, y) => x.seq - y.seq)) if (r.outcome.kind === 'placed' || r.outcome.kind === 'moved') booked.set(r.requestId, r.outcome.reservation === null ? null : r);
    const causeOf = (r) =>
      r.outcome === 'canceled' ? 'canceled' : r.outcome === 'exhausted' ? 'exhausted' : r.outcome === 'incomplete' || r.errorCode === 'stream_undecodable' ? 'transport_error' : 'refused';
    const want = s.requests.filter((r) => r.outcome !== 'succeeded' && booked.get(r.id)).map((r) => {
      const last = r.attempts[r.attempts.length - 1];
      return [r.id, Date.parse(last.startedAt) + (last.durationMs ?? 0), booked.get(r.id).request.attempt, causeOf(r)];
    });
    const read = expired.map((e) => [e.requestId, Date.parse(e.at), e.detail.attempt, e.detail.cause]);
    const order = (x, y) => x[0].localeCompare(y[0]);
    assert.deepEqual(read.sort(order), want.sort(order));
    assert.ok(new Set(read.map((x) => x[3])).size > 1, `the causes read ${[...new Set(read.map((x) => x[3]))].join(', ')}`);
  });

  test(`the four routing conditions are the trailing day's fold of the routing log, one item per subject in the route's order, at ${at}`, async () => {
    const s = await story;
    const day = (await logOf(s)).filter((row) => row.type === 'event' && Date.parse(row.at) >= s.now - DAY && Date.parse(row.at) <= s.now).sort((x, y) => Date.parse(x.at) - Date.parse(y.at));
    const oauth = new Set(s.accounts.filter((a) => a.kind === 'oauth').map((a) => a.id));
    const spent = day.filter((e) => e.kind === 'window_spent' && oauth.has(e.accountId));
    const landed = day.filter((e) => e.kind === 'pin_migrated' && oauth.has(e.accountId) && spent.some((w) => w.accountId === e.accountId && Date.parse(w.at) >= Date.parse(e.at) && Date.parse(w.at) - Date.parse(e.at) <= HOUR));
    const rounds = day.filter((e) => e.kind === 'shedding_round' && e.detail.outcome === 'nothing_to_mark' && e.detail.reason === 'no_runway');
    const firsts = (kind, events, subject) => [...new Set(events.map(subject))].map((id) => [kind, id, Date.parse(events.find((e) => subject(e) === id).at)]);
    // A slow-down span runs from its start to the next end or restart of its account and pool, an open one to now, each
    // clamped to the day, a span with no part inside it not counted, and an account slowed past its bar holds from the
    // own start of its oldest span. The bar is twice the threshold the newest span's start row read, at least an hour.
    // An ended span's slowest factor is its end's, a span still open its start's.
    const spans = new Map();
    const count = (account, start, end, factor) => {
      const slowedMs = Math.min(end, s.now) - Math.max(start.since, s.now - DAY);
      if (slowedMs > 0) spans.set(account, [...(spans.get(account) ?? []), { factor, since: start.since, slowedMs, thresholdMs: start.thresholdMs }]);
    };
    // The spans standing at the cutoff: an account and pool whose newest start or end before the day is a start newer
    // than the newest restart before it.
    const before = (await logOf(s)).filter((row) => row.type === 'event' && Date.parse(row.at) < s.now - DAY).sort((x, y) => Date.parse(x.at) - Date.parse(y.at));
    const restart = Math.max(Number.NEGATIVE_INFINITY, ...before.filter((e) => e.kind === 'router_started').map((e) => Date.parse(e.at)));
    const newest = new Map();
    for (const e of before) if (e.kind === 'slow_down_started' || e.kind === 'slow_down_ended') newest.set(`${e.accountId}|${e.poolId}`, e);
    const standing = new Map([...newest].filter(([, e]) => e.kind === 'slow_down_started' && Date.parse(e.at) > restart).map(([key, e]) => [key, { factor: e.detail.factor, since: Date.parse(e.at), thresholdMs: e.detail.threshold_ms }]));
    for (const e of day) {
      const key = `${e.accountId}|${e.poolId}`;
      if (e.kind === 'router_started') {
        for (const [open, start] of standing) count(open.split('|')[0], start, Date.parse(e.at), start.factor);
        standing.clear();
      } else if (e.kind === 'slow_down_started' && !standing.has(key)) standing.set(key, { factor: e.detail.factor, since: Date.parse(e.at), thresholdMs: e.detail.threshold_ms });
      else if (e.kind === 'slow_down_ended' && standing.has(key)) {
        count(e.accountId, { ...standing.get(key), since: e.detail.since }, e.detail.since + e.detail.duration_ms, e.detail.factor_min);
        standing.delete(key);
      }
    }
    for (const [open, start] of standing) count(open.split('|')[0], start, s.now, start.factor);
    const barOf = (list) => Math.max(2 * list.reduce((a, b) => (b.since > a.since ? b : a)).thresholdMs, HOUR);
    const slowed = [...spans].filter(([, list]) => sum(list, (x) => x.slowedMs) > barOf(list)).map(([id, list]) => ['slowed_long', id, Math.min(...list.map((x) => x.since))]);
    const kinds = ['wall_reached', 'destination_spent', 'shed_no_runway', 'slowed_long'];
    const want = [...firsts('wall_reached', spent, (e) => e.accountId), ...firsts('destination_spent', landed, (e) => e.accountId), ...firsts('shed_no_runway', rounds, (e) => e.poolId), ...slowed].sort(
      (x, y) => kinds.indexOf(x[0]) - kinds.indexOf(y[0]) || x[2] - y[2] || x[1].localeCompare(y[1]),
    );
    const read = s.attention.filter((i) => kinds.includes(i.kind)).map((i) => [i.kind, i.subject.id, Date.parse(i.since)]);
    assert.deepEqual(read, want);
    assert.deepEqual(read.map((x) => `${x[0]} ${x[1]}`), ['wall_reached nightly', 'wall_reached secondary', 'destination_spent nightly', 'shed_no_runway all', 'slowed_long studio']);
    const item = s.attention.find((i) => i.kind === 'slowed_long');
    assert.deepEqual(
      [item.detail.spans, item.detail.open, item.detail.slowed_ms, item.detail.bar_ms, item.detail.factor_min, item.detail.setting, item.detail.setting_values],
      [spans.get('studio').length, true, sum(spans.get('studio'), (x) => x.slowedMs), barOf(spans.get('studio')), Math.min(...spans.get('studio').map((x) => x.factor)), ['slow_down_stretch_seconds'], { slow_down_stretch_seconds: s.pools.find((p) => p.id === 'work').slowDownStretchSeconds }],
      "studio's two spans in work, their bar, the closed one's slowest factor and the pool's stretch",
    );
    assert.deepEqual([item.detail.slowed_ms, item.detail.bar_ms, item.detail.factor_min], [7_700_000, 3_600_000, 0.35], "the closed span of 86 min 20 s and the open one since 13:48, past twice work's 30 minutes");
  });

  test(`the mock's reconciliations name the story's accounts and one off row, at ${at}`, async () => {
    const s = await story;
    const { reconciliations } = await s.api.getAnalyticsReconciliations();
    const oauth = new Set(s.accounts.filter((a) => a.kind === 'oauth').map((a) => a.id));
    for (const r of reconciliations) assert.ok(oauth.has(r.accountId), `${r.accountId}'s window names an OAuth account of the story`);
    const off = reconciliations.filter((r) => s.lib.ratioOff(r));
    assert.deepEqual(off.map((r) => [r.accountId, r.window]), [['nightly', 'five_hour']], 'one row reads off the band');
    // The condition reads each window's newest row, so the off row is the one it holds on.
    const [newest] = reconciliations.filter((r) => r.accountId === 'nightly' && r.window === 'five_hour').sort((x, y) => Date.parse(y.windowEnd) - Date.parse(x.windowEnd));
    assert.equal(off[0], newest, "the off row is nightly's newest 5-hour window");
    const drift = s.attention.filter((i) => i.kind === 'meter_drift');
    assert.deepEqual(
      drift.map((i) => [i.subject.id, i.since, i.detail.windows.map((w) => [w.window, w.ratio, w.observed_rise, w.window_end])]),
      [[off[0].accountId, off[0].windowEnd, [[off[0].window, off[0].ratio, off[0].observedRise, Date.parse(off[0].windowEnd)]]]],
      "the meter drift item names the off row's account, dated from its window's end",
    );
  });

  test(`the slow-down's story: the slowed request's hold and renewal on work, the round its stretch fired for the annex, the failover's cause on all and studio's span rows, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    const log = await logOf(s);
    const record = (id, seq) => records.find((r) => r.requestId === id && r.seq === seq);
    const slowWaits = (rows) => rows.flatMap((r) => r.held?.waits ?? []).filter((w) => w.reason === 'slow_down');

    // The slowed request waits its pin's cycle at half speed on studio's pin, then renews on it and runs the round.
    const row = s.requests.find((r) => r.id === 'req_9f2e7c5a-c49');
    assert.deepEqual([row.poolId, row.keyId, row.accountId, row.held.waits], ['work', 'notebook', 'studio', [{ heldMs: 42_000, reason: 'slow_down' }]]);
    const held = record(row.id, 1);
    const until = iso(Date.parse(row.at) + 42_000);
    assert.deepEqual(
      [held.outcome.kind, held.outcome.hold.reason, held.outcome.hold.factor, held.outcome.hold.cycleMs, held.outcome.hold.until, held.pin.slowDownUntil, held.outcome.hold.boundMs],
      ['held', 'slow_down', 0.5, 42_000, until, until, 120_000],
    );
    const renewal = record(row.id, 2);
    assert.deepEqual([renewal.outcome.kind, renewal.request.at, renewal.outcome.cohortHold], ['renewed', until, row.held], 'the renewal after the wait carries the hold the row names');
    const summaries = log.filter((x) => x.type === 'decision' && x.requestId === row.id).sort((x, y) => x.seq - y.seq);
    assert.deepEqual(summaries.map((x) => [x.by.kind, x.by.value, x.fact.map((f) => f.key)]), [['hold', 'slow_down', ['until', 'factor']], ['hold', 'slow_down', ['held']]]);
    const morning = record('req_8e1c4a6d-b52', 1);
    assert.deepEqual([morning.outcome.hold.reason, morning.outcome.hold.factor, record('req_8e1c4a6d-b52', 2).outcome.shedMarks], ['slow_down', 0.4, null], 'the morning hold renews with no round');

    // The stretch fires studio's round once the safety wait has passed, and its two marks plan the annex.
    const rounds = log.filter((x) => x.type === 'event' && x.kind === 'shedding_round');
    const stretched = rounds.find((e) => e.requestId === row.id);
    assert.deepEqual(
      [stretched.poolId, stretched.accountId, stretched.detail.trigger, stretched.detail.stretch_ms, stretched.detail.threshold_ms, stretched.detail.factor, stretched.detail.pins.filter((p) => p.marked).map((p) => p.planned)],
      ['work', 'studio', 'slow_down', 2_520_000, 1_800_000, 0.5, ['annex', 'annex']],
    );
    assert.deepEqual(renewal.outcome.shedMarks.length, 2);
    for (const e of rounds.filter((x) => x !== stretched)) assert.deepEqual([e.detail.trigger, e.detail.stretch_ms, e.detail.threshold_ms, e.detail.factor], ['projection', null, null, null], `${e.requestId}'s round`);
    assert.equal(rounds.reduce((a, b) => (Date.parse(b.at) > Date.parse(a.at) ? b : a)).detail.trigger, 'projection', 'the newest round is a projection round');
    assert.ok(Date.parse(held.request.at) < Date.parse(stretched.at), 'the hold comes before the round');

    // The failover on all lands with the 429 on the spent 5-hour window that caused it.
    const cause = { reason: 'rate_limit', scope: 'account', window: 'five_hour' };
    const landing = record('req_b8c3e9d4-e61', 2);
    assert.deepEqual([landing.request.poolId, landing.outcome.kind, landing.outcome.fromAccountId, landing.outcome.cause], ['all', 'placed', 'secondary', { cause: null, ...cause, state: null }]);

    // Every read of studio's entry is a pinned selection's, its stretch the time to the reset less the time the demand
    // takes from the utilization to the ceiling, and its factor one less the stretch over the time to the reset.
    const work = s.pools.find((p) => p.id === 'work');
    const reads = records
      .filter((r) => r.request.poolId === 'work' && r.pin?.accountId === 'studio' && r.walk.some((w) => w.step === 'move:shed' && w.facts.some((f) => f.key === 'slow_down' && f.value === 'on')))
      .map((r) => {
        const facts = r.walk.find((w) => w.step === 'move:shed').facts;
        const value = (key) => facts.find((f) => f.key === key)?.value;
        return { at: Date.parse(r.request.at), demand: value('demand_five_hour'), factor: value('factor'), found: r.pin.found, stretchMs: value('stretch_ms'), window: r.candidates.find((c) => c.accountId === 'studio').windows.five_hour };
      })
      .sort((x, y) => x.at - y.at);
    for (const x of reads) {
      const leftMs = Date.parse(x.window.reset) - x.at;
      const stretchMs = Math.max(0, leftMs - ((work.windowCeiling - x.window.utilization) / x.demand) * HOUR);
      assert.ok(x.found === 'live' && x.demand === x.window.demand, `${iso(x.at)}: the read is a pinned selection's on the demand its window reads`);
      assert.ok(Math.abs(x.stretchMs - stretchMs) <= 60_000, `${iso(x.at)}: the stretch reads ${x.stretchMs}, not ${Math.round(stretchMs)}`);
      assert.ok(Math.abs(x.factor - (1 - stretchMs / leftMs)) <= 0.01, `${iso(x.at)}: the factor reads ${x.factor}, not ${1 - stretchMs / leftMs}`);
    }

    // Studio's two spans in work, each opened by a read under 1 with that read's figures. The closed one runs to the
    // first read back at 1, its slowest factor the lowest read in between and its held time its rows' slow-down waits,
    // and it ends at the first read once the factor has read 1 for the safety wait. The open one stands.
    const spans = log.filter((x) => x.type === 'event' && x.kind.startsWith('slow_down_')).sort((x, y) => Date.parse(x.at) - Date.parse(y.at));
    assert.deepEqual(spans.map((e) => [e.kind, e.accountId, e.poolId]), [['slow_down_started', 'studio', 'work'], ['slow_down_ended', 'studio', 'work'], ['slow_down_started', 'studio', 'work']]);
    const [first, ended, open] = spans;
    for (const start of [first, open]) {
      const read = reads.find((x) => x.at === Date.parse(start.at));
      assert.deepEqual(
        [start.detail.factor, start.detail.stretch_ms, start.detail.demand, start.detail.reset, start.detail.threshold_ms],
        [read?.factor, read?.stretchMs, read?.window.demand, Date.parse(read?.window.reset), work.slowDownStretchSeconds * 1_000],
        `the start at ${start.at} carries its own read`,
      );
    }
    const since = Date.parse(first.at);
    const cleared = reads.find((x) => x.at > since && x.factor === 1).at;
    const within = s.requests.filter((r) => r.poolId === 'work' && Date.parse(r.at) >= since && Date.parse(r.at) < cleared);
    assert.deepEqual(
      [ended.detail.since, ended.detail.duration_ms, ended.detail.factor_min, ended.detail.held_ms, Date.parse(ended.at)],
      [since, cleared - since, Math.min(...reads.filter((x) => x.at >= since && x.at < cleared).map((x) => x.factor)), sum(slowWaits(within), (w) => w.heldMs), reads.find((x) => x.at - cleared >= 120_000).at],
    );
    assert.deepEqual([ended.detail.duration_ms, ended.detail.factor_min, ended.detail.held_ms], [5_180_000, 0.35, 36_000]);
    const studio = s.accounts.find((a) => a.id === 'studio');
    assert.deepEqual([studio.slowDown.since, studio.slowDown.factor, studio.slowDown.stretchMs], [open.at, reads.at(-1).factor, reads.at(-1).stretchMs], "the account's entry is the open span's newest read");

    // The held chart folds the same waits per bucket.
    const range = { from: iso(s.now - DAY), to: iso(s.now) };
    const { held: points } = await s.api.getAnalyticsRewrites(range);
    const inRange = s.requests.filter((r) => Date.parse(r.at) >= startOfHour(s.now - DAY) && Date.parse(r.at) < s.now + HOUR);
    assert.deepEqual([sum(points, (p) => p.heldMs), sum(points, (p) => p.requests)], [sum(slowWaits(inRange), (w) => w.heldMs), inRange.filter((r) => slowWaits([r]).length > 0).length]);
    assert.deepEqual([sum(points, (p) => p.heldMs), sum(points, (p) => p.requests)], [78_000, 2]);
  });

  test(`every plan-funded row of a weighted model tier carries non-null shares and equivalents, estimated where the model has no weight of its own, and no API key, overage or fast row carries one, at ${at}`, async () => {
    const s = await story;
    const costs = s.settings.subscriptionCosts;
    let priced = 0;
    let estimated = 0;
    for (const r of s.requests) {
      const serving = r.attempts[r.attempts.length - 1] ?? null;
      const funding = s.lib.fundingOf(serving);
      if (funding !== 'plan') {
        assert.equal(r.costShares, null, `${r.id} is ${funding}-funded and carries ${JSON.stringify(r.costShares)}`);
        assert.equal(r.costEquivalent, null, `${r.id} is ${funding}-funded and carries ${JSON.stringify(r.costEquivalent)}`);
        continue;
      }
      assert.ok(r.costShares !== null && typeof r.costShares === 'object', `${r.id} is plan-funded and carries no share map`);
      assert.ok(r.costEquivalent !== null && typeof r.costEquivalent === 'object', `${r.id} is plan-funded and carries no equivalent`);
      const values = [...Object.values(r.costShares), ...Object.values(r.costEquivalent ?? {})];
      // A model of a weighted model tier prices at its own weight or its tier's latest one.
      const weight = s.lib.weightOrFallback(costs, r.canonicalModel, 'five_hour');
      assert.ok(weight !== null, `${r.id} runs on ${r.canonicalModel}, which the fallback prices`);
      assert.ok(values.length > 0 && values.every((v) => typeof v === 'number'), `${r.id} on ${r.canonicalModel} reads ${JSON.stringify([r.costShares, r.costEquivalent])}`);
      assert.equal(r.estimated, weight?.estimated, `${r.id} on ${r.canonicalModel} reads estimated ${r.estimated}`);
      priced += 1;
      if (r.canonicalModel === 'claude-sonnet-4-6') estimated += 1;
    }
    assert.ok(priced > 0, 'the story prices a share');
    assert.ok(estimated > 0, "the story's Sonnet 4.6 rows price an estimate");
  });

  test(`every story request's dollars are the cells of the wire's price entry in force at its instant, none on a model the entry lacks, and an hour before the entry in force reads the earlier entry's, at ${at}`, async () => {
    const s = await story;
    const revisions = s.settings.apiPrices.revisions ?? [];
    assert.ok(revisions.length > 1, 'the wire lists the price revisions');
    // A dated id prices at its undated row, the ledger's lookup. Null for a model the entry lacks.
    const dollarsAt = (entry, model, u, fast) => {
      const row = entry?.models.find((m) => m.model === model) ?? entry?.models.find((m) => m.model === s.lib.undatedModel(model));
      if (row === undefined) return null;
      const prompt = u.input + u.cacheRead + u.cacheWrite5m + u.cacheWrite1h;
      const cells = fast && row.fast ? row.fast : row.longContext && prompt > row.longContext.over ? row.longContext : row;
      return (u.input * cells.inputPerM + u.cacheRead * cells.cacheReadPerM + u.cacheWrite5m * cells.cacheWrite5mPerM + u.cacheWrite1h * cells.cacheWrite1hPerM + u.output * cells.outputPerM) / 1_000_000;
    };
    let priced = 0;
    let unlisted = 0;
    for (const r of s.requests) {
      const entry = s.lib.revisionAt(revisions, r.at);
      const want = dollarsAt(entry, r.model, r.usage, r.attempts[r.attempts.length - 1]?.response.speed === 'fast');
      if (want === null) {
        assert.equal(r.costUsd, 0, `${r.id} on ${r.model}, which the entry of ${entry?.effective} lacks, reads ${r.costUsd}`);
        unlisted += 1;
        continue;
      }
      // A row's dollars are rounded to the hundredth of a cent.
      assert.ok(Math.abs(r.costUsd - want) <= 0.00005 + 1e-12, `${r.id} on ${r.model} at ${r.at} reads ${r.costUsd} against the entry of ${entry?.effective}'s ${want}`);
      priced += 1;
    }
    assert.ok(priced > 0 && unlisted > 0, `the story prices ${priced} rows and leaves ${unlisted} unlisted`);
    // The request rows all run after the latest entry's date, so the month's synthesized hours before it carry the
    // earlier entry, each pricing its tokens as one answer at the hour, as the mock writes it.
    const latest = s.lib.revisionAt(revisions, s.now)?.effective;
    let earlier = 0;
    for (const r of s.rollups) {
      const entry = s.lib.revisionAt(revisions, r.hour);
      if (r.kind !== 'inference' || entry?.effective === latest) continue;
      const t = r.tokens;
      const want = dollarsAt(entry, r.model, { cacheRead: t.cacheRead, cacheWrite1h: t.cacheWrite1h, cacheWrite5m: t.cacheWrite5m, input: t.uncachedInput, output: t.output }, r.speed === 'fast') ?? 0;
      assert.ok(Math.abs(r.costUsd - want) <= 1e-9 * Math.max(1, want), `the hour ${r.hour} on ${r.model} reads ${r.costUsd} against the entry of ${entry?.effective}'s ${want}`);
      earlier += 1;
    }
    assert.ok(earlier > 0, `no hour of the story prices before the entry of ${latest}`);
  });

  test(`the breakdown by model's 20x equivalents are the sums of the account rows' under the same filters, at ${at}`, async () => {
    const s = await story;
    const range = { from: iso(s.now - DAY), to: iso(s.now) };
    const byModel = await s.api.getAnalyticsBreakdown({ ...range, groupBy: 'model' });
    let compared = 0;
    for (const g of byModel) {
      const accounts = await s.api.getAnalyticsBreakdown({ ...range, groupBy: 'account', models: [g.key] });
      const windows = new Set([...Object.keys(g.costEquivalent ?? {}), ...accounts.flatMap((a) => Object.keys(a.costEquivalent ?? {}))]);
      for (const w of windows) {
        const summed = sum(accounts, (a) => a.costEquivalent?.[w] ?? 0);
        assert.ok(Math.abs((g.costEquivalent?.[w] ?? 0) - summed) <= 1e-9 * Math.max(1, summed), `${g.key}'s ${w} reads ${g.costEquivalent?.[w]} against its accounts' ${summed}`);
      }
      if (windows.size > 0) compared += 1;
    }
    assert.ok(compared > 0, 'a model carries an equivalent');
  });

  test(`every group's billed dollars and its plan rows' dollars add up to its dollars, at ${at}`, async () => {
    const s = await story;
    const range = { from: iso(s.now - DAY), to: iso(s.now) };
    for (const groupBy of ['account', 'model']) {
      const all = await s.api.getAnalyticsBreakdown({ ...range, groupBy });
      const plan = await s.api.getAnalyticsBreakdown({ ...range, funding: 'plan', groupBy });
      assert.ok(all.some((g) => g.billedUsd > 0) && plan.length > 0, `the day bills and plans under ${groupBy}`);
      for (const g of all) {
        const planned = plan.find((p) => p.key === g.key)?.costUsd ?? 0;
        assert.ok(typeof g.billedUsd === 'number' && near(g.billedUsd + planned, g.costUsd), `${groupBy} ${g.key}: ${g.billedUsd} billed and ${planned} planned against ${g.costUsd}`);
      }
    }
  });

  test(`the breakdown by key counts its plan requests and output tokens on every group, at ${at}`, async () => {
    const s = await story;
    const groups = await s.api.getAnalyticsBreakdown({ from: iso(s.now - DAY), groupBy: 'key', to: iso(s.now) });
    assert.ok(groups.length > 0);
    for (const g of groups) assert.ok(typeof g.planRequests === 'number' && typeof g.planOutputTokens === 'number', `${g.key} reads ${g.planRequests} and ${g.planOutputTokens}`);
    assert.ok(groups.some((g) => g.planRequests > 0), 'a key spent a subscription window');
  });

  test(`a breakdown that names one account answers its shares under any grouping, and one that names two answers none, at ${at}`, async () => {
    const s = await story;
    const range = { from: iso(s.now - DAY), to: iso(s.now) };
    const byModel = await s.api.getAnalyticsBreakdown({ ...range, accountIds: ['nightly'], groupBy: 'model' });
    const [own] = await s.api.getAnalyticsBreakdown({ ...range, accountIds: ['nightly'], groupBy: 'account' });
    assert.ok(byModel.length > 1 && own?.key === 'nightly', 'nightly served more than one model');
    for (const g of byModel) assert.ok(g.costShares !== null && g.cacheShares !== null && g.probeShares !== null, `nightly's ${g.key} reads its shares`);
    const week = sum(byModel, (g) => g.costShares?.seven_day ?? 0);
    assert.ok(own.costShares.seven_day > 0 && Math.abs(week - own.costShares.seven_day) <= 1e-9, `the models' 7-day shares ${week} against nightly's ${own.costShares.seven_day}`);
    const two = await s.api.getAnalyticsBreakdown({ ...range, accountIds: ['nightly', 'primary'], groupBy: 'model' });
    assert.ok(two.length > 0);
    for (const g of two) assert.deepEqual([g.costShares, g.cacheShares, g.probeShares], [null, null, null], `two accounts' ${g.key}`);
  });

  test(`the dashboard's day reads its billed dollars and the plan rows' summed 20x equivalents, at ${at}`, async () => {
    const s = await story;
    const plan = s.counted.filter((r) => r.costEquivalent !== null);
    const summed = (w) => sum(plan, (r) => r.costEquivalent[w] ?? 0);
    assert.ok(s.overview.equivalent24h && near(s.overview.equivalent24h.fiveHour, summed('five_hour')) && near(s.overview.equivalent24h.sevenDay, summed('seven_day')), `the day reads ${JSON.stringify(s.overview.equivalent24h)}`);
    assert.ok(near(s.overview.billed24h, sum(s.counted.filter((r) => r.costEquivalent === null), (r) => r.costUsd)), 'the billed rows alone');
    assert.equal(s.overview.estimated24h, s.counted.some((r) => r.estimated), "a fallback-priced row marks the day");
    assert.ok(near(sum(s.overview.hourly, (h) => h.equivalent?.fiveHour ?? 0), s.overview.equivalent24h.fiveHour), 'the hours add up to the day');
  });

  test(`every pinned plan session reads its burn as a 20x equivalent, estimated where the pin's model has no weight of its own, at ${at}`, async () => {
    const s = await story;
    const costs = s.settings.subscriptionCosts;
    const pinned = s.sessions.filter((x) => x.pinned && x.burnUnit === 'window_share');
    assert.ok(pinned.length > 0);
    for (const x of pinned) {
      assert.ok(typeof x.burnEquivalent === 'number' && typeof x.burn === 'number', `${x.sessionId} on ${x.model} burns ${x.burn} and reads ${x.burnEquivalent}`);
      assert.equal(x.estimated, s.lib.weightOrFallback(costs, x.model, 'five_hour')?.estimated, `${x.sessionId} on ${x.model}`);
    }
    assert.ok(pinned.some((x) => x.estimated), "a pin on the story's Sonnet 4.6 burns an estimate");
  });

  test(`every row's phases are in order and agree with its ttfbMs and durationMs, at ${at}`, async () => {
    const s = await story;
    const LATER = ['selectedAt', 'writtenAt', 'headAt', 'firstContentAt', 'streamEndAt', 'finishedAt'];
    for (const r of s.requests) {
      const p = r.phases;
      assert.equal(p.receivedAt, r.at, `${r.id} is received when it arrives`);
      const received = Date.parse(p.receivedAt);
      let prior = received;
      for (const key of LATER) {
        if (p[key] === null) continue;
        const at = Date.parse(p[key]);
        assert.ok(at >= prior, `${r.id}: ${key} at ${p[key]} lands before the phase before it, ${iso(prior)}`);
        prior = at;
      }
      assert.equal(p.selectedAt, r.attempts[0]?.startedAt ?? null, `${r.id} is selected when its first attempt starts`);
      assert.equal(p.headAt === null ? null : Date.parse(p.headAt) - received, r.ttfbMs, `${r.id}'s head against its ttfbMs`);
      assert.equal(p.finishedAt === null ? null : Date.parse(p.finishedAt) - received, r.durationMs, `${r.id}'s finish against its durationMs`);
      // The close is a stream's alone, and one that ended short or was canceled never reached it.
      if (r.transport !== 'sse' || r.outcome === 'incomplete' || r.outcome === 'canceled') assert.equal(p.streamEndAt, null, `${r.id} closes no stream`);
      if (r.kind === 'count_tokens') assert.equal(p.firstContentAt, null, `${r.id} streams no content token`);
    }
  });

  test(`the breakdown's account shares equal the sum of its rows' own shares, the cache splits priced at each row's hour, at ${at}`, async () => {
    const s = await story;
    const costs = s.settings.subscriptionCosts;
    const range = { from: iso(s.now - DAY), to: iso(s.now) };
    const groups = await s.api.getAnalyticsBreakdown({ ...range, groupBy: 'account' });
    const day = s.allRollups.filter((r) => Date.parse(r.hour) >= Date.parse(range.from) && Date.parse(r.hour) <= Date.parse(range.to));
    const kinds = new Map(s.accounts.map((a) => [a.id, a.kind]));
    const tiers = new Map(s.accounts.map((a) => [a.id, a.subscription?.rateLimitTier]));
    const usageOf = (r) => ({ cacheRead: r.tokens.cacheRead, cacheWrite1h: r.tokens.cacheWrite1h, cacheWrite5m: r.tokens.cacheWrite5m, input: r.tokens.uncachedInput, output: r.tokens.output });
    const windowsOf = (r) => ['five_hour', 'seven_day', ...s.lib.modelWeeks(costs, r.model)];
    // The rows' own shares, each request priced at its completion, summed as the route sums them: a window read null by
    // any row reads null for the group.
    const own = (rows) => {
      const out = {};
      for (const r of rows) {
        if (tokensOf(r) === 0) continue;
        for (const [w, share] of Object.entries(r.shares)) out[w] = share === null || out[w] === null ? null : (out[w] ?? 0) + share;
      }
      return out;
    };
    // The cache splits have no per-request figure on a row, so each row's tokens price at its hour.
    const summed = (rows, tier, usage) => {
      const out = {};
      for (const r of rows) {
        if (tokensOf(r) === 0) continue;
        for (const w of windowsOf(r)) {
          const share = s.lib.shareOrFallback(costs, w, tier, r.model, usage(r), Date.parse(r.hour))?.share ?? null;
          out[w] = share === null || out[w] === null ? null : (out[w] ?? 0) + share;
        }
      }
      return out;
    };
    const close = (got, want, where) => {
      if (want === null) assert.equal(got, null, `${where} reads ${got} where the rows price nothing`);
      else assert.ok(typeof got === 'number' && Math.abs(got - want) < 1e-9, `${where} reads ${got} against the rows' ${want}`);
    };
    let compared = 0;
    for (const g of groups) {
      const tier = tiers.get(g.key);
      const plan = day.filter((r) => r.accountId === g.key && r.funding === 'plan');
      const counted = plan.filter((r) => r.kind !== 'probe');
      const probes = plan.filter((r) => r.kind === 'probe');
      // The counts read under every account, a request no account served among them, as the route counts them.
      assert.equal(g.planRequests, sum(counted, requestsOf), `${g.key}'s plan requests`);
      assert.equal(g.planOutputTokens, sum(counted, (r) => r.tokens.output), `${g.key}'s plan output tokens`);
      if (kinds.get(g.key) !== 'oauth' || tier === undefined) {
        assert.deepEqual([g.costShares, g.cacheShares, g.probeShares], [null, null, null], `${g.key} prices a share`);
        continue;
      }
      const cost = own(counted);
      // An unpriced window is absent from the group's map, as the route answers it, while a request row keeps the key
      // with null.
      assert.deepEqual(Object.keys(g.costShares ?? {}).sort(), Object.keys(cost).filter((w) => cost[w] !== null).sort(), `${g.key}'s windows`);
      for (const [w, got] of Object.entries(g.costShares ?? {})) close(got, cost[w], `${g.key}'s ${w}`);
      const read = summed(counted, tier, (r) => ({ ...usageOf(r), cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 }));
      const write = summed(counted, tier, (r) => ({ ...usageOf(r), cacheRead: 0, input: 0, output: 0 }));
      // An unpriced window is absent from the cache and probe maps, as the route answers them.
      assert.deepEqual(Object.keys(g.cacheShares ?? {}).sort(), Object.keys(read).filter((w) => read[w] !== null && write[w] !== null).sort(), `${g.key}'s cache windows`);
      for (const [w, entry] of Object.entries(g.cacheShares ?? {})) {
        close(entry.read, read[w], `${g.key}'s ${w} cache read`);
        close(entry.write, write[w], `${g.key}'s ${w} cache write`);
      }
      const probe = own(probes);
      const probeUsd = sum(probes, (r) => r.probeCostUsd);
      const ratio = probeUsd > 0 ? sum(probes, (r) => r.wastedProbeUsd) / probeUsd : null;
      assert.deepEqual(Object.keys(g.probeShares ?? {}).sort(), Object.keys(probe).filter((w) => probe[w] !== null).sort(), `${g.key}'s probe windows`);
      for (const [w, entry] of Object.entries(g.probeShares ?? {})) {
        close(entry.cost, probe[w], `${g.key}'s ${w} probe cost`);
        close(entry.wasted, ratio === null ? null : probe[w] * ratio, `${g.key}'s ${w} wasted probe share`);
      }
      compared += 1;
    }
    assert.ok(compared > 0 && groups.some((g) => g.costShares !== null && Object.values(g.costShares).some((v) => v !== null)), 'an account carries a priced share');
  });

  test(`the first page under the default range holds the undecodable row and its capture stops after the second delta, at ${at}`, async () => {
    const s = await story;
    const { requests } = await s.api.listRequests({ from: iso(s.now - DAY), limit: 25, to: iso(s.now) });
    const row = requests.find((r) => r.id === 'req_5e2f7a19-b03');
    assert.ok(row, 'the first page holds the row');
    assert.deepEqual([row.keyId, row.errorCode, row.status, row.outcome, row.transport, row.accountId], ['laptop', 'stream_undecodable', 502, 'failed', 'sse', 'primary']);
    assert.deepEqual([row.attempts.length, row.attempts[0].upstreamStatus, row.ttfbMs, row.usage.output], [1, 200, null, 0], "the upstream's head arrived and the relay forwarded none of the stream it could not decode");
    const { response } = await s.api.getCapturedBodies(row.id);
    const events = response.split('\n\n').filter(Boolean).map((frame) => frame.split('\n')[0]);
    assert.deepEqual(events, ['event: message_start', 'event: content_block_start', 'event: content_block_delta', 'event: content_block_delta'], 'the capture ends after the second delta');
  });

  test(`the undecodable row files under its own error on the account that served its head, at ${at}`, async () => {
    const s = await story;
    const groups = await s.api.getAnalyticsBreakdown({ from: iso(s.now - DAY), groupBy: 'account', to: iso(s.now) });
    const primary = groups.find((g) => g.key === 'primary');
    const undecodable = s.counted.filter((r) => r.accountId === 'primary' && r.errorCode === 'stream_undecodable').length;
    const otherErrors = s.counted.filter((r) => r.accountId === 'primary' && r.status !== 200 && r.status !== 429 && r.status !== 529 && r.errorCode !== 'stream_undecodable').length;
    assert.ok(undecodable > 0, 'primary served the head of an undecodable stream');
    assert.deepEqual([primary.errors.stream_undecodable, primary.errors.api_error ?? 0], [undecodable, otherErrors], 'the count leaves the upstream errors for its own entry');
  });

  test(`the first page under the default range holds a plan-funded Fable row with captured bodies, at ${at}`, async () => {
    const s = await story;
    const { requests } = await s.api.listRequests({ from: iso(s.now - DAY), limit: 25, to: iso(s.now) });
    const fable = requests.filter((r) => r.canonicalModel === 'claude-fable-5-1' && r.capturedBodies && s.lib.fundingOf(r.attempts.at(-1) ?? null) === 'plan');
    assert.ok(fable.length > 0, 'the first page holds one');
    assert.ok((await s.api.getCapturedBodies(fable[0].id)) !== null, 'its bodies were captured');
  });

  test(`an answered attempt names its service tier on an inference and none on a token count, whose answer carries no usage, at ${at}`, async () => {
    const s = await story;
    const answered = s.requests.flatMap((r) => r.attempts.filter((a) => a.upstreamStatus === 200).map((a) => ({ id: r.id, kind: r.kind, tier: a.serviceTier })));
    const counts = answered.filter((a) => a.kind === 'count_tokens');
    assert.ok(counts.length > 0, 'the story holds an answered token count');
    assert.deepEqual(counts.filter((a) => a.tier !== null).map((a) => a.id), [], 'a token count reads no tier');
    assert.deepEqual(answered.filter((a) => a.kind === 'inference' && a.tier === null).map((a) => a.id), [], 'an answered inference names its tier');
  });

  test(`every rollup's funding reads the answer that served its requests, an API key account's answer filing under billed and a request no attempt served under plan, at ${at}`, async () => {
    const s = await story;
    // The ledger keys a rollup's funding off the served answer: an API key account's answer files billed whatever
    // its speed or overage flag, a fast answer files fast, a head in overage files overage, and a request no attempt
    // served, whatever speed it asked for, files plan. The requested speed names the speed key alone.
    const fundingOf = (r) => {
      const last = r.attempts[r.attempts.length - 1];
      if (last?.accountKind === 'api_key') return 'billed';
      return last?.response.speed === 'fast' ? 'fast' : last?.overageInUse === true ? 'overage' : 'plan';
    };
    const laneOf = (hour, keyId, poolId, accountId, model, kind, sessionId) => [hour, keyId, poolId, accountId, model, kind, sessionId].join('|');
    const expected = new Map();
    for (const r of s.requests) {
      const lane = laneOf(iso(startOfHour(Date.parse(r.at))), r.keyId, r.poolId, r.accountId ?? '-', r.model, r.kind, r.sessionId);
      expected.set(lane, new Set([...(expected.get(lane) ?? []), fundingOf(r)]));
    }
    const filed = new Map();
    for (const r of s.allRollups) {
      const lane = laneOf(r.hour, r.keyId, r.poolId, r.accountId, r.model, r.kind, r.sessionId);
      filed.set(lane, new Set([...(filed.get(lane) ?? []), r.funding]));
    }
    for (const [lane, want] of expected) assert.deepEqual([...(filed.get(lane) ?? [])].sort(), [...want].sort(), `the rollups of ${lane}`);
    // The history past the request log follows the same rule, so an API key account's hours file under billed in
    // every range the console offers.
    const apiKeys = new Set(s.accounts.filter((a) => a.kind === 'api_key').map((a) => a.id));
    const history = s.allRollups.filter((r) => apiKeys.has(r.accountId) && Date.parse(r.hour) < s.dayFrom);
    assert.ok(history.length > 0, 'the story holds history rows of an API key account');
    for (const r of history) assert.equal(r.funding, 'billed', `the history row of ${r.hour} on ${r.accountId} files under billed`);
    const unserved = s.requests.filter((r) => r.attempts.length === 0 && r.shape.requestedSpeed === 'fast');
    if (unserved.length > 0) {
      const lane = laneOf(iso(startOfHour(Date.parse(unserved[0].at))), unserved[0].keyId, unserved[0].poolId, '-', unserved[0].model, unserved[0].kind, unserved[0].sessionId);
      assert.ok(filed.get(lane)?.has('plan'), `${unserved[0].id} asked for fast and no attempt served it, so its rollup files under plan`);
    }
  });

  test(`a row names who put the diagnostics member on its body exactly when its serving attempt reads a diagnosed reason, at ${at}`, async () => {
    const s = await story;
    const diagnosed = (reason) => reason === 'none' || reason === 'pending' || reason === 'previous_message_not_found' || reason === 'unavailable' || reason.endsWith('_changed');
    for (const r of s.requests) {
      const reason = r.attempts.at(-1)?.cacheMissReason ?? null;
      if (reason !== null && diagnosed(reason)) assert.ok(r.diagnosticsSource === 'client' || r.diagnosticsSource === 'router', `${r.id} reads ${reason} and names ${r.diagnosticsSource}`);
      else assert.equal(r.diagnosticsSource, null, `${r.id} reads ${reason} and names a source`);
    }
    assert.deepEqual([...new Set(s.requests.map((r) => r.diagnosticsSource))].filter((v) => v !== null).sort(), ['client', 'router'], 'both sources occur');
    assert.ok(s.requests.some((r) => r.requestClass === null), 'a row sent no request class');
    assert.ok(s.requests.some((r) => r.shape.effort === null), 'a row asked for no effort');
  });

  test(`the story holds one request whose comparison was still running, on an account that asked, at ${at}`, async () => {
    const s = await story;
    const pending = s.requests.filter((r) => r.attempts.at(-1)?.cacheMissReason === 'pending');
    assert.equal(pending.length, 1, `the pending rows: ${pending.map((r) => r.id).join(', ') || 'none'}`);
    const [row] = pending;
    assert.equal(s.accounts.find((a) => a.id === row.attempts.at(-1).accountId)?.kind, 'oauth', 'an OAuth account asks for the comparison');
    assert.equal(row.diagnosticsSource, 'client', 'the body carried the member');
  });

  // The Analytics page's default range, the last day up to the story's instant, read off the rollups as the mock's
  // `inQuery` reads them: every hour that starts inside the range.
  const defaultRange = (s) => s.allRollups.filter((r) => Date.parse(r.hour) >= s.now - DAY && Date.parse(r.hour) <= s.now);

  test(`every reason the default range's totals count lists a request under it in the window its link writes, at ${at}`, async () => {
    const s = await story;
    const [totals] = s.lib.groupsOf(defaultRange(s), { groupBy: 'all', includeProbes: false });
    const counted = Object.entries(totals.cacheMissReasons).filter(([, n]) => n > 0);
    assert.ok(counted.length > 1, 'the range counts several reasons');
    const filters = { accountIds: [], clientPattern: '', includeProbes: false, keyIds: [], models: [], poolIds: [], sessionId: null, userAgentPattern: '' };
    for (const [reason, count] of counted) {
      const { href } = s.lib.requestsLink({ count, filters, reason, window: { exact: false, fromMs: s.now - DAY, nowMs: s.now, toMs: s.now } });
      const params = new URL(href, 'http://console.test').searchParams;
      const { requests } = await s.api.listRequests({ cacheMissReasons: params.get('cache_miss_reasons').split(','), from: params.get('from'), limit: 1, to: params.get('to') });
      assert.ok(requests.length > 0, `${reason} lists no request from ${params.get('from')} to ${params.get('to')}`);
    }
  });

  test(`the default range's effort and request class breakdowns count the story's rows per folded value, at ${at}`, async () => {
    const s = await story;
    const rows = s.requests.filter((r) => r.kind !== 'probe' && startOfHour(Date.parse(r.at)) >= s.now - DAY);
    const folds = { effort: (r) => s.lib.foldEffort(r.shape.effort), request_class: (r) => s.lib.foldRequestClass(r.requestClass) };
    for (const [groupBy, fold] of Object.entries(folds)) {
      const want = {};
      for (const r of rows) want[fold(r)] = (want[fold(r)] ?? 0) + 1;
      // A group of probe rows alone counts no request while probes are left out, as the route answers it.
      const groups = s.lib.groupsOf(defaultRange(s), { groupBy, includeProbes: false }).filter((g) => g.requests > 0);
      assert.deepEqual(Object.fromEntries(groups.map((g) => [g.key, g.requests]).sort()), Object.fromEntries(Object.entries(want).sort()), groupBy);
    }
  });

  test(`the default range counts a stream the client closed as failed under the code the relay files it with, at ${at}`, async () => {
    const s = await story;
    const closed = s.requests.filter((r) => r.outcome === 'canceled');
    assert.ok(closed.length > 0, 'the story holds a stream the client closed');
    for (const r of closed) assert.equal(r.errorCode, 'client_gone', `${r.id} names the code`);
    const rows = s.requests.filter((r) => r.kind !== 'probe' && startOfHour(Date.parse(r.at)) >= s.now - DAY);
    const [totals] = s.lib.groupsOf(defaultRange(s), { groupBy: 'all', includeProbes: false });
    assert.equal(totals.errors.client_gone ?? 0, rows.filter((r) => r.errorCode === 'client_gone').length, 'the Errors by code panel reads the story');
    // An undecodable stream files under its own code, apart from the upstream errors.
    const upstreamErrors = rows.filter((r) => r.status !== 200 && r.status !== 429 && r.status !== 529 && r.errorCode !== 'stream_undecodable');
    assert.equal(totals.errors.api_error ?? 0, upstreamErrors.length, 'and counts no closed stream as an upstream error');
  });
}

// An instant whose hour a full day back carries traffic before the dashboard's first bucket.
const firstHour = loading.then(() => storyAt('2026-09-16T09:30:00Z'));

test("the hour a full day back counts toward the accounts' and pools' day totals and a pool's first bucket, and toward no dashboard figure", async () => {
  const s = await firstHour;
  const early = s.totalled.filter((r) => Date.parse(r.at) < s.dayFrom);
  assert.ok(early.length > 0, "the story has traffic in the hour before the dashboard's first bucket");
  for (const a of s.accounts) {
    const own = (rows) => rows.filter((r) => r.accountId === a.id).length;
    assert.equal(a.requests.last24h, own(s.counted) + own(early), `${a.id}'s first hour`);
  }
  for (const p of s.pools) {
    assert.equal(p.traffic24h[0].requests, early.filter((r) => r.poolId === p.id).length, `${p.id}'s first hour`);
  }
  assert.equal(s.overview.requests24h, s.counted.length, "the dashboard's day keeps its own first hour");
});
