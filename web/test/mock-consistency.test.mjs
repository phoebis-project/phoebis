import assert from 'node:assert/strict';
import { test } from 'node:test';

// The mock skips its draws by the local hour and every instant below is chosen in UTC, so the file runs there whatever
// the host's zone; `node --test` gives each file a process of its own.
process.env.TZ = 'UTC';

const { accountSlots } = await import('./dist/api.js');

const HOUR = 3_600_000;
const DAY = 24 * HOUR;
const startOfHour = (ms) => Math.floor(ms / HOUR) * HOUR;
const iso = (ms) => new Date(ms).toISOString();

/**
 * Whether an instant, a request's arrival or a rollup's hour, falls in a range as the analytics routes read it: from
 * the hour holding the start up to the hour ceiling of the end, so an end exactly on the hour leaves that hour out.
 */
const inHours = (ms, fromMs, toMs) => ms >= startOfHour(fromMs) && ms < Math.ceil(toMs / HOUR) * HOUR;

const tokensOf = (r) => r.tokens.uncachedInput + r.tokens.cacheRead + r.tokens.cacheWrite5m + r.tokens.cacheWrite1h + r.tokens.output;
const sum = (list, read) => list.reduce((n, x) => n + read(x), 0);
const near = (a, b) => Math.abs(a - b) < 1e-6;
/** A request's five priced token counts, which the ledger prices a row by. */
const requestTokens = (r) => r.usage.input + r.usage.cacheRead + r.usage.cacheWrite5m + r.usage.cacheWrite1h + r.usage.output;
/** Whether a subscription window paid the request: its serving attempt's funding, which a request no account served
 *  has none of. */
const planPaid = (s, r) => r.attempts.length > 0 && s.lib.fundingOf(r.attempts[r.attempts.length - 1]) === 'plan';
/** The windows a plan request's shares are keyed by: the shared windows the account's reading names, both before any,
 *  then the model's weeks. */
const shareWindowsOf = (s, accountId, model) => {
  const ids = s.accounts.find((a) => a.id === accountId)?.windows.map((w) => w.id) ?? [];
  const held = [['five_hour', '5h'], ['seven_day', '7d']].filter(([, id]) => ids.includes(id)).map(([w]) => w);
  return [...(held.length === 0 ? ['five_hour', 'seven_day'] : held), ...s.lib.modelWeeks(s.settings.subscriptionCosts, model)];
};

/** The engine's pin key, so a session's subagents on another model, or on another account, hold pins of their own. */
const pinOf = (r) => `${r.keyId}|${r.sessionId}|${r.agentId}|${r.model}`;
/** The affinity window of the pool a recipe's key routes through, in milliseconds. */
const affinityOf = (s, r) => s.pools.find((p) => p.id === s.keys.find((k) => k.id === r.keyId)?.poolId).affinityTtlSeconds * 1_000;
/** A recipe's last use, its last request or its last keepalive probe, either of which renews its pin. */
const lastUseOf = (r) => Math.max(...[r.lastRequestAt, r.lastProbeAt].flatMap((x) => (x === null ? [] : [Date.parse(x)])));
/**
 * The recipes whose pins the router holds: every live one, and a released one whose last use is inside its pool's
 * affinity window, since the mod's release ends the keepalive record and leaves the pin until the router expires it.
 */
const pinnedRecipesOf = (s, recipes) => recipes.filter((r) => r.agentState !== 'released' || s.now - lastUseOf(r) <= affinityOf(s, r));
/**
 * The account a recipe's pin stands on, its line's, as the router's pin row holds it: a recipe held under `pin_moved`
 * keeps the account its pin left, and a released agent's pin, which draws no line, stands on its recipe's.
 */
const pinAccountOf = (s, r) => s.sessions.find((x) => !x.historical && x.keyId === r.keyId && x.sessionId === r.sessionId)?.agentContexts.find((c) => c.agentId === r.agentId && c.model === r.model)?.accountId ?? r.accountId;
/**
 * Whether no recipe warms a pinned session's main, whose agents then hold one pin each beside the recipes' own. The
 * session's summary reads a subagent's recipe where main holds none, so the recipes themselves are read.
 */
const mainUnwarmed = (recipes, x) => !recipes.some((r) => r.keyId === x.keyId && r.sessionId === x.sessionId && r.agentId === 'main');
const TTL_MS = { '1h': HOUR, '5m': 5 * 60_000 };
/**
 * The longest cache TTL of the pool a recipe's key routes through, the span the router lists a released record for
 * after its release: the main TTL of each rule set its members' kinds bring where the policy is on, and both TTLs where
 * it leaves the markers as sent, as the mock's `composedFor` reads the policy.
 */
const longestTtlOf = (s, r) => {
  const pool = s.pools.find((p) => p.id === s.keys.find((k) => k.id === r.keyId)?.poolId);
  if (!pool.cacheTtl.enabled) return TTL_MS['1h'];
  const kinds = pool.members.map((m) => m.kind);
  const mains = [...(kinds.includes('oauth') ? [pool.cacheTtl.plan.main] : []), ...(kinds.includes('api_key') ? [pool.cacheTtl.metered.main] : [])];
  return Math.max(...mains.map((t) => TTL_MS[t]));
};

/**
 * The story's refusals, each seed flagged as the ledger's landing would flag it: the earlier runner's two sends on ci
 * the gate refused, the runner's send on ci after its key's revocation, the three guardrail blocks on desktop, and the
 * contract and the drift refusals on laptop.
 */
const REFUSED_IDS = ['req_0c4e7a91-b5d', 'req_2d8e5b13-f40', 'req_5d1b8e2f-c64', 'req_6c3a9e27-5b1', 'req_9b4f1c62-a07', 'req_a8f3c6d0-e17', 'req_e0a4c6f2-118', 'req_f1c8e3a5-29d'];

/**
 * Whether the accounts answer withholds a row's windows, by `windows_withheld` in crates/admin/src/routes.rs: a
 * subscription row that is not paired, needs re-auth or whose plan ended, while the ledger keeps its polls' samples.
 */
const answerWithholdsWindows = (a) => a.kind === 'oauth' && (a.status === 'unpaired' || a.status === 'reauth_required' || a.planEndedAt !== null);

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
  // `dayFrom` is the hour 23 hours before the current one. The dashboard's day, an account's or a pool's day total, and
  // the dashboard's and a pool's 25 hourly buckets start an hour earlier, on the hour a full day back, and the week on
  // the hour seven days back, as the routes total their rollups.
  const dayFrom = startOfHour(now) - 23 * HOUR;
  const totalsFrom = startOfHour(now) - 24 * HOUR;
  const weekFrom = startOfHour(now) - 7 * DAY;
  const counted = requests.filter((r) => r.kind !== 'probe' && Date.parse(r.at) >= dayFrom);
  const totalled = requests.filter((r) => r.kind !== 'probe' && Date.parse(r.at) >= totalsFrom);
  // The rows the router asked an account for, which every count reads: a refused request is in the logs alone.
  const asked = (rows) => rows.filter((r) => !r.refused);
  const rollups = d.ALL_ROLLUPS.filter((r) => r.kind !== 'probe');
  const settings = await api.getSettings();
  return { accounts, allRollups: d.ALL_ROLLUPS, api, asked, attention, counted, dayFrom, events, keys, leadOf: d.leadOf, lib: d, now, overview, pools, requests, rollups, sessions, settings, totalled, totalledFrom: totalsFrom, weekFrom };
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
 * The members a record's partitions kept, the set the engine's pick orders by tier and then by score: the members of
 * the record's narrowed set whose `partition` is null, since a partition writes its label on each row it drops.
 */
function keptOf(r) {
  return narrowedSetOf(r).filter((c) => c.partition === null);
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
  'affinity_mode', 'affinity_ttl_seconds', 'auto_mode_server', 'body_capture_hours', 'burn_average_seconds', 'cache_diagnostics', 'cache_ttl', 'enabled', 'fable_reservation_shedding', 'five_hour_ceiling', 'in_flight_cap',
  'keepalive.enabled', 'keepalive.max_idle_seconds', 'keepalive.max_parked_seconds', 'keepalive.scheduled_horizon_seconds', 'label', 'load_shedding',
  'migrate_when_spent', 'min_shared_prefix_tokens', 'model_rules', 'overage_move_grace_minutes', 'overage_policy', 'prefer_subscription', 'retry_in_place_seconds', 'return_to_plan',
  'return_to_plan_dwell_seconds', 'session_placement', 'shed_cap', 'shed_dwell_seconds', 'slow_down', 'slow_down_hold_seconds', 'slow_down_stretch_seconds', 'strategy', 'subagent_placement',
  'thinking_diagnostics', 'warm_sessions', 'weekly_ceiling', 'wind_down_minutes',
];
/** The fields an account edit carries as pairs, beside the flag a staged secret sets. */
const ACCOUNT_DIFF_MEMBERS = ['canceled', 'enabled', 'label', 'state', 'subscription_ends_at'];

/** Whether one tier ranks before another, compared member by member as the engine orders them. */
const tierBefore = (x, y) => {
  const i = x.findIndex((n, j) => n !== y[j]);
  return i >= 0 && x[i] < y[i];
};
const sameTier = (x, y) => x.tier.join('/') === y.tier.join('/');

/** Whether a record's partitions narrow a move's destinations, which the no-fit partition keeps as its gate left them. */
const moving = (r) => r.walk.some((w) => w.step === 'cap_filter');
/**
 * Whether a record lands a failed-over request: a placement after an attempt that names the account the request came
 * from.
 */
const failedOver = (r) => r.outcome.kind === 'placed' && r.outcome.fromAccountId !== null && r.request.attempted.length > 0;

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
 * Whether a member holds no allowance for a request of `model`: a plan window the model draws, both shared windows and
 * a Fable model's week as well, reads 1 or more. An API key reads no plan window, so it always holds allowance.
 */
const spentOn = (c, model) => ['five_hour', 'seven_day', ...(model.includes('fable') ? ['seven_day_fable'] : [])].some((w) => (c.windows[w]?.utilization ?? 0) >= 1);

/**
 * The members a placement's fit reads: the set the spent and cap partitions left, or the whole set the cap holds for
 * a slot when every member is at it. A Fable request's spent member drops unless the pool funds it.
 */
function fitReadOf(set, pool, r) {
  const funded = (c) => pool.overagePolicy === 'allow' && c.credits !== null && c.credits.blocked === null;
  const left = r.request.canonicalModel.includes('fable') ? set.filter((c) => !spentOn(c, r.request.canonicalModel) || funded(c)) : set;
  const free = left.filter((c) => r.request.flags.count || c.inFlight < c.inFlightCap);
  return new Set(free.length > 0 ? free : left);
}

test("the placements answer counts a row from the hour that holds the range's start up to the hour ceiling of its end", async () => {
  const d = await import('./dist/api.js');
  // One static placement that read the cache, a hit, per row.
  const placed = (at) => ({ at, attempts: [{ selection: { cacheRead: 100, placement: { prefixTokens: 0, rung: 'static' } } }] });
  // A past range from 21:31 to 23:31: the route reads 21:00 up to midnight, the hour ceiling, which it leaves out.
  const rows = [placed('2026-09-16T20:59:30.000Z'), placed('2026-09-16T21:10:00.000Z'), placed('2026-09-16T23:45:00.000Z'), placed('2026-09-17T00:00:00.000Z')];
  const answer = d.placementsOf(rows, { from: '2026-09-16T21:31:00.000Z', to: '2026-09-16T23:31:00.000Z' });
  assert.deepEqual(answer.map((r) => [r.rung, r.placements]), [['static', 2]], 'the rows of 21:10 and 23:45');
});

test('the placements answer matches a Clients pattern against the client label folded as the rollups fold it', async () => {
  const d = await import('./dist/api.js');
  // A label of 72 bytes, past the fold's 64, which the route reads as `other`.
  const placed = { at: '2026-09-16T21:10:00.000Z', attempts: [{ selection: { cacheRead: 100, placement: { prefixTokens: 0, rung: 'static' } } }], client: `claude-code/${'x'.repeat(60)}` };
  const range = { from: '2026-09-16T21:00:00.000Z', to: '2026-09-16T22:00:00.000Z' };
  assert.deepEqual(d.placementsOf([placed], { ...range, clients: ['other'] }).map((r) => r.placements), [1], 'under other');
  assert.deepEqual(d.placementsOf([placed], { ...range, clients: ['claude-code/*'] }), [], 'under claude-code/*');
});

test("the rewrites answer counts a move from the hour that holds the range's start up to the hour ceiling of its end", async () => {
  const d = await import('./dist/api.js');
  // A move an account's state forced, which the ledger files in its hour's rollup.
  const forced = (at) => ({ at, detail: { reason: 'drain', reasons: [] }, kind: 'pin_migrated' });
  // Seven days from 21:31 on the 9th: the six-hour buckets run from 18:00 on the 9th to midnight on the 16th, and the
  // route reads the hours from 21:00 on the 9th up to 22:00 on the 16th, the hour ceiling of the end.
  const events = [forced('2026-09-09T20:10:00.000Z'), forced('2026-09-09T21:10:00.000Z'), forced('2026-09-16T21:45:00.000Z'), forced('2026-09-16T22:10:00.000Z')];
  const answer = d.rewritesOf([], [], events, { from: '2026-09-09T21:31:00.000Z', to: '2026-09-16T21:31:00.000Z' });
  assert.deepEqual(answer.moves, { drain: 2 }, 'the moves of 21:10 on the 9th and 21:45 on the 16th');
});

test("the refusal categories answer counts a refusal from the hour that holds the range's start up to the hour ceiling of its end", async () => {
  const d = await import('./dist/api.js');
  // A request whose serving attempt the upstream refused under cyber.
  const refused = (at) => ({ at, attempts: [{ response: { refusalCategory: 'cyber', stopReason: 'refusal' } }], canonicalModel: 'claude-sonnet-5-5', fallbackDigest: null, kind: 'inference' });
  // A past range from 21:31 to 23:31: the route reads 21:00 up to midnight, the hour ceiling, which it leaves out.
  const rows = [refused('2026-09-16T20:59:30.000Z'), refused('2026-09-16T21:10:00.000Z'), refused('2026-09-16T23:45:00.000Z'), refused('2026-09-17T00:00:00.000Z')];
  const answer = d.refusalCategoriesOf(rows, { from: '2026-09-16T21:31:00.000Z', groupBy: 'all', to: '2026-09-16T23:31:00.000Z' });
  assert.deepEqual(answer.rows, [{ category: 'cyber', model: 'claude-sonnet-5-5', requests: 2 }], 'the refusals of 21:10 and 23:45');
});

test('the refusal categories answer counts a request filed under no pool under the `-` pool selector', async () => {
  const d = await import('./dist/api.js');
  // Two refusals of one hour, one of a request the router filed under no pool and one on the work pool.
  const refused = (poolId) => ({ at: '2026-09-16T21:10:00.000Z', attempts: [{ response: { refusalCategory: 'cyber', stopReason: 'refusal' } }], canonicalModel: 'claude-sonnet-5-5', fallbackDigest: null, kind: 'inference', poolId });
  const answer = d.refusalCategoriesOf([refused(null), refused('work')], { from: '2026-09-16T21:00:00.000Z', groupBy: 'all', poolIds: ['-'], to: '2026-09-16T22:00:00.000Z' });
  assert.deepEqual(answer.rows, [{ category: 'cyber', model: 'claude-sonnet-5-5', requests: 1 }], 'the refusal with no pool alone');
});

test("a billed probe's dollars count with probes off", async () => {
  const d = await import('./dist/api.js');
  // Two probe rows of one hour: a usage-based seat's, billed $0.40 for two probes, and a plan's of $0.30 for three.
  const template = d.ALL_ROLLUPS.find((r) => r.kind === 'probe');
  const outcomes = (ok) => ({ client_gone: 0, incomplete: 0, ok, overloaded: 0, rate_limited: 0, router_shutdown: 0, undecodable: 0, upstream_error: 0 });
  const probes = (ok) => ({ failed: 0, held: 0, ok, skipped: 0 });
  const seat = { ...template, cost: { cacheRead: 0.38, cacheWrite: 0, input: 0, output: 0.02 }, costUsd: 0.4, funding: 'usage_based', probes: probes(2), requests: outcomes(2) };
  const plan = { ...template, cost: { cacheRead: 0.28, cacheWrite: 0, input: 0, output: 0.02 }, costUsd: 0.3, funding: 'plan', probes: probes(3), requests: outcomes(3) };
  const query = { from: template.hour, groupBy: 'all', includeProbes: false, to: iso(Date.parse(template.hour) + HOUR) };
  const [all] = d.breakdownOf([seat, plan], query);
  // The seat's dollars and their split under both sums, as the route adds a billed probe whatever the flag, and no
  // request of either row.
  const split = { cacheRead: 0.38, cacheWrite: 0, input: 0, output: 0.02 };
  assert.deepEqual(all && [all.costUsd, all.cost, all.billedUsd, all.billed, all.requests], [0.4, split, 0.4, split, 0]);
  // A group of plan probes alone leaves the answer, as the route drops one from a read that does not count probes.
  assert.deepEqual(d.breakdownOf([plan], query), []);
});

test("the mock counts a probe's cancel under the client's code as failed, as the route keeps the keepalive's own request", async () => {
  const d = await import('./dist/api.js');
  const template = d.ALL_ROLLUPS.find((r) => r.kind === 'probe');
  const requests = { client_gone: 1, incomplete: 0, ok: 0, overloaded: 0, rate_limited: 0, rejected: 0, router_shutdown: 0, undecodable: 0, upstream_error: 0 };
  const cut = { ...template, errors: { client_gone: 1 }, requests };
  const [all] = d.breakdownOf([cut], { from: template.hour, groupBy: 'all', includeProbes: true, to: iso(Date.parse(template.hour) + HOUR) });
  assert.deepEqual(all && [all.clientCanceled, all.failed, { ...all.errors }], [0, 1, { client_gone: 1 }]);
});

test("the burn answer keeps an unregistered account's plan row in its share and plan dollars, and answers a group only where a row holds a token, priced or not", async () => {
  const d = await import('./dist/api.js');
  const hour = '2026-09-16T10:00:00.000Z';
  const tokens = (uncachedInput) => ({ cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, output: 0, uncachedInput });
  const row = (accountId, over) => ({ accountId, client: '-', costUsd: 0, equivalent: 0.01, funding: 'plan', hour, model: 'claude-opus-5-5', tokens: tokens(0), ...over });
  // Four accounts the registry no longer holds, one group each under the account grouping.
  const rows = [
    row('gone-plan', { costUsd: 0.25, tokens: tokens(1_000) }),
    // A request the upstream rate limited, which books the account a row with no token, its figure 0 as the rollups
    // write it.
    row('gone-failed', { equivalent: 0 }),
    // A model the cost model has no weight for, so the row's tokens carry no figure.
    row('gone-unpriced', { equivalent: null, model: 'claude-unknown-1', tokens: tokens(1_000) }),
    // Overage dollars beside a rate limited plan request, whose row holds no token.
    row('gone-billed', { costUsd: 0.4, funding: 'overage', tokens: tokens(1_000) }),
    row('gone-billed', { equivalent: 0 }),
  ];
  const { points } = d.burnOf(rows, { from: hour, to: '2026-09-16T11:00:00.000Z' }, 'account', { accountIds: ['gone-plan', 'gone-failed', 'gone-unpriced', 'gone-billed'] });
  const units = Object.fromEntries(points.map((p) => [p.group, p.unit]));
  const plan = points.find((p) => p.group === 'gone-plan');
  assert.deepEqual([units['gone-plan'], plan?.planUsd, plan?.usd], ['window_share', 0.25, 0], 'its plan row reads a window share with its dollars on the plan side');
  assert.equal(units['gone-failed'], undefined, 'a group whose rows hold no token is left out');
  assert.equal(units['gone-unpriced'], 'window_share', 'a token the cost model cannot weigh keeps its group');
  assert.equal(units['gone-billed'], 'usd', 'a plan row with no token leaves the unit of the billed rows');
});

// Catches a sweep rounded up to the deadline's own instant: the pin's last use plus its TTL falls on a sweep, so the
// row stands a millisecond past that sweep and is gone behind the next one. The case of the purged conversation's turns
// misses it, since their deadlines lie between two sweeps.
test('a pin whose window ends on a purge sweep stands until the sweep behind it', async () => {
  const d = await import('./dist/api.js');
  const TTL = 5 * 60_000;
  const sweep = d.ROUTER_RESTARTED_AT + 10 * 60_000;
  assert.deepEqual([d.pinReadAt(sweep - TTL, TTL, sweep + 1), d.pinReadAt(sweep - TTL, TTL, sweep + 60_001)], ['expired_idle', 'none']);
});

// Catches a read with no test of the TTL, which finds a pin live at its own instant on a pool that pins nothing, and,
// by the second read, one that answers none for every TTL.
test('a pool that pins nothing holds no pin to read', async () => {
  const d = await import('./dist/api.js');
  assert.deepEqual([d.pinReadAt(d.ROUTER_RESTARTED_AT, 0, d.ROUTER_RESTARTED_AT), d.pinReadAt(d.ROUTER_RESTARTED_AT, 1, d.ROUTER_RESTARTED_AT)], ['none', 'live']);
});

// One load at a time, since each pins the clock its bundle reads while it loads.
let loading = Promise.resolve();

// The instant the smoke run and the browser pass pin, whose views the site's screenshots show, and a Sunday night,
// where the draws skip the overnight hours and the weekend damps the synthesized days.
for (const at of ['2026-09-16T14:30:00Z', '2026-09-20T03:40:00Z']) {
  const story = (loading = loading.then(() => storyAt(at)));

  test(`every id a row names is in the mock's own lists or an account the story deleted, under the one label the list gives it, at ${at}`, async () => {
    const s = await story;
    const accountLabel = new Map(s.accounts.map((a) => [a.id, a.label]));
    const poolName = new Map(s.pools.map((p) => [p.id, p.name]));
    const keyName = new Map(s.keys.map((k) => [k.id, k.name]));
    // The rows an account wrote before its delete keep naming it, as the ledger keeps them.
    const deleted = new Set((await logOf(s)).filter((x) => x.type === 'event' && x.kind === 'registry_changed' && x.detail.kind === 'account_deleted').map((x) => x.detail.target));
    const named = (map, id, where) => assert.ok(map.has(id) || (map === accountLabel && deleted.has(id)), `${where} names ${id}, which the list lacks`);
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

  // Catches a mock that prices every regime's entry at one regime, or copies the base entry's figures. Primary reads
  // 0.62 of its 5-hour window under all's ceiling of 0.75, with a demand that reaches the ceiling short of the reset
  // under either prior, and the interactive rates price a fresh session's prior lower, so that runway runs longer. The
  // mock reads a member's figures when its API is built, so the case builds one at the story's instant, as the smoke
  // and the browser pass read them.
  test(`a member's interactive entry reads a runway of its own beside the base entry's, at ${at}`, async () => {
    const s = await story;
    const wall = Date.now;
    Date.now = () => s.now;
    let api;
    try {
      api = new s.lib.MockApi();
    } finally {
      Date.now = wall;
    }
    await api.login('admin');
    const fits = (await api.listPools()).find((p) => p.id === 'all').members.find((m) => m.accountId === 'primary').regimeFits;
    assert.deepEqual(fits.map((f) => [f.regime, f.fit, typeof f.runwayMs]), [[null, 'rewrite_only', 'number'], ['interactive', 'rewrite_only', 'number']]);
    assert.ok(fits[1].runwayMs > fits[0].runwayMs, `the interactive runway ${fits[1].runwayMs} ms runs longer than the base one ${fits[0].runwayMs} ms`);
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
    // The answer withholds the windows of a row whose state reads none, whose usage block keeps the instant of its last
    // reading. An account that holds no window reading takes its facts from the routing log's events alone.
    for (const a of s.accounts) {
      const readings = answerWithholdsWindows(a) ? (a.usage?.fetchedAt ? [Date.parse(a.usage.fetchedAt)] : []) : [...a.windows, ...(a.fableWindow ? [a.fableWindow] : [])].map((w) => Date.parse(w.observedAt));
      if (readings.length === 0) continue;
      const late = facts.filter((r) => r.accountId === a.id && Date.parse(r.hour) > Math.max(...readings));
      assert.equal(late.length, 0, `${a.id}'s newest reading is ${iso(Math.max(...readings))}, before ${late.length} of its account facts from ${late[0]?.hour}`);
    }
  });

  test(`an account whose window set lacks the 5-hour window reads no 5-hour peak and no cooldown in its account facts, at ${at}`, async () => {
    const s = await story;
    const facts = await s.api.listAccountRollups(iso(s.now - 31 * DAY), iso(s.now));
    // A 5-hour peak reads the hour's 5-hour samples and the heads of its attempts, which an account without the window
    // never holds. The drawn cooldowns key off the hour's highest 5-hour sample where one stands, else the 5-hour walk,
    // and an account without the window keys neither. The answer withholds the windows of a row whose state reads none,
    // so such a row's window set reads off the windows of its quota history, which the ledger keeps. An account with no
    // window set takes its cooldowns from the routing log's events, which no window keys.
    const sets = await Promise.all(s.accounts.map(async (a) => [a.id, answerWithholdsWindows(a) ? (await s.api.getQuotaHistory(a.id)).map((h) => h.windowId) : a.windows.map((w) => w.id)]));
    const has5h = new Map(sets.filter(([, ids]) => ids.length > 0).map(([id, ids]) => [id, ids.includes('5h')]));
    const without = facts.filter((r) => has5h.get(r.accountId) === false);
    assert.ok(without.length > 0, 'the story holds account facts of an account without the window');
    assert.deepEqual(without.filter((r) => r.peak['5h'] !== null || r.cooldownSeconds !== 0).map((r) => `${r.accountId} ${r.hour} ${r.peak['5h']} ${r.cooldownSeconds}`), []);
  });

  test(`each account hour's peak is the larger of the hour's quota samples and its attempts' heads, an hour with no request row reads one where its rollups count an attempt, and each sampled hour lists its row, at ${at}`, async () => {
    const s = await story;
    const facts = await s.api.listAccountRollups(iso(s.now - 31 * DAY), iso(s.now + DAY));
    // The two sources the router reads a peak from, each by the hour it falls in: the quota history's samples, and the
    // heads of the attempts on the account by the attempt's start. The route also lists an hour where a sample of the
    // account stands, whatever the other figures and the row's state.
    const peaks = ['5h', '7d', '7d_fable'];
    const highest = new Map();
    const raise = (accountId, ms, windowId, utilization) => {
      if (utilization === null || !peaks.includes(windowId)) return;
      const key = `${accountId} ${startOfHour(ms)} ${windowId}`;
      highest.set(key, Math.max(highest.get(key) ?? 0, utilization));
    };
    const sampled = new Set();
    const sampledWindows = new Set();
    // The answer withholds a row's windows where its state reads none, so such a row's window set reads off the windows
    // of its quota history.
    const holds = new Map();
    for (const a of s.accounts) {
      const history = await s.api.getQuotaHistory(a.id);
      holds.set(a.id, answerWithholdsWindows(a) ? history.map((h) => h.windowId) : [...a.windows.map((w) => w.id), ...(a.fableWindow ? ['7d_fable'] : [])]);
      for (const h of history) {
        for (const x of h.samples) {
          sampled.add(`${a.id} ${startOfHour(Date.parse(x.at))}`);
          sampledWindows.add(`${a.id} ${startOfHour(Date.parse(x.at))} ${h.windowId}`);
          raise(a.id, Date.parse(x.at), h.windowId, x.utilization);
        }
      }
    }
    for (const r of s.requests) for (const t of r.attempts) for (const w of t.windows) raise(t.accountId, Date.parse(t.startedAt), w.id, w.utilization);
    // An hour with no request row holds no attempt of its own, while its rollups count the attempts on an account,
    // whose heads the router folds. So such an hour reads a peak on each window that the account holds and that holds
    // no sample in the hour, and its figure is the story's own. The series answers hourly buckets for a two-day range.
    const logged = new Set(s.requests.map((r) => startOfHour(Date.parse(r.at))));
    const spans = [];
    for (let from = startOfHour(s.now - 31 * DAY); from <= s.now; from += 2 * DAY) spans.push(from);
    const points = (await Promise.all(spans.map((from) => s.api.getAnalyticsSeries({ from: iso(from), groupBy: 'account', includeProbes: true, to: iso(from + 2 * DAY) })))).flat();
    const counted = new Set(points.flatMap((p) => (logged.has(Date.parse(p.at)) ? [] : p.groups.filter((g) => g.attempts > 0).map((g) => `${g.key} ${Date.parse(p.at)}`))));
    const headed = (k, w) => counted.has(k) && (holds.get(k.split(' ')[0]) ?? []).includes(w) && !sampledWindows.has(`${k} ${w}`);
    const read = facts.flatMap((r) => {
      const k = `${r.accountId} ${Date.parse(r.hour)}`;
      return peaks.map((w) => ({ at: `${r.accountId} ${r.hour} ${w}`, headed: headed(k, w), peak: r.peak[w], sources: highest.get(`${k} ${w}`) ?? null }));
    });
    const drawn = read.filter((x) => !x.headed && x.sources === null && x.peak !== null);
    const lifted = read.filter((x) => x.sources !== null && x.peak !== null && x.peak > x.sources);
    const short = read.filter((x) => x.sources !== null && (x.peak === null || x.peak < x.sources));
    const named = (list) => list.slice(0, 3).map((x) => `${x.at} reads ${x.peak}, its sources ${x.sources}`);
    const rowOf = new Map(facts.map((r) => [`${r.accountId} ${Date.parse(r.hour)}`, r]));
    const hourOf = (k) => `${k.split(' ')[0]} ${iso(Number(k.split(' ')[1]))}`;
    const unheaded = [...counted].flatMap((k) => peaks.filter((w) => headed(k, w) && typeof rowOf.get(k)?.peak[w] !== 'number').map((w) => `${hourOf(k)} ${w}`));
    const unlisted = [...sampled].filter((k) => !rowOf.has(k)).map(hourOf);
    assert.deepEqual(
      { drawn: named(drawn), lifted: named(lifted), short: named(short), unheaded: unheaded.slice(0, 3), unlisted: unlisted.slice(0, 3) },
      { drawn: [], lifted: [], short: [], unheaded: [], unlisted: [] },
      `${drawn.length} peaks where the hour holds no source, ${lifted.length} over them, ${short.length} under one, ${unheaded.length} counted attempts' windows with no peak, and ${unlisted.length} sampled hours with no row`,
    );
  });

  test(`a login that no pool with an affinity window holds reads no pin count in its account facts, at ${at}`, async () => {
    const s = await story;
    // legacy, reserve and zephyr stand in no pool (mock/pools.ts), and the router writes a pin only under a pool with
    // an affinity window.
    const logins = ['legacy', 'reserve', 'zephyr'];
    assert.deepEqual(s.pools.filter((p) => p.affinityTtlSeconds > 0 && p.members.some((m) => logins.includes(m.accountId))).map((p) => p.id), [], 'no pool with an affinity window holds the three');
    const facts = (await s.api.listAccountRollups(iso(s.now - 31 * DAY), iso(s.now + DAY))).filter((r) => logins.includes(r.accountId));
    assert.deepEqual([...new Set(facts.map((r) => r.accountId))].sort(), logins, 'each of the three lists account facts');
    assert.deepEqual(facts.filter((r) => r.pins.created + r.pins.migratedIn + r.pins.migratedOut > 0).map((r) => `${r.accountId} ${r.hour}`).slice(0, 3), []);
  });

  test(`each account hour holds a figure: a peak, a time under a cooldown or in overage, a pin count or a sampled window, at ${at}`, async () => {
    const s = await story;
    // The ledger stores no account hour that holds nothing, and the route lists an hour beyond the stored ones only
    // where a sample of the account stands.
    const facts = await s.api.listAccountRollups(iso(s.now - 31 * DAY), iso(s.now + DAY));
    const empty = facts.filter((r) => Object.values(r.peak).every((p) => p === null) && r.cooldownSeconds === 0 && r.overageSeconds === 0 && r.pins.created + r.pins.migratedIn + r.pins.migratedOut === 0 && Object.keys(r.windows).length === 0);
    assert.deepEqual(empty.slice(0, 3).map((r) => `${r.accountId} ${r.hour}`), [], `${empty.length} account hours hold nothing`);
  });

  test(`the accounts answer ends at the current hour and cuts an open cooldown at the read, at ${at}`, async () => {
    const s = await story;
    // Secondary's model row (mock/accounts.ts) and the story attempts' rows on it (mock/requests.ts), counted once
    // where they overlap, from the hour's start to the read. The model row runs from 34 minutes before the read to 26
    // minutes after it, 13:56 to 14:56 at 14:30, so it covers the hour's elapsed part: 1,800 s at 14:30 and 2,040 s
    // at 03:40, where it started at 03:06. The 5-hour row from the 429 of `req_b8c3e9d4-e61` at 14:07:50.9, the
    // account's own row of 14:09:31 and the two 20 s retry-afters at 14:02:11 and 14:06:31 stand inside it.
    const want = { '2026-09-16T14:30:00Z': 1_800, '2026-09-20T03:40:00Z': 2_040 }[at];
    const rows = (await s.api.listAccountRollups(iso(s.now - DAY), iso(s.now + DAY))).filter((r) => r.accountId === 'secondary');
    const newest = rows.map((r) => r.hour).sort().at(-1);
    assert.deepEqual([newest, rows.find((r) => r.hour === newest)?.cooldownSeconds], [iso(startOfHour(s.now)), want]);
  });

  test(`secondary's 7-day sample runs its deadline to the account's end, ahead of the window's reset, at ${at}`, async () => {
    const s = await story;
    // Secondary is canceled with its saved end five days on, and its 7-day window resets five days, two hours and five
    // minutes on (mock/accounts.ts), so the route answers the end as the deadline.
    const secondary = s.accounts.find((a) => a.id === 'secondary');
    const rows = (await s.api.listAccountRollups(iso(s.now - DAY), iso(s.now + DAY))).filter((r) => r.accountId === 'secondary' && r.windows.seven_day !== undefined);
    const sample = rows.reduce((a, b) => (Date.parse(b.hour) > Date.parse(a.hour) ? b : a)).windows.seven_day;
    assert.deepEqual([Date.parse(sample.deadline), Date.parse(sample.resetsAt) - Date.parse(sample.deadline)], [Date.parse(secondary.subscription.endsAt), 2 * HOUR + 5 * 60_000]);
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

  test(`the accounts the rings and the Quota windows lanes draw share a color only where nine of them take eight slots, at ${at}`, async () => {
    const s = await story;
    const slotOf = accountSlots(s.accounts.map((a) => a.id));
    const slots = s.accounts.map((a) => slotOf(a.id));
    // The eight series colors cycle past the eighth id. The lanes draw a listed account with a window, routing or not,
    // the rings the ones that route, and two of them on one slot read as one. Nine drawn accounts on eight slots put
    // one pair on a slot: zephyr, the last of the sorted ids, takes reserve's, so no older account moves, and any
    // other pair is a color the story could have kept apart.
    const drawn = s.accounts.filter((a) => s.lib.isQuotaListed(a));
    const shared = drawn.flatMap((a, i) => drawn.slice(i + 1).filter((b) => slotOf(b.id) === slotOf(a.id)).map((b) => `${a.id} and ${b.id} on slot ${slotOf(a.id)}`));
    assert.deepEqual(shared, ['reserve and zephyr on slot 7'], 'nine drawn accounts on eight slots share one, the last sorted id on the slot of the eighth');
    assert.ok(slots.every((slot) => slot !== null), 'every listed account has a slot');
  });

  test(`a re-pairing previews the identity its account last paired with, at ${at}`, async () => {
    const s = await story;
    const pairing = await s.api.startPairing({ accountId: 'legacy' });
    const preview = await s.api.completePairing(pairing.id, { code: 'code', state: pairing.state });
    assert.deepEqual([preview.email, preview.organization], ['operator+old@example.com', 'Personal']);
  });

  test(`every subscription account's plan is the word the router resolves from its stored profile, and every record's candidate carries its account's word, at ${at}`, async () => {
    const s = await story;
    const oauth = s.accounts.filter((a) => a.kind === 'oauth');
    // Each stored profile's word: the usage-based class first, then the organization type, a Max organization read by
    // its tier's size, and none without a profile.
    const resolved = { contract: 'usage_based', legacy: 'pro', nightly: 'max_5x', primary: 'max_20x', reserve: 'pro', secondary: 'pro', spare: null, studio: 'max_5x', sunset: 'free', workshop: 'max_20x', zenith: 'usage_based', zephyr: 'pro' };
    for (const a of oauth) {
      const plan = a.subscription?.plan ?? null;
      assert.equal(plan, resolved[a.id], `${a.id}'s plan`);
      // An account with no stored profile, spare among them, has no word.
      assert.ok(a.subscription === null ? plan === null : s.lib.PLAN_WORDS.includes(plan), `${a.id}'s word ${plan}`);
    }
    const word = Object.fromEntries(s.accounts.map((a) => [a.id, a.subscription?.plan ?? null]));
    // The seat's usage-based class resolves above its Enterprise type, and sunset's stored document is the free
    // organization the profile read found before it flagged the plan ended.
    assert.deepEqual([word.contract, word.sunset], ['usage_based', 'free']);
    const typed = (type) => oauth.filter((a) => a.subscription?.organizationType === type && !a.usageBased);
    assert.ok(typed('claude_pro').length > 0 && typed('claude_pro').every((a) => a.subscription.plan === 'pro'), 'the Pro organizations read pro');
    const candidates = (await decisionsOf(s)).flatMap((r) => r.candidates.map((c) => ({ c, r })));
    const wrong = candidates.filter(({ c }) => c.planTier !== (c.kind === 'api_key' ? null : word[c.accountId])).map(({ c, r }) => `${r.requestId}/${r.seq} ${c.accountId} ${c.planTier}`);
    assert.ok(candidates.length > 0, 'the story records candidates');
    assert.deepEqual(wrong, [], "a candidate's plan word is its account's");
  });

  test(`the story's serving usage-based seat reads its class and its spend window with no plan window, the one member of its pool behind the key fleet, at ${at}`, async () => {
    const s = await story;
    const seats = s.accounts.filter((a) => a.usageBased);
    const [seat] = seats;
    const firstOfNextMonth = (ms) => Date.UTC(new Date(ms).getUTCFullYear(), new Date(ms).getUTCMonth() + 1, 1);
    assert.deepEqual([seat.kind, seat.status, seat.windows, seat.fableWindow, seat.overage, seat.plan], ['oauth', 'ready', [], null, null, 'Usage-based']);
    const { amounts } = seat.spend;
    assert.deepEqual(
      [amounts.usedMinor, amounts.limitMinor, amounts.exponent, seat.spend.utilization, seat.spend.resetSource, seat.spend.resetsAt],
      [102_500, 250_000, 2, 0.41, 'estimated', iso(firstOfNextMonth(s.now))],
      'the polled amounts, the share, and the reset estimated at 00:00 UTC on the first of the next month',
    );
    assert.ok(Date.parse(amounts.polledAt) < Date.parse(seat.spend.observedAt), 'a head read the share after the poll');
    assert.deepEqual(
      [seat.subscription.organizationType, seat.subscription.billingType, seat.subscription.rateLimitTier, seat.subscription.plan],
      ['claude_enterprise', 'stripe_subscription_contracted', 'default_claude_zero', 'usage_based'],
      'the usage-based class resolves above the organization type',
    );
    assert.deepEqual(s.pools.find((p) => p.id === 'contract')?.members.map((m) => [m.accountId, m.priority]), [['contract', 1]], "the pool's one member");
    assert.deepEqual(s.keys.filter((k) => k.poolId === 'contract').map((k) => k.id), ['fleet']);
    const rows = s.requests.filter((r) => r.keyId === 'fleet');
    assert.ok(rows.length > 0 && rows.every((r) => r.accountId === 'contract' && s.lib.fundingOf(r.attempts.at(-1)) === 'usage_based'), "the key's rows are the seat's, billed as usage-based");
    const rollups = s.allRollups.filter((r) => r.keyId === 'fleet');
    assert.ok(rollups.length > 0 && rollups.every((r) => r.funding === 'usage_based'), 'and its rollups file under the word');
    // The decision card's spend cell and the record checks of a spend entry read these facts, and those checks pass
    // vacuously on records that carry no entry.
    const fleetIds = new Set(rows.map((r) => r.id));
    const records = (await decisionsOf(s)).filter((r) => fleetIds.has(r.requestId));
    assert.deepEqual([...new Set(records.map((r) => r.requestId))].sort(), [...fleetIds].sort(), "each of the key's rows has its decision record");
    // The seat's first record reads no spend window, so the Pace hover's sentence rests on the pace's shape alone.
    const unread = 'req_c94d1e07-b35';
    const filed = records.filter((r) => r.requestId !== unread).map((r) => r.candidates.find((c) => c.accountId === 'contract')).map((c) => [c?.tier[0], c?.windows.spend?.utilization, c?.windows.spend?.source]);
    assert.ok(filed.length > 0 && filed.every(([tier, utilization, source]) => tier === 2 && utilization === 0.41 && source === 'estimated'), `the records file the seat in the metered tier with its spend window: ${JSON.stringify(filed)}`);
    assert.ok(s.accounts.filter((a) => !['contract', 'zenith'].includes(a.id)).every((a) => !a.usageBased && a.spend === null), 'no other account reads the class');
    const headed = s.accounts.filter((a) => a.extraUsage?.enabled && a.extraUsage.usedCredits === null);
    assert.deepEqual(headed.map((a) => [a.id, a.extraUsage.monthlyLimitCredits, a.extraUsage.inUse]), [['nightly', null, true]], "the plan account whose newest reading a head wrote carries no amounts");
  });

  test(`the story's fleet session is listed with its live pin, which the contract pool and its member count, at ${at}`, async () => {
    const s = await story;
    const rows = s.requests.filter((r) => r.keyId === 'fleet');
    const [session] = new Set(rows.map((r) => r.sessionId));
    // The last decision places an Opus pin inside the pool's affinity window.
    const listed = s.sessions.filter((x) => x.sessionId === session).map((x) => [x.keyId, x.accountId, x.model, x.pinned]);
    assert.deepEqual(listed, [['fleet', 'contract', 'claude-opus-5-5', true]], 'the session is listed once, pinned on the seat');
    const pool = s.pools.find((p) => p.id === 'contract');
    // The seat also holds the two pins of the fleet session no mod reports, which has reached no ledger.
    assert.deepEqual([pool.activePins, pool.members.map((m) => [m.accountId, m.activePins])], [3, [['contract', 3]]], 'the pool and its member count the pin beside the two');
  });

  test(`the account-bound session reads the flag and no move cost beside a move size on its row and its line, at ${at}`, async () => {
    const s = await story;
    // The notebook session whose placement names state that studio holds (`ACCOUNT_BOUND_SESSION_ID` in
    // mock/requests.ts). Its pin has the annex for a destination, so the flag alone takes the cost away.
    const row = s.sessions.find((x) => x.sessionId === '2c7e9a41-5b83-4f16-a0d4-8e1b3f6c9d52');
    assert.deepEqual([row.accountBound, row.moveCost, typeof row.moveSize], [true, null, 'number'], 'the row');
    assert.deepEqual(row.agentContexts.map((c) => [c.agentId, c.accountBound, c.moveCost, typeof c.moveSize]), [['main', true, null, 'number']], 'its line');
  });

  test(`every record of one request reads the request's one boundary count, at ${at}`, async () => {
    const s = await story;
    // The failed attempt on secondary writes no row, and the failover's answer writes eleven message rows on primary.
    const records = (await decisionsOf(s)).filter((r) => r.requestId === 'req_1d9c3e55-0b2').sort((a, b) => a.seq - b.seq);
    assert.deepEqual(records.map((r) => [r.seq, r.request.chain.boundaries]), [[1, 11], [2, 11]], 'both records read the count of the request');
  });

  test(`a request the relay sent again on its account after a failed connect holds two attempts and one decision, at ${at}`, async () => {
    const s = await story;
    // The seed req_0e5a7c3b-f92: its connect to primary failed, and the relay sent it once more on primary.
    const resent = s.requests.find((r) => r.id === 'req_0e5a7c3b-f92');
    const records = (await decisionsOf(s)).filter((r) => r.requestId === 'req_0e5a7c3b-f92');
    assert.deepEqual([resent?.attempts.map((a) => [a.accountId, a.upstreamStatus]), records.map((r) => r.seq)], [[['primary', null], ['primary', 200]], [1]], 'the resend writes no decision of its own');
  });

  test(`the story's usage-based seat scores as a paid source and books no plan reservation on the fleet rows' records, at ${at}`, async () => {
    const s = await story;
    const fleetIds = new Set(s.requests.filter((r) => r.keyId === 'fleet').map((r) => r.id));
    const records = (await decisionsOf(s)).filter((r) => fleetIds.has(r.requestId));
    assert.deepEqual([...new Set(records.map((r) => r.requestId))].sort(), ['req_3f6a9c21-d48', 'req_7b2e5d93-a16', 'req_c94d1e07-b35'], "the key's three rows");
    // The engine scores a paid source by its remaining allowance with no hours, no window, no burn and no Fable
    // reservation, and the seat reads no plan window, so all of its allowance is left.
    const paid = { burn: 0, deadline: null, end: null, fableReservation: null, hours: null, remaining: 1, reset: null, score: 1, window: null };
    const rows = (await logOf(s)).filter((x) => x.type === 'decision' && fleetIds.has(x.requestId));
    for (const r of records) {
      const seat = r.candidates.find((c) => c.accountId === 'contract');
      assert.deepEqual(seat?.pace, paid, `${r.requestId}/${r.seq}'s pace`);
      // A score of the remaining share carries no hours, so the engine writes its unit as a share.
      const row = rows.find((x) => x.requestId === r.requestId && x.seq === r.seq);
      const scored = r.walk.find((w) => w.step === 'pick')?.facts.filter((f) => f.key === 'score' && f.accountId === 'contract').map((f) => f.unit);
      assert.deepEqual([scored, [row?.score, row?.runnerUp].filter((x) => x?.accountId === 'contract').map((x) => x.unit)], [['percent'], ['percent']], `${r.requestId}/${r.seq}'s score unit`);
      if (r.outcome.accountId !== 'contract') continue;
      // A source with no plan capacity books nothing, so its placement reaches the reserve station with no booking.
      assert.deepEqual([r.outcome.reservation, r.walk.find((w) => w.step === 'reserve')?.verdict], [null, 'none'], `${r.requestId}/${r.seq}'s reservation`);
    }
  });

  test(`the fresh placement's pick holds a Fable reservation for the one member its first tier ranks and for no other, at ${at}`, async () => {
    const s = await story;
    const record = (await decisionsOf(s)).find((r) => r.requestId === 'req_ef099b34-453' && r.seq === 1);
    // Secondary and overflow are out and the no-fit partition drops nightly, so the pick ranks primary alone. Its seed
    // burns none of its Fable week, which reserves 0, and the members the pick does not rank hold none.
    assert.deepEqual(record?.candidates.map((c) => [c.accountId, c.pace?.fableReservation]), [['primary', 0], ['secondary', null], ['nightly', null], ['overflow', null]]);
  });

  test(`every paid pace carries the whole shape the Pace hover reads, with or without the spend window, at ${at}`, async () => {
    const s = await story;
    const shaped = (pace) => pace !== null && pace.hours === null && pace.deadline === null && (pace.window ?? null) === null;
    const named = ({ c, r }) => `${r.requestId}/${r.seq} ${c.accountId}`;
    const members = (await decisionsOf(s)).flatMap((r) => r.candidates.map((c) => ({ c, r })));
    const oauth = members.filter(({ c }) => c.kind === 'oauth');
    assert.deepEqual(oauth.filter(({ c }) => c.pace !== null && c.pace.hours === null && !shaped(c.pace)).map(named), [], 'a pace with no hours names no deadline and no window');
    const spends = members.filter(({ c }) => c.windows.spend !== undefined);
    assert.ok(spends.length > 0 && spends.every(({ c }) => c.kind === 'oauth' && shaped(c.pace)), `every spend window sits on a paid pace: ${spends.filter(({ c }) => !shaped(c.pace)).map(named).join(', ')}`);
    // The seat's first record carries no spend window (`SPEND_UNREAD` in mock/routingLog.ts), so a hover that read the
    // entry rather than the shape would give that record the plan sentence.
    assert.deepEqual(oauth.filter(({ c }) => shaped(c.pace) && c.windows.spend === undefined).map(named), ['req_c94d1e07-b35/1 contract'], 'the one paid candidate without the spend window');
  });

  test(`the story's usage-based seat knows its allowance as an API key does, at ${at}`, async () => {
    const all = await decisionsOf(await story);
    const records = all.filter((r) => r.candidates.some((c) => c.accountId === 'contract'));
    assert.ok(records.length > 0, 'a record names the seat');
    for (const r of records) {
      const seat = r.candidates.find((c) => c.accountId === 'contract');
      // The engine's paid-source arm knows the allowance of a source read by its spend or its worst bucket, never by a
      // plan window.
      assert.equal(seat.allowanceKnown, true, `${r.requestId}/${r.seq}'s seat`);
    }
    const keys = all.flatMap((r) => r.candidates.filter((c) => c.kind === 'api_key'));
    assert.ok(keys.length > 0 && keys.every((c) => c.allowanceKnown), 'every API key candidate knows its allowance');
  });

  test(`no account bills overage beside the provider's reason for no longer funding it, at ${at}`, async () => {
    const s = await story;
    for (const a of s.accounts) {
      const billing = a.extraUsage?.inUse === true || a.overage?.inUse === true;
      assert.ok(!billing || !a.extraUsage?.disabledReason, `${a.id} reads its overage in use beside "${a.extraUsage?.disabledReason}"`);
    }
  });

  test(`each attempt's trace tier carries its record's overage flag, at ${at}`, async () => {
    const s = await story;
    // The engine files an attempt by the overage it read when the attempt ran, so nightly's attempts before its cap's
    // raise trace no overage although the account reads in overage at the story's now.
    const traced = s.requests.flatMap((r) => r.attempts.filter((a) => a.selection?.tier).map((a) => ({ a, r })));
    const drifted = traced.filter(({ a }) => a.selection.tier[2] !== (a.overageInUse === true ? 1 : 0) || (a.selection.tier[0] === 1 && a.overageInUse !== true));
    assert.deepEqual(drifted.map(({ a, r }) => `${r.id} ${a.accountId} ${a.startedAt}`), [], 'every traced tier reads the overage its attempt records');
    assert.deepEqual([...new Set(traced.map(({ a }) => a.overageInUse === true))].sort(), [false, true], 'the traces hold attempts in overage and out of it');
  });

  test(`each account's day counts its rows from the hour a day back and its lifetime is never under its week, at ${at}`, async () => {
    const s = await story;
    for (const a of s.accounts) {
      assert.equal(a.requests.last24h, s.totalled.filter((r) => r.accountId === a.id).length, `${a.id}'s day`);
      assert.ok(a.requests.lifetime >= a.requests.last7d, `${a.id}'s lifetime reads ${a.requests.lifetime}, under what the rollups hold`);
    }
  });

  test(`each key's week is its live generation's rollups and its lifetime never under them, at ${at}`, async () => {
    const s = await story;
    for (const k of s.keys) {
      const mine = s.rollups.filter((r) => r.keyId === k.id && r.keyGeneration === Date.parse(k.createdAt));
      const week = mine.filter((r) => Date.parse(r.hour) >= s.weekFrom);
      // The week's dollars take a billed probe's too, a probe no plan funded, as the keys route sums them.
      const priced = s.allRollups.filter((r) => (r.kind !== 'probe' || r.funding !== 'plan') && r.keyId === k.id && r.keyGeneration === Date.parse(k.createdAt) && Date.parse(r.hour) >= s.weekFrom);
      assert.equal(k.usage7d.tokens, sum(week, tokensOf), `${k.id}'s week's tokens`);
      assert.ok(near(k.usage7d.cost, sum(priced, (r) => r.costUsd)), `${k.id}'s week's cost`);
      assert.ok(k.lifetime.cost >= sum(mine, (r) => r.costUsd) - 1e-6, `${k.id}'s lifetime under its rollups`);
    }
  });

  test(`each pool's day and its 25 hourly buckets total its rows from the hour a day back, at ${at}`, async () => {
    const s = await story;
    for (const p of s.pools) {
      const mine = s.asked(s.totalled).filter((r) => r.poolId === p.id);
      // The day's dollars take a billed probe's too, as the pools route sums them.
      const priced = s.asked(s.requests.filter((r) => (r.kind !== 'probe' || !planPaid(s, r)) && Date.parse(r.at) >= s.totalledFrom && r.poolId === p.id));
      assert.equal(p.requests24h, mine.length, `${p.id}'s day`);
      assert.ok(near(p.day24h.cost, sum(priced, (r) => r.costUsd)), `${p.id}'s cost`);
      assert.equal(p.traffic24h.length, 25, `${p.id}'s buckets`);
      assert.equal(Date.parse(p.traffic24h[0].at), s.totalledFrom, `${p.id}'s first bucket`);
      assert.equal(sum(p.traffic24h, (b) => b.requests), mine.length, `${p.id}'s buckets`);
    }
  });

  test(`the dashboard's day adds up from the rows, in its hourly buckets and over the pools, at ${at}`, async () => {
    const s = await story;
    assert.equal(s.overview.requests24h, s.asked(s.totalled).length);
    assert.equal(sum(s.overview.hourly, (h) => h.requests), s.overview.requests24h, 'the hourly buckets');
    assert.equal(sum(s.pools, (p) => p.requests24h), s.asked(s.totalled).length, "the pools' days");
  });

  test(`the breakdown of a range starting inside an hour counts the requests of the hour holding its start, at ${at}`, async () => {
    const s = await story;
    const from = s.now - DAY;
    const edge = s.requests.filter((r) => r.kind !== 'probe' && startOfHour(Date.parse(r.at)) === startOfHour(from));
    assert.ok(from > startOfHour(from) && s.asked(edge).length > 0, 'the range starts inside an hour that holds a request');
    const [all] = await s.api.getAnalyticsBreakdown({ from: iso(from), groupBy: 'all', to: iso(s.now) });
    const kept = s.asked(s.requests.filter((r) => r.kind !== 'probe' && inHours(Date.parse(r.at), from, s.now)));
    assert.equal(all.requests, kept.length, `the range counts ${edge.map((r) => r.id).join(', ')}`);
  });

  test(`the breakdown of a range ending on the hour leaves out the requests of the hour its end starts, at ${at}`, async () => {
    const s = await story;
    const from = s.now - DAY;
    const to = startOfHour(s.now);
    const current = s.requests.filter((r) => r.kind !== 'probe' && Date.parse(r.at) >= to);
    assert.ok(current.length > 0, 'the hour the end starts holds requests');
    const [all] = await s.api.getAnalyticsBreakdown({ from: iso(from), groupBy: 'all', to: iso(to) });
    const kept = s.asked(s.requests.filter((r) => r.kind !== 'probe' && inHours(Date.parse(r.at), from, to)));
    assert.equal(all.requests, kept.length, `the range leaves out the ${current.length} requests from ${iso(to)} on`);
  });

  test(`an hour's duration samples count each request an account was asked for that holds a duration, a failed one and a token count among them, at ${at}`, async () => {
    const s = await story;
    // `req_5e2f7a19-b03` (mock/requests.ts) answered 502 after one attempt of 1,900 ms, beside the hour's token count.
    const failed = s.requests.find((r) => r.id === 'req_5e2f7a19-b03');
    const from = startOfHour(Date.parse(failed.at));
    const hour = s.requests.filter((r) => startOfHour(Date.parse(r.at)) === from);
    assert.ok(failed.status === 502 && failed.durationMs === 1_900 && hour.some((r) => r.kind === 'count_tokens' && r.durationMs !== null), 'the hour holds a failed request and a token count, each with a duration');
    // The rollups sample a probe as they sample any request, so both sides read the probes.
    const [all] = await s.api.getAnalyticsBreakdown({ from: iso(from), groupBy: 'all', includeProbes: true, to: iso(s.now) });
    const timed = s.asked(hour).filter((r) => r.durationMs !== null);
    assert.equal(all.histograms.durationMs.reduce((a, b) => a + b, 0), timed.length, `the hour's ${timed.length} requests with a duration`);
  });

  test(`the account facts of a range keep the hour holding its start and leave out the hour its end starts on, at ${at}`, async () => {
    const s = await story;
    // A start inside an hour and an end on one, so each bound reads the hour the route's own bound reads.
    const from = s.now - DAY;
    const to = startOfHour(s.now) - HOUR;
    const hourOf = (r) => Date.parse(r.hour);
    const every = await s.api.listAccountRollups(iso(s.now - 31 * DAY), iso(s.now + DAY));
    assert.ok(from > startOfHour(from) && every.some((r) => hourOf(r) === startOfHour(from)) && every.some((r) => hourOf(r) === to), 'both bounds fall on hours that hold account facts');
    const key = (r) => `${r.accountId} ${r.hour}`;
    const facts = await s.api.listAccountRollups(iso(from), iso(to));
    assert.deepEqual(facts.map(key), every.filter((r) => inHours(hourOf(r), from, to)).map(key));
  });

  test(`the rollups file a request sent under an alias under the model it resolved to, so the model groups name resolved ids alone, at ${at}`, async () => {
    const s = await story;
    const aliased = s.requests.filter((r) => r.model !== r.canonicalModel);
    assert.ok(aliased.length > 0, 'the story holds a request sent under an alias');
    for (const r of aliased) {
      const hour = iso(startOfHour(Date.parse(r.at)));
      const lane = s.allRollups.filter((x) => x.hour === hour && x.keyId === r.keyId && x.poolId === r.poolId && x.accountId === (r.accountId ?? '-') && x.kind === r.kind && x.sessionId === r.sessionId);
      const models = lane.map((x) => x.model);
      assert.ok(models.includes(r.canonicalModel) && !models.includes(r.model), `${r.id}, sent as ${r.model}, files under ${models.join(', ')}`);
    }
    // From the hour a day back the rollups are the request log's own, so their models are the log's resolved ids.
    const groups = await s.api.getAnalyticsBreakdown({ from: iso(s.totalledFrom), groupBy: 'model', to: iso(s.now) });
    const resolved = [...new Set(s.requests.filter((r) => Date.parse(r.at) >= s.totalledFrom).map((r) => r.canonicalModel))].sort();
    assert.deepEqual(groups.map((g) => g.key).sort(), resolved, 'the model groups');
  });

  test(`the story's Sonnet 4.5 rows price nothing, so every sum that holds one reads null and the attention list names the model, at ${at}`, async () => {
    const s = await story;
    const range = { from: iso(s.now - DAY), to: iso(s.now) };
    const holds = (rows) => rows.some((r) => r.canonicalModel === 'claude-sonnet-4-5' && planPaid(s, r) && requestTokens(r) > 0);
    const sonnet = s.requests.filter((r) => r.canonicalModel === 'claude-sonnet-4-5' && planPaid(s, r));
    assert.ok(sonnet.length > 0, 'the story serves Sonnet 4.5 on a plan');
    for (const r of sonnet) assert.ok(r.costEquivalent === null && Object.values(r.costShares ?? {}).every((v) => v === null), `${r.id} prices nothing`);
    assert.ok(s.allRollups.filter((r) => r.model === 'claude-sonnet-4-5' && r.funding === 'plan' && tokensOf(r) > 0).every((r) => r.equivalent === null), "its rollups' rows with tokens hold no figure");
    const models = await s.api.getAnalyticsBreakdown({ ...range, groupBy: 'model' });
    const group = models.find((g) => g.key === 'claude-sonnet-4-5');
    assert.ok(group && group.planRequests > 0 && group.costEquivalent === null && group.equivalentSplit === null, 'its model group reads null');
    assert.ok(models.some((g) => typeof g.costEquivalent === 'number' && g.costEquivalent > 0), 'a model with a weight of its own prices its figure');
    const accounts = await s.api.getAnalyticsBreakdown({ ...range, groupBy: 'account' });
    assert.equal(accounts.find((g) => g.key === 'secondary')?.costEquivalent, null, 'the account that serves it reads null');
    const burn = await s.api.getAnalyticsBurn(range, 'model');
    assert.ok(burn.points.some((p) => p.group === 'claude-sonnet-4-5' && p.unpriced), 'a bucket of it reads flagged');
    assert.deepEqual([...new Set(burn.points.filter((p) => p.unpriced).map((p) => p.group))], ['claude-sonnet-4-5'], 'and no other model flags one');
    const anchor = s.lib.anchorTier(s.settings.subscriptionCosts);
    assert.ok(burn.points.every((p) => !p.unpriced || p.equivalentByTier[anchor] === null), 'a flagged bucket reads null');
    // A share is the row's units over the tier's own capacity, so a tier larger than the anchor reads less of its week.
    const priced = burn.points.filter((p) => p.equivalentByTier[anchor] > 0);
    assert.ok(priced.length > 0 && priced.every((p) => p.equivalentByTier.default_claude_max_20x < p.equivalentByTier[anchor]), 'a priced bucket reads less of an Ultra week than of the anchor tier');
    assert.ok(holds(s.totalled), "the dashboard's day holds one");
    assert.equal(s.overview.day24h.equivalent, null, "so the dashboard's day reads null");
    for (const p of s.pools) assert.equal(p.day24h.equivalent === null, holds(s.totalled.filter((r) => r.poolId === p.id && !r.refused)), `${p.id}'s day`);
    for (const k of s.keys) {
      const rows = s.rollups.filter((r) => r.keyId === k.id && r.keyGeneration === Date.parse(k.createdAt));
      assert.equal(k.lifetime.equivalent === null, rows.some((r) => r.funding === 'plan' && r.equivalent === null), `${k.id}'s lifetime`);
    }
    assert.ok(s.keys.some((k) => k.lifetime.equivalent === null) && s.pools.some((p) => p.day24h.equivalent === null), 'a key and a pool that served it read null');
    const requests = s.requests.filter((r) => s.lib.undatedModel(r.canonicalModel) === 'claude-sonnet-4-5' && planPaid(s, r) && r.costEquivalent === null && requestTokens(r) > 0).length;
    assert.deepEqual(s.attention.filter((i) => i.kind === 'model_unpriced').map((i) => [i.detail, i.link, i.pools, i.subject.type]), [[{ model: 'claude-sonnet-4-5', requests }, null, [], 'model']], 'one item names the model and its plan requests');
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

  test(`the earlier holder's session row reads its own requests inside the session expiry and is listed while its last request is inside the non-companion timeout, at ${at}`, async () => {
    const s = await story;
    const created = Date.parse(s.keys.find((k) => k.id === 'desktop').createdAt);
    // The rows run newest first.
    const earlier = s.requests.filter((r) => r.kind !== 'probe' && r.keyId === 'desktop' && r.keyGeneration !== created);
    assert.ok(earlier.length > 0, 'the earlier holder has requests');
    // The holder's last request ran hours before the story's clock, so the route's list leaves the row out there and
    // keeps it inside the hour of that request.
    assert.ok(!s.sessions.some((x) => x.historical) && !s.lib.sessionsListedAt(s.now, s.lib.SESSIONS, s.settings.sessions.nonCompanionIdleSeconds, s.lib.REQUESTS, s.lib.ROUTING_EVENTS).some((x) => x.historical), "the route's list at the story's clock holds no earlier holder");
    const historical = s.lib.sessionsListedAt(Date.parse(earlier[0].at) + HOUR / 2, s.lib.SESSIONS, s.settings.sessions.nonCompanionIdleSeconds, s.lib.REQUESTS, s.lib.ROUTING_EVENTS).filter((x) => x.historical);
    assert.deepEqual(historical.map((x) => x.keyId), ['desktop'], 'one earlier holder is listed inside the hour of its last request');
    const [held] = historical;
    const holders = s.requests.filter((r) => r.kind !== 'probe' && r.keyId === 'desktop' && r.keyGeneration === held.keyGeneration);
    const own = holders.filter((r) => r.sessionId === held.sessionId);
    assert.ok(held.keyGeneration < created && own.length > 0, 'the row is the earlier generation, with rows of its own');
    assert.deepEqual([held.spend.requests, held.lastRequestAt], [own.length, own[0].at], "the row's figures are its own rows'");
    assert.equal(held.lastRequestAt, holders[0].at, "the holder's last request ran on this session");
    assert.ok(Date.parse(held.lastRequestAt) < created, 'before the live key');
    assert.deepEqual([held.lastUsedAt, held.pinned], [own[0].at, false], 'no pin, so its last use is its last request, as the decoder reads a row with no pin');
    assert.ok(Date.parse(held.lastRequestAt) + s.settings.sessions.expireAfterSeconds * 1000 > s.now, 'still inside the session expiry');
    assert.ok(s.sessions.some((x) => !x.historical && x.keyId === 'desktop' && x.sessionId === held.sessionId), 'the live key holds the same session id');
  });

  test(`the desktop session its key's guardrail refused whole is listed while its newest refusal is inside the non-companion timeout, and none past it, at ${at}`, async () => {
    const s = await story;
    const timeout = s.settings.sessions.nonCompanionIdleSeconds;
    // The rows run newest first.
    const refusals = s.requests.filter((r) => r.kind !== 'probe' && r.keyId === 'desktop' && r.errorCode === 'guardrail_blocked');
    const [newest] = refusals;
    assert.ok(refusals.length > 0 && refusals.every((r) => r.refused && r.sessionId === newest.sessionId), 'one session holds the refusals');
    const listedAt = (now) => s.lib.sessionsListedAt(now, s.lib.SESSIONS, s.settings.sessions.nonCompanionIdleSeconds, s.lib.REQUESTS, s.lib.ROUTING_EVENTS).filter((x) => x.keyId === 'desktop' && x.sessionId === newest.sessionId);
    const listed = listedAt(Date.parse(newest.at) + HOUR / 2);
    assert.ok(newest.userAgent !== null, 'the newest refusal names its client');
    assert.deepEqual(listed.map((x) => [x.accountId, x.spend, x.userAgent, x.lastRequestAt]), [[null, null, newest.userAgent, newest.at]], "listed off the ledger with no spend and the newest refusal's user agent inside the hour of that refusal");
    assert.deepEqual(listedAt(Date.parse(newest.at) + timeout * 1000 + 1_000), [], 'none past the timeout');
    assert.ok(!s.sessions.some((x) => x.keyId === 'desktop' && x.sessionId === newest.sessionId), `the route's list at the story's clock, ${Math.round((s.now - Date.parse(newest.at)) / 60_000)} min after the newest refusal, leaves it out`);
  });

  test(`a live session row counts the non-probe requests of its own key, generation and session, at ${at}`, async () => {
    const s = await story;
    for (const x of s.sessions.filter((row) => !row.historical)) {
      const own = s.requests.filter((r) => r.kind !== 'probe' && r.keyId === x.keyId && r.keyGeneration === x.keyGeneration && r.sessionId === x.sessionId);
      const newest = own.reduce((last, r) => (last === null || Date.parse(r.at) > Date.parse(last) ? r.at : last), null);
      // The route answers the count in the spend, and a row with no spend counts none.
      assert.deepEqual([x.spend?.requests ?? 0, x.lastRequestAt], [s.asked(own).length, newest], `${x.keyId}'s ${x.sessionId}`);
    }
  });

  test(`every agent entry of a session the story seeds names a line of the session or an earlier agent the seed holds, one entry per agent read off its newest client request or its newest probe, at ${at}`, async () => {
    const s = await story;
    const recipes = await s.api.listKeepaliveRecipes();
    // The session expiry lists every row the story seeds, the ones past the non-companion timeout among them.
    const seeded = s.lib.sessionsListedAt(s.now, s.lib.SESSIONS, s.settings.sessions.expireAfterSeconds, s.lib.REQUESTS, s.lib.ROUTING_EVENTS);
    const seed = [...s.lib.requestById.values()];
    // The engine files its recipes under each key's live generation alone, so a row of an earlier holder reads none, as
    // the route clears them.
    const live = new Map(s.keys.map((k) => [k.id, Date.parse(k.createdAt)]));
    const desktop = live.get('desktop');
    const holder = seed.find((r) => r.keyId === 'desktop' && r.keyGeneration !== desktop).keyGeneration;
    // The agents the seed gives requests on a listed row and no line there, by the row's key generation and session id:
    // the ended session's subagent the mod never pinned, story 4's Explore subagent, and story 0's pinned subagent on
    // the earlier holder's row, which lists no recipe. Stories 10 and 11 and the fork session declare none: their
    // subagents' seed rows sit under another key than the listed row's, and the mock lists no row for the fork session.
    const finished = '9b4e2f71-6c3d-4a85-b0e2-5d7f8a1c3e96';
    const earlier = [
      [desktop, finished, 'd41e'],
      [desktop, s.lib.SESSION_IDS[4], '0b7f'],
      [holder, s.lib.SESSION_IDS[0], 'e4b7'],
    ];
    // The route answers one entry per agent. An agent that ran two models reads the model, the account, the type and
    // the instant of its newest client request, the greater id winning a tie of the instant, and an agent the ledger
    // holds probes of alone its newest probe's model with none of the other three.
    const newestOf = (rows) => [...rows].sort(s.lib.compareRequests)[0];
    const read = { probesAlone: 0, twoModels: 0 };
    for (const row of seeded) {
      const name = `${row.keyId} ${row.sessionId}${row.historical ? ', the earlier holder' : ''}`;
      const filed = recipes.filter((r) => r.keyId === row.keyId && live.get(r.keyId) === row.keyGeneration && r.sessionId === row.sessionId);
      const lines = new Set(['main', ...row.agentContexts.map((c) => c.agentId), ...filed.map((r) => r.agentId)]);
      const ids = row.agentRequests.map((entry) => entry.agentId);
      assert.equal(new Set(ids).size, ids.length, `${name} answers one entry per agent: ${ids.join(', ')}`);
      for (const entry of row.agentRequests) {
        const known = lines.has(entry.agentId) || earlier.some(([generation, sessionId, agentId]) => generation === row.keyGeneration && sessionId === row.sessionId && agentId === entry.agentId);
        assert.ok(known, `${name}: ${entry.agentId} on ${entry.model} names no line of the session and no earlier agent`);
        const own = s.requests.filter((r) => !r.refused && r.keyId === row.keyId && r.keyGeneration === row.keyGeneration && r.sessionId === row.sessionId && r.agentId === entry.agentId);
        const client = own.filter((r) => r.kind !== 'probe');
        const got = [entry.model, entry.accountId, entry.agentType, entry.lastRequestAt];
        if (client.length === 0) {
          read.probesAlone += 1;
          assert.deepEqual(got, [newestOf(own).canonicalModel, null, null, null], `${name}: ${entry.agentId}, with probes alone`);
        } else if (new Set(client.map((r) => r.canonicalModel)).size > 1) {
          read.twoModels += 1;
          const newest = newestOf(client);
          assert.deepEqual(got, [newest.canonicalModel, s.lib.servingAttempt(newest)?.accountId ?? null, newest.agentType, newest.at], `${name}: ${entry.agentId}, on two models`);
        }
      }
    }
    // The Sunday night's draws hold no agent with probes alone, so that half reads at the smoke's instant alone.
    assert.ok(read.twoModels > 0 && (at !== '2026-09-16T14:30:00Z' || read.probesAlone > 0), `the story holds the agents both reads need: ${JSON.stringify(read)}`);
    // Story 0's desktop row under the live key, whose plan-funded client rows with tokens secondary serves at both
    // instants and primary on the Sunday night. At the smoke's instant primary serves the row's token count, which
    // stores no usage, and a 502 alone, and `req_9b6275a3-96b` is a plan-funded request on nightly with no token. Each
    // prices nothing, so the fold keys no share for either account there.
    const story0 = seeded.find((row) => row.keyId === 'desktop' && row.keyGeneration === desktop && row.sessionId === s.lib.SESSION_IDS[0]);
    const priced = at === '2026-09-16T14:30:00Z' ? ['secondary'] : ['primary', 'secondary'];
    assert.deepEqual(Object.keys(story0.spend.shares).sort(), priced, "story 0's desktop row keys the accounts that priced its plan rows");
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

  test(`the ci key's job sends as the Agent SDK with a class on every row, and its earlier runner's sends are the gate's refusals, at ${at}`, async () => {
    const s = await story;
    const rows = s.requests.filter((r) => r.keyId === 'ci');
    const refused = rows.filter((r) => r.errorCode === 'client_not_allowed');
    assert.equal(refused.length, 2, 'the earlier runner sent twice');
    for (const r of refused) {
      const read = [r.status, r.refused, r.outcome, r.client, r.userAgent, r.agentId, r.agentType, r.sessionId, r.requestClass, r.model, r.attempts.length, r.costUsd];
      assert.deepEqual(read, [403, true, 'rejected', 'other', 'phoebis-batch/0.9 (external, ci)', null, null, null, null, '', 0, 0], r.id);
    }
    const job = rows.filter((r) => r.errorCode !== 'client_not_allowed');
    assert.ok(job.length > 0, 'the job has requests');
    for (const r of job) {
      const read = [r.client, r.userAgent, r.sessionId !== null, r.requestClass !== null];
      assert.deepEqual(read, ['claude-code/sdk-cli', 'claude-cli/2.1.302 (external, sdk-cli)', true, true], `${r.id} under class ${r.requestClass}`);
    }
    // The ledger keys a rollup by the request's own client and agent, so the refusals roll up apart from the job.
    const lane = s.allRollups.filter((r) => r.keyId === 'ci');
    const gated = lane.filter((r) => (r.refusals.client_not_allowed ?? 0) > 0);
    assert.deepEqual([...new Set(gated.map((r) => `${r.client} ${r.userAgent} ${r.accountId}`))], ['other phoebis-batch/0.9 -'], 'the refusals roll up under their own client and agent');
    assert.equal(sum(gated, (r) => r.refusals.client_not_allowed), 2, 'both refusals roll up');
    const rest = lane.filter((r) => !gated.includes(r));
    assert.deepEqual([...new Set(rest.map((r) => `${r.client} ${r.userAgent}`))], ['claude-code/sdk-cli claude-cli/2.1.302'], 'the job rolls up under its own client and agent');
  });

  test(`the runner's sessions on ci read their last use off their own newest request, at ${at}`, async () => {
    const s = await story;
    // The runner's rows left the route's list under the non-companion timeout, so the case reads the seed through the
    // session expiry, which lists every row the story seeds.
    const runner = s.lib.sessionsListedAt(s.now, s.lib.SESSIONS, s.settings.sessions.expireAfterSeconds, s.lib.REQUESTS, s.lib.ROUTING_EVENTS).filter((x) => x.keyId === 'ci' && !x.historical);
    assert.ok(runner.length > 0, 'the story holds sessions of the ci key');
    for (const x of runner) assert.equal(x.lastUsedAt, x.lastRequestAt, `${x.sessionId}'s last use`);
  });

  test(`each pool's active pins count one pin per key, session, agent and model under its keys, its members' sum and the pins on accounts outside them to it, and the dashboard's are their sum, at ${at}`, async () => {
    const s = await story;
    const poolOf = new Map(s.keys.map((k) => [k.id, k.poolId]));
    const recipes = await s.api.listKeepaliveRecipes();
    const live = pinnedRecipesOf(s, recipes);
    // A pinned session whose main no recipe warms holds a pin per line all the same.
    const unwarmed = (poolId, accountId) => sum(s.sessions.filter((x) => x.pinned && mainUnwarmed(recipes, x) && poolOf.get(x.keyId) === poolId && (accountId === undefined || x.accountId === accountId)), (x) => x.agentContexts.length);
    const pins = (poolId, accountId) => new Set(live.filter((r) => poolOf.get(r.keyId) === poolId && (accountId === undefined || pinAccountOf(s, r) === accountId)).map(pinOf)).size + unwarmed(poolId, accountId);
    const above = {};
    for (const p of s.pools) {
      assert.equal(p.activePins, pins(p.id), `${p.id}'s active pins`);
      for (const m of p.members) assert.equal(m.activePins, pins(p.id, m.accountId), `${p.id}'s ${m.accountId}`);
      // A pin under the pool's keys on an account the pool no longer lists counts in its figure and in no member's.
      const listed = new Set(p.members.map((m) => m.accountId));
      const pinnedOn = new Set([...live.filter((r) => poolOf.get(r.keyId) === p.id).map((r) => pinAccountOf(s, r)), ...s.sessions.filter((x) => x.pinned && poolOf.get(x.keyId) === p.id).map((x) => x.accountId)]);
      const outside = sum([...pinnedOn].filter((a) => !listed.has(a)), (a) => pins(p.id, a));
      assert.equal(sum(p.members, (m) => m.activePins) + outside, p.activePins, `${p.id}'s members' sum and the ${outside} pins on accounts outside them make its figure`);
      above[p.id] = p.activePins - sum(p.members, (m) => m.activePins);
    }
    assert.deepEqual(above, { all: 0, contract: 0, jobs: 0, sidelined: 0, work: 1, workshop: 0 }, 'work holds one pin on a former member and the other pools none');
    assert.ok(s.pools.some((p) => p.activePins > s.sessions.filter((x) => x.pinned && poolOf.get(x.keyId) === p.id).length), 'a pool holds more pins than pinned sessions');
    assert.equal(s.overview.activePins, sum(s.pools, (p) => p.activePins), "the dashboard's active pins");
  });

  test(`two sessions on one account and one model hold two pins, at ${at}`, async () => {
    const s = await story;
    const poolOf = new Map(s.keys.map((k) => [k.id, k.poolId]));
    const live = pinnedRecipesOf(s, await s.api.listKeepaliveRecipes()).filter((r) => poolOf.get(r.keyId) === 'all' && pinAccountOf(s, r) === 'primary');
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
    // The list's order is not the view's: the parked row precedes the idle one, whose last request is newer than the
    // busy one's.
    const words = rows.filter((r) => r.agentState !== 'released').map((r) => (r.agentId === 'main' ? 'main' : r.agentState));
    assert.deepEqual(words, ['main', 'busy', 'parked', 'idle']);
    const lastRequestOf = (word) => Date.parse(rows.find((r) => r.agentId !== 'main' && r.agentState === word).lastRequestAt);
    assert.ok(lastRequestOf('idle') > lastRequestOf('busy'), 'the idle row is the newer request');
    // The one line on the engine's retry ladder holds its agent with the next rung ahead of the clock.
    const exhausted = recipes.filter((r) => r.cause === 'probe_pool_exhausted');
    assert.deepEqual(exhausted.map((r) => [r.sessionId, r.agentId, r.state]), [[rows[1].sessionId, rows[1].agentId, 'held']], "the written session's second line is the pool exhausted one");
    assert.ok(Date.parse(exhausted[0].nextProbeAt) > s.now, `its next probe ${exhausted[0].nextProbeAt} lies ahead of the story's now`);
    const overview = await s.api.getKeepaliveOverview();
    // The engine's cap counts every record it holds, the idle recipe of the row two hours back among them, which the
    // route's list leaves out. The session specs the mod registered or a recipe warms hold 32 agents, the
    // registration-only row none, the auxiliary-only row its one subagent, and the failover session the subagent whose
    // pin its auxiliary call moved.
    assert.equal(overview.limits.agents.count, 34, 'the cap counts every record');
  });

  test(`a probed line of a session row on an account inside an account-wide cooldown reads held on the retry ladder, never scheduled, at ${at}`, async () => {
    const s = await story;
    const recipes = await s.api.listKeepaliveRecipes();
    // The router answers a probe whose pinned member cools with that cooldown as a 429, which the keepalive holds on
    // its retry ladder. Secondary cools from 20 minutes and 29 seconds back to 25 minutes ahead, and of its lines
    // 229bf72c's two are probed, while e5a17c3d's main ended on its idle limit.
    const cooling = new Set(s.accounts.filter((a) => a.cooldowns.some((c) => c.scope === 'account' && Date.parse(c.setAt) <= s.now && s.now < Date.parse(c.until))).map((a) => a.id));
    const probed = recipes.filter((r) => cooling.has(pinAccountOf(s, r)) && r.state !== 'ended' && r.state !== 'not_retained');
    assert.deepEqual(probed.map((r) => [pinAccountOf(s, r), r.sessionId.slice(0, 8), r.agentId, r.cause, r.state]), [
      ['secondary', '229bf72c', 'main', 'probe_quota_exhausted', 'held'],
      ['secondary', '229bf72c', 'a7f3', 'probe_quota_exhausted', 'held'],
    ]);
  });

  test(`the all-released session lists every record released, keeps its row and the pin its last probe renewed, at ${at}`, async () => {
    const s = await story;
    const recipes = await s.api.listKeepaliveRecipes();
    const bySession = new Map();
    for (const r of recipes) bySession.set(`${r.keyId}|${r.sessionId}`, [...(bySession.get(`${r.keyId}|${r.sessionId}`) ?? []), r]);
    // The session the mod ended reads every record released as well, by its end, so its main's cause sets it apart.
    const whole = [...bySession.values()].filter((rows) => rows.every((r) => r.agentState === 'released') && !rows.some((r) => r.cause === 'session_ended'));
    // The other session the mod released whole is the line past its window, which holds no request of the ledger.
    const kept = whole.filter((rows) => rows.some((r) => s.now - lastUseOf(r) <= affinityOf(s, r)));
    assert.deepEqual([whole.length, kept.length], [2, 1], 'two sessions the mod released whole, one inside its window');
    const [rows] = kept;
    const main = rows.find((r) => r.agentId === 'main');
    assert.ok(main !== undefined, 'main among them');
    for (const r of rows) assert.deepEqual([r.cause, r.state, r.nextProbeAt, r.deadline, r.wakeAt], ['agent_released', 'ended', null, null, null], `${r.agentId} reads as a released agent`);
    const row = s.sessions.find((x) => !x.historical && x.keyId === rows[0].keyId && x.sessionId === rows[0].sessionId);
    assert.ok(row !== undefined, 'the session keeps its row');
    // Newest first: the last turns of the subagents `2f9d` and `8c1a` (mock/requests.ts), then main's one turn.
    const own = s.requests.filter((r) => r.kind !== 'probe' && r.keyId === row.keyId && r.sessionId === row.sessionId);
    assert.deepEqual(own.map((r) => r.agentId), ['2f9d', '8c1a', 'main'], "the ledger holds main's one request and its two subagents' turns");
    assert.deepEqual([row.pinned, row.spend.requests, row.lastRequestAt], [true, 3, own[0].at], 'the row reads its newest request as its activity');
    // Main's one request is past the pool's window, so main's pin stands on the probe that followed it.
    assert.ok(s.now - Date.parse(own[2].at) > affinityOf(s, main), "main's request is past the window");
    assert.ok(s.now - Date.parse(main.lastProbeAt) <= affinityOf(s, main), "main's last probe renewed its pin inside the window");
    const poolId = s.keys.find((k) => k.id === row.keyId).poolId;
    const counted = new Set(pinnedRecipesOf(s, recipes).filter((r) => s.keys.find((k) => k.id === r.keyId)?.poolId === poolId && pinAccountOf(s, r) === main.accountId).map(pinOf));
    const member = s.pools.find((p) => p.id === poolId).members.find((m) => m.accountId === main.accountId);
    assert.ok(counted.has(pinOf(main)), 'the release left the pin in place');
    assert.equal(member.activePins, counted.size, `${main.accountId}'s figure counts it`);
  });

  test(`one session the mod ended reads main ended with it and every other line released, each with its probe facts, at ${at}`, async () => {
    const s = await story;
    const recipes = await s.api.listKeepaliveRecipes();
    const ended = s.sessions.filter((x) => !x.historical && recipes.some((r) => r.keyId === x.keyId && r.sessionId === x.sessionId && r.agentId === 'main' && r.cause === 'session_ended'));
    assert.equal(ended.length, 1, 'one session the mod ended');
    const [row] = ended;
    const lines = recipes.filter((r) => r.keyId === row.keyId && r.sessionId === row.sessionId);
    const main = lines.find((r) => r.agentId === 'main');
    assert.deepEqual([main?.state, main?.nextProbeAt], ['ended', null], 'main ended with the session');
    const others = lines.filter((r) => r.agentId !== 'main');
    assert.ok(others.length > 0, 'the session holds subagents');
    for (const r of others) assert.deepEqual([r.agentState, r.cause, r.state, r.nextProbeAt], ['released', 'agent_released', 'ended', null], `${r.agentId} reads as a released agent`);
    for (const r of lines) assert.ok(r.lastProbeAt !== null && r.probesToday > 0, `${r.agentId} keeps its probe facts`);
  });

  test(`every released line dates its release inside its pool's longest cache TTL, past which the router lists it no more, at ${at}`, async () => {
    const s = await story;
    // The story writes no release instant, so a line's last use, its last request or its last probe, stands in for it.
    const released = (await s.api.listKeepaliveRecipes()).filter((r) => r.agentState === 'released');
    assert.ok(released.length > 0, 'the story holds released lines');
    for (const r of released) {
      const age = s.now - lastUseOf(r);
      assert.ok(age >= 0 && age < longestTtlOf(s, r), `${r.keyId} ${r.sessionId} ${r.agentId} dates its release ${age / 60_000} min back, past its pool's ${longestTtlOf(s, r) / 60_000} min`);
    }
  });

  test(`every row and every agent line reads its effort as one of the four words or none, and each line reads warm by its TTL at the story's clock, at ${at}`, async () => {
    const s = await story;
    const words = ['low', 'medium', 'high', 'max', null];
    // The session expiry lists every row the story seeds, the ones past the non-companion timeout among them.
    const seeded = s.lib.sessionsListedAt(s.now, s.lib.SESSIONS, s.settings.sessions.expireAfterSeconds, s.lib.REQUESTS, s.lib.ROUTING_EVENTS);
    for (const row of seeded) {
      assert.ok(Array.isArray(row.agentContexts), `${row.keyId} ${row.sessionId} answers a list`);
      assert.ok(words.includes(row.effort), `${row.keyId} ${row.sessionId} reads the effort ${row.effort}`);
      if (!row.pinned) assert.equal(row.effort, null, `${row.keyId} ${row.sessionId} holds no pin to read an effort from`);
    }
    // The laptop's parked Haiku session is the one whose pin named no effort.
    assert.equal(seeded.filter((x) => x.pinned && x.effort === null).length, 1, 'one pinned row reads no effort');
    const lines = seeded.flatMap((x) => x.agentContexts);
    assert.ok(lines.length > 0, 'the story holds agent lines');
    for (const line of lines) assert.ok(words.includes(line.effort), `${line.agentId} reads the effort ${line.effort}`);
    assert.ok(lines.some((l) => l.warm) && lines.some((l) => !l.warm), 'warm and cold lines both');
    // The written rows' lines on a five-minute TTL: the session no mod reports used main 2 minutes back and its
    // subagent 25, the auxiliary-only session's subagent 3, the finished row main 21 and its released subagents 47 and
    // 34, whose pins it lists since no companion reports it after its end, and the row two hours back and the notebook
    // row main 120 and 20 minutes back. The slowed session's lines sit on the hour's TTL, main last used at 14:05 and
    // its Haiku subagent at 13:59:05, so both read warm where five minutes would read them cold.
    const written = ['b7e2d5a9-6c13-4f80-a4d2-9e1f3c7b0a68', '3e9b7c21-4d58-4a06-8f1e-6c2a9d0b5e74', '9b4e2f71-6c3d-4a85-b0e2-5d7f8a1c3e96', 'e5a17c3d-8f20-4b6e-9d41-2c7b0a9f6e58', '6d2f8b40-1e7a-4c95-a3d8-7b5e0c9f2a61', '5f8a3c1e-9d27-4b6e-8c04-2a7e9b3d1f65'];
    const warmOf = (sessionId) => seeded.find((x) => x.sessionId === sessionId)?.agentContexts.map((c) => [c.agentId, c.warm]);
    assert.deepEqual(written.map(warmOf), [
      [
        ['main', true],
        ['5c8e', false],
      ],
      [['d41c', true]],
      [
        ['main', false],
        ['5d0e', false],
        ['a7f3', false],
      ],
      [['main', false]],
      [['main', false]],
      [
        ['main', true],
        ['7b2e', true],
      ],
    ]);
    // The other rows' refreshes are mostly draws, so each line reads warm while the story's clock lies before its
    // `warmUntil`, its last refresh plus its TTL: main's line on the row's pin the row's own refresh, which a probe or
    // an auxiliary request since its last turn sets apart from its last use, and every other line its last use.
    for (const row of seeded.filter((x) => !written.includes(x.sessionId))) {
      for (const line of row.agentContexts) {
        const refresh = line.agentId === row.agentId && line.model === row.model ? row.lastRefreshAt : line.lastUsedAt;
        const until = refresh === null || line.cacheTtlMs === null ? null : Date.parse(refresh) + line.cacheTtlMs;
        assert.deepEqual([line.warmUntil, line.warm], [until === null ? null : iso(until), until !== null && s.now < until], `${line.agentId} on ${line.model}, refreshed ${refresh} on a ${line.cacheTtlMs} ms TTL`);
      }
    }
  });

  test(`a companion row lists main's line and one per unreleased subagent recipe, main first and then by agent id and model, and none of a released agent, at ${at}`, async () => {
    const s = await story;
    const recipes = await s.api.listKeepaliveRecipes();
    const companions = s.sessions.filter((x) => x.companion !== 'none');
    const ownOf = (row) => recipes.filter((r) => r.keyId === row.keyId && r.sessionId === row.sessionId);
    // The route orders the lines by agent id and then model as bytes, main first, which a locale's collation need not.
    const bytes = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
    const routeOrder = (a, b) => (a.agentId === 'main') !== (b.agentId === 'main') ? (a.agentId === 'main' ? -1 : 1) : bytes(a.agentId, b.agentId) || bytes(a.model, b.model);
    for (const row of companions) {
      const unreleased = ownOf(row).filter((r) => r.agentState !== 'released');
      const want = [...unreleased].sort(routeOrder).map((r) => [r.agentId, r.model, r.accountId]);
      assert.deepEqual(row.agentContexts.map((c) => [c.agentId, c.model, c.accountId]), want, `${row.sessionId} lists its unreleased agents' pins`);
      const main = row.agentContexts.find((c) => c.agentId === 'main');
      if (main !== undefined) {
        const read = (f) => [f.model, f.accountId, f.lastInputTokens, f.effort, f.cacheTtlMs];
        assert.deepEqual(read(main), read(row), `${row.sessionId}: main's line reads the row's pin`);
        assert.equal(row.warm, main.warm, `${row.sessionId}: the row's warmth is main's pin's own`);
      }
    }
    // The row's last use is the newest across its pins: story 0's live row reads its subagent e4b7's use seven minutes
    // before the clock, while main's own lies hours before it.
    const story0 = companions.find((x) => x.sessionId === s.lib.SESSION_IDS[0] && !x.historical);
    assert.equal(story0.lastUsedAt, new Date(s.now - 7 * 60_000).toISOString(), "story 0's row reads its newest subagent's use");
    assert.ok(companions.some((x) => x.agentContexts[0]?.agentId === 'main' && x.agentContexts.length >= 3), 'a companion session carries main and two subagents');
    // The two sessions the mod released whole hold every record released, main's among them.
    const allReleased = companions.filter((x) => ownOf(x).length > 0 && ownOf(x).every((r) => r.agentState === 'released'));
    assert.equal(allReleased.length, 2, 'two companion sessions whose every agent is released');
    for (const row of allReleased) assert.deepEqual(row.agentContexts, [], `${row.sessionId} carries an empty list`);
  });

  test(`a plain row lists one line per pin within the non-companion timeout, the slowed session two, and a line leaves past the timeout while its row stays, at ${at}`, async () => {
    const s = await story;
    const plain = s.sessions.filter((x) => x.companion === 'none');
    for (const row of plain) {
      if (!row.pinned) assert.deepEqual(row.agentContexts, [], `${row.keyId} ${row.sessionId} holds no pin and draws no line`);
      const main = row.agentContexts.find((c) => c.agentId === row.agentId && c.model === row.model);
      if (row.pinned) assert.ok(main !== undefined && main.lastInputTokens === row.lastInputTokens && main.effort === row.effort, `${row.keyId} ${row.sessionId}: the row's pin draws its line`);
    }
    // The slowed notebook session holds main's pin and its subagent's on Haiku.
    const slowed = plain.filter((x) => x.keyId === 'notebook' && x.agentContexts.length === 2);
    assert.deepEqual(
      slowed.map((x) => x.agentContexts.map((c) => [c.agentId, c.model])),
      [
        [
          ['main', 'claude-sonnet-5-5'],
          ['7b2e', 'claude-haiku-4-5-20251001'],
        ],
      ],
    );
    // The session no mod reports on fleet: its cold line passes the timeout forty minutes on, while its warm one keeps
    // the row. The desktop failover session holds a warm line and a cold one too, both inside the timeout then.
    const noMod = plain.find((x) => x.keyId === 'fleet' && x.agentContexts.length === 2 && x.agentContexts.some((c) => c.warm) && x.agentContexts.some((c) => !c.warm));
    assert.ok(noMod !== undefined, 'the session no mod reports is listed');
    const later = s.lib.sessionsListedAt(s.now + 40 * 60_000, s.lib.SESSIONS, s.settings.sessions.nonCompanionIdleSeconds, s.lib.REQUESTS, s.lib.ROUTING_EVENTS).find((x) => x.keyId === noMod.keyId && x.sessionId === noMod.sessionId);
    assert.deepEqual(later?.agentContexts.map((c) => c.warm), [true], 'the warm line stays and the cold one leaves');
  });

  test(`the auxiliary-only session reads its subagent's pin on the row with no main line, at ${at}`, async () => {
    const s = await story;
    const recipes = await s.api.listKeepaliveRecipes();
    const ownOf = (row) => recipes.filter((r) => r.keyId === row.keyId && r.sessionId === row.sessionId);
    // Main holds an auxiliary pin alone, which draws no line, so the row falls back to the subagent's pin.
    const aux = s.sessions.filter((x) => x.companion !== 'none' && x.pinned && x.agentId !== 'main');
    assert.equal(aux.length, 1, 'one companion row reads a subagent pin');
    const [row] = aux;
    // Main holds no recipe, so the session's one recipe is the subagent's.
    assert.deepEqual(ownOf(row).map((r) => r.agentId), [row.agentId], 'one subagent recipe and none of main');
    const read = (f) => [f.agentId, f.model, f.accountId, f.lastInputTokens, f.effort];
    assert.deepEqual(row.agentContexts.map(read), [read(row)], "the subagent's line, the one the row reads");
  });

  test(`one subagent's recipe stands under compacted after its compaction completed: ended, last guarded as a compaction, with no probe scheduled, at ${at}`, async () => {
    const s = await story;
    const recipes = await s.api.listKeepaliveRecipes();
    const compacted = recipes.filter((r) => r.cause === 'compacted');
    assert.equal(compacted.length, 1, 'one recipe stands under compacted');
    const [line] = compacted;
    assert.deepEqual([line.agentId === 'main', line.lastGuard, line.state, line.nextProbeAt], [false, 'compaction', 'ended', null]);
    assert.notEqual(s.sessions.find((x) => x.keyId === line.keyId && x.sessionId === line.sessionId)?.companion ?? 'none', 'none', 'the agent belongs to a companion session');
  });

  test(`a released agent keeps its pin while its last request or probe is inside its pool's affinity window and loses it past the window, the written session's three released lines counting, at ${at}`, async () => {
    const s = await story;
    const recipes = await s.api.listKeepaliveRecipes();
    const poolOf = new Map(s.keys.map((k) => [k.id, k.poolId]));
    const inside = (r) => s.now - lastUseOf(r) <= affinityOf(s, r);
    // The written session is the one that holds an agent in each lifecycle word.
    const bySession = new Map();
    for (const r of recipes) bySession.set(r.sessionId, [...(bySession.get(r.sessionId) ?? []), r]);
    const full = [...bySession.values()].filter((rows) => ['busy', 'idle', 'parked', 'released'].every((word) => rows.some((r) => r.agentState === word)));
    assert.equal(full.length, 1, 'one session holds every lifecycle word');
    const lines = full[0].filter((r) => r.agentState === 'released');
    assert.equal(lines.length, 3, 'three released lines');
    for (const r of lines) assert.ok(inside(r), `${r.agentId}'s last use is inside its pool's window`);
    assert.ok(lines.some((r) => s.now - Date.parse(r.lastRequestAt) > affinityOf(s, r)), 'a line whose last request is past the window counts by its probe');
    // A pinned session whose main no recipe warms holds a pin per line all the same.
    const unwarmed = (poolId, accountId) => sum(s.sessions.filter((x) => x.pinned && mainUnwarmed(recipes, x) && poolOf.get(x.keyId) === poolId && x.accountId === accountId), (x) => x.agentContexts.length);
    for (const p of s.pools) {
      for (const m of p.members) {
        const own = recipes.filter((r) => poolOf.get(r.keyId) === p.id && pinAccountOf(s, r) === m.accountId);
        const released = own.filter((r) => r.agentState === 'released');
        const kept = released.filter(inside).map((r) => r.agentId);
        const gone = released.filter((r) => !inside(r)).map((r) => r.agentId);
        const pins = new Set(own.filter((r) => r.agentState !== 'released' || inside(r)).map(pinOf)).size + unwarmed(p.id, m.accountId);
        assert.equal(m.activePins, pins, `${p.id}'s ${m.accountId} with the released ${kept.join(', ') || 'none'} kept and ${gone.join(', ') || 'none'} past the window`);
      }
    }
  });

  test(`one released line's last request and last probe lie past its pool's affinity window, so the router expired its pin, at ${at}`, async () => {
    const s = await story;
    const recipes = await s.api.listKeepaliveRecipes();
    const past = recipes.filter((r) => r.agentState === 'released' && s.now - lastUseOf(r) > affinityOf(s, r));
    assert.equal(past.length, 1, past.map((r) => `${r.sessionId} ${r.agentId}`).join(', '));
    const [line] = past;
    assert.ok(affinityOf(s, line) > 0, 'on a pool whose pins outlive their last use');
    assert.ok(line.lastProbeAt !== null && Date.parse(line.lastProbeAt) <= Date.parse(line.lastRequestAt), `probed at ${line.lastProbeAt}, before its last request at ${line.lastRequestAt}`);
    const poolId = s.keys.find((k) => k.id === line.keyId).poolId;
    const own = recipes.filter((r) => s.keys.find((k) => k.id === r.keyId)?.poolId === poolId && pinAccountOf(s, r) === line.accountId);
    const unwarmed = sum(s.sessions.filter((x) => x.pinned && mainUnwarmed(recipes, x) && s.keys.find((k) => k.id === x.keyId)?.poolId === poolId && x.accountId === line.accountId), (x) => x.agentContexts.length);
    const member = s.pools.find((p) => p.id === poolId).members.find((m) => m.accountId === line.accountId);
    assert.equal(member.activePins, new Set(pinnedRecipesOf(s, own).map(pinOf)).size + unwarmed, `${line.accountId}'s figure`);
    assert.ok(!pinnedRecipesOf(s, own).some((r) => pinOf(r) === pinOf(line)), 'the pin left with the window');
    const row = s.sessions.find((x) => !x.historical && x.keyId === line.keyId && x.sessionId === line.sessionId);
    assert.ok(row !== undefined && s.now - Date.parse(row.lastUsedAt) > affinityOf(s, line), "the session row reads its last use past the window too");
  });

  test(`session 0's main line reads its last use off the session's own newest request, as its recipe does, at ${at}`, async () => {
    const s = await story;
    const recipes = await s.api.listKeepaliveRecipes();
    // Session 0 is the busy desktop session whose main still awaits its cache evidence.
    const mains = recipes.filter((r) => r.agentId === 'main' && r.cause === 'awaiting_cache_evidence');
    assert.equal(mains.length, 1);
    const [main] = mains;
    const row = s.sessions.find((x) => !x.historical && x.keyId === main.keyId && x.sessionId === main.sessionId);
    const own = s.requests.filter((r) => r.kind !== 'probe' && r.keyId === row.keyId && r.keyGeneration === row.keyGeneration && r.sessionId === row.sessionId);
    assert.ok(own.length > 0, 'the ledger holds its requests');
    const newest = own.reduce((a, b) => (Date.parse(b.at) > Date.parse(a.at) ? b : a));
    // The row's own last use is the newest across its pins, which its subagents' turns can move past main's.
    const line = row.agentContexts.find((c) => c.agentId === 'main');
    assert.deepEqual([line?.lastUsedAt, row.lastRequestAt, main.lastRequestAt], [newest.at, newest.at, newest.at]);
  });

  test(`no sample of legacy is newer than its last clean poll, at ${at}`, async () => {
    const s = await story;
    const clean = Date.parse(s.accounts.find((a) => a.id === 'legacy').usage.poll.lastCleanAt);
    const samples = (await s.api.getQuotaHistory('legacy')).flatMap((h) => h.samples);
    assert.ok(samples.length > 0, 'legacy has samples');
    const late = samples.filter((x) => Date.parse(x.at) > clean);
    assert.deepEqual(late.map((x) => x.at), [], `samples past the last clean poll of ${iso(clean)}`);
  });

  test(`legacy's history carries the readings its spent and cleared rows name, and the clock announced the reset that passed after its last clean poll, at ${at}`, async () => {
    const s = await story;
    const log = (await logOf(s)).filter((row) => row.type === 'event' && row.accountId === 'legacy');
    const legacy = s.accounts.find((a) => a.id === 'legacy');
    const samples = (await s.api.getQuotaHistory('legacy')).find((h) => h.windowId === '5h').samples;
    const sampleAt = (at) => samples.find((x) => x.at === at);
    const [spent, cleared] = ['window_spent', 'window_cleared'].map((kind) => log.find((e) => e.kind === kind && e.detail.window === 'five_hour'));
    // Each row reads a poll, so the history holds a sample at its instant with the reading it names, and the samples
    // between read the window at its limit.
    assert.deepEqual([sampleAt(spent.at)?.utilization, sampleAt(cleared.at)?.utilization], [spent.detail.utilization, cleared.detail.utilization]);
    const between = samples.filter((x) => Date.parse(x.at) > Date.parse(spent.at) && Date.parse(x.at) < Date.parse(cleared.at));
    assert.ok(between.length > 0 && between.every((x) => x.utilization === cleared.detail.utilization_before), between.map((x) => `${x.at} ${x.utilization}`).join(', '));
    assert.equal(samples.at(-1).at, cleared.at, 'the samples end at the last clean poll');
    // No poll read the window after its reset, so the clock announced it, from the reading the last clean poll stored.
    // The answer withholds the windows of an account that needs re-auth, so that reading is the cleared row's, at the
    // last clean poll its usage block keeps.
    const resets = log.filter((e) => e.kind === 'window_reset' && e.detail.window === 'five_hour');
    const reset = cleared.detail.reset;
    assert.deepEqual(resets.map((e) => [e.at, e.detail]), [[iso(reset), { next_reset: null, observed: 'clock', plan: legacy.subscription.plan, reset_at: reset, tier: legacy.subscription.rateLimitTier, utilization_before: cleared.detail.utilization, window: 'five_hour' }]]);
    assert.ok(reset > Date.parse(cleared.at) && cleared.at === legacy.usage.poll.lastCleanAt);
  });

  test(`the story's weekly resets run a week apart back from each window's reset, and the unused allowance route answers them with sunset's term end, at ${at}`, async () => {
    const s = await story;
    const WEEK = 7 * DAY;
    const span = s.lib.allowanceSpan(s.now);
    const page = await s.api.listRoutingLog({ from: span.from, kinds: ['window_reset'], limit: 200, to: span.to });
    assert.equal(page.nextCursor, null, 'one page holds the four weeks');
    const weekly = page.rows.filter((e) => e.type === 'event' && e.detail.window !== 'five_hour');
    const accountOf = new Map(s.accounts.map((a) => [a.id, a]));
    const sunset = accountOf.get('sunset');
    const end = Date.parse(sunset.subscription.endsAt);
    // Four resets of each weekly window an account reports, a week apart back from its current reset. Sunset reports
    // none since its plan ended, so its three run back from the reset its seed's `win('7d', ...)` names, four days
    // after the end, and the end itself is the fourth.
    const expected = ['primary', 'secondary', 'nightly', 'studio'].flatMap((id) => {
      const a = accountOf.get(id);
      return [['seven_day', a.windows.find((w) => w.id === '7d')], ['seven_day_fable', a.fableWindow]].flatMap(([window, w]) => (w ? [1, 2, 3, 4].map((k) => `${id} ${window} ${iso(Date.parse(w.resetsAt) - k * WEEK)}`) : []));
    });
    expected.push(...[1, 2, 3].map((k) => `sunset seven_day ${iso(end + 4 * DAY - k * WEEK)}`));
    assert.deepEqual(weekly.map((e) => `${e.accountId} ${e.detail.window} ${iso(e.detail.reset_at)}`).sort(), expected.sort());
    // Each row in the engine's shape, announced at its reset, a sample's naming the next reset a week on and the clock
    // walk's none, all before the day the dashboard and the routing log count.
    const next = (e) => (e.detail.observed === 'clock' ? null : e.detail.reset_at + WEEK);
    const off = weekly.filter((e) => JSON.stringify(Object.keys(e.detail).sort()) !== '["next_reset","observed","plan","reset_at","tier","utilization_before","window"]' || e.at !== iso(e.detail.reset_at) || e.detail.next_reset !== next(e) || !(e.detail.utilization_before > 0 && e.detail.utilization_before < 1) || Date.parse(e.at) >= s.now - DAY);
    assert.deepEqual(off.map((e) => `${e.accountId} ${e.at} ${JSON.stringify(e.detail)}`), []);
    assert.deepEqual(weekly.filter((e) => e.detail.observed !== 'sample').map((e) => `${e.accountId} ${e.detail.window} ${e.detail.observed}`), ['nightly seven_day clock'], 'one clock walk among the samples');
    // Each reset carries the pair of the reading it replaced. Studio's seed (`claude_max` on `default_claude_max_5x`)
    // upgraded 16 days back, inside its second-newest week, so its third and fourth weeks ran under Pro on both
    // windows, one poll's readings naming one plan. Sunset's weeks before its plan ended ran under Pro too. Every
    // other reset, the five-hour ones among them, carries its account's current pair.
    const pairOf = (e) => ({ plan: e.detail.plan, tier: e.detail.tier });
    const current = (id) => ({ plan: accountOf.get(id).subscription.plan, tier: accountOf.get(id).subscription.rateLimitTier });
    const PRO = { plan: 'pro', tier: 'default_claude_ai' };
    assert.deepEqual(current('studio'), { plan: 'max_5x', tier: 'default_claude_max_5x' }, "studio's current pair");
    for (const window of ['seven_day', 'seven_day_fable']) {
      const studio = weekly.filter((e) => e.accountId === 'studio' && e.detail.window === window).sort((x, y) => y.detail.reset_at - x.detail.reset_at);
      assert.deepEqual(studio.map(pairOf), [current('studio'), current('studio'), PRO, PRO], `studio ${window}, newest first`);
    }
    assert.deepEqual(weekly.filter((e) => e.accountId === 'sunset').map(pairOf), [PRO, PRO, PRO], "sunset's weeks before its plan ended");
    const resets = page.rows.filter((e) => e.type === 'event' && e.accountId !== 'studio' && e.accountId !== 'sunset');
    assert.ok(resets.some((e) => e.detail.window === 'five_hour'), 'the five-hour resets read too');
    assert.deepEqual(resets.filter((e) => JSON.stringify(pairOf(e)) !== JSON.stringify(current(e.accountId))).map((e) => `${e.accountId} ${e.detail.window} ${e.at} ${JSON.stringify(pairOf(e))}`), [], "every other reset carries its account's current pair");

    const answer = await s.api.getAnalyticsUnusedAllowance(span);
    assert.deepEqual([answer.from, answer.to], [span.from, span.to]);
    assert.deepEqual(answer.accounts.map((a) => a.accountId), ['nightly', 'primary', 'secondary', 'studio', 'sunset'], 'one entry per subscription account with a weekly reset, by id');
    // Sunset's first row is its term's end and studio's third Fable row its plan change, which no `window_reset` wrote.
    const unlogged = (a, w) => (a.accountId === 'sunset' ? w.resets.slice(1) : a.accountId === 'studio' && w.window === 'seven_day_fable' ? w.resets.toSpliced(2, 1) : w.resets);
    for (const a of answer.accounts) {
      const account = accountOf.get(a.accountId);
      assert.deepEqual([a.plan, a.tier], [account.subscription.plan, account.subscription.rateLimitTier], a.accountId);
      assert.deepEqual(a.windows.map((w) => w.window), account.fableWindow ? ['seven_day', 'seven_day_fable'] : ['seven_day'], a.accountId);
      for (const w of a.windows) {
        const fromLog = weekly
          .filter((e) => e.accountId === a.accountId && e.detail.window === w.window)
          .sort((x, y) => y.detail.reset_at - x.detail.reset_at)
          .map((e) => ({ observedAt: e.at, plan: e.detail.plan, resetAt: iso(e.detail.reset_at), shareLeft: Math.min(1, Math.max(0, 1 - e.detail.utilization_before)), source: e.detail.observed, tier: e.detail.tier }));
        assert.deepEqual(unlogged(a, w), fromLog, `${a.accountId} ${w.window}: the log's resets, newest first`);
        assert.ok(near(w.totalShareLeft, sum([...w.resets, ...w.claims], (r) => r.shareLeft)), `${a.accountId} ${w.window}: the total over the rows of both lists`);
      }
    }
    // Studio's upgrade at its subscription's start dropped its Fable week from 0.14 (`STUDIO_PLAN_CHANGE` in
    // mock/routingLog.ts), so 0.86 was left under Pro's pair. The poll behind the one at the start wrote the drop under
    // the reset of the week open then, two weeks before the window's current one, and the row stands at its drop,
    // behind the week that ended nine days back.
    const upgraded = accountOf.get('studio');
    const upgradedFable = answer.accounts.find((a) => a.accountId === 'studio').windows.find((w) => w.window === 'seven_day_fable');
    assert.deepEqual(upgradedFable.resets[2], { observedAt: iso(Date.parse(upgraded.subscription.since) + 5 * 60_000), plan: 'pro', resetAt: iso(Date.parse(upgraded.fableWindow.resetsAt) - 2 * WEEK), shareLeft: 0.86, source: 'plan_changed', tier: 'default_claude_ai' }, "studio's plan change");
    // The week the end cut short reads the last sample of sunset's 7-day series before the end, the reading its seed
    // names, 0.46, so 0.54 of the week was left. Its pair is that of the stored reading, the series' newest sample:
    // sunset's last clean poll, five minutes before the end and so before the plan-ended flag, which read it under Pro.
    const series = (await s.api.getQuotaHistory('sunset')).find((h) => h.windowId === '7d');
    const before = series.samples.filter((x) => Date.parse(x.at) < end).at(-1);
    const ended = answer.accounts.find((a) => a.accountId === 'sunset').windows[0].resets[0];
    assert.deepEqual(ended, { observedAt: before.at, plan: 'pro', resetAt: iso(end), shareLeft: 1 - before.utilization, source: 'term_ended', tier: 'default_claude_ai' }, "the term's end carries the Pro pair its stored reading held before the flag");
    assert.ok(near(ended.shareLeft, 0.54), `sunset left ${ended.shareLeft} of its last week`);
    const one = await s.api.getAnalyticsUnusedAllowance(span, { accountIds: ['studio'] });
    assert.deepEqual(one.accounts, answer.accounts.filter((a) => a.accountId === 'studio'), 'the Accounts filter keeps its account alone');
  });

  test(`the allowance answer lists a used reset for the window it cleared alone, at ${at}`, async () => {
    const s = await story;
    const span = s.lib.allowanceSpan(s.now);
    const claimsOf = (answer) => answer.accounts.flatMap((a) => a.windows.flatMap((w) => w.claims.map((r) => [a.accountId, w.window, r])));
    // Studio's use three hours back dropped its Fable week from 0.75 (the seed beside its claim in mock/routingLog.ts),
    // so 0.25 of the week was left, under its current pair, at the week's own reset.
    const studio = s.accounts.find((a) => a.id === 'studio');
    const studioRow = { observedAt: iso(s.now - 3 * HOUR), plan: 'max_5x', resetAt: iso(Date.parse(studio.fableWindow.resetsAt)), shareLeft: 0.25, source: 'claim', tier: 'default_claude_max_5x' };
    assert.deepEqual(claimsOf(await s.api.getAnalyticsUnusedAllowance(span)), [['studio', 'seven_day_fable', studioRow]]);
    // A use of primary's Fable reset clears its Fable week from 0.12 and gives the 7-day window back the week's six
    // points, 0.73 to 0.67, and the log holds both drops. The claim names the Fable week alone, so the answer lists its
    // row, 0.88 left, and none for the 7-day window. A fresh mock, so the use leaves the story the other cases read.
    const api = new s.lib.MockApi();
    await api.login('admin');
    const primary = s.accounts.find((a) => a.id === 'primary');
    const wall = Date.now;
    Date.now = () => s.now;
    try {
      await api.useLimitReset('primary', 'rg-primary-1');
      const drops = (await api.listRoutingLog({ from: iso(s.now - HOUR), kinds: ['window_dropped'], to: iso(s.now) })).rows.filter((e) => e.accountId === 'primary');
      assert.deepEqual(drops.map((e) => [e.detail.window, e.detail.utilization_before, e.detail.utilization]).sort(), [['seven_day', 0.73, 0.67], ['seven_day_fable', 0.12, 0]]);
      const primaryRow = { observedAt: iso(s.now), plan: 'max_20x', resetAt: iso(Date.parse(primary.fableWindow.resetsAt)), shareLeft: 0.88, source: 'claim', tier: 'default_claude_max_20x' };
      assert.deepEqual(claimsOf(await api.getAnalyticsUnusedAllowance(span, { accountIds: ['primary'] })), [['primary', 'seven_day_fable', primaryRow]]);
    } finally {
      Date.now = wall;
    }
  });

  test(`an end before the span drops the resets after it and adds no term_ended row, at ${at}`, async () => {
    const s = await story;
    // A fresh mock, so the edit leaves the story the other cases read as it was.
    const api = new s.lib.MockApi();
    await api.login('admin');
    await api.updateAccount('primary', { canceled: true, subscriptionEndsAt: iso(s.now - 31 * DAY) });
    // Every weekly reset of primary falls after the end, and the end falls before the span's start, so none lists.
    const answer = await api.getAnalyticsUnusedAllowance(s.lib.allowanceSpan(s.now), { accountIds: ['primary'] });
    assert.deepEqual(answer.accounts, []);
  });

  test(`spare's samples end at the reading its snapshot keeps from before the disconnect, with no poll, at ${at}`, async () => {
    const s = await story;
    const spare = s.accounts.find((a) => a.id === 'spare');
    // The polls stop with the credential, so the snapshot holds the last reading and no poll's standing.
    assert.deepEqual([spare.usage.error, spare.usage.heldUntil, spare.usage.poll], [null, null, null], "spare's snapshot");
    const read = Date.parse(spare.usage.fetchedAt);
    const history = await s.api.getQuotaHistory('spare');
    // The answer withholds a disconnected row's windows, so the reading's are those of spare's seed in
    // `web/src/mock/accounts.ts`: the 5-hour, the 7-day and the 7-day Fable window.
    assert.deepEqual(history.map((h) => h.windowId), ['5h', '7d', '7d_fable'], 'one history per window of the reading');
    assert.ok(history.length > 0 && history.every((h) => h.samples.length > 0), 'spare has samples');
    assert.deepEqual(
      history.map((h) => h.samples.at(-1).at),
      history.map(() => iso(read)),
      `each window's last sample at the reading of ${iso(read)}`,
    );
  });

  test(`spare's reading lies inside every window's current period and before its reset, where its samples end, at ${at}`, async () => {
    const s = await story;
    const spare = s.accounts.find((a) => a.id === 'spare');
    const read = Date.parse(spare.usage.fetchedAt);
    const history = await s.api.getQuotaHistory('spare');
    // The answer withholds a disconnected row's windows, so the periods are those of spare's seed in
    // `web/src/mock/accounts.ts`: the 5-hour window reset 3 days 15 hours back, and both weeks reset 3 days 4 hours on.
    const windows = [
      { id: '5h', periodHours: 5, resetsAt: iso(s.now - (3 * DAY + 15 * HOUR)) },
      { id: '7d', periodHours: 168, resetsAt: iso(s.now + 3 * DAY + 4 * HOUR) },
      { id: '7d_fable', periodHours: 168, resetsAt: iso(s.now + 3 * DAY + 4 * HOUR) },
    ];
    assert.deepEqual(windows.map((w) => w.id), history.map((h) => h.windowId), 'the 5-hour, the 7-day and the 7-day Fable window');
    const off = windows.flatMap((w) => {
      const reset = Date.parse(w.resetsAt);
      const opened = reset - w.periodHours * HOUR;
      const last = Date.parse(history.find((h) => h.windowId === w.id).samples.at(-1).at);
      return opened <= last && last < reset && read < reset ? [] : [`${w.id}: the period ${iso(opened)} to ${iso(reset)}, the last sample ${iso(last)}, the reading ${iso(read)}`];
    });
    assert.deepEqual(off, []);
  });

  test(`every account's samples end at its last reading on the utilization it answers, at ${at}`, async () => {
    const s = await story;
    // The last reading: the last clean poll where every poll after it failed, the reading an unpaired account keeps
    // from before its disconnect, else now.
    const endOf = (a) => {
      const u = a.usage;
      const last = u?.poll?.lastOutcome === 'failed' ? u.poll.lastCleanAt : a.status === 'unpaired' ? u?.fetchedAt : null;
      return iso(last ? Date.parse(last) : s.now);
    };
    // A flagged account, a disconnected one and one that needs re-auth answer no window, so their series read against
    // the readings of their seeds' literals: sunset's `win('5h', 0.12, ...)` and `win('7d', 0.46, ...)`, the last clean
    // poll's before the end, legacy's `win('5h', 0.18, ...)` and `win('7d', 0.52, ...)`, and spare's
    // `win('5h', 0.4, ...)`, `win('7d', 0.02, ...)` and `win('7d_fable', 0, ...)`. A window with no share, zephyr's
    // `win('7d', null, ...)`, ends on a sample with no figure.
    const SEEDED = { legacy: { '5h': 0.18, '7d': 0.52 }, spare: { '5h': 0.4, '7d': 0.02, '7d_fable': 0 }, sunset: { '5h': 0.12, '7d': 0.46 } };
    const off = [];
    let read = 0;
    for (const a of s.accounts) {
      const windows = [...a.windows, ...(a.fableWindow ? [a.fableWindow] : [])];
      for (const h of await s.api.getQuotaHistory(a.id)) {
        read += 1;
        const answered = windows.find((x) => x.id === h.windowId);
        const reading = answered === undefined ? SEEDED[a.id]?.[h.windowId] : answered.utilization;
        const last = h.samples.at(-1);
        const ends = reading === null ? last?.utilization === null : last !== undefined && Math.abs(last.utilization - reading) <= 0.001;
        if (reading === undefined || !last || last.at !== endOf(a) || !ends) off.push(`${a.id} ${h.windowId}: the last sample ${last?.at} at ${last?.utilization}, the reading ${endOf(a)} at ${reading}`);
      }
    }
    assert.ok(read > 0, 'the story samples its windows');
    assert.deepEqual(off, []);
  });

  test(`a Fable request's member without the model week's reading scores untouched on the model week, at ${at}`, async () => {
    const s = await story;
    const records = (await decisionsOf(s)).filter((r) => r.request.canonicalModel.includes('fable') && r.candidates.some((c) => c.pace !== null));
    const unread = records.flatMap((r) => r.candidates.filter((c) => c.kind === 'oauth' && c.pace !== null && c.windows.seven_day_fable === undefined).map((c) => [r, c]));
    assert.ok(records.length > 0 && unread.length > 0, 'the story runs a Fable request under Pace past a member that reports no model week');
    for (const [r, c] of unread) {
      assert.deepEqual([c.pace.remaining, c.pace.reset, c.pace.window], [1, null, 'seven_day_fable'], `${r.requestId}: ${c.accountId}`);
      assert.ok(s.lib.candidateCells(c, r, s.now).paceTitle.startsWith('100% of the 7-day Fable window left'), `${r.requestId}: ${c.accountId}`);
    }
    for (const r of records) for (const c of r.candidates) if (c.kind === 'oauth' && c.pace !== null) assert.equal(c.pace.window, 'seven_day_fable', r.requestId);
  });

  test(`every Pace placement's winner scores highest in its tier among the eligible members, at ${at}`, async () => {
    const s = await story;
    let seen = 0;
    for (const r of await decisionsOf(s)) {
      if (!['placed', 'moved'].includes(r.outcome.kind) || r.settings?.strategy !== 'pace' || r.outcome.rung !== null) continue;
      // A placement a fallback credit or a judged turn directed went ahead of the strategy, which picked nothing.
      const pick = r.walk.find((w) => w.step === 'pick');
      if (pick?.facts.some((f) => f.key === 'reason' && ['classifier', 'credit'].includes(f.value))) continue;
      const id = `${r.requestId}/${r.seq}`;
      const winner = r.candidates.find((c) => c.accountId === r.outcome.accountId);
      // A move picks among its destinations and a placement among the eligible members, each past the partitions.
      const tier = keptOf(r).filter((c) => c !== winner && c.accountId !== r.outcome.fromAccountId && sameTier(c, winner));
      assert.equal(winner.score, winner.pace.score, `${id}: ${winner.accountId}'s score is the one its reading gives`);
      for (const c of tier) {
        assert.ok(winner.pace.score >= c.pace.score, `${id}: ${winner.accountId} scores ${winner.pace.score} under ${c.accountId}'s ${c.pace.score}`);
      }
      seen += 1;
    }
    assert.ok(seen > 0, 'the story places by Pace');
  });

  test(`every placed story decision takes a member that fits wherever a rival under its cap fits, at ${at}`, async () => {
    const s = await story;
    let seen = 0;
    for (const r of await decisionsOf(s)) {
      if (!s.lib.STORY_IDS.has(r.requestId) || r.outcome.kind !== 'placed') continue;
      const id = `${r.requestId}/${r.seq}`;
      const winner = r.candidates.find((c) => c.accountId === r.outcome.accountId);
      // A rung places past the strategy, yet its member still has to fit wherever a rival under its cap does.
      const fitting = r.candidates.filter((c) => c !== winner && c.obstacle === null && c.inFlight < c.inFlightCap && c.fit === 'fits');
      assert.ok(winner.fit === null || winner.fit === 'fits' || fitting.length === 0, `${id}: ${winner.accountId} reads ${winner.fit} where ${fitting.map((c) => c.accountId).join(', ')} fit`);
      seen += 1;
    }
    assert.ok(seen > 10, `${seen} story placements`);
  });

  test(`a story renewal whose move rule fired reads its cap filter dropped, at ${at}`, async () => {
    const s = await story;
    // The renewal's at-cap mark is off for the story, so only the story's own slots can hold such a renewal in place.
    const passed = [];
    let seen = 0;
    for (const r of await decisionsOf(s)) {
      if (!s.lib.STORY_IDS.has(r.requestId) || r.outcome.kind !== 'renewed') continue;
      if (!r.walk.some((w) => w.step.startsWith('move:') && w.verdict === 'fired')) continue;
      seen += 1;
      const capFilter = r.walk.find((w) => w.step === 'cap_filter');
      if (capFilter?.verdict !== 'dropped') passed.push(`${r.requestId}/${r.seq} ${capFilter?.verdict ?? 'no cap filter'}`);
    }
    assert.deepEqual(passed, [], 'a renewal whose rule fired and whose cap filter passed would have moved its pin');
    assert.ok(seen > 0, 'the story holds a renewal whose move rule fired');
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

  test(`a candidate the operator took out reads the operator obstacle with no reason, its state or its routing flag left on the candidate, at ${at}`, async () => {
    const s = await story;
    const operator = (await decisionsOf(s)).flatMap((r) => r.candidates.filter((c) => c.obstacle?.name === 'operator').map((c) => [`${r.requestId}/${r.seq} ${c.accountId}`, c]));
    assert.ok(operator.length > 0, 'the story holds candidates the operator took out');
    for (const [id, c] of operator) {
      assert.deepEqual([c.obstacle.reason, c.obstacle.until, c.obstacle.window], [null, null, null], id);
      assert.ok(c.state !== 'ready' || c.enabled === false, `${id} reads state ${c.state} and enabled ${c.enabled}`);
    }
  });

  test(`no token count starts between nightly's spend cap raise and its overage start, so a count's order and the records read its overage alike, at ${at}`, async () => {
    const s = await story;
    // A token count's order reads nightly in overage from the raise, and the records from its first head after it. Any
    // other request in between is placed by its record alone, which reads the head.
    const nightly = (await logOf(s)).filter((x) => x.type === 'event' && x.accountId === 'nightly');
    const raise = nightly.find((e) => e.kind === 'fast_credits_changed' && e.detail.blocked_before === 'org_spend_cap_reached' && e.detail.blocked_after === null);
    const started = nightly.find((e) => e.kind === 'overage_started');
    assert.ok(raise !== undefined && started !== undefined, 'the log holds the raise and the overage start');
    // The instant the records' order reads, which the log's raise has to name too.
    const [from, until] = [s.lib.NIGHTLY_CAP_RAISED, Date.parse(started.at)];
    assert.equal(Date.parse(raise.at), from, "the log's raise sits at the instant the story fixes");
    assert.ok(from < until, `the raise at ${raise.at} precedes the overage start at ${started.at}`);
    const between = s.requests.filter((r) => r.kind === 'count_tokens').flatMap((r) => r.attempts.filter((a) => Date.parse(a.startedAt) >= from && Date.parse(a.startedAt) < until).map((a) => `${r.id} on ${a.accountId} at ${a.startedAt}`));
    assert.deepEqual(between, []);
  });

  test(`a run-out writes one held record per account its hold waited on, and its exhausted record carries the held and answer facts, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    const from = iso(Math.min(...s.requests.map((r) => Date.parse(r.at))) - HOUR);
    const runOuts = s.requests.filter((r) => r.errorCode === 'accounts_at_cap');
    assert.ok(runOuts.length > 0, 'the story holds a run-out');
    // Behind a dialed attempt the router answers a run-out by its exhaustion's kind, each at the clock that draws it: a
    // model no member serves 404, and an untimed exhaustion 503, which it reads as the cap only with no leg dialed.
    const DIALED = { 'req_05945d59-178': 503, 'req_48db1b37-495': 404 };
    assert.ok(runOuts.some((r) => r.id in DIALED), `the story's run-outs: ${runOuts.map((r) => r.id).join(', ')}`);
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
      // The router writes the row's answer from the function that gives the record its status.
      assert.deepEqual(facts, [['held', bound, 'ms'], ['answer', exhausted.outcome.exhaustion?.status, 'count']], `${req.id}/${exhausted.seq}'s facts`);
      if (req.id in DIALED) assert.deepEqual([exhausted.outcome.exhaustion?.status, facts[1][1]], [DIALED[req.id], DIALED[req.id]], `${req.id}/${exhausted.seq}'s status and answer`);
    }
  });

  test(`a pick line under Pace carries no rank or turn, a lone seat's reads the only candidate and a tie's the gap over its runner-up, at ${at}`, async () => {
    const s = await story;
    const rows = (await logOf(s)).filter((x) => x.type === 'decision' && x.kind === 'placed' && x.by.kind === 'strategy' && x.rung === null);
    const keys = (row) => row.fact.map((f) => f.key);
    for (const row of rows) assert.ok(row.strategy !== 'pace' || !keys(row).some((k) => k === 'rank' || k === 'turn'), `${row.requestId}/${row.seq} reads ${keys(row).join(', ')}`);
    // The contract pool holds its seat alone, so the fleet's first turn has no tier mate, and the tie's failover picks
    // primary over nightly at the same spare rate, a gap of 0.
    const pairsOf = (id) => rows.find((x) => `${x.requestId}/${x.seq}` === id)?.fact.map((f) => [f.key, f.value]);
    assert.deepEqual(pairsOf('req_c94d1e07-b35/1'), [['pick', 'only candidate']]);
    assert.deepEqual(pairsOf('req_8c41f2d7-a93/2'), [['gap', 0], ['over', 'nightly']]);
  });

  test(`one story decision writes a long session's cache-map rows on two tiers as the router writes them, at ${at}`, async () => {
    const s = await story;
    const long = (await decisionsOf(s)).filter((r) => (r.outcome.completion?.cacheMapWritten?.length ?? 0) >= s.lib.CACHE_MAP_COMPACT_FROM);
    assert.deepEqual(long.map((r) => `${r.requestId}/${r.seq}`), ['req_1d9c3e55-0b2/2']);
    const [r] = long;
    const entries = r.outcome.completion.cacheMapWritten;
    const tier = (name) => entries.filter((e) => e.tier === name);
    assert.equal(entries.length, 13);
    assert.deepEqual([...new Set(entries.map((e) => e.tier))], ['message', 'static'], 'the message chain before the static one, as the router writes them');
    assert.deepEqual(tier('message').map((e) => e.depth), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
    assert.deepEqual(tier('static').map((e) => e.depth), [1, 2], 'one row per static boundary, numbered by its place in the chain');
    assert.ok(entries.every((e) => e.accountId === r.outcome.accountId), 'every entry is on the account the decision placed');
    // Every message row carries the answer's one prefix, which read nothing and wrote it whole at 1h.
    const { usage } = s.lib.requestById.get(r.requestId);
    const prefix = usage.cacheRead + usage.cacheWrite5m + usage.cacheWrite1h;
    assert.equal(prefix, usage.cacheWrite1h);
    assert.deepEqual([...new Set(tier('message').map((e) => e.prefixTokens))], [prefix]);
    // The static chain the rung placed on is the one the answer writes again, its deepest row the rung's own size.
    const [, deepestStatic] = tier('static');
    assert.deepEqual([deepestStatic.depth, deepestStatic.prefixTokens], [r.outcome.rung.depth, r.outcome.rung.prefixTokens]);
    assert.ok(tier('static').every((e) => e.prefixTokens < prefix), 'a static prefix is part of the message prefix');
    const row = s.lib.outcomeRows(r, s.now).find((x) => x.label === 'Cache-map entries');
    assert.deepEqual(row?.chips.at(-1), { label: 'prefix', value: s.lib.fmtCompact(prefix) });
    assert.equal(row?.more?.count, 13);
  });

  test(`an inference record's chain holds at least the message boundaries its answer writes rows for, the long session's all eleven, at ${at}`, async () => {
    const s = await story;
    // The router writes a row for each boundary new to the account, so an answer writes no more rows than its chain
    // holds boundaries, and the long session's answer, where every boundary is new, writes one per boundary.
    const written = (await decisionsOf(s)).filter((r) => !r.request.flags.probe && !r.request.flags.count && r.outcome.completion?.cacheMapWritten);
    const counts = written.map((r) => [`${r.requestId}/${r.seq}`, r.request.chain.boundaries, r.outcome.completion.cacheMapWritten.filter((e) => e.tier === 'message').length]);
    assert.deepEqual(counts.find(([id]) => id === 'req_1d9c3e55-0b2/2'), ['req_1d9c3e55-0b2/2', 11, 11]);
    assert.deepEqual(counts.filter(([, boundaries, rows]) => boundaries < rows), []);
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
    // The comparison runs where a record reads the account, since a record reads the members of its pool alone. A
    // spent row no decision met before its clear stands with no record reading it.
    const read = new Set(records.flatMap((r) => r.candidates.map((c) => c.accountId)));
    const edges = (key, reset) => events.filter((x) => (x.kind === 'window_spent' || x.kind === 'window_cleared') && `${x.accountId} ${x.detail.window}` === key && x.detail.reset === reset).sort((x, y) => Date.parse(x.at) - Date.parse(y.at));
    const met = (key, reset) => {
      const [account, window] = key.split(' ');
      const [from, until] = [Date.parse(edges(key, reset)[0].at), Date.parse(edges(key, reset).find((x) => x.kind === 'window_cleared')?.at ?? new Date(reset).toISOString())];
      return records.some((r) => Date.parse(r.request.at) >= from && Date.parse(r.request.at) < until && r.candidates.some((c) => c.accountId === account && c.windows[window] !== undefined));
    };
    for (const key of new Set([...crossings.keys(), ...spent.keys()].filter((key) => read.has(key.split(' ')[0])))) {
      const unread = [...(spent.get(key) ?? [])].filter((reset) => !(crossings.get(key)?.has(reset) ?? false) && !met(key, reset));
      assert.deepEqual([...(spent.get(key) ?? [])].filter((reset) => !unread.includes(reset)).sort(), [...(crossings.get(key) ?? [])].sort(), `${key}'s crossings`);
    }
    // A window spent again after a clear under one reset is a crossing of its own, so each reset's rows alternate.
    for (const [key, resets] of spent) {
      for (const reset of resets) {
        const kinds = edges(key, reset).map((x) => x.kind);
        assert.ok(kinds.every((kind, i) => kind === (i % 2 === 0 ? 'window_spent' : 'window_cleared')), `${key} under ${new Date(reset).toISOString()}: ${kinds.join(', ')}`);
      }
    }
  });

  test(`nightly's 5-hour window reads spent, cleared on its first reading under the limit and spent again under one reset, as the engine's edges announce them, at ${at}`, async () => {
    const s = await story;
    const events = (await logOf(s)).filter((x) => x.type === 'event' && x.accountId === 'nightly' && x.detail.window === 'five_hour' && (x.kind === 'window_spent' || x.kind === 'window_cleared')).sort((x, y) => Date.parse(x.at) - Date.parse(y.at));
    const [spent] = events;
    const reset = spent.detail.reset;
    const under = events.filter((e) => e.detail.reset === reset);
    // Nightly's readings of the window under that reset after the spent edge: each answer's head on it, and the reading
    // each decision met there.
    const heads = s.requests.flatMap((r) => r.attempts.filter((a) => a.accountId === 'nightly').map((a) => ({ at: Date.parse(a.startedAt) + (a.durationMs ?? 0), requestId: r.id, source: 'head', w: a.windows.find((x) => x.id === '5h') })));
    const met = (await decisionsOf(s)).flatMap((r) => {
      const w = r.candidates.find((c) => c.accountId === 'nightly')?.windows.five_hour;
      return w ? [{ at: Date.parse(w.observedAt), requestId: w.source === 'head' ? r.requestId : null, source: w.source, w: { resetsAt: w.reset, utilization: w.utilization } }] : [];
    });
    const reads = [...heads, ...met].filter((x) => x.w !== undefined && Date.parse(x.w.resetsAt) === reset && x.at > Date.parse(spent.at)).sort((x, y) => x.at - y.at);
    const first = reads.find((x) => x.w.utilization < 1);
    const limit = first && reads.find((x) => heads.includes(x) && x.at > first.at && x.w.utilization >= 1);
    // The first reading under the limit clears the window, and the 429's head that reads the limit again before the
    // reset is spent again. With no reading under it in between, the poll's edge stands alone.
    assert.deepEqual(
      under.map((e) => [e.kind, e.at, e.detail.source, e.requestId]),
      [
        ['window_spent', spent.at, 'poll', null],
        ...(first ? [['window_cleared', iso(first.at), first.source, first.requestId]] : []),
        ...(limit ? [['window_spent', iso(limit.at), 'head', limit.requestId]] : []),
      ],
    );
    if (first) assert.deepEqual([under[1].detail.utilization, under[1].detail.utilization_before], [first.w.utilization, 1]);
    assert.ok(under.every((e) => Date.parse(e.at) < reset));
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
      assert.ok(spent.some((x) => Date.parse(x.at) < Date.parse(e.at)), `${id} follows a spent row under its reset`);
      // A reading back at the limit under the same reset is spent again, so up to the next spent row the readings of
      // the window under that reset stand under the limit, the account's standing reading among them.
      const next = spent.find((x) => Date.parse(x.at) > Date.parse(e.at));
      const before = (ms) => next === undefined || ms < Date.parse(next.at);
      const limits = [
        ...records.filter((r) => after(Date.parse(r.request.at)) && before(Date.parse(r.request.at))).flatMap((r) => r.candidates.filter((c) => c.accountId === e.accountId).map((c) => c.windows[d.window])),
        ...s.requests.flatMap((r) => r.attempts.filter((a) => a.accountId === e.accountId && after(Date.parse(a.startedAt) + (a.durationMs ?? 0)) && before(Date.parse(a.startedAt) + (a.durationMs ?? 0))).flatMap((a) => a.windows.filter((w) => WINDOW[w.id] === d.window))).map((w) => ({ reset: w.resetsAt, utilization: w.utilization })),
        ...(next === undefined ? s.accounts.filter((a) => a.id === e.accountId).flatMap((a) => a.windows.filter((w) => WINDOW[w.id] === d.window)).map((w) => ({ reset: w.resetsAt, utilization: w.utilization })) : []),
      ].filter((w) => w !== undefined && w.reset !== null && Date.parse(w.reset) === d.reset && (w.utilization ?? 0) >= 1);
      assert.deepEqual(limits, [], `${id} has no reading at the limit after it under its reset before the next spent row`);
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
        // A spend window names its reset's source, which no poll gives.
        assert.ok(Object.entries(c.windows).every(([name, w]) => (name === 'spend' ? ['estimated', 'head'] : ['head', 'poll']).includes(w.source)), id);
      }
    }
  });

  test(`each event of the eight kinds names its pool, at ${at}`, async () => {
    const s = await story;
    // The router reads the pool of a pin event off its key and of a request's event off the request, so the Pool
    // filter keeps each of them.
    const kinds = ['mark_cleared', 'model_permitted', 'model_rejected', 'pin_created', 'pin_expired', 'pin_migrated', 'stream_interrupted', 'warm_prefix_expired'];
    const events = (await logOf(s)).filter((x) => x.type === 'event' && kinds.includes(x.kind));
    assert.deepEqual(kinds.filter((kind) => !events.some((e) => e.kind === kind)), [], 'the story holds each kind');
    assert.deepEqual(events.filter((e) => e.poolId === null).map((e) => `${e.kind} ${e.id}`), []);
  });

  test(`the refused records carry their codes, at ${at}`, async () => {
    const s = await story;
    const summaries = (await logOf(s)).filter((x) => x.type === 'decision' && x.kind === 'refused');
    const refused = (await decisionsOf(s)).filter((r) => r.outcome.kind === 'refused');
    assert.deepEqual(refused.map((r) => `${r.outcome.refusal.kind} ${r.outcome.refusal.code}`).sort(), ['client contract', 'client drift', 'guardrail guardrail_blocked', 'key revoked']);
    for (const r of refused) {
      const id = `${r.requestId}/${r.seq}`;
      assert.equal(summaries.find((x) => x.requestId === r.requestId && x.seq === r.seq)?.refusal?.code, r.outcome.refusal.code, `${id}'s row`);
    }
  });

  test(`a guardrail's block writes one refused record under the name of the guardrail its key enforces, its request facts as the contract's, at ${at}`, async () => {
    const s = await story;
    const blocks = (await decisionsOf(s)).filter((r) => r.outcome.refusal?.kind === 'guardrail');
    assert.equal(blocks.length, 1, 'one block of the story writes its record');
    const [r] = blocks;
    const id = `${r.requestId}/${r.seq}`;
    // The relay answers the stored name of the guardrail the request's matches mark blocking.
    const row = s.requests.find((x) => x.id === r.requestId);
    const blocker = (await s.api.listGuardrails()).find((g) => g.id === row?.guardrails.find((m) => m.blocked)?.id);
    assert.deepEqual([row?.errorCode, blocker?.mode, blocker?.keys.map((k) => k.id)], ['guardrail_blocked', 'enforce', [r.request.keyId]], `${id}: the request row keeps its code under a guardrail its key enforces`);
    assert.deepEqual(r.outcome.refusal, { code: 'guardrail_blocked', kind: 'guardrail', reason: blocker.name });
    // The block reads the class and the count flag, and refuses before any selection reads the rest.
    const { chain, flags, holds, lane, parentAgentId, requestClass } = r.request;
    assert.deepEqual(
      { chain, flags, holds, lane, parentAgentId, requestClass, walk: r.walk.map((w) => [w.step, w.verdict, w.facts.map((f) => `${f.key} ${f.value}`)]) },
      {
        chain: { boundaries: 0, staticBoundaries: 0, staticHash: false },
        flags: { contextCompacted: false, count: row.kind === 'count_tokens', fast: false, marker: false, portable: false, probe: row.kind === 'probe' },
        holds: [],
        lane: { effort: '', speed: '', thinking: '' },
        parentAgentId: null,
        requestClass: row.requestClass,
        walk: [['admission', 'refused', [`reason ${blocker.name}`]], ['selection', 'not_run', []]],
      },
    );
    assert.deepEqual(r.request.attempted, []);
    const preset = await s.api.listRoutingLog({ from: iso(Date.parse(r.request.at) - HOUR), kinds: ['refused'], limit: 200, to: iso(s.now) });
    assert.ok(preset.rows.some((x) => x.type === 'decision' && x.requestId === r.requestId && x.refusal?.kind === 'guardrail'), `${id}: the refused kind lists the record`);
  });

  test(`a client's refusal writes its record with the selection's facts empty, as a guardrail's block does, at ${at}`, async () => {
    const s = await story;
    const refused = (await decisionsOf(s)).filter((r) => r.outcome.refusal?.kind === 'client');
    assert.equal(refused.length, 2, 'the drift and the contract write their records');
    for (const r of refused) {
      const id = `${r.requestId}/${r.seq}`;
      const { attempted, chain, flags, holds, lane, parentAgentId, requestClass } = r.request;
      assert.deepEqual(
        { attempted, chain, flags, holds, lane, parentAgentId, requestClass },
        {
          attempted: [],
          chain: { boundaries: 0, staticBoundaries: 0, staticHash: false },
          // Both refusals are inference requests, so neither reads a count or a probe flag.
          flags: { contextCompacted: false, count: false, fast: false, marker: false, portable: false, probe: false },
          holds: [],
          lane: { effort: '', speed: '', thinking: '' },
          parentAgentId: null,
          // The drift sent the main class, which the route reads no class for, and the contract refusal sent no class
          // header.
          requestClass: null,
        },
        `${id}: the refusal leaves the selection's facts empty`,
      );
    }
  });

  test(`the usage poll's two backoff closes read their causes, the plan's end at the pause's instant, at ${at}`, async () => {
    const s = await story;
    const log = await logOf(s);
    const closes = log.filter((x) => x.type === 'event' && x.kind === 'poll_backoff_ended');
    assert.deepEqual(closes.map((e) => [e.accountId, e.detail.cause]).sort(), [['primary', 'clean_poll'], ['sunset', 'plan_ended']]);
    const ended = closes.find((e) => e.detail.cause === 'plan_ended');
    const flag = log.find((x) => x.type === 'event' && x.kind === 'plan_ended' && x.accountId === ended?.accountId);
    assert.deepEqual([ended?.detail.resumed_at, Date.parse(ended?.at ?? '')], [Date.parse(flag?.at ?? ''), Date.parse(flag?.at ?? '')], "the close stands at the plan_ended row's instant and names it as the pause's");
    assert.ok(ended?.detail.attempts === 1 && ended.detail.duration_ms > 0, `the run before the plan ended reads its attempt and its span: ${JSON.stringify(ended?.detail)}`);
  });

  test(`every registry change carries the diff its writer stores, and a record reads the pool's stamp at its instant, at ${at}`, async () => {
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
      // The ledger's mirror names each change's subject in the column of its kind: an account's change its account, a
      // key's its key and a pool's its pool.
      const subject = (prefix) => (e.detail.kind.startsWith(prefix) ? e.detail.target : null);
      assert.deepEqual([e.accountId ?? null, e.keyId ?? null, e.poolId ?? null], [subject('account_'), subject('key_'), subject('pool_')], `${e.id}: a registry row names its subject in its own column`);
      if (e.detail.kind === 'pool_updated') {
        const pairs = pairsOf(e, POOL_DIFF_MEMBERS);
        assert.ok(!('secret_replaced' in e.detail.diff), `${e.id}: a pool holds no secret`);
        // The story's one edit of the pool is its last, so each new value is the one the pool holds, its label as the
        // pool's name.
        const pool = s.pools.find((x) => x.id === e.detail.target);
        assert.ok(pool, `${e.id}: the edit names a pool of the story`);
        const member = (field) => (field === 'label' ? 'name' : field.replace(/_([a-z])/g, (_, c) => c.toUpperCase()));
        for (const [field, change] of Object.entries(pairs)) if (member(field) in pool) assert.deepEqual(change.to, pool[member(field)], `${e.id}: ${field} reads the pool's value`);
        // A policy member's dotted field names its path under the pool's policy.
        const policy = Object.entries(pairs).filter(([field]) => field.startsWith('keepalive.'));
        assert.ok(policy.length > 0, `${e.id}: the edit moves a policy member`);
        for (const [field, change] of policy) {
          const [, name] = member(field).split('.');
          assert.deepEqual(change.to, pool.keepalive[name], `${e.id}: ${field} reads the pool's policy`);
        }
        // A decision record carries the pool's settings it was decided under, so the records before the edit read each
        // pair's old value and the records after it the new one. No record carries the keepalive policy.
        const records = (await decisionsOf(s)).filter((r) => r.request.poolId === e.detail.target && r.settings !== null);
        const before = records.filter((r) => Date.parse(r.request.at) < Date.parse(e.at));
        const after = records.filter((r) => Date.parse(r.request.at) >= Date.parse(e.at));
        assert.ok(before.length > 0 && after.length > 0, `${e.id}: records stand on both sides of the edit`);
        for (const [field, change] of Object.entries(pairs)) {
          if (field.startsWith('keepalive.')) continue;
          const setting = member(field);
          assert.ok(setting in before[0].settings, `${e.id}: ${field} is a setting the records carry`);
          assert.deepEqual([...new Set(before.map((r) => r.settings[setting]))], [change.from], `${e.id}: ${field} on the records before the edit`);
          assert.deepEqual([...new Set(after.map((r) => r.settings[setting]))], [change.to], `${e.id}: ${field} on the records after it`);
        }
        // The instant the settings took force moves with them, and a later write of the pool's members moves the stamp
        // again, so a record reads the edit's stamp up to that write and the write's from it on.
        const written = changes.filter((c) => c.detail.kind === 'pool_members_set' && c.detail.target === e.detail.target && Date.parse(c.at) > Date.parse(e.at)).map((c) => Date.parse(c.at));
        const heldAt = (at) => Math.max(Date.parse(e.at), ...written.filter((w) => w <= at));
        assert.ok(written.length > 0 && after.some((r) => Date.parse(r.request.at) < written[0]) && after.some((r) => Date.parse(r.request.at) >= written[0]), `${e.id}: records stand on both sides of the members write`);
        assert.ok(before.every((r) => Date.parse(r.settings.poolUpdatedAt) < Date.parse(e.at)), `${e.id}: the records before the edit read an earlier stamp`);
        assert.ok(after.every((r) => Date.parse(r.settings.poolUpdatedAt) === heldAt(Date.parse(r.request.at))), `${e.id}: the records after it read the stamp the pool held at their instant`);
      }
      if (e.detail.kind === 'account_updated') {
        const pairs = pairsOf(e, ACCOUNT_DIFF_MEMBERS);
        const account = s.accounts.find((x) => x.id === e.detail.target);
        if ('secret_replaced' in e.detail.diff) {
          assert.equal(e.detail.diff.secret_replaced, true, `${e.id}: the edit staged a secret, which reads as the flag alone`);
          assert.equal(account?.kind, 'api_key', `${e.id}: only an API-key account holds a secret`);
          assert.ok(!('canceled' in pairs) && !('subscription_ends_at' in pairs), `${e.id}: an API-key account has no end`);
        } else {
          // An operator's pause writes the switch's pair alone, the one diff `set_enabled` stores.
          assert.deepEqual(e.detail.diff, { enabled: { from: true, to: false } }, `${e.id}: a pause stores the switch's pair alone`);
        }
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
      const session = r.request.sessionId;
      for (const [name, at] of [['refresh', r.pin.lastRefreshAt], ['last use', r.pin.lastUsedAt]]) {
        assert.ok(written(session, at), `${r.requestId}/${r.seq}: the pin's ${name} at ${at} has no request of session ${session} at it`);
      }
      seen += 1;
    }
    assert.ok(seen > 0, 'the story reads a live pin');
    for (const e of (await logOf(s)).filter((x) => x.type === 'event' && ['pin_created', 'pin_migrated'].includes(x.kind))) {
      assert.ok(written(e.sessionId, e.at), `the ${e.kind} event ${e.id} at ${e.at} has no request of session ${e.sessionId} at it`);
    }
  });

  test(`no record reads a fresh placement over a pin the day's earlier rows left live on another account, at ${at}`, async () => {
    const s = await story;
    // A request's first record, since the router's record after a failover reads no pin by its own rule.
    const first = new Map();
    for (const r of await decisionsOf(s)) if (!first.has(r.requestId) || r.seq < first.get(r.requestId).seq) first.set(r.requestId, r);
    // The router keys a pin by the model it resolved the request to.
    const keyOf = (keyId, sessionId, agentId, model) => `${keyId}|${sessionId}|${agentId}|${model}`;
    // A request's detail lists every event of its window, so one release reaches several details.
    const releases = [...new Map(s.events.filter((e) => e.kind === 'pin_expired').map((e) => [`${e.at}|${e.accountId}|${keyOf(e.keyId, e.sessionId, e.detail?.agent_id, e.model)}`, e])).values()].sort(
      (x, y) => Date.parse(x.at) - Date.parse(y.at),
    );
    const pins = new Map();
    const fresh = [];
    let released = 0;
    let seen = 0;
    for (const r of [...s.requests].sort((x, y) => Date.parse(x.at) - Date.parse(y.at))) {
      const head = r.attempts[0];
      if (r.sessionId === null || head === undefined) continue;
      const ttl = affinityOf(s, r);
      if (ttl === 0) continue;
      const start = Date.parse(head.startedAt);
      for (; released < releases.length && Date.parse(releases[released].at) <= start; released += 1) {
        const e = releases[released];
        const key = keyOf(e.keyId, e.sessionId, e.detail?.agent_id, e.model);
        if (pins.get(key)?.accountId === e.accountId) pins.delete(key);
      }
      const key = keyOf(r.keyId, r.sessionId, r.agentId, r.canonicalModel);
      const held = pins.get(key);
      const live = held !== undefined && start - held.at <= ttl ? held : undefined;
      const drawn = !s.lib.STORY_IDS.has(r.id) && !s.lib.UNSEEDED_IDS.has(r.id);
      if (drawn && live !== undefined) seen += 1;
      if (drawn && live !== undefined && live.accountId !== head.accountId && first.get(r.id)?.pin?.found === 'none') fresh.push(`${r.id}: placed on ${head.accountId} over the pin on ${live.accountId}`);
      // A classifier call holds no pin of its own.
      if (r.classifier !== null) continue;
      // A cooling answer deletes the pin, and a token count renews a live one and creates none.
      const last = r.attempts[r.attempts.length - 1];
      if (last.cooldownSet !== null || last.upstreamStatus === 429) pins.delete(key);
      else if (live?.accountId === last.accountId) pins.set(key, { ...live, at: Date.parse(last.startedAt) });
      else if (r.kind !== 'count_tokens') pins.set(key, { accountId: last.accountId, at: Date.parse(last.startedAt) });
    }
    assert.ok(seen > 0, 'a drawn row finds a live pin');
    assert.deepEqual(fresh, []);
  });

  // Catches a purge at the turn's own instant, a purge on another phase than the story router's sweeps, and a walk with
  // no purge, which hands a turn the size of a report the sweep took. The held conversation's next turn comes past its
  // pin's window and ahead of the sweep that takes the row (`REPLACED_PIN_ID` in mock/requests.ts), the purged
  // conversation's second turn behind that sweep and ahead of the wall clock's next minute, and its third behind the
  // sweep that takes the second's pin, on the second's account.
  test(`a turn past its pin's window reads the row idle ahead of the sweep that purges it, and no row and no size behind it, at ${at}`, async () => {
    const s = await story;
    const [replaced, second] = await Promise.all([s.api.getDecision('req_6f3e1b84-d29', 1), s.api.getDecision('req_8b2f4c71-e05', 1)]);
    const third = s.requests.find((r) => r.id === 'req_d5e17a39-c48');
    assert.deepEqual([replaced?.pin?.found, second?.pin?.found, third?.attempts[0]?.previousInputTokens], ['expired_idle', 'none', null]);
  });

  // Catches a walk that files the reason by the purge alone: both later turns of the purged conversation come behind a
  // sweep, the second onto another account than the one that answered the first, the third back onto the second's.
  test(`a turn behind a purge that lands off its last answer's account reads previous_message_not_found, and one that lands back reads none, at ${at}`, async () => {
    const s = await story;
    const reasonOf = (id) => s.requests.find((r) => r.id === id)?.attempts[0]?.cacheMissReason;
    assert.deepEqual([reasonOf('req_8b2f4c71-e05'), reasonOf('req_d5e17a39-c48')], ['previous_message_not_found', 'none']);
  });

  // Catches a draw that sends a probe whose session holds no live pin and places a pin for it: the selector answers
  // such a probe the pinned obstacle `no_pin` with no attempt, so no record of a probe places or moves a pin.
  test(`no probe places a pin, and a probe that sent nothing reads the no_pin answer unless its pin moved off its recipe's account, at ${at}`, async () => {
    const s = await story;
    const records = (await decisionsOf(s)).filter((r) => r.request.flags.probe);
    const placed = records.filter((r) => r.outcome.kind === 'placed' || r.outcome.kind === 'moved').map((r) => `${r.requestId}/${r.seq}`);
    const answers = s.requests
      .filter((r) => r.kind === 'probe' && r.attempts.length === 0)
      .map((r) => {
        const own = records.filter((x) => x.requestId === r.id);
        return [r.id, r.status, r.errorCode, r.outcome, own.map((x) => [x.outcome.kind, x.outcome.exhaustion?.kind, x.outcome.exhaustion?.obstacle])];
      });
    assert.deepEqual(placed, [], 'a probe record that places or moves a pin');
    // The failover session's subagent `6c2b` probes after its auxiliary call moved its pin to nightly while its recipe
    // stayed on primary (`MOVED_PIN_PROBE_ID` in mock/requests.ts). Every other probe that sent nothing found no pin.
    const moved = 'req_3c7f2e98-d16';
    for (const [id, ...answer] of answers.filter(([id]) => id !== moved)) assert.deepEqual(answer, [503, 'accounts_unavailable', 'rejected', [['exhausted', 'pinned', 'no_pin']]], id);
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
        const higher = keptOf(r).filter((c) => c.accountId !== r.outcome.fromAccountId && tierBefore(c.tier, winner.tier));
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
    const seen = { exhausted: 0, held: 0, ranOut: 0, reset: 0, rewrite: 0, slowDown: 0 };
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
      // So does a moved pin's wait for the head of the request that carried the move.
      if (r.outcome.hold?.reason === 'rewrite') {
        seen.rewrite += 1;
        assert.deepEqual(
          [r.pin?.found, steps.indexOf('move:shed') > steps.indexOf('pin'), steps.includes('partitions'), firstRung, steps.includes('pick'), steps[steps.length - 1]],
          ['live', true, false, -1, false, 'reserve'],
          walk,
        );
        continue;
      }
      // A probe's pinned answer ends at the probe step before any placement, which the strip's read of the story's own
      // answer holds.
      if (r.outcome.exhaustion?.kind === 'pinned') continue;
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
    assert.ok(seen.exhausted > 0 && seen.held > 0 && seen.ranOut > 0 && seen.reset > 0 && seen.rewrite > 0 && seen.slowDown > 0, JSON.stringify(seen));
    assert.ok(records.some((r) => r.requestId === 'req_51d0e2b8-7a0' && r.walk.some((w) => w.step === 'partitions')), 'the empty-eligible exhaustion walks the partitions');
  });

  /** The partitions read of one story, a function of it so that the noon story below takes the same read. */
  const readPartitions = async (s) => {
    const d = s.lib;
    const records = await decisionsOf(s);
    let seen = 0;
    let fitted = 0;
    let unfit = 0;
    let ladders = 0;
    for (const r of records) {
      const steps = r.walk.map((w) => w.step);
      if (!steps.some((x) => x.startsWith('rung:'))) continue;
      const set = narrowedSetOf(r);
      const pool = d.poolById.get(r.request.poolId);
      // A move's destinations carry its gate's fit. A placement reads a fit on the members that the spent and cap
      // partitions left, and the members that they dropped read none.
      // A drawn token count and a drawn hold read no fit: the generator draws their account without one, and
      // neither takes a slot the cap could keep that choice by.
      const unread = !d.STORY_IDS.has(r.requestId) && (r.request.flags.count || r.outcome.kind === 'held');
      if (unread) assert.ok(set.every((c) => c.fit === null), `${r.requestId}/${r.seq} reads a fit`);
      else if (!moving(r)) {
        const read = fitReadOf(set, pool, r);
        for (const c of set) {
          assert.equal(c.fit !== null, read.has(c), `${r.requestId}/${r.seq}: ${c.accountId} reads fit ${c.fit}`);
          if (read.has(c)) fitted += 1;
          if (read.has(c) && c.fit !== 'fits' && !d.STORY_IDS.has(r.requestId)) unfit += 1;
        }
      }
      for (const w of r.walk.filter((x) => x.step === 'partitions')) {
        seen += 1;
        // A landing that is no move's destination set names the rung of the ladder its no-fit partition reached, after
        // the no-fit count, unless the spent partition left no member to reach it.
        const spent = w.facts.find((f) => f.key === 'spent')?.value;
        const laddered = !moving(r) && set.length - (typeof spent === 'number' ? spent : 0) > 0;
        assert.deepEqual(w.facts.map((f) => f.key), laddered ? ['spent', 'at_cap', 'no_fit', 'ladder', 'winding_down'] : ['spent', 'at_cap', 'no_fit', 'winding_down'], `${r.requestId}/${r.seq}`);
        if (laddered) ladders += 1;
        // The engine writes a partition's count bare, as it writes its words.
        assert.ok(w.facts.every((f) => f.unit === null), `${r.requestId}/${r.seq}: ${w.facts.map((f) => f.unit).join(', ')}`);
        for (const f of w.facts) {
          if (typeof f.value === 'number') assert.ok(f.value >= 0 && f.value <= set.length, `${r.requestId}: ${f.key} ${f.value} over ${set.length}`);
          else assert.ok((f.key === 'ladder' ? ['ending', 'every', 'fits', 'least_bad', 'rewrite_only'] : ['fell_through', 'held', 'off']).includes(String(f.value)), `${r.requestId}: ${f.key} ${f.value}`);
        }
      }
    }
    assert.ok(seen > 10);
    assert.ok(ladders > 10, `${ladders} ladder lines`);
    assert.ok(fitted > 10 && unfit > 0, `${fitted} members read a fit, ${unfit} of the drawn placements' not fitting`);

    const read = (id, keys) => {
      const w = records.find((r) => `${r.requestId}/${r.seq}` === id)?.walk.find((x) => x.step === 'partitions');
      return [w?.verdict, (w?.facts ?? []).filter((f) => keys.includes(f.key)).map((f) => [f.key, f.value])];
    };
    // The cohort hold waits with every member it could take at its cap, under a non-Fable model. Neither fits, and the
    // ladder keeps primary, whose rewrite lands under the ceiling, over nightly, whose rewrite does not.
    assert.deepEqual(read('req_69e30548-32e/1', ['spent', 'at_cap', 'no_fit', 'ladder']), ['match', [['spent', 'off'], ['at_cap', 'held'], ['no_fit', 1], ['ladder', 'rewrite_only']]]);
    // The spare-rate placement passes over secondary, which falls short of the ceiling.
    assert.deepEqual(read('req_2c7d9f15-b63/1', ['no_fit']), ['match', [['no_fit', 1]]]);
    // Nothing is left to narrow, so the step reads none with every count 0 or off.
    assert.deepEqual(read('req_51d0e2b8-7a0/1', ['spent', 'at_cap', 'no_fit', 'winding_down']), ['none', [['spent', 'off'], ['at_cap', 0], ['no_fit', 0], ['winding_down', 0]]]);
    // Four of the story's failovers by the rung of the ladder each reached: the long session's and the one from
    // secondary keep primary, whose runway outlasts nightly's, and the two on nightly read least bad.
    assert.deepEqual(
      ['req_1d9c3e55-0b2/2', 'req_2e7c90a1-6d4/2', 'req_4b0e9d17-c31/2', 'req_b8c3e9d4-e61/2'].map((id) => read(id, ['no_fit', 'ladder'])),
      [
        ['match', [['no_fit', 1], ['ladder', 'rewrite_only']]],
        ['match', [['no_fit', 0], ['ladder', 'least_bad']]],
        ['match', [['no_fit', 0], ['ladder', 'least_bad']]],
        ['match', [['no_fit', 1], ['ladder', 'rewrite_only']]],
      ],
    );
    // On the two least-bad landings nightly's row states the runway to 1.0 over the windows whose runway ends by their
    // reset: the 5-hour one, which ends ahead of the reset at 18:11, where the 7-day one passes its reset. At its burn
    // of 0.019 an hour and the prior's 0.01, (1 - 0.91 - 0.00015) / 0.029 is 3.09828 h, and (1 - 0.94 - 0.0038) / 0.029
    // is 1.93793 h.
    const nightlyOn = (id) => records.find((r) => `${r.requestId}/${r.seq}` === id)?.candidates.find((c) => c.accountId === 'nightly');
    assert.deepEqual(['req_2e7c90a1-6d4/2', 'req_4b0e9d17-c31/2'].map((id) => [nightlyOn(id)?.runwayMs, nightlyOn(id)?.binding]), [[11_153_793, 'five_hour'], [6_976_552, 'five_hour']]);

    // Each partition writes its label on the row of every member it drops, so a count names as many rows as it counts,
    // and one that falls through, holds for a slot or stays off names none.
    const labels = { at_cap: 0, no_fit: 0, spent: 0, winding_down: 0 };
    for (const r of records) {
      const w = r.walk.find((x) => x.step === 'partitions');
      if (!w) continue;
      const id = `${r.requestId}/${r.seq}`;
      const counts = w.facts.filter((f) => f.key in labels);
      assert.deepEqual(
        counts.map((f) => [f.key, r.candidates.filter((c) => c.partition === f.key).length]),
        counts.map((f) => [f.key, typeof f.value === 'number' ? f.value : 0]),
        `${id}: ${r.candidates.map((c) => `${c.accountId} ${c.partition}`).join(', ')}`,
      );
      // A member read no fit for stands on the fitting rung, and a least-bad or winding-down one with no allowance on
      // the last.
      const model = r.request.canonicalModel;
      const rungOf = (c) => (c.fit === null ? 'fits' : ['fits', 'rewrite_only'].includes(c.fit) || !spentOn(c, model) ? c.fit : 'every');
      const ladder = w.facts.find((f) => f.key === 'ladder')?.value;
      const kept = keptOf(r);
      const destinations = new Set(moving(r) ? narrowedSetOf(r).map((x) => x.accountId) : []);
      for (const c of r.candidates.filter((x) => x.partition !== null)) {
        labels[c.partition] += 1;
        const row = `${id}: ${c.accountId} reads ${c.partition}`;
        if (c.partition === 'at_cap') assert.ok(c.inFlight >= c.inFlightCap, `${row} at ${c.inFlight} of ${c.inFlightCap}`);
        // The rewrite-only and least-bad rungs keep the longest runway, so a member can drop off the ladder's own rung.
        if (c.partition === 'no_fit') assert.ok(rungOf(c) !== ladder || (['least_bad', 'rewrite_only'].includes(ladder) && kept.some((k) => rungOf(k) === ladder)), `${row} on ${rungOf(c)} under ${ladder}`);
        // The router reads a dropped member by its role in the record: the source the outcome names reads `source`, a
        // destination of the gate's move `destination` and any other member but the outcome's account `out`.
        const role = c.accountId === r.outcome.accountId ? null : c.accountId === r.outcome.fromAccountId ? 'source' : destinations.has(c.accountId) ? 'destination' : 'out';
        if (role !== null) assert.equal(c.verdict, role, row);
      }
    }
    assert.ok(labels.at_cap > 0 && labels.no_fit > 0, JSON.stringify(labels));
    return records;
  };
  test(`every partitions step reads its four facts bare and within the record's own set, a landing's ladder after its no-fit count, at ${at}`, async () => readPartitions(await story));

  // Noon's story, loaded once beside the first instant. Its draws place req_ba0f39bd-fc9/1 on nightly on the least-bad
  // rung beside secondary, which outlasts it, and neither suite instant draws such a record.
  if (at === '2026-09-16T14:30:00Z') {
    const noon = (loading = loading.then(() => storyAt('2026-09-16T12:00:00Z')));
    test('every partitions step reads its four facts bare and within its own set, and a drawn least-bad landing keeps its account, at 2026-09-16T12:00:00Z', async () => {
      const records = await readPartitions(await noon);
      const facts = records.find((r) => r.requestId === 'req_ba0f39bd-fc9' && r.seq === 1)?.walk.find((w) => w.step === 'partitions')?.facts ?? [];
      // The rewrite of 0.01151 takes nightly's 5-hour window from 0.99 past 1, a wall of 0, and leaves secondary's 0.8
      // under 1 with no burn, so no wall, the longest. The ladder's marks hold secondary at its cap beside primary,
      // which outscores nightly, and overflow, which fits, so nightly stands alone on the rung.
      assert.deepEqual(facts.filter((f) => ['at_cap', 'no_fit', 'ladder'].includes(f.key)).map((f) => [f.key, f.value]), [['at_cap', 3], ['no_fit', 0], ['ladder', 'least_bad']]);
    });
  }

  const rowOf = (records, id, accountId) => {
    const row = records.find((r) => `${r.requestId}/${r.seq}` === id)?.candidates.find((c) => c.accountId === accountId);
    return [row?.fit, row?.runwayMs, row?.binding];
  };
  test(`a candidate row states the runway over the windows whose runway ends by their reset, to the ceiling on a fitting row and to 1.0 on a least-bad one, and none where each one passes it, at ${at}`, async () => {
    const records = await decisionsOf(await story);
    // On the shed move of 11:08:30, at the pin's burn of 0.0142 and 0.0016 an hour and its rewrite of 0.00264 and
    // 0.00031, primary's 5-hour runway, (0.75 - 0.49 - 0.00264) / (0.085 + 0.0142), 2.594 h, ends past the reset at
    // 11:44. Its 7-day runway, (0.78 - 0.70 - 0.00031) / (0.006 + 0.0016), 10.48553 h, ends past the window's horizon
    // five hours out and ahead of its reset 81 h out, so the row states that one.
    // On the placement of 13:41:15, a fresh session's 41,000 tokens rewrite 0.00123 and 0.00014. Nightly's 5-hour
    // runway, (0.75 - 0.40 - 0.00123) / (0.019 + 0.01), 12.03 h, ends past the reset at 18:11, and its 7-day runway,
    // (0.78 - 0.34 - 0.00014) / 0.0012, 366.6 h, past the reset 43 h out, so the row states none.
    // On the least-bad landing of 13:10:20, with primary and secondary at their cap, a fresh session's 43,000 tokens
    // rewrite 0.00129 and 0.00015. Nightly's 0.77 on its 5-hour window stands over the 0.75 ceiling, so its row states
    // the runway to 1.0. Its 5-hour wall, (1 - 0.77 - 0.00129) / (0.019 + 0.01), 7.887 h, ends past the reset at 13:11,
    // and its 7-day wall, (1 - 0.34 - 0.00015) / 0.0012, 549.9 h, past the reset 43.5 h out, so the row states none.
    assert.deepEqual(
      [rowOf(records, 'req_868264bc-611/1', 'primary'), rowOf(records, 'req_7a3f0c26-d81/1', 'nightly'), rowOf(records, 'req_9d4c1a73-e58/1', 'nightly')],
      [
        ['fits', 37_747_895, 'seven_day'],
        ['fits', null, null],
        ['least_bad', null, null],
      ],
    );
  });

  test(`a placement's least-bad row of a member that holds no allowance reads spent with no runway and no window, at ${at}`, async () => {
    const records = await decisionsOf(await story);
    // On the main turn of 14:03:00, secondary's 5-hour window has read 1 since 14:02:11, so it holds no allowance for
    // the request. Its read over the ceiling is least bad, and the ladder refuses a member with no allowance that rung,
    // so its row states no runway toward it and no window.
    assert.deepEqual(rowOf(records, 'req_7a2d4c18-e95/1', 'secondary'), ['spent', null, null]);
  });

  test(`the member a placement, a move or a hold names is one the partitions kept, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    let seen = 0;
    for (const r of records) {
      const partitions = r.walk.find((w) => w.step === 'partitions');
      if (!partitions || !['held', 'moved', 'placed'].includes(r.outcome.kind)) continue;
      const id = `${r.requestId}/${r.seq}`;
      const named = r.candidates.find((c) => c.accountId === r.outcome.accountId);
      assert.ok(named !== undefined, `${id}: ${r.outcome.accountId} is no candidate`);
      seen += 1;
      assert.equal(keptOf(r).includes(named), true, `${id}: ${r.outcome.kind} on ${named.accountId} over ${partitions.facts.map((f) => `${f.key} ${f.value}`).join(', ')}`);
    }
    assert.ok(seen > 10);
  });

  test(`a pick line names the ties the longest runway kept only where it narrowed them, on a move's fitting order or a failover's fitting members, at ${at}`, async () => {
    const records = await decisionsOf(await story);
    const runway = (c) => c.runwayMs ?? Number.POSITIVE_INFINITY;
    let read = 0;
    for (const r of records) {
      const pick = r.walk.find((w) => w.step === 'pick' && w.verdict === 'chosen');
      if (!pick) continue;
      // The strategy's ties are the ranked tier's best scores under Pace and the whole tier under Priority, and round
      // robin's cursor picks alone.
      const ranked = r.candidates.filter((c) => c.rank !== null);
      const ties = r.settings.strategy === 'pace' ? ranked.filter((c) => c.score === Math.max(...ranked.map((x) => x.score))) : ranked;
      const gateFits = r.walk.find((w) => w.step === 'fit')?.verdict === 'fits';
      const reads = r.settings.strategy === 'round_robin' ? [] : ties.filter((c) => (moving(r) ? gateFits : failedOver(r) && c.fit === 'fits'));
      const kept = ties.filter((c) => !reads.includes(c) || runway(c) === Math.max(...reads.map(runway)));
      const want = reads.length === 0 || kept.length === ties.length ? undefined : kept.map((c) => c.accountId);
      assert.deepEqual(pick.facts.find((f) => f.key === 'by_runway')?.value, want, `${r.requestId}/${r.seq}`);
      if (reads.length > 0) read += 1;
    }
    assert.ok(read > 0, 'a pick reads a runway');
  });

  test(`the story's pick lines write the draw the engine writes: the strategy's ties under Pace and Priority, the ones the runway kept where it narrowed them and whether a draw was left, and under round robin the turn with no draw, at ${at}`, async () => {
    const records = await decisionsOf(await story);
    const seen = new Set();
    const drawOf = (pick) => ['ties', 'by_runway', 'drawn', 'turn'].map((key) => pick.facts.find((f) => f.key === key)?.value);
    for (const r of records) {
      const pick = r.walk.find((w) => w.step === 'pick' && w.verdict === 'chosen');
      if (!pick) continue;
      const id = `${r.requestId}/${r.seq}`;
      // A pick a judged turn or a fallback credit directed names its account and its reason, and the strategy drew
      // nothing.
      if (pick.facts.some((f) => f.key === 'reason')) {
        assert.deepEqual(drawOf(pick), [undefined, undefined, undefined, undefined], `${id}: a directed pick's draw`);
        continue;
      }
      const [ties, kept, drawn, turn] = drawOf(pick);
      seen.add(r.settings.strategy);
      // The round-robin pool holds research alone, so its cursor names the one turn and leaves no draw.
      if (r.settings.strategy === 'round_robin') {
        assert.deepEqual(drawOf(pick), [undefined, undefined, false, '1 of 1'], id);
        continue;
      }
      if (kept !== undefined) assert.ok(kept.length > 0 && kept.length < ties.length && kept.every((x) => ties.includes(x)), `${id}: by runway ${kept.join(', ')}`);
      // The draw is left to the runway's ties where they narrowed the strategy's, else to the strategy's own.
      assert.deepEqual([drawn, turn], [(kept ?? ties).length > 1, undefined], `${id}: the draw`);
    }
    assert.deepEqual([...seen].sort(), ['pace', 'priority', 'round_robin']);
    // The tie's failover ties primary and nightly on Pace, both fitting, and the runway keeps primary, so no draw is
    // left. The work pool files studio and the annex in tiers of their own, so its placement at 13:47:24 ties studio
    // alone.
    const pickOf = (id) => drawOf(records.find((r) => `${r.requestId}/${r.seq}` === id).walk.find((w) => w.step === 'pick'));
    assert.deepEqual(pickOf('req_8c41f2d7-a93/2'), [['primary', 'nightly'], ['primary'], false, undefined]);
    assert.deepEqual(pickOf('req_b1e7c4a9-a69/1'), [['studio'], undefined, false, undefined]);
  });

  test(`every chosen pick line writes its tier as the engine's list of three whole numbers' digits, and no fact holds a slash-joined triple, at ${at}`, async () => {
    const records = await decisionsOf(await story);
    const triple = /^\d+\/\d+\/\d+$/;
    let picked = 0;
    for (const r of records) {
      const id = `${r.requestId}/${r.seq}`;
      // An exhausted record's pick names none and writes the strategy alone, so the chosen picks are the ones carrying
      // the tier, but a directed one, which the strategy ranked nothing for.
      const pick = r.walk.find((s) => s.step === 'pick' && s.verdict === 'chosen');
      if (pick) {
        picked += 1;
        const directed = pick.facts.some((f) => f.key === 'reason');
        const tiers = pick.facts.filter((f) => f.key === 'tier').map((f) => f.value);
        assert.equal(tiers.length, directed ? 0 : 1, `${id}: ${directed ? 'no' : 'one'} tier fact`);
        const [tier] = tiers;
        if (!directed) assert.ok(Array.isArray(tier) && tier.length === 3 && tier.every((x) => typeof x === 'string' && /^\d+$/.test(x)), `${id}: the tier ${JSON.stringify(tier)}`);
      }
      const joined = r.walk.flatMap((s) => s.facts.flatMap((f) => (Array.isArray(f.value) ? f.value : [f.value]).filter((x) => typeof x === 'string' && triple.test(x)).map((x) => `${s.step} ${f.key} ${x}`)));
      assert.deepEqual(joined, [], `${id}: a slash-joined triple`);
    }
    assert.ok(picked > 0, 'a record picks');
  });

  test(`one story placement ties two tier mates on Pace and the runway keeps one, so no draw was left, at ${at}`, async () => {
    const s = await story;
    const narrowed = (await decisionsOf(s)).filter((r) => s.lib.STORY_IDS.has(r.requestId) && r.walk.some((w) => w.step === 'pick' && w.facts.some((f) => f.key === 'by_runway')));
    assert.equal(narrowed.length, 1, narrowed.map((r) => `${r.requestId}/${r.seq}`).join(', '));
    const [r] = narrowed;
    const value = (key) => r.walk.find((w) => w.step === 'pick').facts.find((f) => f.key === key)?.value;
    const tied = (value('ties') ?? []).map((id) => r.candidates.find((c) => c.accountId === id));
    assert.deepEqual([r.outcome.kind, failedOver(r), r.settings.strategy, tied.length, value('by_runway'), value('drawn')], ['placed', true, 'pace', 2, [r.outcome.accountId], false]);
    // The two share the tier and the score, and both fit, the kept one to the longer runway.
    assert.ok(sameTier(tied[0], tied[1]) && tied[0].pace.score === tied[1].pace.score, tied.map((c) => `${c.accountId} ${c.tier.join('/')} ${c.pace.score}`).join(', '));
    const kept = tied.find((c) => c.accountId === r.outcome.accountId);
    const other = tied.find((c) => c !== kept);
    assert.deepEqual([kept.fit, other.fit], ['fits', 'fits']);
    assert.ok((kept.runwayMs ?? Number.POSITIVE_INFINITY) > (other.runwayMs ?? Number.POSITIVE_INFINITY), `${kept.runwayMs} over ${other.runwayMs}`);
  });

  test(`the story lists three forks, each with its base under its own key, at ${at}`, async () => {
    const s = await story;
    const forks = s.sessions.filter((x) => x.forkedFrom !== null);
    assert.equal(forks.length, 3, `${forks.length} forks are listed`);
    for (const fork of forks) {
      const base = s.sessions.find((x) => x.sessionId === fork.forkedFrom && x.keyId === fork.keyId);
      assert.ok(base, `${fork.sessionId}'s base ${fork.forkedFrom} is listed under ${fork.keyId}`);
    }
  });

  test(`one window of the story reads ahead of pace and one behind at ${at}`, async () => {
    const s = await story;
    const lead = (accountId, windowId) => {
      const account = s.accounts.find((a) => a.id === accountId);
      const w = account.windows.find((x) => x.id === windowId);
      return s.leadOf(w, w.periodHours * HOUR, s.now, s.lib.endOf(account, s.now));
    };
    assert.ok(lead('primary', '5h') > 0.05, `primary's 5h window reads ${lead('primary', '5h')}, not ahead of pace`);
    assert.ok(lead('nightly', '7d') < -0.05, `nightly's 7d window reads ${lead('nightly', '7d')}, not behind pace`);
    assert.ok(lead('secondary', '5h') > 0 && lead('secondary', '7d') > 0, `secondary's windows read ${lead('secondary', '5h')} and ${lead('secondary', '7d')}, not both ahead of pace`);
  });

  test(`every account window answers the lead the router's arithmetic gives its own reading, at ${at}`, async () => {
    const s = await story;
    let read = 0;
    for (const a of s.accounts) {
      const end = s.lib.endOf(a, s.now);
      for (const w of [...a.windows, ...(a.fableWindow === null ? [] : [a.fableWindow])]) {
        // The route maps the lead over the share, so a window with no share answers none.
        const want = w.utilization === null ? null : s.leadOf(w, w.periodHours * HOUR, s.now, end);
        assert.ok(want === null ? w.lead === null : typeof w.lead === 'number' && near(w.lead, want), `${a.id}'s ${w.id} leads ${w.lead}, not ${want}`);
        read += 1;
      }
    }
    assert.ok(read > 0, 'the story holds windows');
  });

  test(`the default range's kept-warm saving nets a figure per pool, a saving where the pool keeps caches warm and a loss on one whose probes cost more than they saved, at ${at}`, async () => {
    const s = await story;
    const groups = s.lib.breakdownOf(s.allRollups, { ...defaultRange(s), groupBy: 'pool', includeProbes: false });
    const nets = groups.map((g) => ({ kept: g.keptWarm.tokens1h + g.keptWarm.tokens5m, net: g.savingUsd - g.probeCostUsd, pool: g.key }));
    assert.ok(nets.every((n) => Number.isFinite(n.net)), `every pool nets a figure: ${JSON.stringify(nets)}`);
    assert.ok(nets.some((n) => n.kept > 0 && n.net > 0), `a pool that keeps caches warm nets a saving: ${JSON.stringify(nets)}`);
    assert.equal(nets.filter((n) => n.net < 0).length, 1, `one pool's probes cost more than they saved: ${JSON.stringify(nets)}`);
    assert.ok(nets.filter((n) => n.net >= 0).every((n) => n.kept > 0 || n.net === 0), `every other pool that kept nothing warm nets nothing: ${JSON.stringify(nets)}`);
    const warmed = groups.filter((g) => g.keptWarm.tokens1h > 0 && g.keptWarm.tokens5m > 0);
    assert.ok(warmed.length > 0, 'the kept tokens split between the two TTLs');
  });

  test(`a decision record reads a plan window's lead and projects it to its horizon, and no record projects a spend window, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    for (const r of records) {
      for (const c of r.candidates) {
        // A usage-based seat's spend window is no plan window, so the engine projects nothing on it.
        const w = c.windows.spend;
        if (w !== undefined) assert.deepEqual([w.horizon, w.projection, w.ahead], [null, null, null], `${r.requestId}/${r.seq}: ${c.accountId}'s spend window projects nothing`);
      }
    }
    // The record is nightly's reset hold at 13:10:21, 79 min 39 s before the story's clock. Primary's walked-back
    // readings, 0.32 of its 5-hour window and 0.71 of its week, stand against the shares elapsed, 0.2878 and 0.5298,
    // and burn 0.085 and 0.006 an hour: the 5-hour window runs to its reset at 16:44, the week five hours out. Nightly
    // reads its 5-hour window spent 39 s before the reset at 13:11, so its burn of 0.019 an hour adds under a
    // thousandth, and its week reads 0.34 with no burn.
    const held = records.find((r) => r.requestId === 'req_9d4c1a73-e58' && r.seq === 2);
    const fiveHoursOut = iso(s.now - 79 * 60_000 - 39_000 + 5 * HOUR);
    const read = (accountId, name) => {
      const w = held.candidates.find((c) => c.accountId === accountId).windows[name];
      return [w.ahead, w.horizon, w.projection];
    };
    assert.deepEqual(
      [read('primary', 'five_hour'), read('primary', 'seven_day'), read('nightly', 'five_hour'), read('nightly', 'seven_day')],
      [
        [0.032, iso(s.now + 2 * HOUR + 14 * 60_000), 0.623],
        [0.18, fiveHoursOut, 0.74],
        [0.002, iso(s.now - 79 * 60_000), 1],
        [-0.401, fiveHoursOut, 0.34],
      ],
    );
  });

  test(`the rewrites answer counts each reason's moves over its fits, two reasons by different fits, at ${at}`, async () => {
    const s = await story;
    const { moves, movesByReasonAndFit } = await s.api.getAnalyticsRewrites({ from: iso(s.now - 7 * DAY), to: iso(s.now) });
    assert.deepEqual(Object.keys(movesByReasonAndFit).sort(), Object.keys(moves).sort());
    for (const [reason, fits] of Object.entries(movesByReasonAndFit)) assert.equal(sum(Object.values(fits), (n) => n), moves[reason], reason);
    const shapes = new Set(Object.values(movesByReasonAndFit).map((fits) => JSON.stringify(Object.keys(fits).sort())));
    assert.ok(shapes.size > 1, `the reasons read ${[...shapes].join(', ')}`);
  });

  test(`the story's move an account's state forced counts in a range that starts after it inside its hour, at ${at}`, async () => {
    const s = await story;
    // The move lands 2 h 58 min 50 s before the clock (11:31:10 at 14:30, the seed `CAUSED_MOVE_ID` in
    // `web/src/mock/requests.ts`), so a range from 2 h 50 min back starts after it inside its hour, which the route
    // reads whole as the range's first bucket.
    const { moves, movesByReasonAndFit } = await s.api.getAnalyticsRewrites({ from: iso(s.now - 170 * 60_000), to: iso(s.now) });
    assert.deepEqual([moves.funding_required, movesByReasonAndFit.funding_required], [1, { fits: 1 }]);
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

  test(`every shedding round's pin whose move drops no thinking reads its burn per token of its last prompt to seven digits, the stretch round's first marked pin per token of its weighted size, and a pin with no prompt per its one token, at ${at}`, async () => {
    const s = await story;
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round');
    assert.ok(rounds.length > 0);
    for (const e of rounds) {
      // The router sizes a pin with no prompt at one token, so that pin's figure is no quotient of a prompt.
      for (const p of e.detail.pins.filter((x) => x.burn !== null && x.last_input_tokens > 0 && (x.dropped_thinking_tokens ?? 0) === 0)) {
        const want = p.burn / p.last_input_tokens;
        assert.ok(p.per_rewrite > 0 && Math.abs(p.per_rewrite - want) <= want * 1e-6, `${e.requestId}: ${p.session_id} reads ${p.per_rewrite}, not ${want}`);
      }
    }
    // The stretch round's first marked pin drops the 480 thinking tokens of its turn on studio, so its move rewrites
    // 29,520 of its 30,000 and weighs the 480 at the output's 5 over the write's 1.25, 1,920 more: its 0.028 over
    // 31,440 is 8.905852e-7.
    const stretch = rounds.find((e) => e.requestId === 'req_9f2e7c5a-c49')?.detail.pins.find((p) => p.marked);
    assert.deepEqual([stretch?.dropped_thinking_tokens, stretch?.per_rewrite], [480, 8.905852e-7]);
    // The 11:00 round's subagent pin `f2c8` holds no prompt and a burn of 0, so its figure is 0 over its one token.
    const unprompted = rounds.find((e) => e.requestId === 'req_1eaa9bc1-a18')?.detail.pins.find((p) => p.agent_id === 'f2c8');
    assert.equal(unprompted?.per_rewrite, 0);
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
      if (!value('five_hour_ceiling') || r.pin?.accountId === null || r.pin?.accountId === undefined) continue;
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

  test(`every round's pin that carries a rewrite prices it as the Pro-week figure with the window it compared on named beside a figure alone, and the story's two oldest rounds, an earlier build's, as they stored them, at ${at}`, async () => {
    const s = await story;
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round');
    // The model's own week where it draws one, else the 7-day window.
    const weekOf = (model) => (model.includes('fable') ? 'seven_day_fable' : 'seven_day');
    // The 08:30 and 11:00 rounds, which an earlier build wrote before the unit.
    const older = new Set([...rounds].sort((x, y) => Date.parse(x.at) - Date.parse(y.at)).slice(0, 2));
    assert.deepEqual(rounds.map((e) => [older.has(e), e.detail.rewrite_unit]).sort(), [[false, 'pro_week'], [false, 'pro_week'], [false, 'pro_week'], [false, 'pro_week'], [true, undefined], [true, undefined]], 'the newer rounds name the unit and the older ones none');
    // An earlier build stored a marked Sonnet pin's rewrite as its tokens times 1.4e-8, the share of the 7-day window
    // one token takes, times the Sonnet weight of 0.25, to five places: 0.00008 and 0.00031 for the 11:00 round's
    // 22,000 and 88,000 tokens. A newer round prices the prompt written to the cache again at the destination as the
    // Pro-week figure, the last prompt less the thinking the move drops: the stretch's 29,520 tokens, 30,000 less
    // 480, and 40,000 Sonnet 5.5 tokens, both pins on a 1-hour lane, at its weight 1.5 and the 1-hour write's 1.25 are
    // 55,350 and 75,000 units, over the Pro tier's 90,000,000 on the week. The reservation round's 36,000 tokens at the
    // 5-minute write's 1.25 are 67,500 units on the desktop key, whose CLI client prices under the interactive regime's
    // Pro week of 112,500,000. The three pins the stretch round's pick compared and left unmarked, each on a 1-hour
    // lane of the notebook key, which names no client: 61,000 Haiku 4.5 tokens at its weight 1 and 52,000 and
    // 69,000 Sonnet 5.5 tokens at 1.5, each at the 1-hour write's 1.25, are 76,250, 97,500 and 129,375 units over
    // 90,000,000.
    const storedRewrite = { 22_000: 0.00008, 88_000: 0.00031 };
    const proWeekRewrite = { 29_520: 55_350 / 90_000_000, 36_000: 67_500 / 112_500_000, 40_000: 75_000 / 90_000_000, 52_000: 97_500 / 90_000_000, 61_000: 76_250 / 90_000_000, 69_000: 129_375 / 90_000_000 };
    const read = { api_key: 0, oauth: 0 };
    let stored = 0;
    for (const e of rounds) {
      for (const p of e.detail.pins) {
        const where = `${e.requestId}: ${p.session_id}`;
        if (p.rewrite === null) {
          // Each mark of the story found its destination this round and holds a prompt size on a model with a weight,
          // so the router's record carries its rewrite.
          assert.deepEqual([p.marked, p.rewrite_window], [false, null], where);
          continue;
        }
        const figure = older.has(e) ? storedRewrite[p.last_input_tokens] : proWeekRewrite[p.last_input_tokens - (p.dropped_thinking_tokens ?? 0)];
        assert.equal(p.rewrite_window, weekOf(p.model), where);
        // The figure the console reads off the cost model reaches its last digit by another order of operations.
        assert.ok(Math.abs(p.rewrite - figure) <= figure * 1e-12, `${where}: ${p.rewrite} against ${figure}`);
        if (older.has(e)) stored += 1;
        // The record names a mark's destination alone.
        if (p.planned !== null) read[s.accounts.find((a) => a.id === p.planned).kind] += 1;
      }
    }
    assert.ok(stored > 0, 'an older round keeps a stored figure, so the Rewrite cell reads both kinds');
    assert.ok(read.api_key > 0 && read.oauth > 0, `the pins the pick read by destination kind: ${JSON.stringify(read)}`);
  });

  test(`a round's unmarked pin carries a rewrite exactly where the pick compared it beside its mark, the three the stretch round's pick left, at ${at}`, async () => {
    const s = await story;
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round');
    // The stretch round of 14:05, 25 minutes before the clock, marks its 0.028 pin at its own position, since no pin
    // covers the target there. At the 0.027 pin's position each pin left covers the rest, so the pick reads those four
    // with the first mark's booking in, marks the 0.027 pin and leaves `7b2e`, the 0.0206 pin and the current pin with
    // the figures it read. No step reads the sliver `3c9a`, and the 11:00 round's pick ends its walk ahead of its
    // current and laptop pins.
    assert.deepEqual(
      rounds.flatMap((e) => e.detail.pins.filter((p) => p.rewrite !== null && p.planned === null).map((p) => [e.at, p.session_id, p.agent_id])),
      [
        [iso(s.now - 25 * 60_000), '5f8a3c1e-9d27-4b6e-8c04-2a7e9b3d1f65', '7b2e'],
        [iso(s.now - 25 * 60_000), 'c8f4a2e7-5d19-4b36-a7c2-1e9b3d6f8a24', 'main'],
        [iso(s.now - 25 * 60_000), '5f8a3c1e-9d27-4b6e-8c04-2a7e9b3d1f65', 'main'],
      ],
    );
  });

  test(`a round's pin row states the runway its destination reads over the windows whose runway ends by their reset, at ${at}`, async () => {
    const round = (await logOf(await story)).find((row) => row.type === 'event' && row.kind === 'shedding_round' && row.requestId === 'req_1eaa9bc1-a18');
    // The 11:00 round's first mark, 22,000 Sonnet tokens at 0.0034 an hour, reads primary with no move booked yet. Its
    // rewrite of 0.00066 and 0.00008 leaves a 5-hour runway of (0.75 - 0.49 - 0.00066) / (0.085 + 0.0034), 2.934 h,
    // which ends past the reset at 11:44, and a 7-day runway of (0.78 - 0.70 - 0.00008) / 0.006, 13.32 h, which ends
    // ahead of its reset 81 h out, so the row states that one.
    const first = round?.detail.pins.find((p) => p.last_input_tokens === 22_000);
    assert.deepEqual([first?.marked, first?.planned, first?.runway_ms], [true, 'primary', 47_952_000]);
  });

  test(`the Fable-week round walks its Fable pin alone, where it is the last pin, leaves its Opus pin off the window and marks nothing for the last pin, at ${at}`, async () => {
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
    // The round runs in a standard request, so the current pin walks the order on the 7-day Fable window, the one pin
    // there and so the last one, which the engine never marks. The Opus pin draws no 7-day Fable window and leaves
    // before the order with no figure.
    const figures = (p) => [p.burn, p.per_rewrite, p.rewrite, p.planned];
    assert.deepEqual(
      [e.accountId, e.detail.window, e.detail.pins.map((p) => p.model), fable.skip, opus.skip, figures(opus), e.detail.burn_sum, e.detail.marks, e.detail.outcome, e.detail.reason],
      ['nightly', 'seven_day_fable', ['claude-fable-5-1', 'claude-opus-5'], 'last_pin', 'off_window', [null, null, null, null], fable.burn, [], 'nothing_to_mark', 'last_pin'],
    );
    assert.deepEqual([r.request.lane.speed, fable.note, fable.burn, fable.rewrite, fable.planned], ['standard', 'current', r.candidates.find((c) => c.accountId === 'nightly').burn.seven_day_fable, null, null]);
    // The 5-hour window opened 19 minutes before, under the hold of twice all's half-hour average.
    assert.deepEqual([value('age_five_hour'), value('hold_five_hour')].map((f) => [f?.value, f?.unit]), [[1_140_000, 'ms'], [3_600_000, 'ms']]);
  });

  test(`a renewal's summary names a week that a pin of the pool on the account draws, whatever the request's model, while that pin stands ahead of its sweep, at ${at}`, async () => {
    const s = await story;
    const rows = (await logOf(s)).filter((row) => row.type === 'decision' && row.seq === 1);
    const first = rows.find((row) => row.requestId === 'req_b4d29e61-a73');
    // An Opus renewal of the laptop conversation on nightly inside the dwell of the 08:30 round. It writes nightly's
    // Fable week at 0.65 with the Fable session's burn of 0.03 an hour, which projects 0.80 over all's weekly ceiling
    // of 0.78, and Opus draws no week. The Fable session's pin was last used at 08:30:09.400, so it stands past its
    // 600 s window from 08:40:09.400, 20,990.6 s before the load, until the sweep of 08:40:40, 20,960 s before it, and
    // the renewal stands inside that span.
    const offset = (row) => Date.parse(row?.at) - s.now;
    assert.ok(offset(first) > -20_990_600 && offset(first) < -20_960_000, `the renewal at ${offset(first)} ms`);
    assert.deepEqual(first.fact, [{ entity: null, key: 'seven_day_fable', unit: 'percent', value: 0.65 }]);
  });

  test(`a renewal's shed line inside a round's dwell reads a week that a pin of the pool on the account draws, whatever the request's model, until the sweep takes that pin, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    const recordOf = (id) => records.find((x) => x.requestId === id && x.seq === 1);
    const first = recordOf('req_b4d29e61-a73');
    const second = recordOf('req_e61f0b48-c25');
    const lineOf = (r) => r?.walk.find((w) => w.step === 'move:shed');
    const value = (r, key) => lineOf(r)?.facts.find((f) => f.key === key)?.value;
    // The Opus renewal of 08:40:20 on nightly, inside the 900 s dwell of the 08:30 round, while the Fable session's pin
    // stands ahead of its sweep: nightly's Fable week projects 0.65 + 0.03 x 5 = 0.80 over all's weekly ceiling of
    // 0.78, and the shared windows read under their lines. The renewal of 08:41:30 stands behind the sweep of 08:40:40,
    // 20,960 s before the load, which takes the one pin that draws the week.
    assert.deepEqual([value(first, 'trigger'), value(first, 'round'), value(first, 'projection_seven_day_fable')], ['seven_day_fable', 'within_dwell', 0.8]);
    assert.ok(Date.parse(second?.request.at) - s.now > -20_960_000, `the second renewal at ${Date.parse(second?.request.at) - s.now} ms`);
    assert.deepEqual(lineOf(second)?.facts.filter((f) => f.key.includes('seven_day_fable')).map((f) => f.key), []);
  });

  test(`a round reads off the window exactly where it walked no pin and left one out off the window that fired, at ${at}`, async () => {
    const s = await story;
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round');
    // The words a pin leaves the round under before the walk, which the router writes on the left-out rows alone.
    const LEFT_OUT = ['fast_lane', 'off_window'];
    for (const e of rounds) {
      const walked = e.detail.pins.filter((p) => !LEFT_OUT.includes(p.skip));
      const offWindow = walked.length === 0 && e.detail.pins.some((p) => p.skip === 'off_window');
      assert.equal(e.detail.reason === 'off_window', offWindow, `${e.requestId}: ${e.detail.reason}`);
    }
    assert.deepEqual(rounds.filter((e) => e.detail.reason === 'off_window').map((e) => [e.requestId, e.accountId, e.detail.outcome]), [], 'no round of the story walks no pin');
  });

  test(`one round of the story marks nothing for the last pin, every pin it walked the last one left, at ${at}`, async () => {
    const s = await story;
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round');
    const LEFT_OUT = ['fast_lane', 'off_window'];
    const last = rounds.filter((e) => e.detail.reason === 'last_pin');
    assert.deepEqual(last.map((e) => [e.requestId, e.detail.outcome]), [['req_3e7a5c19-f84', 'nothing_to_mark']]);
    for (const e of last) {
      const walked = e.detail.pins.filter((p) => !LEFT_OUT.includes(p.skip));
      assert.ok(walked.length > 0 && walked.every((p) => p.skip === 'last_pin'), `${e.requestId}: ${walked.map((p) => p.skip).join(', ')}`);
    }
  });

  test(`every story round records the line its trigger tested, the wall at a 5-hour reset and the window's own ceiling past the five-hour bound and at every horizon of the stretch, at ${at}`, async () => {
    const s = await story;
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round').sort((x, y) => Date.parse(x.at) - Date.parse(y.at));
    // The 08:30 round's 7-day Fable reset lies days past the five-hour bound, so it tests all's weekly ceiling of 0.78
    // (`weeklyCeiling` in mock/pools.ts), and so does the 11:13:40 round's 7-day projection, which the Fable
    // reservation's round reads as a projection round does. The stretch tests work's 5-hour ceiling of 0.95 at every
    // horizon. Primary's two afternoon rounds read their 5-hour reset at 16:44 as the wall.
    assert.deepEqual(
      rounds.map((e) => [e.requestId, e.detail.window, e.detail.trigger, e.detail.ceiling]),
      [
        ['req_3e7a5c19-f84', 'seven_day_fable', 'projection', 0.78],
        ['req_1eaa9bc1-a18', 'five_hour', 'projection', 1],
        ['req_5d1f8c3a-b27', 'seven_day', 'fable_reservation', 0.78],
        ['req_9f2e7c5a-c49', 'five_hour', 'slow_down', 0.95],
        ['req_6e1f3b98-a24', 'five_hour', 'projection', 1],
        ['req_9e1b4c63-a52', 'five_hour', 'projection', 1],
      ],
    );
  });

  test(`every round's marks leave a tenth of its burn sum on its unmarked priced pins unless an unpriced pin stands, and an ended agent's pin burns nothing in it, at ${at}`, async () => {
    const s = await story;
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round').sort((x, y) => Date.parse(x.at) - Date.parse(y.at));
    const round5 = (x) => Math.round(x * 1e5) / 1e5;
    // Each round's burn sum, the priced burn its marks leave and whether an unpriced pin the walk ordered stands
    // unmarked, which the router's guard reads by its missing burn, off the seeds in mock/routingLog.ts, each left
    // figure at least a tenth of its sum: the Fable session's 0.03 of `FABLE_WEEK_BURN`, the 0.0052 and 0.0071
    // `ROUND_PINS` leave of their 0.0299 beside the subagent's pin that burns 0, the 0.002, 0.026, 0.0206 and 0.025
    // `STRETCH_PINS` leave of their 0.1286, the 0.212 and 0.012 of the two priced pins of `NO_RUNWAY_PINS`, and the
    // 0.1165, 0.0301 and 0.0774 of the three priced pins of `MOVED_PIN_ROUND_PINS`, which unpriced pins stand beside in
    // both. The afternoon round lists the pin of the session the mod released whole under `ended` with no burn, and the
    // last round the pins of three subagents whose last turn ended, so each sum is its priced pins' alone. The one
    // priced pin of `RESERVATION_PINS` takes its whole 0.0016 and leaves nothing, which its unpriced current pin
    // allows.
    assert.deepEqual(
      {
        ended: rounds.flatMap((e) => e.detail.pins.filter((p) => p.skip === 'ended').map((p) => [e.requestId, p.burn])),
        left: rounds.map((e) => [
          e.requestId,
          e.detail.burn_sum,
          round5(e.detail.pins.filter((p) => p.burn !== null && !p.marked).reduce((n, p) => n + p.burn, 0)),
          e.detail.pins.some((p) => p.burn === null && !p.marked && (p.skip === null || p.skip === 'unpriced')),
        ]),
      },
      {
        ended: [
          ['req_6e1f3b98-a24', null],
          ['req_9e1b4c63-a52', null],
          ['req_9e1b4c63-a52', null],
          ['req_9e1b4c63-a52', null],
        ],
        left: [
          ['req_3e7a5c19-f84', 0.03, 0.03, false],
          ['req_1eaa9bc1-a18', 0.0299, 0.0123, false],
          ['req_5d1f8c3a-b27', 0.0016, 0, true],
          ['req_9f2e7c5a-c49', 0.1286, 0.0736, false],
          ['req_6e1f3b98-a24', 0.224, 0.224, true],
          ['req_9e1b4c63-a52', 0.224, 0.224, true],
        ],
      },
    );
  });

  test(`a story round's burn sum is the burn its account's row reads on the trigger's window, the demand there for a round the stretch fired and the rate it records for a round the Fable reservation fired, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round');
    // The router's projection sums the burn of the pool's pins on the account and of its bookings in flight, and the
    // round sums the same pins, so the two read one figure where the record holds no booking on the window. A round
    // the Fable reservation fired walks the pins whose model draws no Fable week alone, and the router records their
    // burn as the round's rate, since its test names none.
    const read = rounds.flatMap((e) => {
      const r = records.find((x) => x.requestId === e.requestId && x.request.at === e.at);
      const source = r.candidates.find((c) => c.accountId === e.accountId);
      const window = source.windows[e.detail.window];
      if ((window?.reserved ?? 0) !== 0) return [];
      const burn = e.detail.trigger === 'slow_down' ? window.demand : e.detail.trigger === 'fable_reservation' ? e.detail.excess : source.burn[e.detail.window];
      return [[e.requestId, e.detail.burn_sum, burn]];
    });
    assert.ok(read.length > 0, 'a round whose record holds no booking in flight');
    for (const [id, sum, burn] of read) assert.equal(sum, burn, `${id}: a burn sum of ${sum} against a burn of ${burn}`);
  });

  test(`a shed line reads within_dwell only with a round inside the pool's dwell before it, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round');
    let within = 0;
    for (const r of records) {
      const line = r.walk.find((w) => w.step === 'move:shed');
      if (line?.facts.find((f) => f.key === 'round')?.value !== 'within_dwell') continue;
      within += 1;
      const at = Date.parse(r.request.at);
      const dwell = r.settings.shedDwellSeconds * 1_000;
      const round = rounds.find((e) => e.accountId === r.pin?.accountId && e.poolId === r.request.poolId && Date.parse(e.at) <= at && at - Date.parse(e.at) < dwell);
      assert.ok(round, `${r.requestId}/${r.seq}: within_dwell on ${r.pin?.accountId} with no round in the ${dwell / 1_000} s before it`);
    }
    assert.ok(within > 0, 'a shed line inside a dwell');
  });

  test(`a round leaves a pin out on the fast lane only under window share, where the pin's stored lane is fast, and never the current pin of a standard request, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    const rounds = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'shedding_round');
    const fast = rounds.flatMap((e) => e.detail.pins.filter((p) => p.skip === 'fast_lane').map((p) => [e, p]));
    assert.ok(fast.length > 0, 'the story leaves a fast-lane pin out of a round');
    for (const [e, p] of fast) {
      // Fast mode serves the desktop key's Opus turns alone, so a stored lane reads fast there and nowhere else, and
      // the pin left out carries no burn.
      assert.deepEqual([e.detail.unit, e.detail.window, p.key_id, p.model.includes('opus'), p.burn, p.note], ['window_share', 'five_hour', 'desktop', true, null, null], `${e.requestId}: ${p.session_id}`);
    }
    // The current pin's lane is the request's own.
    for (const e of rounds) {
      const r = records.find((x) => x.requestId === e.requestId && x.walk.some((w) => w.step === 'move:shed' && w.facts.some((f) => f.key === 'round' && f.value === 'ran')));
      const current = e.detail.pins.find((p) => p.note === 'current');
      if (r && current) assert.ok(current.skip !== 'fast_lane' || r.request.lane.speed === 'fast', `${e.requestId}'s current pin on a ${r.request.lane.speed} request`);
    }
  });

  test(`the story writes nightly's failed usage poll, legacy's at the poll's cadence until its refresh failed, reserve's at the cadence to its last attempt, workshop's one, and one stream cut after its head with their detail members, at ${at}`, async () => {
    const s = await story;
    const log = await logOf(s);
    const polls = log.filter((row) => row.type === 'event' && row.kind === 'poll_failed');
    const nightly = polls.filter((p) => p.accountId === 'nightly');
    assert.equal(nightly.length, 1);
    const [poll] = nightly;
    // A 503 is neither the poll's 200, its 429 nor the usage endpoint's 401, so the poll records it as a passing fault.
    assert.deepEqual([poll.accountId, Object.keys(poll.detail).sort(), poll.detail.endpoint, poll.detail.status, poll.detail.transport], ['nightly', ['endpoint', 'error', 'status', 'transport'], 'usage', 503, false]);
    assert.ok(typeof poll.detail.error === 'string' && poll.detail.error.length > 0, 'the error the poll recorded');
    // The router's usage poll asks every five minutes and asks no account whose credential has to be paired again, so
    // legacy failed once per slot from its last clean poll until the refresh that lapsed it, and never since.
    const CADENCE = 5 * 60_000;
    const legacy = s.accounts.find((a) => a.id === 'legacy');
    const clean = Date.parse(legacy.usage.poll.lastCleanAt);
    const lapsed = log.find((row) => row.type === 'event' && row.kind === 'account_state_changed' && row.accountId === 'legacy' && row.detail.to === 'reauth_required');
    const slots = [];
    for (let t = clean + CADENCE; t < Date.parse(lapsed.at); t += CADENCE) slots.push(iso(t));
    const own = polls.filter((p) => p.accountId === 'legacy').sort((x, y) => Date.parse(x.at) - Date.parse(y.at));
    assert.ok(slots.length > 0);
    assert.deepEqual(own.map((p) => p.at), slots, "legacy's failed polls");
    for (const p of own) assert.deepEqual(p.detail, poll.detail, `${p.at} reads the 503 nightly's poll read`);
    // A 503 holds nothing back, so reserve's poll asked at every slot after its last clean read and failed each time,
    // its last attempt among them, and no slot has come since.
    const reserve = s.accounts.find((a) => a.id === 'reserve');
    const lastAttempt = Date.parse(reserve.usage.poll.lastAttemptAt);
    const reserveSlots = [];
    for (let t = Date.parse(reserve.usage.poll.lastCleanAt) + CADENCE; t <= lastAttempt; t += CADENCE) reserveSlots.push(iso(t));
    const reserved = polls.filter((p) => p.accountId === 'reserve').sort((x, y) => Date.parse(x.at) - Date.parse(y.at));
    assert.deepEqual(reserved.map((p) => p.at), reserveSlots, "reserve's failed polls");
    assert.equal(reserved.at(-1)?.at, reserve.usage.poll.lastAttemptAt, 'the last row is the last attempt');
    for (const p of reserved) assert.deepEqual(p.detail, poll.detail, `${p.at} reads the 503 nightly's poll read`);
    // Reserve's six: its seed's last clean read 35 minutes back and its last attempt 5 minutes back. Workshop's one:
    // its seed's last clean read 5 min 40 s back and its last attempt 40 s back, the one slot since.
    assert.equal(polls.length, 1 + slots.length + 6 + 1, 'no account but nightly, legacy, reserve and workshop failed a poll');
    // The poll's last attempt is the slot whose acquisition ran the refresh that failed, after the last 503.
    const last = Date.parse(legacy.usage.poll.lastAttemptAt);
    assert.ok(last <= Date.parse(lapsed.at) && last > Date.parse(own.at(-1).at), `the last attempt at ${legacy.usage.poll.lastAttemptAt}`);
    assert.deepEqual([legacy.usage.poll.lastOutcome, legacy.usage.poll.nextAttemptAt], ['failed', null]);
    const cuts = log.filter((row) => row.type === 'event' && row.kind === 'stream_interrupted');
    assert.equal(cuts.length, 1);
    const [cut] = cuts;
    const r = s.lib.requestById.get(cut.requestId);
    assert.ok(r, `the cut names a request of the story: ${cut.requestId}`);
    const a = r.attempts.at(-1);
    // The row is written at the attempt's completion on the account that served the head, under the request's own
    // session, model and error word.
    assert.deepEqual(
      [cut.requestId, cut.at, cut.accountId, cut.sessionId, cut.model, Object.keys(cut.detail).sort()],
      ['req_5e3c8a41-d27', new Date(Date.parse(a.startedAt) + a.durationMs).toISOString(), a.accountId, r.sessionId, r.canonicalModel, ['attempt', 'bytes_relayed', 'code', 'text', 'upstream_request_id']],
    );
    assert.deepEqual([cut.detail.code, cut.detail.attempt, cut.detail.upstream_request_id], [r.errorCode, a.index + 1, a.upstreamRequestId]);
    // The relay forwarded the head and part of the body before the decode failed, with no output usage.
    assert.deepEqual([cut.detail.bytes_relayed, r.usage.output], [846, 0]);
    assert.ok(typeof cut.detail.text === 'string' && cut.detail.text.length > 0, "the error's own words");
    // A joined value would read as two in the panel, so neither seed's words carries a comma or a semicolon.
    for (const words of [poll.detail.error, cut.detail.text]) assert.ok(!/[,;]/.test(words), words);
  });

  test(`the stream cut's row names the key its request came in under, as the router writes the row's key, at ${at}`, async () => {
    const s = await story;
    const cuts = (await logOf(s)).filter((row) => row.type === 'event' && row.kind === 'stream_interrupted');
    assert.deepEqual(cuts.map((cut) => [cut.requestId, cut.keyId]), cuts.map((cut) => [cut.requestId, s.lib.requestById.get(cut.requestId)?.keyId]));
    assert.deepEqual(cuts.map((cut) => cut.keyId), ['laptop']);
  });

  test(`the story ends a subscribed account's plan and restores it a minute later with the profile's members, in a minute that places nothing on it and polls nothing there, at ${at}`, async () => {
    const s = await story;
    const log = await logOf(s);
    const rows = log.filter((row) => row.type === 'event' && row.accountId === 'primary' && (row.kind === 'plan_ended' || row.kind === 'plan_restored')).sort((x, y) => Date.parse(x.at) - Date.parse(y.at));
    assert.deepEqual(rows.map((e) => [e.kind, e.accountId]), [['plan_ended', 'primary'], ['plan_restored', 'primary']]);
    const [ended, restored] = rows;
    assert.equal(Date.parse(restored.at) - Date.parse(ended.at), 60_000);
    const account = s.accounts.find((a) => a.id === ended.accountId);
    assert.ok(account.kind === 'oauth' && account.subscription !== null, 'an OAuth account with a running subscription');
    // The ledger's two members: a usage 403 made the read due that found no plan, and the next profile read found the
    // account's own plan again.
    assert.deepEqual([ended.detail, restored.detail], [
      { organization_type: 'claude_free', rate_limit_tier: 'default_claude_ai', source: 'usage_403', subscription_status: 'canceled' },
      { organization_type: account.subscription.organizationType, rate_limit_tier: account.subscription.rateLimitTier, source: 'profile', subscription_status: 'active' },
    ]);
    assert.ok(Date.parse(ended.at) >= s.now - DAY && Date.parse(restored.at) < s.now - HOUR, `the day's rows at ${ended.at} and ${restored.at}, before the story hour`);
    // While the flag stood the routing read the account as past its end and its poll sent it no usage request.
    const within = (row) => Date.parse(row.at) >= Date.parse(ended.at) && Date.parse(row.at) <= Date.parse(restored.at);
    const there = log.filter((row) => within(row) && (row.type === 'decision' ? row.accountId === account.id || row.fromAccountId === account.id : row.accountId === account.id && row.kind.startsWith('poll_')));
    assert.deepEqual(there.map((row) => `${row.type} ${row.kind} ${row.at}`), []);
  });

  test(`the story holds one plan-ended account whose saved end passed before a profile read set the flag, with no window, a paused poll and its trend up to its last clean poll, at ${at}`, async () => {
    const s = await story;
    const flagged = s.accounts.filter((a) => a.planEndedAt !== null);
    assert.deepEqual(flagged.map((a) => [a.id, a.kind]), [['sunset', 'oauth']], 'one subscription account carries the flag');
    const [a] = flagged;
    const end = Date.parse(a.subscription.endsAt);
    const flag = Date.parse(a.planEndedAt);
    assert.equal(end, s.now - 3 * DAY, 'the saved end three days back');
    assert.ok(end < flag && flag < s.now, `the flag at ${a.planEndedAt} after the end at ${a.subscription.endsAt} and before now`);
    // The router stores the document a profile read found before it flags the account, the free organization the
    // canceled plan left on Pro's tier word, so the word it resolves and the badge read Free.
    assert.deepEqual([a.subscription.organizationType, a.subscription.rateLimitTier, a.subscription.plan, a.plan], ['claude_free', 'default_claude_ai', 'free', 'Free']);
    assert.deepEqual([a.windows, a.fableWindow], [[], null], 'the route answers no window for a flagged account');
    // The poll pauses at the flag, so the read that set it is the last attempt, and the last clean poll came one poll
    // interval before the end.
    const poll = a.usage.poll;
    assert.deepEqual([poll.lastOutcome, poll.lastError, poll.nextAttemptAt, poll.lastAttemptAt], ['failed', 'plan ended', null, a.planEndedAt]);
    assert.equal(Date.parse(poll.lastCleanAt), end - 5 * 60_000, 'the last clean poll one poll interval before the end');
    const history = await s.api.getQuotaHistory(a.id);
    assert.deepEqual(
      history.map((h) => [h.windowId, h.samples.at(-1)?.at]),
      [
        ['5h', poll.lastCleanAt],
        ['7d', poll.lastCleanAt],
      ],
      'the 5-hour and 7-day trends end at the last clean poll',
    );
    const log = await logOf(s);
    const own = log.filter((row) => row.type === 'event' && row.accountId === a.id && (row.kind === 'plan_ended' || row.kind === 'plan_restored'));
    // The profile read found the free organization the canceled plan left, and no read since found a plan.
    assert.deepEqual(own.map((e) => [e.kind, e.at, e.detail]), [['plan_ended', a.planEndedAt, { organization_type: 'claude_free', rate_limit_tier: 'default_claude_ai', source: 'profile', subscription_status: 'canceled' }]]);
    const decided = log.filter((row) => row.type === 'decision' && (row.accountId === a.id || row.fromAccountId === a.id));
    assert.deepEqual(decided.map((row) => `${row.kind} ${row.at}`), [], 'no decision touches an account in a pool no key routes to');
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
    // A request the relay rejected before any attempt ends at its placement.
    const want = s.requests.filter((r) => r.outcome !== 'succeeded' && booked.get(r.id)).map((r) => {
      const last = r.attempts[r.attempts.length - 1];
      const ended = last === undefined ? Date.parse(booked.get(r.id).request.at) : Date.parse(last.startedAt) + (last.durationMs ?? 0);
      return [r.id, ended, booked.get(r.id).request.attempt, causeOf(r)];
    });
    const read = expired.map((e) => [e.requestId, Date.parse(e.at), e.detail.attempt, e.detail.cause]);
    const order = (x, y) => x[0].localeCompare(y[0]);
    assert.deepEqual(read.sort(order), want.sort(order));
    assert.ok(new Set(read.map((x) => x[3])).size > 1, `the causes read ${[...new Set(read.map((x) => x[3]))].join(', ')}`);
  });

  test(`the story's shed move stamps its pin with the round and its failed-over pin with the cooldown, which the next request's record reads, at ${at}`, async () => {
    const s = await story;
    const records = await decisionsOf(s);
    const keyOf = (r) => `${r.request.keyId}|${r.request.sessionId}|${r.request.agentId}|${r.request.canonicalModel}`;
    const writer = (id, seq) => records.find((r) => r.requestId === id && r.seq === seq);
    // A record reads the pin row from before its decision, so a stamp shows on the pin's next request.
    const next = (w) => records.filter((r) => keyOf(r) === keyOf(w) && Date.parse(r.request.at) > Date.parse(w.request.at)).sort((x, y) => Date.parse(x.request.at) - Date.parse(y.request.at))[0];
    const moved = writer('req_868264bc-611', 1);
    const failedOver = writer('req_b8c3e9d4-e61', 2);
    assert.deepEqual(next(moved)?.pin?.shed, { at: moved.request.at, from: 'secondary', reason: 'round' }, "the shed move's session");
    assert.deepEqual(next(failedOver)?.pin?.shed, { at: failedOver.request.at, from: 'secondary', reason: 'cooldown' }, 'the failed-over session');
    // Every stamp names the move or the failover landing on its pin that wrote it, by the round or the failover's word.
    // A landing writes its row afresh, so the writer is the record of the stamp's instant that wrote the pin off the
    // stamp's source.
    for (const r of records.filter((x) => x.pin?.shed)) {
      const w = records.find((x) => keyOf(x) === keyOf(r) && x.request.at === r.pin.shed.at && x.outcome.fromAccountId === r.pin.shed.from && x.outcome.pinWritten !== null);
      assert.ok(w, `${r.requestId}/${r.seq} reads a stamp no record of its pin wrote`);
      const word = w.outcome.rule === 'shed' ? 'round' : w.outcome.pinEvents.find((e) => e.kind === 'pin_migrated')?.reason;
      assert.equal(r.pin.shed.reason, word, `${r.requestId}/${r.seq} after ${w.requestId}/${w.seq}`);
    }
  });

  test(`no row the story writes whole carries a placement on an attempt's trace, at ${at}`, async () => {
    const s = await story;
    const details = await Promise.all([...s.lib.UNSEEDED_IDS].map((id) => s.api.getRequest(id)));
    const placed = details.filter((d) => d.request.attempts.some((a) => a.selection?.placement)).map((d) => d.request.id);
    assert.deepEqual(placed, [], 'the written rows whose trace carries a placement');
  });

  test(`a release beside a pin created on another account names the idle row, and a placement names a source where it moved a pin, at ${at}`, async () => {
    const s = await story;
    const events = (await logOf(s)).filter((x) => x.type === 'event');
    // A live pin replaced on another account moves, so a release beside a pin created elsewhere is an expiry.
    const created = (e) => events.some((c) => c.kind === 'pin_created' && c.sessionId === e.sessionId && c.agentId === e.agentId && c.at === e.at && c.accountId !== e.accountId);
    const replaced = events.filter((e) => e.kind === 'pin_expired' && created(e));
    assert.ok(replaced.length > 0, 'the story holds a release beside a pin created on another account');
    for (const e of replaced) assert.ok(e.detail.reasons.includes('expired'), `event ${e.id} on ${e.accountId} lists ${e.detail.reasons.join(', ')}`);
    // An idle expiry's placement names no account it left, as a fresh session's does.
    const records = await decisionsOf(s);
    const named = records.filter((r) => r.outcome.kind === 'placed' && (r.outcome.fromAccountId !== null) !== r.outcome.pinEvents.some((e) => e.kind === 'pin_migrated'));
    assert.deepEqual(named.map((r) => `${r.requestId}/${r.seq} from ${r.outcome.fromAccountId}`), [], 'the placements whose source and pin move part');
    // A first attempt's placement that moved a pin moved it for its account's state, as no rule did, so the router
    // writes the new row as created (the seeds `CAUSED_MOVE_ID` and `DELETED_MOVE_ID` in `web/src/mock/requests.ts`),
    // newest first.
    const forced = records.filter((r) => r.outcome.kind === 'placed' && r.request.attempted.length === 0 && r.outcome.pinEvents.some((e) => e.kind === 'pin_migrated'));
    assert.deepEqual(forced.map((r) => [`${r.requestId}/${r.seq}`, r.outcome.pinWritten?.action]), [['req_c7e41d93-a26/1', 'created'], ['req_d71c5a3e-f48/1', 'created']], 'the forced moves and the pin each writes');
  });

  // Catches a source arm that reads the account a request came from with no test of the outcome: the probe's answered
  // attempt came from secondary and left no account, so secondary keeps its obstacle's verdict.
  test(`a record whose outcome names no account it left words no member source, at ${at}`, async () => {
    const s = await story;
    const r = await s.api.getDecision('req_c41d0311-200', 2);
    assert.deepEqual([r.outcome.fromAccountId, r.candidates.find((c) => c.accountId === 'secondary')?.verdict], [null, 'out']);
  });

  test(`a wall reads the newest round on its newest wall's window before it by the round's trigger, at ${at}`, async () => {
    const s = await story;
    const events = (await logOf(s)).filter((row) => row.type === 'event').sort((x, y) => Date.parse(x.at) - Date.parse(y.at));
    const walls = s.attention.filter((i) => i.kind === 'wall_reached');
    for (const item of walls) {
      const newest = events.filter((e) => e.kind === 'window_spent' && e.accountId === item.subject.id && Date.parse(e.at) >= s.now - DAY && Date.parse(e.at) <= s.now).at(-1);
      const wallAt = Date.parse(newest.at);
      const length = newest.detail.window === 'five_hour' ? 5 * HOUR : 7 * DAY;
      const round = events.filter((e) => e.kind === 'shedding_round' && e.accountId === item.subject.id && e.detail.window === newest.detail.window && Date.parse(e.at) <= wallAt && wallAt - Date.parse(e.at) <= length).at(-1);
      // A stretch round's projection is the demand's at a moved horizon, so its wall reads how early it came instead.
      const slowed = round?.detail.trigger === 'slow_down';
      const early = slowed && typeof newest.detail.reset === 'number' ? Math.max(0, newest.detail.reset - wallAt) : null;
      const error = round && !slowed ? Math.round((1 - round.detail.projection) * 1_000) / 1_000 : null;
      assert.deepEqual([item.detail.early_ms, item.detail.projection_error], [early, error], item.subject.id);
    }
    // Nightly's wall follows no 5-hour round and secondary's the projection round at 11:00, so neither reads early, and
    // studio's follows the round its stretch fired at 14:05, so it reads how long before its reset it came.
    assert.deepEqual(walls.map((i) => [i.subject.id, i.detail.early_ms === null ? null : i.detail.early_ms > 0, i.detail.projection_error]), [['nightly', null, null], ['secondary', null, -0.087], ['studio', true, null]]);
  });

  test(`the four routing conditions name the story's subjects in the route's order, studio's slow-down with its spans and the pool's stretch, at ${at}`, async () => {
    const s = await story;
    const kinds = ['wall_reached', 'destination_spent', 'shed_no_runway', 'slowed_long'];
    // A time on the story's clock, which reads 14:30 at the instant the story loads.
    const storyTime = (h, m, sec = 0) => iso(s.now - ((14 - h) * 60 + 30 - m) * 60_000 + sec * 1_000);
    // Each condition dates from its first event of the trailing day: nightly's 5-hour window read spent by the poll at
    // 12:01:05, secondary's by the 429 answered at 14:02:11 and studio's by the poll at 14:20, the least-bad move onto
    // nightly at 11:01:50 within the hour before its wall, all's round at 14:12:30 that found no runway, and studio's
    // oldest slow-down span from 09:12.
    const read = s.attention.filter((i) => kinds.includes(i.kind)).map((i) => [i.kind, i.subject.id, i.since]);
    assert.deepEqual(read, [
      ['wall_reached', 'nightly', storyTime(12, 1, 5)],
      ['wall_reached', 'secondary', storyTime(14, 2, 11)],
      ['wall_reached', 'studio', storyTime(14, 20)],
      ['destination_spent', 'nightly', storyTime(11, 1, 50)],
      ['shed_no_runway', 'all', storyTime(14, 12, 30)],
      ['slowed_long', 'studio', storyTime(9, 12)],
    ]);
    const item = s.attention.find((i) => i.kind === 'slowed_long');
    assert.deepEqual(
      [item.detail.spans, item.detail.open, item.detail.setting, item.detail.setting_values],
      [2, true, ['slow_down_stretch_seconds'], { slow_down_stretch_seconds: s.pools.find((p) => p.id === 'work').slowDownStretchSeconds }],
      "studio's two spans in work, the open one standing, and the pool's stretch",
    );
  });

  test(`the mock's reconciliations name the story's accounts, and of each account's newest windows nightly's 5-hour one alone reads off, at ${at}`, async () => {
    const s = await story;
    const { reconciliations } = await s.api.getAnalyticsReconciliations();
    const oauth = new Set(s.accounts.filter((a) => a.kind === 'oauth').map((a) => a.id));
    for (const r of reconciliations) assert.ok(oauth.has(r.accountId), `${r.accountId}'s window names an OAuth account of the story`);
    // The newest row of each window kind is the summary's line, and the condition reads the newest row whose rise reaches
    // the floor: every newest row is judged, so the two read one row.
    const latest = Object.values(s.lib.meterGroups(reconciliations)).flat().map((g) => g.latest);
    for (const r of latest) assert.ok(s.lib.judged(r), `${r.accountId}'s newest ${r.window} window reaches the floor on its rise and its priced share`);
    const off = latest.filter((r) => s.lib.ratioOff(r));
    assert.deepEqual(off.map((r) => [r.accountId, r.window]), [['nightly', 'five_hour']], "of the newest windows nightly's 5-hour one alone reads off the band");
    const drift = s.attention.filter((i) => i.kind === 'meter_drift');
    assert.deepEqual(
      drift.map((i) => [i.subject.id, i.since, i.detail.windows.map((w) => [w.window, w.ratio, w.observed_rise, w.window_end])]),
      [[off[0].accountId, off[0].windowEnd, [[off[0].window, off[0].ratio, off[0].observedRise, Date.parse(off[0].windowEnd)]]]],
      "the meter drift item names the off row's account, dated from its window's end",
    );
  });

  test(`the mock's reconciliations read as each priced account's windows over the trailing 30 days, at ${at}`, async () => {
    const s = await story;
    const { reconciliations: rows } = await s.api.getAnalyticsReconciliations();
    // The ledger reconciles a window the router priced a request in: legacy stopped a week back, while reserve, spare
    // and the seats priced nothing, sunset sits in a pool no key routes to and an API key reports no plan window.
    const current = ['nightly', 'primary', 'secondary', 'studio'];
    assert.deepEqual([...new Set(rows.map((r) => r.accountId))].sort(), ['legacy', ...current], 'the accounts with rows');
    const order = (x, y) => Date.parse(y.windowEnd) - Date.parse(x.windowEnd) || (x.accountId < y.accountId ? -1 : x.accountId > y.accountId ? 1 : 0) || (x.window < y.window ? -1 : x.window > y.window ? 1 : 0);
    assert.ok(
      rows.every((r, i) => i === 0 || order(rows[i - 1], r) <= 0),
      "the rows run newest first, then by account and window, the route's order",
    );
    for (const r of rows) {
      const end = Date.parse(r.windowEnd);
      const where = `${r.accountId} ${r.window} ending ${r.windowEnd}`;
      assert.equal(r.ratio, r.observedRise / r.predictedShare, `${where}: the ratio is the rise over the share`);
      assert.ok(end >= s.now - 30 * DAY && end < Date.parse(r.computedAt) && Date.parse(r.computedAt) <= s.now, `${where}: inside the 30 days, reconciled after its end and before now`);
      assert.ok(Date.parse(r.firstReadingAt) < Date.parse(r.lastReadingAt) && Date.parse(r.lastReadingAt) <= end, `${where}: its readings inside the window`);
    }
    for (const id of ['legacy', ...current]) {
      const a = s.accounts.find((x) => x.id === id);
      const mine = rows.filter((r) => r.accountId === id);
      assert.deepEqual([...new Set(mine.map((r) => r.window))].sort(), ['five_hour', 'seven_day', ...(a.fableWindow ? ['seven_day_fable'] : [])], `${id} reconciles its 5-hour and 7-day windows, and a Fable week where it reports one`);
      for (const [window, id5, period] of [
        ['five_hour', '5h', 5 * HOUR],
        ['seven_day', '7d', 7 * DAY],
        ['seven_day_fable', null, 7 * DAY],
      ]) {
        const ends = mine.filter((r) => r.window === window).map((r) => Date.parse(r.windowEnd));
        if (ends.length === 0) continue;
        const gaps = ends.slice(1).map((e, i) => ends[i] - e);
        if (window === 'five_hour') assert.ok(ends.length > 10 && gaps.every((g) => g >= period), `${id}'s 5-hour windows end at least 5 hours apart, more of them than an open group lists at first`);
        else assert.ok(ends.length >= 2 && gaps.every((g) => g === period), `${id}'s ${window} windows end a week apart`);
        if (id === 'legacy') assert.ok(ends[0] < s.now - 7 * DAY, "legacy's history stops before its idle week");
        else {
          const live = id5 === null ? a.fableWindow : a.windows.find((w) => w.id === id5);
          assert.equal(ends[0], Date.parse(live.resetsAt) - period, `${id}'s newest ${window} window ends where its current one began`);
        }
      }
    }
    const nightly = rows.filter((r) => r.accountId === 'nightly' && r.window === 'five_hour');
    assert.ok(nightly.filter((r) => s.lib.ratioOff(r)).length >= 3 && s.lib.ratioOff(nightly[0]), "nightly's 5-hour windows read off the band at least three times, the newest among them");
    assert.ok(rows.some((r) => !s.lib.judged(r)), 'a window under the floor shows a ratio the band does not judge');
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
    // The span that opened at 13:48 has stood past work's burn average by the held turn, so the hold applies the full
    // factor, where the span's first read applies none of it.
    const rampOf = (r) => r.walk.find((w) => w.step === 'move:shed')?.facts.find((f) => f.key === 'ramp')?.value;
    assert.deepEqual([rampOf(held), rampOf(record('req_4f2a8d6e-b7a', 1))], [1, 0], "the slowed card's shed line reads the full ramp and the span's first read none");
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
    // The round primary's Fable reservation fired at 11:13:40 is no stretch's either.
    for (const e of rounds.filter((x) => x !== stretched)) assert.deepEqual([e.detail.trigger, e.detail.stretch_ms, e.detail.threshold_ms, e.detail.factor], [e.requestId === 'req_5d1f8c3a-b27' ? 'fable_reservation' : 'projection', null, null, null], `${e.requestId}'s round`);
    assert.equal(rounds.reduce((a, b) => (Date.parse(b.at) > Date.parse(a.at) ? b : a)).detail.trigger, 'projection', 'the newest round is a projection round');
    assert.ok(Date.parse(held.request.at) < Date.parse(stretched.at), 'the hold comes before the round');

    // The failover on all lands with the 429 on the spent 5-hour window that caused it.
    const cause = { reason: 'rate_limit', scope: 'account', window: 'five_hour' };
    const landing = record('req_b8c3e9d4-e61', 2);
    assert.deepEqual([landing.request.poolId, landing.outcome.kind, landing.outcome.fromAccountId, landing.outcome.cause], ['all', 'placed', 'secondary', { cause: null, ...cause, state: null }]);
    const failover = log.find((x) => x.type === 'event' && x.kind === 'pin_migrated' && x.sessionId === landing.request.sessionId && x.at === landing.request.at);
    assert.deepEqual([failover?.detail.reason, failover?.detail.cause], ['cooldown', cause], 'the event carries the cause the landing names');

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
      const stretchMs = Math.max(0, leftMs - ((work.fiveHourCeiling - x.window.utilization) / x.demand) * HOUR);
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

  test(`the stretch round's shed line reads the excess at the fired window's horizon less the threshold, at ${at}`, async () => {
    const s = await story;
    const round = (await decisionsOf(s)).find((r) => r.requestId === 'req_9f2e7c5a-c49' && r.seq === 2);
    const studio = round.candidates.find((c) => c.accountId === 'studio');
    const x = studio.windows.five_hour;
    // Studio has no end, so its horizon is its reset at 15:29, and the round's shed line reads the excess there. The 84
    // minutes from 14:05 less work's 30-minute threshold leave 54, in which a demand of 0.1286 an hour carries the
    // head's 0.86 to 0.02574 past the 0.95 ceiling, 0.0286 an hour.
    assert.equal(x.horizon, x.reset);
    assert.equal(round.walk.find((w) => w.step === 'move:shed').facts.find((f) => f.key === 'excess')?.value, 0.0286);
  });

  test(`the stretch round's event reads the projection its trigger fired on, at the fired window's horizon less the threshold, at ${at}`, async () => {
    const s = await story;
    const round = (await decisionsOf(s)).find((r) => r.requestId === 'req_9f2e7c5a-c49' && r.seq === 2);
    const x = round.candidates.find((c) => c.accountId === 'studio').windows.five_hour;
    const event = (await logOf(s)).find((row) => row.type === 'event' && row.kind === 'shedding_round' && row.requestId === round.requestId);
    // The round at 14:05 reads studio's horizon at its 15:29 reset less work's 30-minute threshold, 14:59, and the 54
    // minutes to there carry the head's 0.86 at the demand of 0.1286 an hour to 0.97574, which the event states to
    // three digits. At the reset itself the same demand would read 1.04.
    assert.deepEqual([event.detail.horizon, event.detail.projection], [Date.parse(x.horizon) - 30 * 60_000, 0.976]);
  });

  test(`the first turn's speed reads 1 where no pin of the account holds a cycle yet, on the shed line and the hold alike, at ${at}`, async () => {
    const s = await story;
    const work = s.lib.poolById.get('work');
    const studio = (await decisionsOf(s))
      .filter((r) => r.request.poolId === 'work' && r.pin?.found === 'live' && r.pin.accountId === 'studio' && r.walk.some((w) => w.step === 'move:shed'))
      .sort((x, y) => Date.parse(x.request.at) - Date.parse(y.request.at) || x.seq - y.seq);
    // A pin holds its cycle from the turn that gave it one while the pin lives, its pool's pin window past that turn.
    const cycled = (r) => studio.some((x) => x.pin.cycleMs !== null && Date.parse(x.request.at) <= Date.parse(r.request.at) && Date.parse(r.request.at) - Date.parse(x.request.at) <= work.affinityTtlSeconds * 1_000);
    const reads = studio.flatMap((r) => {
      const id = `${r.requestId}/${r.seq}`;
      const speed = r.walk.find((w) => w.step === 'move:shed').facts.find((f) => f.key === 'speed')?.value;
      if (speed === undefined) return [];
      if (!cycled(r)) assert.equal(speed, 1, `${id}: a first turn reads ${speed}`);
      if (r.outcome.hold?.reason === 'slow_down') assert.equal(r.outcome.hold.speed, speed, `${id}: the hold's speed`);
      return [[id, cycled(r), speed]];
    });
    // The first turns after the placements of 09:11:24, 10:11 and 13:47:24 and the two of the afternoon read 1, and the
    // turns of the pins the morning's 24 s cycle and the slowed request's 42 s one gave read the slow-down's speed.
    assert.deepEqual(reads, [
      ['req_a3d8f1b6-c25/1', false, 1],
      ['req_2b9e4c7f-d36/1', false, 1],
      ['req_8e1c4a6d-b52/1', true, 0.4],
      ['req_8e1c4a6d-b52/2', true, 0.35],
      ['req_f6a1d3e8-e47/1', true, 1],
      ['req_8d4b7f2c-f58/1', true, 1],
      ['req_4f2a8d6e-b7a/1', false, 1],
      ['req_4a9c3e7b-a27/1', false, 1],
      ['req_e1b6d4f8-b38/1', false, 1],
      ['req_9f2e7c5a-c49/1', true, 0.5],
      ['req_9f2e7c5a-c49/2', true, 0.5],
    ]);
  });

  test(`every plan-funded row carries its Pro-week figure and its shares on the windows its account's set has and its model's weeks, null on a model without a weight of its own, and no API key, overage or fast row carries one, at ${at}`, async () => {
    const s = await story;
    // The cost model weighs every model the story's plan rows run on but Sonnet 4.5, which prices nothing.
    const pricedOn = { 'claude-fable-5-1': true, 'claude-haiku-4-5-20251001': true, 'claude-opus-5': true, 'claude-opus-5-5': true, 'claude-sonnet-4-5': false, 'claude-sonnet-4-6': true, 'claude-sonnet-5': true, 'claude-sonnet-5-5': true };
    let priced = 0;
    let unpriced = 0;
    const keys = new Map();
    for (const r of s.requests) {
      const serving = r.attempts[r.attempts.length - 1] ?? null;
      const funding = s.lib.fundingOf(serving);
      if (funding !== 'plan') {
        assert.equal(r.costShares, null, `${r.id} is ${funding}-funded and carries ${JSON.stringify(r.costShares)}`);
        assert.equal(r.costEquivalent, null, `${r.id} is ${funding}-funded and carries ${JSON.stringify(r.costEquivalent)}`);
        continue;
      }
      assert.ok(r.costShares !== null && typeof r.costShares === 'object', `${r.id} is plan-funded and carries no share map`);
      if (r.classifier !== null) continue;
      assert.deepEqual(Object.keys(r.costShares), shareWindowsOf(s, serving.accountId, r.canonicalModel), `${r.id} on ${serving.accountId} keys its shares`);
      keys.set(`${serving.accountId} ${r.canonicalModel}`, Object.keys(r.costShares));
      if (pricedOn[r.canonicalModel]) {
        assert.ok(typeof r.costEquivalent === 'number' && Object.values(r.costShares).every((v) => typeof v === 'number'), `${r.id} on ${r.canonicalModel} reads ${JSON.stringify([r.costShares, r.costEquivalent])}`);
        priced += 1;
      } else {
        assert.ok(r.costEquivalent === null && Object.values(r.costShares).every((v) => v === null), `${r.id} on ${r.canonicalModel} prices nothing`);
        unpriced += 1;
      }
    }
    assert.ok(priced > 0 && unpriced > 0, `the story prices ${priced} rows and leaves ${unpriced} unpriced`);
    assert.deepEqual([keys.get('workshop claude-fable-5-1'), keys.get('primary claude-fable-5-1')], [['seven_day', 'seven_day_fable'], ['five_hour', 'seven_day', 'seven_day_fable']], 'the approved Ultra prices no 5-hour share, the unapproved one does');
  });

  test(`the story's two classifier calls of one session read zero shares and zero dollars with the reason and book nothing, at ${at}`, async () => {
    const s = await story;
    const calls = s.requests.filter((r) => r.classifier === 'client');
    assert.equal(calls.length, 2, "two rows are the client's classifier calls");
    assert.equal(new Set(calls.map((r) => `${r.keyId}|${r.sessionId}`)).size, 1, 'of one session under one key');
    for (const r of calls) {
      assert.deepEqual([r.requestClass, r.shareReason, s.lib.fundingOf(r.attempts.at(-1))], ['auxiliary', 'client_classifier', 'plan'], r.id);
      const shares = Object.values(r.costShares ?? {});
      assert.ok(shares.length > 0 && [...shares, r.costEquivalent].every((v) => v === 0), `${r.id} reads ${JSON.stringify([r.costShares, r.costEquivalent])}`);
      assert.equal(r.costUsd, 0, `${r.id} reads its dollars`);
    }
    // The router answers a zero split for a free call, so the rollup part that files the two prices nothing either.
    const parts = s.allRollups.filter((r) => r.classifier === 'client');
    assert.deepEqual(parts.map((r) => r.cost), [{ cacheRead: 0, cacheWrite: 0, input: 0, output: 0 }], 'the one rollup part of the client classifier calls');
    assert.deepEqual(s.requests.filter((r) => r.shareReason !== null).map((r) => r.id).sort(), calls.map((r) => r.id).sort(), 'the reason stands on the classifier calls alone');
    // A classifier selection books nothing, as a fast one does, so its walk ends at a reserve station that booked none.
    const records = (await decisionsOf(s)).filter((rec) => calls.some((r) => r.id === rec.requestId));
    assert.equal(records.length, calls.length, 'each call has its decision');
    for (const rec of records) assert.deepEqual([rec.outcome.kind, rec.outcome.reservation, rec.walk.at(-1)?.step, rec.walk.at(-1)?.verdict], ['placed', null, 'reserve', 'none'], rec.requestId);
  });

  test(`no classifier call of the story holds a pin: its decision names the reason, and no pin event and no round's pin stands on its pin key, at ${at}`, async () => {
    const s = await story;
    const calls = s.requests.filter((r) => r.classifier !== null);
    const keys = new Set(calls.map(pinOf));
    const records = (await decisionsOf(s)).filter((rec) => calls.some((r) => r.id === rec.requestId));
    const events = (await logOf(s)).filter((row) => row.type === 'event');
    const roundPins = events.filter((e) => e.kind === 'shedding_round').flatMap((e) => e.detail.pins.map((p) => ({ pin: { agentId: p.agent_id, keyId: p.key_id, model: p.model, sessionId: p.session_id }, round: e.requestId })));
    assert.deepEqual(
      {
        calls: calls.map((r) => r.classifier).sort(),
        decisions: records.map((rec) => [rec.requestId, rec.pin, rec.pinReason]).sort(),
        pinEvents: events.filter((e) => e.kind.startsWith('pin_') && keys.has(pinOf(e))).map((e) => `${e.kind} at ${e.at}`),
        roundPins: roundPins.filter((x) => keys.has(pinOf(x.pin))).map((x) => `${x.round}: ${x.pin.sessionId}`),
      },
      {
        calls: ['client', 'client', 'client_off'],
        decisions: [['req_4d9a1e6b-c20', null, 'classifier'], ['req_63a8d2f5-b17', null, 'classifier'], ['req_7c2e9f41-d03', null, 'classifier']],
        pinEvents: [],
        roundPins: [],
      },
    );
  });

  // The router places a classifier call on its judged turn's account or that turn's pin's without ranking, and names
  // `classifier` as the pick's reason there alone (`judged_candidate` in `crates/routing/src/select.rs`). A call its
  // Pin step leaves to the strategy is ranked as any other request.
  test(`a classifier call's pick names classifier as its reason exactly where its Pin step directs the call, at ${at}`, async () => {
    const s = await story;
    const calls = new Set(s.requests.filter((r) => r.classifier !== null).map((r) => r.id));
    const read = (await decisionsOf(s))
      .filter((rec) => calls.has(rec.requestId))
      .map((rec) => ({
        direction: rec.walk.find((w) => w.step === 'pin')?.facts.find((f) => f.key === 'direction')?.value,
        id: rec.requestId,
        named: rec.walk.find((w) => w.step === 'pick')?.facts.some((f) => f.key === 'reason' && f.value === 'classifier') ?? false,
      }));
    const directed = read.filter((x) => ['pin_account', 'turn_account'].includes(x.direction));
    const left = read.filter((x) => !directed.includes(x));
    assert.ok(directed.length > 0 && left.length > 0, `the story's classifier calls read ${JSON.stringify(read)}`);
    assert.deepEqual(
      { directed: directed.filter((x) => !x.named).map((x) => x.id), left: left.filter((x) => x.named).map((x) => `${x.id} (${x.direction})`) },
      { directed: [], left: [] },
    );
  });

  test(`the story's contract refusal names the missing item on a Claude Code row no account served, at ${at}`, async () => {
    const s = await story;
    const refusals = s.requests.filter((r) => r.errorCode === 'client_contract');
    assert.equal(refusals.length, 1, 'one row is a contract refusal');
    const [refused] = refusals;
    assert.deepEqual([refused.clientContract, refused.status, refused.outcome, refused.attempts.length, refused.requestClass, refused.userAgent?.startsWith('claude-cli/')], ['class_header', 403, 'rejected', 0, null, true]);
    assert.deepEqual(s.requests.filter((r) => r.clientContract !== null).map((r) => r.id), [refused.id], 'the item stands on the refusal alone');
  });

  test(`the story's requests read their dollars at the cells of the wire's price entry in force at their instant, none on a model the entry lacks, and an hour before the entry in force reads the earlier entry's, at ${at}`, async () => {
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
    // The story's rows run under the latest entry, each priced to the hundredth of a cent: the fleet's first turn on
    // Sonnet 5.5, 2,900 input, 12,400 cache read, 6,200 5-minute write and 1,120 output tokens at $2, $0.10, $2.50 and
    // $10 a million, its Opus 5.5 turn, 2,400, 20,000, 2,200 and 1,540 at $4, $0.20, $5 and $20, and the slowed
    // session's first turn on Sonnet 5.5, 2,400 input, 64,000 1-hour write and 910 output at $2, $4 and $10. The ci
    // key's three refusals named no model, which the entry lacks.
    const named = ['req_c94d1e07-b35', 'req_3f6a9c21-d48', 'req_c5d8e2b7-e95', 'req_6c3a9e27-5b1', 'req_9b4f1c62-a07', 'req_2d8e5b13-f40'];
    const costOf = (id) => s.requests.find((r) => r.id === id)?.costUsd;
    assert.deepEqual(named.map(costOf), [0.0337, 0.0554, 0.2699, 0, 0, 0]);
    // The other rows mostly draw their tokens, so each reads the cells of the entry in force at its instant.
    let priced = 0;
    // The classifier case reads the free calls' zero.
    for (const r of s.requests.filter((x) => !named.includes(x.id) && x.shareReason === null)) {
      const entry = s.lib.revisionAt(revisions, r.at);
      const want = dollarsAt(entry, r.model, r.usage, r.attempts[r.attempts.length - 1]?.response.speed === 'fast');
      if (want === null) {
        assert.equal(r.costUsd, 0, `${r.id} on ${r.model}, which the entry of ${entry?.effective} lacks, reads ${r.costUsd}`);
        continue;
      }
      // A row's dollars are rounded to the hundredth of a cent.
      assert.ok(Math.abs(r.costUsd - want) <= 0.00005 + 1e-12, `${r.id} on ${r.model} at ${r.at} reads ${r.costUsd} against the entry of ${entry?.effective}'s ${want}`);
      priced += 1;
    }
    assert.ok(priced > 0, 'the story prices its drawn rows');
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

  test(`the breakdown by model's Pro-week figures are the sums of the account rows' under the same filters, null where one is, at ${at}`, async () => {
    const s = await story;
    const range = { from: iso(s.now - DAY), to: iso(s.now) };
    const byModel = await s.api.getAnalyticsBreakdown({ ...range, groupBy: 'model' });
    let compared = 0;
    for (const g of byModel) {
      const accounts = await s.api.getAnalyticsBreakdown({ ...range, groupBy: 'account', models: [g.key] });
      const figures = accounts.map((a) => a.costEquivalent);
      if (figures.some((v) => v === null)) {
        assert.equal(g.costEquivalent, null, `${g.key} reads ${g.costEquivalent} where an account priced nothing`);
        continue;
      }
      const summed = sum(figures, (v) => v);
      assert.ok(typeof g.costEquivalent === 'number' && Math.abs(g.costEquivalent - summed) <= 1e-9 * Math.max(1, summed), `${g.key} reads ${g.costEquivalent} against its accounts' ${summed}`);
      if (summed > 0) compared += 1;
    }
    assert.ok(compared > 0, 'a model carries a figure');
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
    for (const g of byModel) assert.ok(g.costShares !== null, `nightly's ${g.key} reads its shares`);
    const week = sum(byModel, (g) => g.costShares?.seven_day ?? 0);
    assert.ok(own.costShares.seven_day > 0 && Math.abs(week - own.costShares.seven_day) <= 1e-9, `the models' 7-day shares ${week} against nightly's ${own.costShares.seven_day}`);
    const two = await s.api.getAnalyticsBreakdown({ ...range, accountIds: ['nightly', 'primary'], groupBy: 'model' });
    assert.ok(two.length > 0);
    for (const g of two) assert.equal(g.costShares, null, `two accounts' ${g.key}`);
  });

  test(`the dashboard's day reads its billed dollars and the plan rows' summed Pro-week figure, null where a plan row priced nothing, at ${at}`, async () => {
    const s = await story;
    const figureOf = (rows) => {
      const plan = rows.filter((r) => planPaid(s, r));
      return plan.some((r) => requestTokens(r) > 0 && r.costEquivalent === null) ? null : sum(plan, (r) => r.costEquivalent ?? 0);
    };
    assert.equal(s.overview.day24h.equivalent, figureOf(s.totalled), `the day reads ${s.overview.day24h.equivalent}`);
    // The billed rows alone, a billed probe's among them, as the dashboard's route sums its day.
    const billed = s.requests.filter((r) => Date.parse(r.at) >= s.totalledFrom && !planPaid(s, r));
    assert.ok(near(s.overview.day24h.billedUsd, sum(billed, (r) => r.costUsd)), 'the billed rows alone');
    for (const h of s.overview.hourly) {
      const rows = s.totalled.filter((r) => startOfHour(Date.parse(r.at)) === Date.parse(h.at));
      const want = figureOf(rows);
      assert.ok(want === null ? h.equivalent === null : near(h.equivalent, want), `${h.at} reads ${h.equivalent} against ${want}`);
    }
    assert.ok(s.overview.hourly.some((h) => h.equivalent === null) && s.overview.hourly.some((h) => typeof h.equivalent === 'number' && h.equivalent > 0), 'an hour of Sonnet 4.5 reads null and a priced one its figure');
  });

  test(`every pinned plan session reads its burn on the window its account's set names, and a pin on a model without a weight of its own prices no share, its burn and its Pro-week figure null, at ${at}`, async () => {
    const s = await story;
    // The cost model weighs every model the story's plan pins run on but Sonnet 4.5, which prices nothing.
    const pricedOn = { 'claude-fable-5-1': true, 'claude-haiku-4-5-20251001': true, 'claude-opus-5': true, 'claude-opus-5-5': true, 'claude-sonnet-4-5': false, 'claude-sonnet-5': true, 'claude-sonnet-5-5': true };
    const pinned = s.sessions.filter((x) => x.pinned && x.burnUnit === 'window_share');
    assert.ok(pinned.length > 0);
    let unpriced = 0;
    for (const x of pinned) {
      const windows = s.accounts.find((a) => a.id === x.accountId)?.windows.map((w) => w.id) ?? [];
      assert.equal(x.burnWindow, windows.includes('5h') || !windows.includes('7d') ? 'five_hour' : 'seven_day', `${x.sessionId} on ${x.accountId}`);
      if (!pricedOn[x.model]) {
        assert.deepEqual([x.burn, x.burnEquivalent], [null, null], `${x.sessionId} on ${x.model} prices no share`);
        unpriced += 1;
        continue;
      }
      assert.ok(typeof x.burn === 'number', `${x.sessionId} on ${x.model} burns ${x.burn}`);
    }
    assert.ok(unpriced > 0, "a pin on the story's Sonnet 4.5 prices no share");
    assert.deepEqual(pinned.filter((x) => x.accountId === 'workshop').map((x) => x.burnWindow), ['seven_day'], "the approved Ultra's pin burns its 7-day window");
    assert.ok(s.sessions.filter((x) => x.burnUnit !== 'window_share').every((x) => x.burnWindow === null), 'a session off plan quota names no window');
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

  test(`the stream cut after its head reads incomplete under the cut's code, its account's breakdown counts that code, and its detail carries the cut, at ${at}`, async () => {
    const s = await story;
    const row = s.requests.find((r) => r.id === 'req_5e3c8a41-d27');
    const relayed = s.requests.find((r) => r.id === 'req_5e2f7a19-b03');
    assert.ok(row && relayed, 'the story holds both undecodable rows');
    // The head went out under its 200, so the router records the request incomplete with the cut's code and its one
    // attempt as an upstream error.
    assert.deepEqual(
      [row.keyId, row.accountId, row.status, row.errorCode, row.outcome, row.attempts.map((a) => [a.accountId, a.upstreamStatus, a.outcome])],
      ['laptop', 'primary', 200, 'stream_undecodable', 'incomplete', [['primary', 200, 'upstream_error']]],
    );
    assert.equal(row.sessionId, relayed.sessionId, "the 502 row's session holds both");
    // A cold request whose input went up, with no output usage back.
    assert.deepEqual([row.usage.input > 0, row.usage.cacheRead, row.usage.cacheWrite5m + row.usage.cacheWrite1h, row.usage.output], [true, 0, 0, 0]);
    // The ledger counts a request's own code beside its outcome, so the breakdown files the row under the cut's code.
    const groups = await s.api.getAnalyticsBreakdown({ from: iso(s.now - DAY), groupBy: 'account', to: iso(s.now) });
    const filed = s.counted.filter((r) => r.accountId === 'primary' && r.errorCode === 'stream_undecodable').map((r) => r.id);
    assert.deepEqual([filed.includes(row.id), groups.find((g) => g.key === 'primary')?.errors.stream_undecodable], [true, filed.length], "primary's entry counts it");
    // An incomplete request files under the code it ended on, so stream_incomplete counts the rows that carry it alone.
    const ended = s.counted.filter((r) => r.accountId === 'primary' && r.errorCode === 'stream_incomplete').length;
    assert.ok(ended > 0, 'primary served a stream that closed without its end');
    assert.equal(groups.find((g) => g.key === 'primary')?.errors.stream_incomplete ?? 0, ended);
    assert.deepEqual((await decisionsOf(s)).filter((r) => r.requestId === row.id).map((r) => [r.outcome.kind, r.outcome.accountId]), [['renewed', 'primary']], 'its decision renews the pin the 502 row placed');
    // The route lifts the member off the newest cut row of the request, and no other request carries one.
    const [cut] = (await logOf(s)).filter((e) => e.type === 'event' && e.kind === 'stream_interrupted' && e.requestId === row.id);
    const detail = await s.api.getRequest(row.id);
    assert.deepEqual(detail.streamCut, { at: cut.at, attempt: cut.detail.attempt, bytesRelayed: cut.detail.bytes_relayed, code: row.errorCode, text: cut.detail.text, upstreamRequestId: cut.detail.upstream_request_id });
    const carrying = (await Promise.all(s.requests.map((r) => s.api.getRequest(r.id)))).filter((d) => d.streamCut !== null).map((d) => d.request.id);
    assert.deepEqual(carrying, [row.id]);
  });

  test(`an incomplete stream's attempt names no stop reason and files as an upstream error, while a completed answer ends its turn, at ${at}`, async () => {
    const s = await story;
    // Neither stream reached its message_delta, which carries the stop reason, and the ledger files an incomplete
    // request's attempt as an upstream error.
    const served = (id) => {
      const a = s.lib.requestById.get(id).attempts.at(-1);
      return [id, a.upstreamStatus, a.outcome, a.response.stopReason];
    };
    assert.deepEqual(['req_5e3c8a41-d27', 'req_31f0b7c2-6e1'].map(served), [
      ['req_5e3c8a41-d27', 200, 'upstream_error', null],
      ['req_31f0b7c2-6e1', 200, 'upstream_error', null],
    ]);
    const completed = s.requests.filter((r) => s.lib.STORY_IDS.has(r.id) && r.outcome === 'succeeded' && r.attempts.at(-1)?.upstreamStatus === 200);
    assert.ok(completed.length > 0, 'the story completes answers');
    assert.deepEqual(completed.filter((r) => r.attempts.at(-1).response.stopReason !== 'end_turn').map((r) => r.id), []);
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

  test(`the story's lanes file their funding by the answer that served them, an API key account's history under billed and a request no attempt served under plan, at ${at}`, async () => {
    const s = await story;
    const laneOf = (hour, keyId, poolId, accountId, model, kind, sessionId) => [hour, keyId, poolId, accountId, model, kind, sessionId].join('|');
    const filed = new Map();
    for (const r of s.allRollups) {
      const lane = laneOf(r.hour, r.keyId, r.poolId, r.accountId, r.model, r.kind, r.sessionId);
      filed.set(lane, new Set([...(filed.get(lane) ?? []), r.funding]));
    }
    const fundingOf = (id) => {
      const r = s.requests.find((x) => x.id === id);
      return [...(filed.get(laneOf(iso(startOfHour(Date.parse(r.at))), r.keyId, r.poolId, r.accountId ?? '-', r.canonicalModel, r.kind, r.sessionId)) ?? [])];
    };
    // The story's own lanes, each filed by the answer that served it: billed for the Overflow workspace's answer to the
    // rejected Sonnet 4.6 turn, usage_based for the fleet's first turn on the seat, fast for the desktop's Opus turn
    // that nightly answered fast while in overage, overage for the laptop's Haiku turn nightly answered in overage, and
    // plan for primary's answer at 14:03.
    assert.deepEqual(['req_7015b657-290', 'req_c94d1e07-b35', 'req_4b0e9d17-c31', 'req_2e7c90a1-6d4', 'req_7a2d4c18-e95'].map(fundingOf), [['billed'], ['usage_based'], ['fast'], ['overage'], ['plan']]);
    // The history past the request log files an API key account's hours under billed in every range the console
    // offers.
    const apiKeys = new Set(s.accounts.filter((a) => a.kind === 'api_key').map((a) => a.id));
    const history = s.allRollups.filter((r) => apiKeys.has(r.accountId) && Date.parse(r.hour) < s.dayFrom);
    assert.ok(history.length > 0, 'the story holds history rows of an API key account');
    for (const r of history) assert.equal(r.funding, 'billed', `the history row of ${r.hour} on ${r.accountId} files under billed`);
    const unserved = s.requests.filter((r) => r.attempts.length === 0 && r.shape.requestedSpeed === 'fast');
    if (unserved.length > 0) {
      const lane = laneOf(iso(startOfHour(Date.parse(unserved[0].at))), unserved[0].keyId, unserved[0].poolId, '-', unserved[0].canonicalModel, unserved[0].kind, unserved[0].sessionId);
      assert.ok(filed.get(lane)?.has('plan'), `${unserved[0].id} asked for fast and no attempt served it, so its rollup files under plan`);
    }
  });

  test(`an answered row names who put the diagnostics member on its body exactly when its serving attempt reads a word other than not_requested, at ${at}`, async () => {
    const s = await story;
    const diagnosed = (reason) => reason === 'none' || reason === 'not_returned' || reason === 'pending' || reason === 'previous_message_not_found' || reason === 'unavailable' || reason.endsWith('_changed');
    for (const r of s.requests) {
      const reason = r.attempts.at(-1)?.cacheMissReason ?? null;
      // An attempt with no answer files no word, whatever its body carried, while a token count and a request no
      // attempt served carry no member at all.
      if (reason === null) {
        if (r.kind === 'count_tokens' || r.attempts.length === 0) assert.equal(r.diagnosticsSource, null, `${r.id} sent no member and names a source`);
        continue;
      }
      if (diagnosed(reason)) assert.ok(r.diagnosticsSource === 'client' || r.diagnosticsSource === 'router', `${r.id} reads ${reason} and names ${r.diagnosticsSource}`);
      else assert.equal(r.diagnosticsSource, null, `${r.id} reads ${reason} and names a source`);
    }
    assert.ok(s.requests.some((r) => r.requestClass === null), 'a row sent no request class');
    assert.ok(s.requests.some((r) => r.shape.effort === null), 'a row asked for no effort');
  });

  test(`a row names who sent the thinking token wherever its serving attempt reports the dropped blocks, at ${at}`, async () => {
    const s = await story;
    // The router writes a reported map only beside the token's sender, while it can write a sender beside a null map,
    // so a row without a report is read for nothing.
    const reported = s.requests.filter((r) => (r.attempts.at(-1)?.response.inputTransformations ?? null) !== null);
    assert.ok(reported.length > 0, 'a request reports its dropped blocks');
    for (const r of reported) assert.ok(r.thinkingDiagnosticsSource === 'client' || r.thinkingDiagnosticsSource === 'router', `${r.id} reports its dropped blocks and names ${r.thinkingDiagnosticsSource}`);
  });

  test(`an hour with no rows counts no dropped block on a key that sends no thinking token, and each answered request once on a key that sends one, at ${at}`, async () => {
    const s = await story;
    // The keys whose client sends the token or whose pool adds it, and the keys whose rows carry none.
    const sends = ['ci', 'desktop', 'laptop'];
    const sendsNone = ['batch', 'bench', 'fleet', 'notebook'];
    const hours = new Set(s.requests.map((r) => `${iso(startOfHour(Date.parse(r.at)))}|${r.keyId}`));
    const silent = s.allRollups.filter((r) => sendsNone.includes(r.keyId) && !hours.has(`${r.hour}|${r.keyId}`));
    // The hours ahead of the request log's first one hold no row of their own.
    const drawn = s.allRollups.filter((r) => sends.includes(r.keyId) && Date.parse(r.hour) < s.totalledFrom && r.requests.ok > 0);
    assert.ok(silent.length > 0 && drawn.length > 0, 'the story holds hours with no rows of both kinds of key');
    for (const r of silent) assert.deepEqual([Object.entries(r.inputTransformations), r.inputTransformationsMoved, r.inputTransformationsNone, r.inputTransformationsReported], [[], 0, 0, 0], `${r.hour} on ${r.keyId}, a key that sends no token, counts a dropped block or a report`);
    for (const r of drawn) {
      assert.equal(sum(Object.values(r.inputTransformations), (n) => n) + r.inputTransformationsNone, r.requests.ok, `${r.hour} on ${r.keyId} does not count each of its answered requests once`);
      assert.equal(r.inputTransformationsReported, r.requests.ok, `${r.hour} on ${r.keyId} does not count each of its answered requests as reported`);
    }
  });

  test(`the story holds one request whose comparison was still running, landed on its agent's last account under the client's member, at ${at}`, async () => {
    const s = await story;
    const pending = s.requests.filter((r) => r.attempts.at(-1)?.cacheMissReason === 'pending');
    assert.deepEqual(pending.map((r) => r.id), ['req_6e1f3b98-a24'], 'the renewal that follows its placement on primary');
    const [row] = pending;
    assert.equal(s.accounts.find((a) => a.id === row.attempts.at(-1).accountId)?.kind, 'oauth', 'an OAuth account asks for the comparison');
    assert.equal(row.diagnosticsSource, 'client', 'the body carried the member');
  });

  // The Analytics page's default range, the last day up to the story's instant, which `breakdownOf` reads as the route
  // does: from the hour holding the range's start up to the hour ceiling of its end.
  const defaultRange = (s) => ({ from: iso(s.now - DAY), to: iso(s.now) });

  test(`every reason the default range's totals count lists a request under it in the window its link writes, at ${at}`, async () => {
    const s = await story;
    const [totals] = s.lib.breakdownOf(s.allRollups, { ...defaultRange(s), groupBy: 'all', includeProbes: false });
    const counted = Object.entries(totals.cacheMissReasons).filter(([, n]) => n > 0);
    assert.ok(counted.length > 1, 'the range counts several reasons');
    const filters = { accountIds: [], clientPattern: '', includeProbes: false, keyIds: [], models: [], poolIds: [], sessionId: null, userAgentPattern: '' };
    for (const [reason, count] of counted) {
      const { href } = s.lib.requestsLink({ count, filters, narrow: { cacheMissReasons: [reason] }, window: { exact: false, fromMs: s.now - DAY, nowMs: s.now, toMs: s.now } });
      const params = new URL(href, 'http://console.test').searchParams;
      const { requests } = await s.api.listRequests({ cacheMissReasons: params.get('cache_miss_reasons').split(','), from: params.get('from'), limit: 1, to: params.get('to') });
      assert.ok(requests.length > 0, `${reason} lists no request from ${params.get('from')} to ${params.get('to')}`);
    }
  });

  test(`the default range's effort, request class, classifier, client and user agent breakdowns count the story's rows per folded value, at ${at}`, async () => {
    const s = await story;
    const rows = s.asked(s.requests.filter((r) => r.kind !== 'probe' && startOfHour(Date.parse(r.at)) >= startOfHour(s.now - DAY)));
    const folds = {
      classifier: (r) => s.lib.foldClassifier(r.classifier),
      client: (r) => r.client ?? '-',
      effort: (r) => s.lib.foldEffort(r.shape.effort),
      request_class: (r) => s.lib.foldRequestClass(r.requestClass),
      user_agent: (r) => (r.userAgent === null ? '-' : s.lib.foldUserAgent(r.userAgent)),
    };
    for (const [groupBy, fold] of Object.entries(folds)) {
      const want = {};
      for (const r of rows) want[fold(r)] = (want[fold(r)] ?? 0) + 1;
      // A group of probe rows alone counts no request while probes are left out, as the route answers it.
      const groups = s.lib.breakdownOf(s.allRollups, { ...defaultRange(s), groupBy, includeProbes: false }).filter((g) => g.requests > 0);
      assert.deepEqual(Object.fromEntries(groups.map((g) => [g.key, g.requests]).sort()), Object.fromEntries(Object.entries(want).sort()), groupBy);
    }
    // A client classifier call takes no plan share, so its group's cost, cache, saving and split figures read zero,
    // its account's shares and the Pro-week figures alike.
    const [free] = s.lib.breakdownOf(s.allRollups, { ...defaultRange(s), accountIds: ['primary'], groupBy: 'classifier', includeProbes: false }).filter((g) => g.key === 'client');
    const figures = [free?.costEquivalent, free?.cacheEquivalent?.read, free?.cacheEquivalent?.write, free?.savingEquivalent, ...Object.values(free?.equivalentSplit ?? {}), ...Object.values(free?.costShares ?? {})];
    assert.ok(free?.requests === 2 && figures.every((v) => v === 0), `the client group reads ${JSON.stringify(free && { cacheEquivalent: free.cacheEquivalent, costEquivalent: free.costEquivalent, costShares: free.costShares, equivalentSplit: free.equivalentSplit, requests: free.requests, savingEquivalent: free.savingEquivalent })}`);
  });

  test(`the story holds two laptop requests the router's own stop cut, one past its head and one before any, at ${at}`, async () => {
    const s = await story;
    const cut = s.requests.filter((r) => r.errorCode === 'router_shutdown');
    assert.equal(cut.length, 2, `the rows under the code: ${cut.map((r) => r.id).join(', ') || 'none'}`);
    for (const r of cut) {
      assert.deepEqual([r.outcome, r.keyId, r.kind, r.transport, r.attempts.length], ['canceled', 'laptop', 'inference', 'sse', 1], `${r.id} is a laptop stream of one attempt filed canceled`);
      // The routing log's records of its last hour are written by hand and name no cut request.
      assert.ok(Date.parse(r.at) < s.now - HOUR, `${r.id} lands before the routing log's hand-written hour`);
      // A cut stream never reached its message_delta, so no attempt reads a stop reason.
      for (const a of r.attempts) assert.deepEqual([a.outcome, a.response.stopReason], ['canceled', null], `${r.id}'s attempt ${a.index + 1}`);
    }
    assert.equal(cut.filter((r) => r.status === 200 && r.ttfbMs !== null).length, 1, 'one cut after its head went out');
    const headless = cut.filter((r) => r.status === null);
    assert.equal(headless.length, 1, 'one cut before any head');
    const [row] = headless;
    assert.deepEqual([row.ttfbMs, row.ttftMs, row.capturedBodies], [null, null, false], `${row.id} forwarded nothing to the client`);
    for (const a of row.attempts) {
      const facts = { cacheMissReason: a.cacheMissReason, inferenceGeo: a.response.inferenceGeo, serviceTier: a.serviceTier, speed: a.response.speed, stopReason: a.response.stopReason, upstreamMessageId: a.upstreamMessageId, upstreamStatus: a.upstreamStatus };
      assert.deepEqual(facts, Object.fromEntries(Object.keys(facts).map((k) => [k, null])), `${row.id}'s attempt ${a.index + 1} carries no answer`);
    }
    // A client that left on its own keeps its code, after the head its stream sent.
    const gone = s.requests.filter((r) => r.errorCode === 'client_gone');
    assert.ok(gone.length > 0, 'the story holds a stream the client closed');
    for (const r of gone) assert.deepEqual([r.outcome, r.status, r.ttfbMs !== null, r.attempts.every((a) => a.outcome === 'canceled')], ['canceled', 200, true, true], `${r.id} keeps its reading`);
  });

  test(`the default range counts a client's cancel apart from the failures and the error codes, and the router's cuts as failed under their code, at ${at}`, async () => {
    const s = await story;
    const closed = s.requests.filter((r) => r.outcome === 'canceled');
    assert.ok(closed.length > 0, 'the story holds a canceled request');
    for (const r of closed) assert.ok(['client_gone', 'router_shutdown'].includes(r.errorCode), `${r.id} names the code, not ${r.errorCode}`);
    const rows = s.asked(s.requests.filter((r) => r.kind !== 'probe' && startOfHour(Date.parse(r.at)) >= startOfHour(s.now - DAY)));
    const [totals] = s.lib.breakdownOf(s.allRollups, { ...defaultRange(s), groupBy: 'all', includeProbes: false });
    const gone = rows.filter((r) => r.errorCode === 'client_gone').length;
    assert.ok(gone > 0, "the default range holds the client's cancel");
    assert.deepEqual([totals.clientCanceled, totals.errors.client_gone], [gone, undefined], 'the Client canceled column reads the story, and the Errors by code panel names no client_gone');
    assert.equal(totals.failed, rows.filter((r) => r.outcome === 'failed' || r.errorCode === 'router_shutdown').length, "and Failed counts the failures and the router's cuts alone");
    // A canceled request is a cut and no upstream failure, and a failure whose row names a code of its own files under
    // it: the stream the relay could not decode, the 400 under the type its body names, the bare 502 under its status
    // and the relay's rejection under its own word.
    const owned = ['accounts_unavailable', 'invalid_request_error', 'stream_undecodable', 'upstream_502'];
    const upstreamErrors = rows.filter((r) => r.outcome !== 'canceled' && r.status !== 200 && r.status !== 429 && r.status !== 529 && !owned.includes(r.errorCode));
    assert.equal(totals.errors.api_error ?? 0, upstreamErrors.length, 'and counts no canceled request as an upstream error');
    assert.equal(totals.errors.router_shutdown, rows.filter((r) => r.errorCode === 'router_shutdown').length, "and the router's own cut beside the client's");
  });

  test(`the default range's outcome totals add up to its request total, and that total to the rows it counts, at ${at}`, async () => {
    const s = await story;
    const rows = s.asked(s.requests.filter((r) => r.kind !== 'probe' && startOfHour(Date.parse(r.at)) >= startOfHour(s.now - DAY)));
    const [totals] = s.lib.breakdownOf(s.allRollups, { ...defaultRange(s), groupBy: 'all', includeProbes: false });
    const outcomes = { clientCanceled: totals.clientCanceled, exhausted: totals.exhausted, failed: totals.failed, incomplete: totals.incomplete, rateLimited: totals.rateLimited, rejected: totals.rejected, succeeded: totals.succeeded };
    assert.equal(sum(Object.values(outcomes), (n) => n), totals.requests, `the outcomes ${JSON.stringify(outcomes)} against ${totals.requests} requests`);
    assert.equal(totals.requests, rows.length, 'each row counts once');
  });

  // The route's own fixture, `analytics_refusal_categories`, comes with the admin capture, and from then on the sync
  // test reads this answer against it.
  test(`the story's refused answers fall under two categories and one with none across two sessions, each a request that succeeded, and the refusal route's rows sum to them, at ${at}`, async () => {
    const s = await story;
    const served = (r) => r.attempts.at(-1)?.response ?? null;
    const word = (r) => served(r).refusalCategory ?? '-';
    const refused = s.requests.filter((r) => served(r)?.stopReason === 'refusal');
    assert.ok(refused.length >= 5, `the story holds ${refused.length} refused answers`);
    assert.deepEqual([...new Set(refused.map(word))].sort(), ['-', 'cyber', 'harmful']);
    assert.ok(new Set(refused.map((r) => r.sessionId)).size >= 2, 'the refused answers span two sessions at least');
    for (const r of refused) {
      assert.deepEqual([r.outcome, r.refused], ['succeeded', false], `${r.id} is a request that succeeded`);
      assert.ok(served(r).refusalRecommendedModel !== null, `${r.id} names the model to try`);
    }
    assert.deepEqual(s.requests.filter((r) => 'refusalCategory' in r || 'stopReason' in r).map((r) => r.id), [], 'the serving attempt alone carries the refusal');
    // Requests by outcome and the Traffic table's OK column count a refused answer as the request that succeeded.
    const rows = s.asked(s.requests.filter((r) => r.kind !== 'probe' && startOfHour(Date.parse(r.at)) >= startOfHour(s.now - DAY)));
    const [totals] = s.lib.breakdownOf(s.allRollups, { ...defaultRange(s), groupBy: 'all', includeProbes: false });
    assert.ok(refused.some((r) => rows.includes(r)), 'the default range holds a refused answer');
    assert.equal(totals.succeeded, rows.filter((r) => r.outcome === 'succeeded').length, 'the OK count holds the refused answers');
    const range = { from: iso(Math.min(...s.requests.map((r) => Date.parse(r.at)))), groupBy: 'all', to: iso(s.now) };
    const answer = await s.api.getAnalyticsRefusalCategories(range);
    assert.equal(sum(answer.rows, (row) => row.requests), refused.length, 'the rows sum to the refused answers');
  });

  test(`the routing log lists the story's late event once the clock reaches it a few seconds after the load, on top, at ${at}`, async () => {
    const s = await story;
    // The route's clock is read as the call is made, so it is set for the call alone.
    const readAt = (clock) => {
      const wall = Date.now;
      Date.now = () => clock;
      try {
        return s.api.listRoutingLog({ from: iso(s.now - HOUR), limit: 25, to: iso(s.now + DAY) });
      } finally {
        Date.now = wall;
      }
    };
    const late = (rows) => rows.filter((r) => Date.parse(r.at) > s.now);
    const before = (await readAt(s.now + 5_000)).rows;
    const after = (await readAt(s.now + 6_000)).rows;
    assert.deepEqual(late(before), [], 'nothing past the load is written five seconds on');
    assert.deepEqual(late(after).map((r) => [r.type, r.kind, r.accountId, r.at]), [['event', 'refresh_succeeded', 'studio', iso(s.now + 6_000)]], 'six seconds on, the late event alone');
    assert.equal(after[0], late(after)[0], 'on top of the log');
  });

  test(`only_the_storys_fork_and_blocked_rows_carry_a_match at ${at}`, async () => {
    const s = await story;
    const ids = s.lib.GUARDRAIL_STORY_IDS;
    assert.ok(ids !== undefined && ids.size > 0, 'the story seeds rows a guardrail matched');
    assert.deepEqual(s.requests.filter((r) => r.guardrails.length > 0).map((r) => r.id).sort(), [...ids].sort(), 'the seeds alone carry a match');
    // The request fixture's row carries a match, which a row drawn over the sample would inherit.
    const unmatched = s.requests.filter((r) => !ids.has(r.id) && (r.guardrailId !== null || r.guardrails.length > 0)).map((r) => r.id);
    assert.deepEqual(unmatched, []);
    assert.ok(s.requests.some((r) => r.kind === 'probe'), 'the read holds the probes');
    for (const r of s.requests.filter((x) => ids.has(x.id))) {
      assert.notEqual(r.kind, 'probe', `${r.id} is no probe`);
      assert.equal(r.guardrailId, r.guardrails.find((m) => m.blocked)?.id ?? null, `${r.id} names the guardrail that blocked it, or none`);
    }
  });

  test(`every_blocked_story_row_reads_no_account_and_no_usage_and_keeps_its_captured_body at ${at}`, async () => {
    const s = await story;
    const blocked = s.requests.filter((r) => r.guardrailId !== null);
    assert.equal(blocked.length, 3, 'the story blocks three requests');
    for (const r of blocked) {
      const usage = r.usage.input + r.usage.cacheRead + r.usage.cacheWrite5m + r.usage.cacheWrite1h + r.usage.output;
      assert.deepEqual([r.accountId, usage], [null, 0], r.id);
      assert.ok((await s.api.getCapturedBodies(r.id)) !== null, `${r.id} keeps its body`);
    }
  });

  test(`the_counts_route_names_each_guardrail_and_the_guardrails_views_day_reads_the_same at ${at}`, async () => {
    const s = await story;
    const from = s.now - DAY;
    const counts = await s.api.getAnalyticsGuardrails({ from: iso(from), to: iso(s.now) });
    const read = counts.guardrails.map((c) => [c.id, c.observed, c.blocked, c.blockedBytes]);
    assert.ok(read.some(([, observed]) => observed > 0) && read.some(([, , blocked]) => blocked > 0), 'the day holds an observed match and a block');
    const names = new Map((await s.api.listGuardrails()).map((g) => [g.id, g.name]));
    assert.deepEqual(counts.guardrails.map((c) => c.name), counts.guardrails.map((c) => names.get(c.id) ?? null));
    const day = await s.api.getAnalyticsGuardrails('24h');
    assert.deepEqual([day.from, day.to, day.guardrails], [iso(from), iso(s.now), counts.guardrails], "the view's day is the last 24 hours");
  });

  test(`the guardrails answer counts a match from the hour that holds a custom range's start, at ${at}`, async () => {
    const s = await story;
    // The story's four summary forks run from 12:40 to 12:41:30 of its pinned day, inside one hour at either instant,
    // and the desktop block of 12:52:20 follows them. A range from a minute after the last fork, req_e916a3d7-c42,
    // reads the forks from the start of their hour, each one observed match beside the block's.
    const fork = s.requests.find((r) => r.id === 'req_e916a3d7-c42');
    const counts = await s.api.getAnalyticsGuardrails({ from: iso(Date.parse(fork.at) + 60_000), to: iso(s.now) });
    assert.deepEqual(counts.guardrails.map((c) => [c.id, c.observed, c.blocked]), [['large-auxiliary', 0, 1], ['subagent-status-summary', 5, 0]]);
  });

  test(`the_mock_dashboard_leaves_the_refused_rows_out_of_its_counts_and_reads_them_in_the_refused_members at ${at}`, async () => {
    const s = await story;
    const refused = s.totalled.filter((r) => r.refused);
    const asked = s.asked(s.totalled);
    const codesOf = (rows) => {
      const codes = new Map();
      for (const r of rows) if (r.errorCode) codes.set(r.errorCode, (codes.get(r.errorCode) ?? 0) + 1);
      return codes;
    };
    assert.deepEqual(refused.map((r) => r.id).sort(), REFUSED_IDS, "the day holds every one of the story's refusals");
    assert.equal(s.overview.requests24h, asked.length, 'the count leaves the refused rows out');
    assert.equal(sum(s.overview.hourly, (h) => h.requests), asked.length, 'the hourly requests too');
    assert.equal(s.overview.refused24h, refused.length, 'the refused member counts them');
    assert.equal(sum(s.overview.hourly, (h) => h.refused), refused.length, 'the hourly refused too');
    const refusals = codesOf(refused);
    assert.equal(refusals.get(s.overview.topRefusalCode ?? '') ?? 0, Math.max(...refusals.values()), 'the top refusal code is their most frequent');
    // The ledger's rate: the asked requests less the succeeded ones and the client's cancels, so the router's cut
    // answered with a 200 counts and a stream its client closed does not.
    const left = (r) => r.outcome === 'canceled' && r.errorCode === 'client_gone';
    assert.ok(asked.some(left), 'the day holds a stream its client closed');
    const failed = asked.filter((r) => r.outcome !== 'succeeded' && !left(r));
    assert.ok(failed.some((r) => r.status === 200), 'the day holds a request that did not succeed behind a 200');
    assert.equal(s.overview.errorRate24h, failed.length / asked.length, "the rate is the asked requests that did not succeed over the asked requests, the client's cancels left out");
    assert.equal(sum(s.overview.hourly, (h) => h.errors), failed.length, "the hourly errors leave the refused rows and the client's cancels out");
    const codes = codesOf(asked.filter((r) => !left(r)));
    assert.equal(codes.get(s.overview.topErrorCode ?? '') ?? 0, Math.max(0, ...codes.values()), "the top code is the asked requests' most frequent");
  });

  test(`the_rollups_refusals_map_sums_to_the_storys_refused_rows_by_code_and_key at ${at}`, async () => {
    const s = await story;
    const byKey = {};
    for (const r of s.allRollups) {
      for (const [code, n] of Object.entries(r.refusals)) {
        const map = (byKey[r.keyId] ??= {});
        map[code] = (map[code] ?? 0) + n;
      }
    }
    assert.deepEqual(byKey, { ci: { auth_unknown: 1, client_not_allowed: 2 }, desktop: { guardrail_blocked: 3 }, laptop: { client_contract: 1, client_drift: 1 } });
  });

  test(`the_breakdowns_refused_column_agrees_with_the_map_and_the_guardrails_cards_blocked_count_on_desktop at ${at}`, async () => {
    const s = await story;
    const range = { from: iso(s.now - DAY), to: iso(s.now) };
    const groups = await s.api.getAnalyticsBreakdown({ ...range, groupBy: 'key' });
    const desktop = groups.find((g) => g.key === 'desktop');
    const blocked = (await s.api.getAnalyticsGuardrails(range)).guardrails.find((c) => c.id === 'large-auxiliary')?.blocked;
    assert.deepEqual([desktop?.refused, desktop?.refusals.guardrail_blocked, blocked], [3, 3, 3], "desktop's Refused, its map and the card's blocked count");
    // The day's refused rows by the group the route files them under, a live key's by its bare id.
    const live = new Map(s.keys.map((k) => [k.id, Date.parse(k.createdAt)]));
    const want = {};
    for (const r of s.requests.filter((x) => x.refused && startOfHour(Date.parse(x.at)) >= s.now - DAY)) {
      const map = (want[live.get(r.keyId) === r.keyGeneration ? r.keyId : `${r.keyId}@${r.keyGeneration}`] ??= {});
      map[r.errorCode] = (map[r.errorCode] ?? 0) + 1;
    }
    assert.deepEqual(want, { ci: { auth_unknown: 1, client_not_allowed: 2 }, desktop: { guardrail_blocked: 3 }, laptop: { client_contract: 1, client_drift: 1 } }, "the day's refused rows by key");
    for (const g of groups) {
      const map = want[g.key] ?? {};
      assert.deepEqual([g.refused, { ...g.refusals }], [sum(Object.values(map), (n) => n), map], `${g.key}'s Refused and its map read its refused rows`);
    }
  });

  test(`each request the default range counts reads one cache class, a failed one as uncached, at ${at}`, async () => {
    const s = await story;
    const [all] = await s.api.getAnalyticsBreakdown({ from: iso(s.now - DAY), groupBy: 'all', to: iso(s.now) });
    assert.equal(all.cacheHits + all.cacheMisses + all.uncached, all.requests, 'the hits, the misses and the uncached requests');
  });

  test(`the_requests_pages_guardrail_facet_lists_the_three_blocks_under_large_auxiliary at ${at}`, async () => {
    const s = await story;
    const { requests } = await s.api.listRequests({ guardrails: ['large-auxiliary'], limit: 100_000 });
    const blocked = s.requests.filter((r) => r.errorCode === 'guardrail_blocked').map((r) => r.id);
    assert.equal(blocked.length, 3, 'the story blocks three requests');
    assert.deepEqual(requests.map((r) => r.id).sort(), blocked.sort(), 'the log lists every block under the guardrail that blocked it');
    for (const r of requests) assert.equal(r.guardrailId, 'large-auxiliary', r.id);
  });
}

// The instant the smoke run and the browser pass pin, whose 14:00 hour the routing log's story writes by hand.
const pinned = (loading = loading.then(() => storyAt('2026-09-16T14:30:00Z')));

test("the story's 14:00 hour counts desktop's token count as uncacheable and the turn its pool's policy marked as no such request", async () => {
  const s = await pinned;
  // Three of desktop's answers in the hour read and wrote nothing: the subagent turn whose body marked no breakpoint
  // (req_69e30548-32e), which all's cache TTL policy marks, the token count (req_3b8e51f4-0a9) and the classifier call
  // that marked one (req_7c2e9f41-d03).
  const [desktop] = s.lib.breakdownOf(s.allRollups, { from: '2026-09-16T14:00:00Z', groupBy: 'all', includeProbes: false, keyIds: ['desktop'], to: '2026-09-16T15:00:00Z' });
  assert.deepEqual(desktop && [desktop.uncached, desktop.uncacheable], [3, 1]);
});

// Catches a row the relay refuses behind its read of the body answered unmarked. The relay publishes the marker it
// computed from the body before a guardrail blocks it, and the block's session sits on a pool that pins it, so its body
// marks a breakpoint. A seed's marker reads no clock, so one load holds the case.
test('a guardrail block carries the marker the relay computed behind its read of the body', async () => {
  const s = await pinned;
  const detail = await s.api.getRequest('req_a8f3c6d0-e17');
  assert.equal(detail?.request.cacheMarker, true);
});

// Catches a pin's ended mark read off the lifecycle word `released`, which would end the finished session's main and
// the written session's c3a9, both released by the session's end, and a subagent's mark read off the written session's
// idle subagent alone, which would leave the finished session's two subagents, released by the mod, live.
test("a pin reads ended where the mod released its agent and live where the session's end released it", async () => {
  const s = await pinned;
  const finished = s.sessions.find((x) => x.sessionId === '9b4e2f71-6c3d-4a85-b0e2-5d7f8a1c3e96');
  assert.deepEqual(finished?.agentContexts.map((c) => [c.agentId, c.ended]), [['main', false], ['5d0e', true], ['a7f3', true]]);
  const pins = await s.api.listPoolPins('all');
  assert.equal(pins.find((p) => p.sessionId === '7bea4514-4087-4b6a-a410-0dd5cd31cfad' && p.agentId === 'c3a9')?.ended, false, "the written session's c3a9");
});

// Catches two builders of the mock that date one pin apart: the sessions route keeps a line of a pinned row whose pin
// the pins route of its key's pool has swept, as a pins route does that dates the pin of a main whose recipe holds no
// last probe by its last request alone. The case reads the lines that a recipe names, since those pins take one date
// from two sources: the pins route dates such a pin from its recipe, and the sessions route dates its line from the
// row. A line that no recipe names takes its own last use in both routes.
test("each line of a pinned session that a recipe names stands in the pins route of its key's pool", async () => {
  const s = await pinned;
  const named = new Set((await s.api.listKeepaliveRecipes()).map(pinOf));
  const poolOf = new Map(s.keys.map((k) => [k.id, k.poolId]));
  const lines = s.sessions
    .filter((x) => x.pinned)
    .flatMap((row) => row.agentContexts.map((line) => ({ pin: pinOf({ agentId: line.agentId, keyId: row.keyId, model: line.model, sessionId: row.sessionId }), poolId: poolOf.get(row.keyId) })))
    .filter((line) => named.has(line.pin));
  const listed = new Map();
  for (const poolId of new Set(lines.map((line) => line.poolId))) listed.set(poolId, new Set((await s.api.listPoolPins(poolId)).map(pinOf)));
  // A scope that reads no line would pass on any builder.
  assert.notEqual(lines.length, 0, 'no line of a pinned session is named by a recipe');
  assert.deepEqual(lines.filter((line) => !listed.get(line.poolId).has(line.pin)).map((line) => line.pin), []);
});

// Laptop's session c52dbc8e lists the pin its subagent's failover wrote. Loaded at 04:30, the log holds no pin event of
// its main, so the route counts nothing for the row.
const subagentPinned = (loading = loading.then(() => storyAt('2026-09-16T04:30:00.000Z')));

test("a session's placements count no seeded creation of a pin that a subagent holds", async () => {
  const s = await subagentPinned;
  const ofSession = (x) => x.keyId === 'laptop' && x.sessionId?.startsWith('c52dbc8e');
  const mainEvents = s.lib.ROUTING_EVENTS.filter((e) => ofSession(e) && e.agentId === 'main' && (e.kind === 'pin_created' || e.kind === 'pin_migrated'));
  assert.equal(mainEvents.length, 0, "main's pin events at this load");
  const row = s.sessions.find((x) => !x.historical && ofSession(x));
  assert.equal(row?.agentId, '4d1a', "the row's pin");
  assert.equal(row?.placements, 0, 'the placements the route counts');
});

// Catches a walk that reads the pool's tier for an entry's lifetime, by the first turn, and one that states the entry
// past its lifetime whatever the instant, by the second. Laptop's main turn on secondary of 15:33:36 the day before
// (req_b9b779ef-af6) comes 9 min 40 s behind its pin's last turn, past the five minutes its pin's line holds and inside
// the hour that all's policy gives a plan account's main turn. Desktop's main turn on primary of 14:12:30
// (req_6e1f3b98-a24) comes 46 s behind its pin's last turn.
test("a turn reads its pin's entry past its lifetime by the pin's own TTL and counts, and a turn inside it reads its entry alive", async () => {
  const s = await pinned;
  const factOf = async (id) => {
    const row = (await s.api.getRequest(id))?.request;
    return row && [row.attempts[0].entryExpired, s.lib.expiredBetweenTurns(row)];
  };
  assert.deepEqual(await Promise.all([factOf('req_b9b779ef-af6'), factOf('req_6e1f3b98-a24')]), [
    [true, true],
    [false, false],
  ]);
});

// Catches a mock that feeds the Cache tab and the keepalive's two counts one count. The day holds six expiries,
// laptop's two of them: its main turn of 15:33:36 the day before (req_b9b779ef-af6) and the night's turn of its
// subagent e90b at 04:37:26 (req_f05195b5-f2f), which read the report of the agent's turn of 04:27:56 with no request
// of its pin between. That turn's answer stopped with `end_turn`, which ends a subagent under the router's rule, so the
// turn of 04:37:26 is the agent's return. The Cache tab's laptop row counts both, and the overview and the attention
// item count the five expiries of the agents that the keepalive held.
test("a released agent's return counts in its key's Cache row and in neither the keepalive overview's count nor the attention item's", async () => {
  const s = await pinned;
  const groups = await s.api.getAnalyticsBreakdown({ from: iso(s.now - DAY), groupBy: 'key', to: iso(s.now) });
  const overview = await s.api.getKeepaliveOverview();
  const item = s.attention.find((i) => i.kind === 'expired_between_turns');
  assert.deepEqual([groups.find((g) => g.key === 'laptop')?.expiredBetweenTurns, overview.expiredBetweenTurns24h, item?.detail.expired], [2, 5, 5]);
});

// Catches a fold that books a move on its destination alone. The deleted retired workspace holds no window reading,
// and its pin of 09:40:20 moved to primary at 09:47:30 (mock/routingLog.ts), so its one hour reads both.
test('an account with no window reading lists the hour of its pin events, a move counted on the account it left', async () => {
  const s = await pinned;
  const rows = (await s.api.listAccountRollups(iso(s.now - DAY), iso(s.now + DAY))).filter((r) => r.accountId === 'retired');
  assert.deepEqual(rows.map((r) => [r.hour, r.pins]), [['2026-09-16T09:00:00.000Z', { created: 1, migratedIn: 0, migratedOut: 1 }]]);
});

// Catches a fold that books no cooldown time for an account with no window reading, or books it to another hour. The
// 429 of research at 03:38:20.093 (req_b68906d8-252) set a cooldown of 65 s, to 03:39:25.093.
test('an account with no window reading counts the time of its cooldown in the hour it elapsed in', async () => {
  const s = await pinned;
  const rows = (await s.api.listAccountRollups(iso(s.now - DAY), iso(s.now + DAY))).filter((r) => r.accountId === 'research');
  assert.deepEqual(rows.map((r) => [r.hour, r.cooldownSeconds]), [['2026-09-16T03:00:00.000Z', 65]]);
});

// Catches a dropped lookup of an ended plan's windows, which leaves its sampled hours with their peaks and no window.
// The last clean poll of sunset read its 5-hour window at 0.12 at 14:25 three days and five minutes back, resetting at
// 16:30, two hours past the plan's saved end at 14:30 (mock/accounts.ts), so the hour states the end as the deadline.
test("an ended plan's sampled hour states its last sample of a window, with the deadline at the account's end", async () => {
  const s = await pinned;
  const row = (await s.api.listAccountRollups(iso(s.now - 4 * DAY), iso(s.now))).find((r) => r.accountId === 'sunset' && r.hour === '2026-09-13T14:00:00.000Z');
  const w = row?.windows.five_hour;
  assert.deepEqual(w && [w.utilization, w.sampledAt, w.resetsAt, w.deadline], [0.12, '2026-09-13T14:25:00.000Z', '2026-09-13T16:30:00.000Z', '2026-09-13T14:30:00.000Z']);
});

// Catches a budget row that pairs one period's limit or spend with the other period's. The Overflow workspace holds
// both budgets in all, $1.10 of $5.00 for the day and $25.00 of $50.00 for the month (mock/pools.ts), and its
// placement of 10:52:30 (req_4e8b1d36-f72) reads them.
test("a member's budget rows read each period's own limit and spend", async () => {
  const record = (await decisionsOf(await pinned)).find((r) => r.requestId === 'req_4e8b1d36-f72' && r.seq === 1);
  const overflow = record?.candidates.find((c) => c.accountId === 'overflow');
  assert.deepEqual(Object.fromEntries((overflow?.budget ?? []).map((b) => [b.period, [b.budgetUsd, b.spentUsd]])), { day: [5, 1.1], month: [50, 25] });
});

// Catches a reached budget's obstacle that names no period, so the exhaustion's Spend row finds no budget to draw.
// Research crossed its day's budget of $20.00 at 12:41:07 (mock/time.ts), so the jobs pool's exhaustion of 14:16:40
// (req_51d0e2b8-7a0) reads it out on the day's budget until the next UTC midnight.
test("a member past its day's budget reads the budget obstacle on the day, until the day ends", async () => {
  const record = (await decisionsOf(await pinned)).find((r) => r.requestId === 'req_51d0e2b8-7a0' && r.seq === 1);
  const research = record?.candidates.find((c) => c.accountId === 'research');
  assert.deepEqual([research?.obstacle?.name, research?.obstacle?.reason, research?.obstacle?.until], ['budget', 'day', '2026-09-17T00:00:00.000Z']);
});

// An instant whose hour a full day back carries traffic.
const firstHour = loading.then(() => storyAt('2026-09-16T09:30:00Z'));

test("the hour a full day back counts toward the accounts' and pools' day totals and a pool's first bucket", async () => {
  const s = await firstHour;
  const early = s.totalled.filter((r) => Date.parse(r.at) < s.dayFrom);
  assert.ok(early.length > 0, 'the story has traffic in the hour a full day back');
  for (const a of s.accounts) {
    const own = (rows) => rows.filter((r) => r.accountId === a.id).length;
    assert.equal(a.requests.last24h, own(s.counted) + own(early), `${a.id}'s first hour`);
  }
  for (const p of s.pools) {
    assert.equal(p.traffic24h[0].requests, s.asked(early).filter((r) => r.poolId === p.id).length, `${p.id}'s first hour`);
  }
});

// An evening instant, whose drawn day holds a laptop turn that found its pin live on nightly in overage. It loads
// behind the first hour's load, since two loads at once would each restore the other's pinned clock.
const evening = (loading = firstHour.then(() => storyAt('2026-09-16T18:10:00Z')));

test("a drawn turn whose live pin the overage rule moves stays on its own plan rather than renewing the pin", async () => {
  const s = await evening;
  // Nightly reads overage from the first head after its cap's raise at 11:30:12, and all prefers the plan, so step 4
  // hands the 15:19 turn's pin from nightly to primary, which holds plan allowance, ahead of any renewal.
  const first = (await s.api.getRequest('req_2aab0f53-c41'))?.request.attempts[0];
  assert.deepEqual(first && [first.accountId, first.overageInUse, first.selection?.renewed], ['primary', false, false]);
});

test("a paired row's first reading is stamped at the list read that lands it, after the confirm", async () => {
  // Every story load pins the clock its bundle reads while it loads, so this case waits for them, then holds the clock
  // the mock reads and steps it past the confirm's first poll rather than waiting the poll out.
  await loading;
  const d = await import('./dist/api.js');
  const wall = Date.now;
  let now = wall();
  Date.now = () => now;
  try {
    const api = new d.MockApi();
    await api.login('admin');
    const started = await api.startPairing({ id: 'stamped', label: 'Stamped' });
    await api.completePairing(started.id, { code: 'code', state: started.state });
    const confirmed = now;
    await api.confirmPairing(started.id);
    // The confirm's first poll lands a few seconds on, on the mock's clock.
    now += 2_200;
    const row = (await api.listAccounts()).find((a) => a.id === 'stamped');
    assert.ok(row.windows.length > 0, 'the first reading landed');
    for (const w of row.windows) assert.equal(w.observedAt, iso(now), `${w.id} read at the list read, ${iso(now)}, after the confirm at ${iso(confirmed)}`);
  } finally {
    Date.now = wall;
  }
});
