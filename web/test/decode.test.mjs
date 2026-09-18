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
