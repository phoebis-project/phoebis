import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;
const isIso = (s) => typeof s === 'string' && !Number.isNaN(Date.parse(s)) && s.endsWith('Z');

test('an account decodes with its windows, plan, pools, and models', () => {
  const a = d.decodeAccount(fixture('account'));
  assert.equal(a.id, 'business');
  assert.equal(a.kind, 'oauth');
  assert.equal(a.plan, 'Max 5x');
  assert.equal(a.subscription.rateLimitTier, 'default_claude_max_5x');
  assert.deepEqual([a.subscription.since, a.subscription.status, a.subscription.trialEndsAt], [null, null, null], 'the route answers none of these, so the card leaves them out rather than reading active');
  assert.deepEqual(fixture('account').windows.map((w) => w.id), ['7d', 'fable', 'five_hour'], 'the route passes each sampler spelling through, in the ledger order');
  assert.deepEqual(a.windows.map((w) => w.id), ['5h', '7d'], 'the poll spelling folds onto the UI id and the order is the drawn one');
  assert.equal(a.fableWindow.id, '7d_fable');
  assert.ok(isIso(a.windows[0].resetsAt) && isIso(a.windows[0].observedAt));
  assert.equal(a.windows[0].utilization, 0.62, 'the five_hour sample is the 5h window');
  assert.deepEqual(a.pools, [{ poolId: 'main', priority: 0 }]);
  assert.equal(a.extraUsage.monthlyLimitCredits, 5000);
  assert.equal(a.requests.lifetime, 3);
  assert.ok(a.servedModels.some((m) => m.id === 'claude-sonnet-5' && m.selected));
  assert.equal(a.status, 'ready');
  assert.equal(a.email, 'test@example.invalid');
});

test('an account carries extra headers only when the route answers an object of strings', () => {
  const base = fixture('account');
  assert.equal(d.decodeAccount(base).headers, null, 'a route that reports no headers answers none');
  assert.equal(d.decodeAccount({ ...base, headers: {} }).headers, null, 'an empty object is no headers rather than an empty list');
  assert.equal(d.decodeAccount({ ...base, headers: { 'x-retries': 3, 'x-team': 'core' } }).headers, null, 'one non-string value refuses the map instead of dropping that entry');
  assert.deepEqual(d.decodeAccount({ ...base, headers: { 'x-team': 'core' } }).headers, { 'x-team': 'core' });
});

test('the account list folds state, enabled, and cooldowns onto one status', () => {
  const list = fixture('accounts').accounts.map(d.decodeAccount);
  assert.deepEqual(list.map((a) => a.id), ['business', 'direct'], 'sorted by label');
  const direct = list.find((a) => a.id === 'direct');
  assert.equal(direct.kind, 'api_key');
  assert.equal(direct.subscription, null);
  assert.equal(direct.extraUsage, null);
  assert.equal(d.decodeAccount({ ...fixture('account'), enabled: false }).status, 'paused');
  assert.equal(d.decodeAccount({ ...fixture('account'), state: 'reauth_required' }).status, 'reauth_required');
  assert.equal(d.decodeAccount({ ...fixture('account'), state: 'funding_required' }).status, 'funding_required');
  assert.equal(d.decodeAccount({ ...fixture('account'), state: 'unpaired' }).status, 'unpaired');
  assert.equal(d.decodeAccount({ ...fixture('account'), cooldowns: [{ scope: 'account', until: 1, reason: 'rate_limit', window: '5h', set_at: 0 }] }).status, 'cooldown');
});

test('a window both samplers wrote folds to one entry carrying the newer reading', () => {
  const base = fixture('account');
  const older = { id: '5h', observed_at: 1_000_000, resets_at: 4_600_000, utilization: 0.11 };
  const newer = { id: 'five_hour', observed_at: 1_900_000, resets_at: 4_600_000, utilization: 0.77 };
  for (const windows of [[older, newer], [newer, older]]) {
    const a = d.decodeAccount({ ...base, windows });
    assert.deepEqual(a.windows.map((w) => w.id), ['5h'], 'the header and the poll spelling are one window');
    assert.equal(a.windows[0].utilization, 0.77, 'the newer reading wins, whichever order the route sent the two in');
    assert.equal(a.windows[0].observedAt, new Date(1_900_000).toISOString());
  }
  const all = d.decodeAccount({
    ...base,
    windows: [{ id: 'fable', observed_at: 1_000_000, resets_at: 9, utilization: 0.1 }, newer, { id: 'seven_day', observed_at: 1_000_000, resets_at: 9, utilization: 0.4 }],
  });
  assert.deepEqual(all.windows.map((w) => w.id), ['5h', '7d'], 'the fold draws in page order, not in the order the route sent');
  assert.equal(all.fableWindow.id, '7d_fable');
});

test('a quota history both samplers wrote merges into one series in time order', () => {
  const history = d.decodeQuotaHistory({
    account_id: 'business',
    windows: [
      { id: '5h', samples: [{ at: 2_000_000, utilization: 0.5 }] },
      { id: 'seven_day', samples: [{ at: 1_000_000, utilization: 0.2 }] },
      { id: 'five_hour', samples: [{ at: 1_000_000, utilization: 0.3 }] },
    ],
  });
  assert.deepEqual(history.map((h) => h.windowId), ['5h', '7d'], 'merged, and in the order the pages draw them');
  assert.deepEqual(history[0].samples.map((s) => s.utilization), [0.3, 0.5], 'both samplers land in one series, oldest first');
  assert.equal(history[0].accountId, 'business');
});

test('plan labels read the tier', () => {
  assert.equal(d.planLabel('default_claude_max_20x'), 'Max 20x');
  assert.equal(d.planLabel('claude_pro'), 'Pro');
  assert.equal(d.planLabel('something_else'), 'something_else');
});

test('keys carry status, totals, and the created secret stays out of the row', () => {
  const [k] = fixture('keys').keys.map(d.decodeKey);
  assert.equal(k.name, 'Desktop');
  assert.equal(k.status, 'active');
  assert.deepEqual(k.allowedModels, ['claude-sonnet-5']);
  assert.equal(k.lifetime.requests, 3);
  assert.equal(k.usage7d.tokens > 0, true);
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], revoked_at: 5 }).status, 'revoked');
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], expires_at: 5 }).status, 'expired');
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], enabled: false }).status, 'disabled', 'routing refuses a key that is switched off, so the row may not read active');
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], enabled: false, revoked_at: 5 }).status, 'revoked', 'the more final state wins');
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], enabled: false, expires_at: 5 }).status, 'expired');
  assert.equal('secret' in d.decodeKey(fixture('key_created')), false);
});

test('a pool decodes its policy, status, members, and traffic', () => {
  const [p] = fixture('pools').pools.map(d.decodePool);
  assert.equal(p.name, 'Main');
  assert.equal(p.strategy, 'least_utilized');
  assert.equal(p.keepalive.costCapUsdPerDay, 4);
  assert.equal(p.keepalive.warmSubagents, null);
  assert.equal(p.cacheTtl, null);
  assert.equal(p.status, 'active');
  assert.equal(p.traffic24h.length, 24);
  assert.ok(isIso(p.traffic24h[0].at));
  assert.deepEqual(p.members.map((m) => m.accountId), ['business', 'direct']);
  assert.deepEqual(p.members.map((m) => m.eligible), ['eligible', 'eligible'], 'both members are servable in the captured pool');
  assert.equal(d.decodePool({ ...fixture('pools').pools[0], members: [{ account_id: 'business', priority: 0, eligible: 'overage' }] }).members[0].eligible, 'overage');
  assert.equal(d.decodePool({ ...fixture('pools').pools[0], members: [{ account_id: 'business', priority: 0 }] }).members[0].eligible, 'unavailable', 'a member row without the field reads as not servable');
});

test('a request page carries the trail with zero-based attempt indexes and the cursor', () => {
  const page = d.decodeRequestPage(fixture('requests'));
  assert.equal(page.nextCursor, null);
  const failover = page.requests.find((r) => r.id === 'r-failover');
  assert.deepEqual(failover.attempts.map((a) => [a.index, a.accountId, a.upstreamStatus, a.outcome]), [[0, 'direct', 429, 'rate_limited'], [1, 'business', 200, 'succeeded']]);
  assert.equal(failover.attempts[0].cooldownSet.reason, 'account');
  assert.ok(isIso(failover.at) && isIso(failover.attempts[0].startedAt));
  assert.equal(failover.status, 200);
  assert.equal(failover.transport, 'sse');
  assert.equal(failover.usage.cacheRead, 500);
  const failed = page.requests.find((r) => r.id === 'r-failed');
  assert.equal(failed.errorCode, 'upstream_error');
  assert.equal(failed.outcome, 'failed');
  assert.equal(d.decodeRequest(fixture('request').request).id, 'r-failover');
  assert.equal(d.decodeBodies(fixture('request_bodies')).request.model, 'claude-sonnet-5');
});

test('analytics groups and points keep the histograms and reasons whole', () => {
  const rows = fixture('analytics_breakdown').rows.map(d.decodeAnalyticsGroup);
  const desktop = rows.find((g) => g.key === 'desktop');
  assert.equal(desktop.requests, 3);
  assert.equal(desktop.histograms.ttfbMs.length, 64);
  assert.equal(typeof desktop.percentiles.ttfbMs.p50, 'number');
  assert.equal(desktop.cacheMissReasons.none, 3);
  assert.equal(desktop.savings.wastedProbeReasons.idle_limit, 0);
  assert.equal(desktop.errors.upstream_error, 1);
  const points = fixture('analytics_series').points.map(d.decodeAnalyticsPoint);
  assert.ok(points.length >= 24 && isIso(points[0].at));
  const rollups = d.decodeAccountRollups(fixture('analytics_accounts'));
  assert.ok(rollups.every((r) => isIso(r.hour) && typeof r.peak['5h'] === 'number'));
});

test('request rows fold into analytics groups with the same sums the routes answer', () => {
  const page = d.decodeRequestPage(fixture('requests'));
  const [all] = d.foldGroups(page.requests, 'all');
  assert.equal(all.requests, 3);
  assert.equal(all.succeeded, 2);
  assert.equal(all.failed, 1);
  assert.equal(all.attempts, 4);
  assert.equal(all.failovers, 1);
  assert.equal(all.tokens.cacheRead, 1500);
  assert.equal(all.histograms.ttfbMs.reduce((a, b) => a + b, 0), 3);
  assert.equal(all.cacheMissReasons.none, 3);
  assert.equal(all.probes.ok, 0);
  const byKey = d.foldGroups(page.requests, 'key');
  assert.deepEqual(byKey.map((g) => g.key), ['desktop']);
  const points = d.foldRequests(page.requests, 3_600_000, page.requests[2].at, page.requests[0].at);
  assert.ok(points.length >= 1 && points.every((p) => p.groups.length === 1));
});

test('the fold uses the ledger formulas for failovers, throughput, and cache misses', () => {
  const base = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-captured');
  const attempt = base.attempts[0];
  // One attempt pair on the same account is a retry, not a failover.
  const retried = { ...base, attempts: [attempt, { ...attempt, index: 1 }] };
  assert.equal(d.foldGroups([retried], 'all')[0].failovers, 0);
  assert.equal(d.foldGroups([{ ...retried, attempts: [attempt, { ...attempt, accountId: 'other', index: 1 }] }], 'all')[0].failovers, 1);
  // 100 tokens with first content at nine seconds of ten: 100 tokens per second over the last second.
  const slowStart = { ...base, durationMs: 10_000, ttftMs: 9_000, usage: { ...base.usage, output: 100 } };
  const [g] = d.foldGroups([slowStart], 'all');
  assert.ok(g.percentiles.outputTps.p50 >= 90 && g.percentiles.outputTps.p50 <= 110, `p50 ${g.percentiles.outputTps.p50} is the bucket holding 100`);
  assert.equal(d.foldGroups([{ ...slowStart, ttftMs: null }], 'all')[0].histograms.outputTps.reduce((a, b) => a + b, 0), 0, 'no first-content timing, no sample');
  // An API-key request with writes and no diagnostic is a miss by tokens, whatever the reason says.
  const apiKey = { ...base, attempts: [{ ...attempt, cacheMissReason: null }], usage: { ...base.usage, cacheRead: 0, cacheWrite5m: 300 } };
  const [m] = d.foldGroups([apiKey], 'all');
  assert.deepEqual([m.cacheHits, m.cacheMisses, m.uncached], [0, 1, 0]);
  assert.equal(m.cacheMissReasons.none, 0, 'the reason breakdown stays its own count');
});

test('the downstream snippet keeps the placeholder and names the inference listener when it is known', () => {
  const known = d.snippetFor('127.0.0.1:8080').split('\n');
  assert.equal(known.length, 4);
  assert.equal(known[0], '# inference is served on 127.0.0.1:8080; use the URL that reaches that listener');
  assert.equal(known[1], `ANTHROPIC_BASE_URL=${d.PLACEHOLDER_URL}`, 'the admin origin is never substituted here');
  const unknown = d.snippetFor(null).split('\n');
  assert.deepEqual(unknown, [`ANTHROPIC_BASE_URL=${d.PLACEHOLDER_URL}`, 'CLAUDE_CODE_OAUTH_TOKEN=<api key>', '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1']);
});

test('a session fold groups on the client each row named', () => {
  const [captured, failed] = d.decodeRequestPage(fixture('requests')).requests;
  assert.equal(captured.userAgent, 'claude-cli/2.1.269 (external, cli)', 'the row carries the agent the route answers');
  const rows = [captured, { ...failed, userAgent: 'phoebis-batch/0.9 (external, ci)' }, { ...failed, id: 'r-none', userAgent: null }];
  const groups = d.foldGroups(rows, 'user_agent');
  assert.deepEqual(groups.map((g) => g.key).sort(), ['claude-cli/2.1.269 (external, cli)', 'phoebis-batch/0.9 (external, ci)', 'unknown']);
  assert.deepEqual(groups.map((g) => g.requests), [1, 1, 1]);
});

test('a folded bucket carries one group per key the page groups on', () => {
  const [captured, failed] = d.decodeRequestPage(fixture('requests')).requests;
  const rows = [captured, { ...failed, keyId: 'laptop' }];
  const [bucket] = d.foldRequests(rows, 3_600_000, failed.at, captured.at, 'key');
  assert.deepEqual(bucket.groups.map((g) => g.key).sort(), ['desktop', 'laptop']);
  assert.deepEqual(d.foldRequests(rows, 3_600_000, failed.at, captured.at)[0].groups.map((g) => g.key), ['all'], 'the ungrouped default is unchanged');
  assert.equal(d.foldRequests(rows, 3_600_000, '1970-01-01T00:00:00.000Z', '1970-01-01T01:00:00.000Z').length, 1, 'a range ending on a bucket start adds no empty trailing bucket');
});

test('a session view folds every page the cursor names', () => {
  const page = d.decodeRequestPage(fixture('requests'));
  const pages = [{ nextCursor: 'c1', requests: page.requests.slice(0, 2) }, { nextCursor: null, requests: page.requests.slice(2) }];
  let calls = 0;
  const load = async (cursor) => { calls += 1; return cursor === undefined ? pages[0] : pages[1]; };
  return d.loadSessionRequests(load, 20).then(({ partial, requests }) => {
    assert.equal(calls, 2);
    assert.equal(requests.length, 3, 'both pages folded');
    assert.equal(partial, false);
    return d.loadSessionRequests(async () => ({ nextCursor: 'more', requests: page.requests }), 2);
  }).then(({ partial, requests }) => {
    assert.equal(partial, true, 'the page bound marks the result partial');
    assert.equal(requests.length, 6);
  });
});

test('the dashboard, sessions, recipes, and overview decode', () => {
  const o = d.decodeDashboard(fixture('dashboard'));
  assert.equal(o.hourly.length, 24);
  assert.equal(o.requests24h, 3);
  const sessions = fixture('sessions').sessions.map(d.decodeSession);
  const s1 = sessions.find((s) => s.sessionId === 's1');
  assert.equal(s1.companion, 'reporting');
  assert.equal(s1.keepalive.state, 'busy');
  assert.equal(s1.requests, 2);
  assert.ok(isIso(s1.lastUsedAt));
  const [recipe] = fixture('keepalive_recipes').recipes.map(d.decodeRecipe);
  assert.equal(recipe.state, 'armed');
  assert.equal(recipe.reason, null);
  const ov = d.decodeKeepaliveOverview(fixture('keepalive_overview'));
  assert.equal(ov.sessionsWithCompanion, 1);
  assert.equal(ov.costToday[0].capUsd, 4);
});

test('settings, pricing, the import preview, pairing, quota, and usage decode', () => {
  const s = d.decodeSettings(fixture('settings'));
  assert.equal(s.access.viewerConfigured, true);
  assert.equal(s.about.publicUrl, null);
  assert.equal(s.sessions.expireAfterSeconds, 86400);
  assert.equal(s.keepalive.maxWorkers, 4);
  assert.equal(s.listeners.metrics, '127.0.0.1:9464');
  assert.ok(isIso(s.lastSyncAt), 'the route answers the newest catalog sync as milliseconds');
  assert.equal(d.decodeSettings({ ...fixture('settings'), last_sync_at: null }).lastSyncAt, null, 'a registry that never synced answers no instant, and the strip says so');
  assert.equal(s.pricing.models.find((m) => m.model === 'claude-sonnet-5').inputPerM, 2);
  const preview = d.decodePriceImport(fixture('pricing_import'));
  // The imported file carries the one model the router serves, at the five prices the shipped table already
  // holds, so an exact diff would count no change. `diff_against` compares the five fields with `!=`, and the
  // file's 2e-07 per token reads back as 0.19999999999999998 per million against a stored 0.2, so the route
  // answers one. This pins the route's answer, artefact and all; the tolerance belongs to the import, not here.
  assert.equal(preview.inFile, 1);
  assert.equal(preview.changes, 1);
  assert.ok(preview.token.startsWith('imp_'));
  const pairing = d.decodePairingSession(fixture('pairing_started'));
  assert.ok(pairing.id && pairing.state && pairing.authorizeUrl.includes('state=') && isIso(pairing.expiresAt));
  const history = d.decodeQuotaHistory(fixture('account_quota'));
  assert.deepEqual(history.map((h) => h.windowId), ['5h', '7d', '7d_fable']);
  assert.ok(history.every((h) => h.samples.every((x) => isIso(x.at))));
  const snapshot = d.decodeSnapshot('business', fixture('account_usage'));
  assert.equal(snapshot.status, 'success');
  assert.deepEqual(d.decodeCatalog([d.decodeAccount(fixture('account'))]).map((m) => [m.id, m.longContext]), [['claude-sonnet-5', true]]);
});

test('the live snapshot and every delta decode with ISO times and null-safe fields', () => {
  const { snapshot, deltas } = fixture('live_messages');
  const snap = d.decodeLiveSnapshot(snapshot.snapshot);
  assert.ok(isIso(snap.tick.at) && Array.isArray(snap.tick.metrics.ttfbP50.series));
  const types = deltas.map((m) => d.decodeLiveDelta(m.delta)?.type);
  assert.deepEqual(types.filter((t) => t !== 'tick'), ['started', 'selected', 'head', 'progress', 'preview', 'finished']);
  const started = d.decodeLiveDelta(deltas[0].delta);
  assert.equal(started.request.attempts.length, 0);
  assert.ok(isIso(started.request.phases.receivedAt) && started.request.phases.headAt === undefined);
  const finished = d.decodeLiveDelta(deltas.find((m) => m.delta.type === 'finished').delta);
  assert.equal(finished.status, 200);
  assert.ok(isIso(finished.phases.finishedAt) && isIso(finished.phases.firstContentAt));
  assert.deepEqual(finished.attempts.map((a) => [a.index, a.outcome]), [[0, 'succeeded']], 'the request ends with the attempts the feed settled, so a row leaves in flight behind');
  const failed = d.decodeLiveDelta({ ...deltas.find((m) => m.delta.type === 'finished').delta, attempts: [{ account_id: 'business', cooldown_set: null, index: 1, outcome: 'transport_error', started_at: 1000005, upstream_status: null, windows: [] }] });
  assert.deepEqual(failed.attempts.map((a) => a.outcome), ['transport_error']);
  assert.equal(d.decodeLiveDelta({ type: 'nope', cursor: 1 }), null);
});

test('alert rules, events, and deliveries decode with the secret reduced to has_secret', () => {
  const rules = fixture('alert_rules').rules.map(d.decodeAlertRule);
  const util = rules.find((r) => r.kind === 'utilization_above');
  assert.equal(util.hasSecret, true);
  assert.equal(util.windowId, '7d_oi');
  assert.equal(util.threshold, 0.9);
  assert.deepEqual(util.scope, { id: 'main', type: 'pool' });
  assert.ok(isIso(util.createdAt) && !('secret' in util));
  assert.equal(rules.find((r) => r.kind === 'client_drift').enabled, false);
  const [event] = fixture('alert_events').events.map(d.decodeAlertEvent);
  assert.equal(event.state, 'open');
  assert.equal(event.detail.trigger, 'hash');
  assert.deepEqual(event.deliveries.map((x) => [x.attempt, x.status, x.error]), [[1, null, 'refused'], [2, 500, null], [3, 200, null]]);
  assert.ok(isIso(event.openedAt) && event.resolvedAt === null);
  assert.equal(fixture('alert_deliveries').deliveries.map(d.decodeDelivery).length, 3);
});
