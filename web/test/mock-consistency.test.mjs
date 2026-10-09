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

/** A rollup row's requests, as the analytics routes count them. */
const requestsOf = (r) => r.requests.ok + r.requests.incomplete + r.requests.rate_limited + r.requests.overloaded + r.requests.upstream_error;
const tokensOf = (r) => r.tokens.uncachedInput + r.tokens.cacheRead + r.tokens.cacheWrite5m + r.tokens.cacheWrite1h + r.tokens.output;
const sum = (list, read) => list.reduce((n, x) => n + read(x), 0);
const near = (a, b) => Math.abs(a - b) < 1e-6;

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
  const { requests } = await api.listRequests({ limit: 100_000 });
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
  return { accounts, allRollups: d.ALL_ROLLUPS, api, attention, counted, dayFrom, events, keys, lib: d, now, overview, pools, requests, rollups, sessions, settings, totalled, totalledFrom: totalsFrom, weekFrom };
}

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
    const pairing = await s.api.startPairing('legacy');
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

  test(`each pool's active pins are the pinned sessions under its keys, and the dashboard's are their sum, at ${at}`, async () => {
    const s = await story;
    const poolOf = new Map(s.keys.map((k) => [k.id, k.poolId]));
    for (const p of s.pools) {
      assert.equal(p.activePins, s.sessions.filter((x) => x.pinned && poolOf.get(x.keyId) === p.id).length, `${p.id}'s active pins`);
    }
    assert.equal(s.overview.activePins, sum(s.pools, (p) => p.activePins), "the dashboard's active pins");
  });

  test(`the key with one request in its life reads one, at ${at}`, async () => {
    const s = await story;
    const notebook = s.keys.find((k) => k.id === 'notebook');
    assert.deepEqual([notebook.lifetime.requests, notebook.usage7d.requests], [1, 1]);
  });

  test(`every plan-funded row of a weighted model carries non-null shares and no API key, overage or fast row carries one, at ${at}`, async () => {
    const s = await story;
    const costs = s.settings.subscriptionCosts;
    let priced = 0;
    for (const r of s.requests) {
      const serving = r.attempts[r.attempts.length - 1] ?? null;
      const funding = s.lib.fundingOf(serving);
      if (funding !== 'plan') {
        assert.equal(r.costShares, null, `${r.id} is ${funding}-funded and carries ${JSON.stringify(r.costShares)}`);
        continue;
      }
      assert.ok(r.costShares !== null && typeof r.costShares === 'object', `${r.id} is plan-funded and carries no share map`);
      const values = Object.values(r.costShares);
      // A window the model has no weight on reads null, as the route answers an applicable window it cannot price.
      if (s.lib.isWeighted(costs, r.canonicalModel)) {
        assert.ok(values.length > 0 && values.every((v) => typeof v === 'number'), `${r.id} on ${r.canonicalModel} reads ${JSON.stringify(r.costShares)}`);
        priced += 1;
      } else assert.ok(values.length > 0 && values.every((v) => v === null), `${r.id} on the unweighted ${r.canonicalModel} reads ${JSON.stringify(r.costShares)}`);
    }
    assert.ok(priced > 0, 'the story prices a share');
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
    }
  });

  test(`the breakdown's account shares equal the sum of its rows' shares, at ${at}`, async () => {
    const s = await story;
    const costs = s.settings.subscriptionCosts;
    const range = { from: iso(s.now - DAY), to: iso(s.now) };
    const groups = await s.api.getAnalyticsBreakdown({ ...range, groupBy: 'account' });
    const day = s.allRollups.filter((r) => Date.parse(r.hour) >= Date.parse(range.from) && Date.parse(r.hour) <= Date.parse(range.to));
    const kinds = new Map(s.accounts.map((a) => [a.id, a.kind]));
    const tiers = new Map(s.accounts.map((a) => [a.id, a.subscription?.rateLimitTier]));
    const usageOf = (r) => ({ cacheRead: r.tokens.cacheRead, cacheWrite1h: r.tokens.cacheWrite1h, cacheWrite5m: r.tokens.cacheWrite5m, input: r.tokens.uncachedInput, output: r.tokens.output });
    const windowsOf = (r) => ['five_hour', 'seven_day', ...s.lib.modelWeeks(costs, r.model)];
    // The rows' shares summed as the route sums them: a window read null by any row reads null for the group.
    const summed = (rows, tier, usage) => {
      const out = {};
      for (const r of rows) {
        if (tokensOf(r) === 0) continue;
        for (const w of windowsOf(r)) {
          const share = s.lib.shareOf(costs, w, tier, r.model, usage(r), Date.parse(r.hour));
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
      if (kinds.get(g.key) !== 'oauth' || tier === undefined) {
        assert.deepEqual([g.costShares, g.cacheShares, g.probeShares, g.planRequests, g.planOutputTokens], [null, null, null, null, null], `${g.key} prices a share`);
        continue;
      }
      const plan = day.filter((r) => r.accountId === g.key && r.funding === 'plan');
      const counted = plan.filter((r) => r.kind !== 'probe');
      const probes = plan.filter((r) => r.kind === 'probe');
      const cost = summed(counted, tier, usageOf);
      // An unpriced window is absent from the group's map, as the route answers it; a request row keeps the key with null.
      assert.deepEqual(Object.keys(g.costShares ?? {}).sort(), Object.keys(cost).filter((w) => cost[w] !== null).sort(), `${g.key}'s windows`);
      for (const [w, want] of Object.entries(g.costShares ?? {})) close(want, cost[w], `${g.key}'s ${w}`);
      const read = summed(counted, tier, (r) => ({ ...usageOf(r), cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 }));
      const write = summed(counted, tier, (r) => ({ ...usageOf(r), cacheRead: 0, input: 0, output: 0 }));
      // An unpriced window is absent from the cache and probe maps, as the route answers them.
      assert.deepEqual(Object.keys(g.cacheShares ?? {}).sort(), Object.keys(read).filter((w) => read[w] !== null && write[w] !== null).sort(), `${g.key}'s cache windows`);
      for (const [w, entry] of Object.entries(g.cacheShares ?? {})) {
        close(entry.read, read[w], `${g.key}'s ${w} cache read`);
        close(entry.write, write[w], `${g.key}'s ${w} cache write`);
      }
      const probe = summed(probes, tier, usageOf);
      const probeUsd = sum(probes, (r) => r.probeCostUsd);
      const ratio = probeUsd > 0 ? sum(probes, (r) => r.wastedProbeUsd) / probeUsd : null;
      assert.deepEqual(Object.keys(g.probeShares ?? {}).sort(), Object.keys(probe).filter((w) => probe[w] !== null).sort(), `${g.key}'s probe windows`);
      for (const [w, entry] of Object.entries(g.probeShares ?? {})) {
        close(entry.cost, probe[w], `${g.key}'s ${w} probe cost`);
        close(entry.wasted, ratio === null ? null : probe[w] * ratio, `${g.key}'s ${w} wasted probe share`);
      }
      assert.equal(g.planRequests, sum(counted, requestsOf), `${g.key}'s plan requests`);
      assert.equal(g.planOutputTokens, sum(counted, (r) => r.tokens.output), `${g.key}'s plan output tokens`);
      compared += 1;
    }
    assert.ok(compared > 0 && groups.some((g) => g.costShares !== null && Object.values(g.costShares).some((v) => v !== null)), 'an account carries a priced share');
  });

  test(`every rollup's funding reads the answer that served its requests, a request no attempt served filing under plan, at ${at}`, async () => {
    const s = await story;
    // The ledger keys a rollup's funding off the served answer: a fast answer files fast whatever the account's
    // kind, a head in overage files overage, and a request no attempt served, whatever speed it asked for, files
    // plan. The requested speed names the speed key alone.
    const fundingOf = (r) => {
      const last = r.attempts[r.attempts.length - 1];
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
    const unserved = s.requests.filter((r) => r.attempts.length === 0 && r.shape.requestedSpeed === 'fast');
    if (unserved.length > 0) {
      const lane = laneOf(iso(startOfHour(Date.parse(unserved[0].at))), unserved[0].keyId, unserved[0].poolId, '-', unserved[0].model, unserved[0].kind, unserved[0].sessionId);
      assert.ok(filed.get(lane)?.has('plan'), `${unserved[0].id} asked for fast and no attempt served it, so its rollup files under plan`);
    }
  });
}

// An instant whose hour a full day back carries traffic before the dashboard's first bucket.
const firstHour = (loading = loading.then(() => storyAt('2026-09-16T09:30:00Z')));

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

test("the mock prices an answer at the cells its own prompt selects, as the ledger does", async () => {
  const d = await import('./dist/api.js');
  const row = d.PRICES.find((p) => p.model === 'claude-haiku-5-5');
  assert.ok(row.longContext, 'the Haiku 5.5 row carries a long-context set');
  // The prompt is the four input-side counts together, and `input` is set so the sum lands on the threshold or one over.
  const usage = (input) => ({ input, cacheRead: 50_000, cacheWrite5m: 15_000, cacheWrite1h: 10_000, output: 1_000, thinking: 0, webFetch: 0, webSearch: 0 });
  const at = usage(row.longContext.over - 75_000);
  const over = usage(row.longContext.over - 75_000 + 1);
  assert.equal(d.cellsFor('claude-haiku-5-5', at), row, 'a prompt of exactly the threshold prices at the standard cells');
  assert.equal(d.cellsFor('claude-haiku-5-5', over), row.longContext, 'one token more prices at the long-context cells');
  const split = d.costSplitOf('claude-haiku-5-5', over);
  assert.ok(near(split.input, (over.input * row.longContext.inputPerM) / 1_000_000), 'the input at the long-context cell');
  assert.ok(near(split.cacheRead, (over.cacheRead * row.longContext.cacheReadPerM) / 1_000_000), 'the cache read too');
  assert.ok(near(d.costSplitOf('claude-haiku-5-5', at).input, (at.input * row.inputPerM) / 1_000_000), 'and the threshold itself at the standard cell');
  // No mock row carries both a fast and a long-context set, so this shows only that a fast answer reads the fast
  // cells. The fast-before-prompt order is the ledger's, proven by its own
  // `price_at_selects_the_fast_cells_then_the_long_context_cells_then_the_standard_ones`.
  const opus = d.PRICES.find((p) => p.model === 'claude-opus-5-5');
  assert.equal(d.cellsFor('claude-opus-5-5', usage(10_000_000), true), opus.fast, 'a fast answer on a row with fast cells reads the fast cells');
});
