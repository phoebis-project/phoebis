import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;

const HOUR = 3_600_000;
const point = (at, group, over = {}) => {
  const p = { at: new Date(at).toISOString(), cachedTokens: 0, fiveHourShare: null, group, sevenDayShare: null, uncachedTokens: 0, unit: null, usd: 0, ...over };
  return { tokens: p.cachedTokens + p.uncachedTokens, ...p };
};
const NONE = new Set();

test('a conversation class and an overhead class split as the legend says', () => {
  for (const conversation of ['compaction', 'main', 'subagent']) assert.equal(d.classLegendOf(conversation), 'conversation', conversation);
  for (const overhead of ['auxiliary', 'workflow', 'a-class-added-later']) assert.equal(d.classLegendOf(overhead), 'overhead', overhead);
  assert.equal(d.classLegendOf('-'), 'overhead', 'a request that named no class is overhead');
  assert.equal(d.classLegendOf('Main'), 'overhead', 'the class is the header as sent, so the match is exact');

  // The burn fold draws the two legend entries whatever classes the range holds, and sums each side.
  const { lines, rows } = d.foldBurn(
    [point(0, 'main', { cachedTokens: 600 }), point(0, 'subagent', { uncachedTokens: 60 }), point(0, 'auxiliary', { cachedTokens: 120 }), point(0, '-', { uncachedTokens: 60 })],
    'request_class',
    60,
    NONE,
  );
  assert.deepEqual(lines.map((l) => l.key), ['conversation', 'overhead']);
  assert.deepEqual([rows[0].cached, rows[0].uncached], [[600, 120], [60, 60]], 'a one-minute bucket reads its tokens as they are');
});

test('the burn fold reads every token and dollar figure per minute of its bucket and both window shares per hour', () => {
  const { lines, rows } = d.foldBurn(
    [point(0, 'business', { cachedTokens: 36_000, fiveHourShare: 0.3, sevenDayShare: 0.06, uncachedTokens: 3_600, unit: 'window_share' }), point(6 * HOUR, 'business', { unit: 'window_share' })],
    'account',
    6 * 3_600,
    new Set(['business']),
  );
  assert.deepEqual([rows[0].cached[0], rows[0].uncached[0], rows[0].usd[0]], [100, 10, 0], 'six hours of tokens over 360 minutes');
  assert.ok(Math.abs(rows[0].fiveHour[0] - 0.05) < 1e-12, 'a 30% share of the 5-hour window over six hours is 5% an hour');
  assert.ok(Math.abs(rows[0].sevenDay[0] - 0.01) < 1e-12, 'a 6% share of the 7-day window over six hours is 1% an hour');
  assert.deepEqual([rows[1].fiveHour[0], rows[1].sevenDay[0]], [0, 0], 'a bucket with no traffic reads zero on a window the line priced');
  assert.equal(lines[0].fiveHourPeak, rows[0].fiveHour[0]);
  assert.ok(Math.abs(lines[0].fiveHourPerHour - 0.025) < 1e-12, 'the range average spreads the 5-hour share over both buckets');
  assert.ok(Math.abs(lines[0].sevenDayPerHour - 0.005) < 1e-12, 'and the 7-day share');
  assert.ok(Math.abs(lines[0].tokensPerMinute - 55) < 1e-9, 'the range average spreads the tokens over both buckets');
});

test('an upstream burns in the unit its figures come in, a subscription whose tier the cost table does not size in none, and every other grouping in dollars', () => {
  const answered = [
    point(0, 'console', { unit: 'usd', usd: 1.2 }),
    point(0, 'personal', { fiveHourShare: 0.01, sevenDayShare: 0.001, unit: 'window_share' }),
    point(0, 'tierless', { uncachedTokens: 40, unit: 'window_share' }),
    point(0, 'idle', { fiveHourShare: 0, sevenDayShare: 0, unit: 'window_share' }),
    point(0, 'departed', { uncachedTokens: 20 }),
  ];
  const { lines } = d.foldBurn(answered, 'account', 3_600, new Set(['idle', 'personal']));
  const byKey = Object.fromEntries(lines.map((l) => [l.key, l]));
  assert.deepEqual(
    ['console', 'personal', 'tierless', 'idle', 'departed'].map((k) => byKey[k].unit),
    ['usd', 'window_share', 'unknown', 'window_share', 'usd'],
    'a subscription upstream whose tier the table sizes charts in window share; one whose tier it does not size is tier unknown',
  );
  assert.deepEqual([byKey.tierless.fiveHourPeak, byKey.tierless.fiveHourPerHour, byKey.tierless.sevenDayPerHour, byKey.tierless.usdPerMinute], [null, null, null, 0], 'a tier-unknown upstream has no figure to chart');
  assert.deepEqual([byKey.idle.unit, byKey.idle.fiveHourPeak, byKey.idle.fiveHourPerHour], ['window_share', null, null], 'a sized upstream with no traffic in range priced none, so it reads a dash');
  assert.ok(d.foldBurn(answered.map((p) => ({ ...p, group: `k-${p.group}` })), 'key', 3_600, NONE).lines.every((l) => l.unit === 'usd'));
});

test('the burn fold keeps the groups that burn the most tokens and sums the rest as other', () => {
  const answered = Array.from({ length: 9 }, (_, i) => point(0, `s${i}`, { cachedTokens: (i + 1) * 60, usd: 0.6 }));
  const { lines, rows } = d.foldBurn(answered, 'session', 60, NONE);
  assert.deepEqual(lines.map((l) => l.key), ['s8', 's7', 's6', 's5', 's4', 's3', 's2', 'other']);
  assert.equal(lines.at(-1).label, 'Other');
  assert.deepEqual([rows[0].cached.at(-1), rows[0].usd.at(-1)], [60 + 120, 1.2], 'the two smallest sessions, summed');
  assert.deepEqual([rows[0].fiveHour.at(-1), rows[0].sevenDay.at(-1)], [null, null], 'shares of different windows do not sum');
  assert.deepEqual(d.foldBurn([point(0, '-', { cachedTokens: 60 })], 'key', 60, NONE).lines.map((l) => l.label), ['unattributed']);
});

test('under the upstream grouping the rest folds into one other per unit, so no dollars hide behind a window share', () => {
  // Seven upstreams burn the most; the three smallest are a priced subscription, a tier-unknown one and an API key.
  const kept = Array.from({ length: 7 }, (_, i) => point(0, `u${i}`, { cachedTokens: 1_000 * (i + 2), fiveHourShare: 0.001, unit: 'window_share' }));
  const answered = [
    ...kept,
    point(0, 'small-plan', { cachedTokens: 120, fiveHourShare: 0.01, sevenDayShare: 0.002, unit: 'window_share' }),
    point(0, 'small-tierless', { cachedTokens: 90, unit: 'window_share' }),
    point(0, 'small-key', { cachedTokens: 60, unit: 'usd', usd: 0.6 }),
  ];
  const sized = new Set([...kept.map((p) => p.group), 'small-plan']);
  const { lines, rows } = d.foldBurn(answered, 'account', 60, sized);
  const rest = lines.slice(7);
  assert.deepEqual(rest.map((l) => [l.label, l.unit, l.other]), [['Other (plan)', 'window_share', true], ['Other (tier unknown)', 'unknown', true], ['Other (dollars)', 'usd', true]]);
  assert.equal(new Set(lines.map((l) => l.key)).size, lines.length, 'each line keeps a key of its own');
  assert.deepEqual([rows[0].cached[7], rows[0].cached[8], rows[0].cached[9]], [120, 90, 60], 'each rest carries its own unit\'s upstreams');
  assert.deepEqual([rows[0].usd[7], rows[0].usd[8], rows[0].usd[9]], [0, 0, 0.6], 'the dollar side keeps the API-key upstream\'s dollars');
  assert.deepEqual([rows[0].fiveHour[7], rows[0].sevenDay[7]], [null, null], 'a folded rest draws no share');
  const oneUnit = d.foldBurn([...kept, point(0, 'small-key', { cachedTokens: 60, unit: 'usd', usd: 0.6 }), point(0, 'tiny-key', { cachedTokens: 30, unit: 'usd', usd: 0.3 })], 'account', 60, sized);
  assert.deepEqual(oneUnit.lines.slice(7).map((l) => l.label), ['Other (dollars)'], 'a rest of one unit folds into that unit alone');
});

test('an idle bucket reads zero only on a window that priced traffic, and a window with none stays null', () => {
  const plan = (at, over) => point(at * HOUR, 'business', { unit: 'window_share', ...over });
  const { lines, rows } = d.foldBurn(
    [plan(0, { fiveHourShare: 0.08, tokens: 100 }), plan(1, { tokens: 100 }), plan(2, { tokens: 0 })],
    'account',
    3_600,
    new Set(['business']),
  );
  assert.deepEqual(rows.map((r) => r.fiveHour[0]), [0.08, null, 0], 'a priced bucket, one whose traffic went unpriced, and an idle one');
  assert.deepEqual(rows.map((r) => r.sevenDay[0]), [null, null, null], 'the idle bucket stays null on a window no bucket priced');
  assert.ok(Math.abs(lines[0].fiveHourPerHour - 0.04) < 1e-12, 'the mean over the priced and the idle bucket');
  assert.equal(lines[0].sevenDayPerHour, null);
  assert.deepEqual(lines[0].unpriced, { fiveHour: true, sevenDay: true });
});

test('a bucket whose rows carried no token is idle whatever share its rows answered', () => {
  // A failed request books a row with no token, which a listed model prices at zero on either window.
  const plan = (at, over) => point(at * HOUR, 'business', { unit: 'window_share', ...over });
  const { lines, rows } = d.foldBurn([plan(0, { fiveHourShare: 0.08, tokens: 100 }), plan(1, { fiveHourShare: 0, sevenDayShare: 0, tokens: 0 })], 'account', 3_600, new Set(['business']));
  assert.deepEqual(rows.map((r) => r.fiveHour[0]), [0.08, 0]);
  assert.deepEqual(rows.map((r) => r.sevenDay[0]), [null, null], 'no bucket priced traffic on the 7-day window, so its legend reads a dash rather than 0%');
  assert.equal(lines[0].sevenDayPerHour, null);
});

test('a bucket with cache writes and output alone is not idle', () => {
  const plan = (at, over) => point(at * HOUR, 'business', { unit: 'window_share', ...over });
  const { lines, rows } = d.foldBurn([plan(0, { fiveHourShare: 0.02, uncachedTokens: 10 }), plan(1, { cachedTokens: 0, tokens: 50, uncachedTokens: 0 })], 'account', 3_600, new Set(['business']));
  assert.deepEqual(rows.map((r) => r.fiveHour[0]), [0.02, null], 'its traffic went unpriced rather than absent');
  assert.equal(lines[0].unpriced.fiveHour, true);
});

test('a known tier whose every share is null is a planned line listed as model unlisted', () => {
  // A Max 20x upstream whose range traffic ran on a model the table leaves unmeasured throughout.
  const answered = [point(0, 'a', { tokens: 300, uncachedTokens: 300, unit: 'window_share' }), point(HOUR, 'a', { tokens: 200, uncachedTokens: 200, unit: 'window_share' })];
  const [line] = d.foldBurn(answered, 'account', 3_600, new Set(['a'])).lines;
  assert.deepEqual([line.unit, line.fiveHourPerHour, line.sevenDayPerHour, line.unpriced], ['window_share', null, null, { fiveHour: true, sevenDay: true }]);
  assert.equal(d.foldBurn(answered, 'account', 3_600, NONE).lines[0].unit, 'unknown', 'the same traffic on an upstream whose tier the table does not size');
});

test('a known tier with only a priced 7-day share keeps its 7-day series', () => {
  const answered = [0, 1].map((h) => point(h * HOUR, 'a', { sevenDayShare: 0.01, tokens: 100, uncachedTokens: 100, unit: 'window_share' }));
  const [line] = d.foldBurn(answered, 'account', 3_600, new Set(['a'])).lines;
  assert.equal(line.unit, 'window_share');
  assert.notEqual(line.sevenDayPerHour, null);
  assert.equal(line.fiveHourPerHour, null);
  assert.deepEqual(line.unpriced, { fiveHour: true, sevenDay: false });
});

test("a bucket's share is null when any row is unpriced, in either order", () => {
  assert.equal(d.bucketShare([0.1, null]), null);
  assert.equal(d.bucketShare([null, 0.1]), null);
  assert.ok(Math.abs(d.bucketShare([0.1, 0.2]) - 0.3) < 1e-12);
  assert.equal(d.bucketShare([]), null, 'a bucket with no row has no share');
});

test("the mock's burn fold prices only rows with tokens and nulls a window on any unpriced row, in either order", () => {
  // Wednesday 2026-09-16 at 10:00 UTC, before the peak band opens in Los Angeles; business is the story's Max 20x.
  const hour = Date.parse('2026-09-16T10:00:00Z');
  const row = (model, uncachedInput) => ({ accountId: 'business', costUsd: 0, hour: new Date(hour).toISOString(), keyId: 'desktop', model, requestClass: 'main', sessionId: 's', tokens: { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, output: 0, uncachedInput } });
  const range = { from: new Date(hour).toISOString(), to: new Date(hour + HOUR).toISOString() };
  const shareOf = (rows) => d.burnOf(rows, range, 'account').points.find((p) => p.group === 'business' && Date.parse(p.at) === hour).fiveHourShare;
  const priced = row('claude-opus-5-5', 1_000_000);
  // A failed request on Sonnet 4.5, which the table has no row for, carries no token.
  const failed = row('claude-sonnet-4-5', 0);
  for (const rows of [[priced, failed], [failed, priced]]) assert.ok(Math.abs(shareOf(rows) - 0.01461) < 1e-12, 'a row with no token leaves the priced share');
  assert.equal(shareOf([failed, row('claude-opus-5-5', 0)]), null, 'rows with no token alone are an idle bucket, not a share of zero');
  const unpriced = row('claude-sonnet-5', 1_000);
  for (const rows of [[priced, unpriced], [unpriced, priced]]) assert.equal(shareOf(rows), null, 'a row the table cannot price nulls the window');
});

test('an upstream is sized when the cost table holds its stored tier', () => {
  const account = (id, rateLimitTier) => ({ id, kind: rateLimitTier === null ? 'api_key' : 'oauth', subscription: rateLimitTier === null ? null : { rateLimitTier } });
  const accounts = [account('stored', 'default_claude_max_20x'), account('bare', 'claude_max_20x'), account('team', 'claude_team'), account('key', null)];
  const tiers = { default_claude_max_20x: { fiveHour: 1, sevenDay: 1 } };
  assert.deepEqual([...d.sizedUpstreams(accounts, tiers)].sort(), ['bare', 'stored']);
});

test("the placements table's hit share formats", () => {
  const row = { cacheReadTokens: 184_000, hitShare: 2 / 3, hits: 2, placements: 3, promisedTokens: 190_500, rung: 'content' };
  assert.deepEqual(d.placementCells(row), ['3', '2', '67%', '184K', '190.5K'], 'placements, hits, hit share, tokens read, tokens promised');
  assert.equal(d.placementCells({ ...row, hitShare: 1, hits: 3 })[2], '100%');
  assert.equal(d.placementCells({ ...row, hitShare: 0, hits: 0 })[2], '0%', 'a rung whose every placement missed reads zero');
  assert.equal(d.placementCells({ ...row, hitShare: null, hits: 0, placements: 0 })[2], '-', 'a rung with no placement has no share to read');
  assert.equal(d.placementCells({ ...row, promisedTokens: 0, rung: 'with_main' })[4], '-', 'a placement beside the main conversation promises no size');
  assert.deepEqual(['content', 'static', 'with_main', 'by_moonlight'].map(d.placementRungLabel), ['by its messages', 'by its static prefix', 'with the main conversation', 'by_moonlight']);
});

test("the mock files its hourly rows under the speed each hour's answers reported, one key's Opus hours fast", () => {
  const groups = d.groupsOf(d.ALL_ROLLUPS, { from: d.ALL_ROLLUPS[0].hour, groupBy: 'speed', to: d.ALL_ROLLUPS.at(-1).hour });
  assert.deepEqual(groups.map((g) => g.key), ['fast', 'standard']);
  const fast = d.ALL_ROLLUPS.filter((r) => r.speed === 'fast');
  assert.ok(fast.length > 0 && fast.every((r) => r.keyId === 'desktop' && r.model.includes('opus') && r.kind === 'inference'));
  const served = (rows) => rows.filter((r) => r.kind !== 'probe').reduce((n, r) => n + r.requests.ok + r.requests.rate_limited + r.requests.overloaded + r.requests.upstream_error, 0);
  assert.deepEqual(groups.map((g) => g.requests), [served(fast), served(d.ALL_ROLLUPS.filter((r) => r.speed === 'standard'))]);
});

test("the mock prices a fast hour and a fast answer at the model's fast cells", async () => {
  const api = new d.MockApi();
  const cells = new Map((await api.getSettings()).pricing.models.map((p) => [p.model, p.fast]));
  const priced = (fast, u) => (u.input * fast.inputPerM + u.cacheRead * fast.cacheReadPerM + u.cacheWrite5m * fast.cacheWrite5mPerM + u.cacheWrite1h * fast.cacheWrite1hPerM + u.output * fast.outputPerM) / 1_000_000;
  // A synthesized hour prices its tokens whole; one folded from the log sums its requests, each rounded to 1e-4.
  const hours = d.ALL_ROLLUPS.filter((r) => r.speed === 'fast');
  assert.ok(hours.length > 0);
  for (const r of hours) {
    const want = priced(cells.get(r.model), { ...r.tokens, input: r.tokens.uncachedInput });
    assert.ok(Math.abs(r.costUsd - want) <= 0.001 + want * 0.001, `${r.hour} ${r.model}: ${r.costUsd} against ${want}`);
  }
  const { requests } = await api.listRequests({ speeds: ['fast'] });
  assert.ok(requests.length > 0);
  for (const r of requests) assert.equal(r.costUsd, Math.round(priced(cells.get(r.model), r.usage) * 10_000) / 10_000, r.id);
});

test('the session fold files a request under the speed its serving attempt reported, standard when it named none', () => {
  const rows = d.decodeRequestPage(fixture('requests')).requests;
  const served = (r) => r.attempts.at(-1)?.response.speed;
  assert.deepEqual(d.foldGroups(rows, 'speed').map((g) => [g.key, g.requests]).sort(), [['fast', rows.filter((r) => served(r) === 'fast').length], ['standard', rows.filter((r) => served(r) !== 'fast').length]]);
  const named = rows.find((r) => served(r) === 'fast');
  const unnamed = { ...named, attempts: named.attempts.map((a) => ({ ...a, response: { ...a.response, speed: null } })) };
  assert.deepEqual(d.foldGroups([unnamed], 'speed').map((g) => g.key), ['standard'], 'an answer that named no speed files as standard, as the rollups file it');
  // The fixture's fast request failed over: its first attempt was standard and its serving one fast.
  assert.equal(named.attempts[0].response.speed, 'standard');
});
