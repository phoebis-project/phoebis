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
  assert.deepEqual(Object.keys(a.subscription).sort(), ['billingType', 'organizationType', 'rateLimitTier'], "the route answers the profile's three labels and nothing more, so the card draws nothing beside them");
  assert.deepEqual(fixture('account').windows.map((w) => w.id), ['7d', 'fable', 'five_hour'], 'the route passes each sampler spelling through, in the ledger order');
  assert.deepEqual(a.windows.map((w) => w.id), ['5h', '7d'], 'the poll spelling folds onto the UI id and the order is the drawn one');
  assert.equal(a.fableWindow.id, '7d_fable');
  assert.ok(isIso(a.windows[0].resetsAt) && isIso(a.windows[0].observedAt));
  assert.equal(a.windows[0].utilization, 0.62, 'the five_hour sample is the 5h window');
  assert.deepEqual(a.pools, [{ poolId: 'main', priority: 0 }]);
  assert.equal(a.extraUsage.monthlyLimitCredits, 5000);
  assert.equal(a.requests.lifetime, 4);
  assert.ok(a.servedModels.some((m) => m.id === 'claude-sonnet-5' && m.selected));
  assert.equal(a.status, 'ready');
  assert.equal(a.email, 'test@example.invalid');
});

test('an account decodes its catalog error, its quarantine flag, and the router\'s overage evidence', () => {
  const body = fixture('account');
  const quiet = d.decodeAccount(body);
  assert.deepEqual([quiet.catalogError, quiet.credential.quarantined, quiet.overage], [body.catalog_error, false, null]);
  const loud = d.decodeAccount({ ...body, catalog_error: 'the models request answered 503', credential: { ...body.credential, quarantined: true }, overage: { in_use: true, observed_at: 3_000, reset: 9_000, since: 1_000 } });
  assert.equal(loud.catalogError, 'the models request answered 503');
  assert.equal(loud.credential.quarantined, true);
  assert.deepEqual(loud.overage, { inUse: true, observedAt: d.iso(3_000), reset: d.iso(9_000), since: d.iso(1_000) });
  assert.deepEqual(d.decodeAccount({ ...body, overage: { in_use: false, observed_at: 3_000, reset: null, since: null } }).overage, { inUse: false, observedAt: d.iso(3_000), reset: null, since: null }, 'a sample reports no start, so the line under the badge has nothing to count from');
  assert.equal(d.decodeAccount({ ...body, credential: null }).credential.quarantined, false, 'a slot with no credential has no attempt to be unsure about');
});

test('extra usage the provider switched off keeps the reason it gave', () => {
  const body = fixture('account');
  const off = d.decodeAccount({ ...body, extra_usage: { ...body.extra_usage, disabled_reason: 'card declined', in_use: false, is_enabled: false } }).extraUsage;
  assert.deepEqual([off.enabled, off.inUse, off.disabledReason], [false, false, 'card declined'], 'the reason exists for exactly the account whose extra usage is off');
  assert.equal(d.decodeAccount({ ...body, extra_usage: null }).extraUsage, null);
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
  assert.equal(typeof k.sessionsOnRetiredSecret, 'number');
  assert.equal(k.retiredSecretLastUsedAt, null, 'a key whose retired secret never authenticated carries no time');
  assert.equal(
    d.decodeKey({ ...fixture('keys').keys[0], retired_secret_last_used_at: 1790029200000 }).retiredSecretLastUsedAt,
    new Date(1790029200000).toISOString(),
    'a retired secret carries the instant it was last used, not just a type',
  );
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], revoked_at: 5 }).status, 'revoked');
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], expires_at: 5 }).status, 'expired');
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], enabled: false }).status, 'disabled', 'routing refuses a key that is switched off, so the row may not read active');
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], enabled: false, revoked_at: 5 }).status, 'revoked', 'the more final state wins');
  assert.equal(d.decodeKey({ ...fixture('keys').keys[0], enabled: false, expires_at: 5 }).status, 'expired');
  assert.equal('secret' in d.decodeKey(fixture('key_created')), false);
});

test('a key keeps its enabled flag beside the status the flag folds into', () => {
  const row = fixture('keys').keys[0];
  assert.equal(d.decodeKey(row).enabled, true);
  const off = d.decodeKey({ ...row, enabled: false });
  assert.deepEqual([off.enabled, off.status], [false, 'disabled']);
  const lapsed = d.decodeKey({ ...row, enabled: false, expires_at: 5 });
  assert.deepEqual([lapsed.enabled, lapsed.status], [false, 'expired'], 'an expired key that was also switched off says so, which the status alone cannot');
  const silent = { ...row };
  delete silent.enabled;
  assert.equal(d.decodeKey(silent).enabled, true, 'a row without the flag reads as on, as its status does');
});

test('a key edit sends enabled every time and expires_at only when the dialog changed it', () => {
  const input = { allowedModels: null, bodyCaptureHours: null, enabled: false, keepaliveOptOut: false, name: 'batch', notes: null, poolId: 'codex' };
  const kept = d.encodeKey({ ...input, expiresAt: undefined });
  assert.equal(kept.enabled, false);
  assert.equal('expires_at' in kept, false, 'an absent expiry leaves the stored one alone');
  assert.equal(d.encodeKey({ ...input, expiresAt: null }).expires_at, null, 'null clears the expiry');
  assert.equal(d.encodeKey({ ...input, enabled: true, expiresAt: '2026-10-01T00:00:00.000Z' }).expires_at, Date.parse('2026-10-01T00:00:00.000Z'), 'the route takes milliseconds');
  assert.equal(d.encodeKey({ ...input, enabled: true, expiresAt: undefined }).enabled, true);
});

test('a pool decodes its policy, status, members, and traffic', () => {
  const [p] = fixture('pools').pools.map(d.decodePool);
  assert.equal(p.name, 'Main');
  assert.equal(p.strategy, 'least_utilized');
  assert.equal(p.keepalive.costCapUsdPerDay, 4);
  assert.equal(p.keepalive.warmSubagents, true);
  assert.equal(p.cacheTtl.enabled, false);
  assert.equal(p.cacheTtl.main, '1h');
  assert.equal(p.cacheDiagnostics, null);
  assert.equal(p.status, 'active');
  assert.equal(p.traffic24h.length, 24);
  assert.ok(isIso(p.traffic24h[0].at));
  assert.deepEqual(p.members.map((m) => m.accountId), ['business', 'direct']);
  assert.deepEqual(p.members.map((m) => m.eligible), ['eligible', 'eligible'], 'both members are servable in the captured pool');
  assert.equal(d.decodePool({ ...fixture('pools').pools[0], members: [{ account_id: 'business', priority: 0, eligible: 'overage' }] }).members[0].eligible, 'overage');
  assert.equal(d.decodePool({ ...fixture('pools').pools[0], members: [{ account_id: 'business', priority: 0 }] }).members[0].eligible, 'unavailable', 'a member row without the field reads as not servable');
});

test('a pool decodes its select-below threshold and encodes it back', () => {
  const row = fixture('pools').pools[0];
  assert.equal(d.decodePool(row).selectBelowUtilization, null, 'the captured pool sets none');
  const p = d.decodePool({ ...row, select_below_utilization: 0.4 });
  assert.equal(p.selectBelowUtilization, 0.4);
  const back = d.encodePool({ ...p, name: p.name, status: 'active' });
  assert.equal(back.select_below_utilization, 0.4);
  assert.equal(d.encodePool({ ...p, name: p.name, selectBelowUtilization: null, status: 'active' }).select_below_utilization, null, 'null clears it rather than leaving it alone');
});

test('a pool decodes round robin, both subscription flags, and the sub-strategy, and encodes the stored one back', () => {
  const wire = { ...fixture('pools').pools[0], strategy: 'round_robin', prefer_subscription: true, prefer_subscription_effective: true, api_key_sub_strategy: 'round_robin' };
  const p = d.decodePool(wire);
  assert.deepEqual([p.strategy, p.preferSubscription, p.preferSubscriptionEffective, p.apiKeySubStrategy], ['round_robin', true, true, 'round_robin']);
  const back = d.encodePool({ ...p, name: p.name, status: 'active' });
  assert.deepEqual([back.strategy, back.prefer_subscription, back.api_key_sub_strategy], ['round_robin', true, 'round_robin']);
  assert.equal('prefer_subscription_effective' in back, false, 'the effective value is the route\'s answer, never an input');
  const forced = d.decodePool({ ...fixture('pools').pools[0], strategy: 'least_utilized', prefer_subscription: false, prefer_subscription_effective: true });
  assert.deepEqual([forced.preferSubscription, forced.preferSubscriptionEffective], [false, true], 'the stored answer and the forced order are read apart');
  assert.equal(d.decodePool(fixture('pools').pools[0]).apiKeySubStrategy, 'priority_random', 'the recorded row carries the default');
});

test('a pool decodes its cache TTL policy, its diagnostics override, and warm_subagents, and encodes them back', () => {
  const row = fixture('pools').pools[0];
  const wire = {
    ...row,
    cache_ttl: { enabled: true, main: '5m', subagent: '1h', in_overage: '5m', models: [{ model: 'claude-haiku-4-5-20251001', main: '5m', subagent: '5m' }] },
    cache_diagnostics: true,
    keepalive: { ...row.keepalive, warm_subagents: false },
  };
  const p = d.decodePool(wire);
  assert.deepEqual([p.cacheTtl.enabled, p.cacheTtl.main, p.cacheTtl.subagent, p.cacheTtl.inOverage], [true, '5m', '1h', '5m']);
  assert.deepEqual(p.cacheTtl.models, [{ main: '5m', model: 'claude-haiku-4-5-20251001', subagent: '5m' }]);
  assert.equal(p.cacheDiagnostics, true);
  assert.equal(p.keepalive.warmSubagents, false);
  const back = d.encodePool({ ...p, name: p.name, status: 'active' });
  assert.equal(back.cache_ttl.enabled, true);
  assert.equal(back.cache_ttl.in_overage, '5m');
  assert.equal(back.cache_ttl.models[0].model, 'claude-haiku-4-5-20251001');
  assert.equal(back.cache_diagnostics, true);
  assert.equal(back.keepalive.warm_subagents, false);
  assert.equal(d.decodePool({ ...row, cache_diagnostics: false }).cacheDiagnostics, false);
});

test('a pool the route answers without a cache TTL policy decodes to the defaults', () => {
  const wire = { ...fixture('pools').pools[0] };
  delete wire.cache_ttl;
  assert.deepEqual(d.decodePool(wire).cacheTtl, { enabled: false, inOverage: null, main: '1h', models: [], subagent: '5m' });
  const partial = d.decodePool({ ...wire, cache_ttl: { enabled: true } });
  assert.deepEqual([partial.cacheTtl.enabled, partial.cacheTtl.main, partial.cacheTtl.subagent, partial.cacheTtl.inOverage], [true, '1h', '5m', null], 'a field the route leaves out falls back one field at a time');
  const keepalive = { ...wire.keepalive };
  delete keepalive.warm_subagents;
  assert.equal(d.decodePool({ ...wire, keepalive }).keepalive.warmSubagents, true, 'a keepalive block without the flag warms subagents');
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
  assert.equal(failover.usage.thinking, 40);
  assert.deepEqual(failover.shape, { contextEdits: ['clear_thinking_20251015'], effort: 'high', maxTokens: 32_000, messageCount: 1, requestedServiceTier: null, requestedSpeed: null, systemBlocks: 4, temperature: null, thinkingBudget: 31_999, thinkingType: 'enabled', toolCount: 12 });
  assert.deepEqual(failover.attempts[1].response, { clearedInputTokens: null, inferenceGeo: 'us', refusalCategory: null, refusalRecommendedModel: null, servedModel: null, speed: 'standard', stopReason: 'end_turn', stopSequence: null });
  assert.equal(failover.responseTruncated, false);
  const failed = page.requests.find((r) => r.id === 'r-failed');
  assert.equal(failed.errorCode, 'upstream_error');
  assert.equal(failed.clientDrift, 'hash');
  assert.equal(failed.outcome, 'failed');
  assert.equal(d.decodeRequest(fixture('request').request).id, 'r-failover');
  const moved = fixture('request').events.map(d.decodeRequestEvent);
  assert.deepEqual([moved[0].kind, moved[0].accountId, moved[0].detail.from_account, moved[0].detail.reason], ['pin_migrated', 'business', 'direct', 'spent']);
  assert.deepEqual([moved[0].keyId, moved[0].sessionId, moved[0].model], ['desktop', 's1', 'claude-sonnet-5'], 'the pin the event belongs to');
  assert.ok(isIso(moved[0].at));
  assert.deepEqual([moved[1].kind, moved[1].accountId, moved[1].detail.reasons], ['pin_expired', 'business', ['cooldown', 'spent']]);
  assert.equal(d.decodeRequest(fixture('request').request).canonicalModel, 'claude-sonnet-5');
});

test('a request row carries the generation of the key it was read under', () => {
  const body = fixture('requests');
  const live = body.requests.find((r) => r.id === 'r-captured').key_generation;
  const earlier = body.requests.find((r) => r.id === 'r-earlier-holder').key_generation;
  assert.ok(Number.isInteger(live) && Number.isInteger(earlier) && live > earlier, 'the fixture holds two generations of desktop');
  const rows = d.decodeRequestPage(body).requests;
  assert.deepEqual(rows.map((r) => [r.id, r.keyGeneration]), body.requests.map((r) => [r.id, r.key_generation]));
  assert.equal(d.decodeRequest(fixture('request').request).keyGeneration, fixture('request').request.key_generation, 'the detail route answers the same row');
});

test('an attempt window with a reset and no utilization reads as not reported, not as zero', () => {
  const windows = [{ id: '5h', resets_at: 1_000, utilization: null }, { id: '7d', resets_at: 2_000, utilization: 0.5 }];
  const [row] = d.decodeRequestPage({ next_cursor: null, requests: [{ attempts: [{ index: 1, windows }] }] }).requests;
  assert.deepEqual(row.attempts[0].windows.map((w) => [w.id, w.utilization]), [['5h', null], ['7d', 0.5]]);
  const live = d.decodeLiveRequest({ attempts: [{ index: 1, windows }] });
  assert.deepEqual(live.attempts[0].windows.map((w) => w.utilization), [null, 0.5], 'the live feed reads its windows through the same decoder');
});

test('an account hour with no utilization sample carries no peak rather than a zero one', () => {
  const [hour] = d.decodeAccountRollups(fixture('analytics_accounts'));
  const wire = fixture('analytics_accounts').accounts[0].hours[0];
  assert.deepEqual([wire.utilization_5h_max, wire.utilization_7d_max], [0.4, null], 'the fixture samples the 5-hour window alone');
  assert.deepEqual([hour.peak['5h'], hour.peak['7d'], hour.peak['7d_fable']], [0.4, null, null]);
});

test('a captured body carries the bytes the route counted, never the bytes the drawer would measure', () => {
  const whole = d.decodeBodies(fixture('request_bodies'));
  assert.equal(whole.request.model, 'claude-sonnet-5');
  assert.equal(whole.requestBytesStored, whole.requestBytesSeen, 'nothing was cut, so both sides read the same count');
  assert.equal(whole.responseTruncated, false);
  // A stream is stored as its own text: rendering it back through JSON.stringify adds the escapes, and a
  // length measured there would answer less than the stored count rather than the ten bytes that were cut.
  const stream = d.decodeBodies({ request_id: 'r-cut', response: 'event: message_start\ndata: {"type":"message_start"}\n\n', response_bytes_stored: 8_388_608, response_bytes_seen: 8_388_618, response_truncated: true });
  assert.equal(d.bytesDropped(stream.responseBytesSeen, stream.responseBytesStored), 10);
  const multibyte = d.decodeBodies({ request_id: 'r-utf8', request: { text: 'héllo' }, request_bytes_stored: 17, request_bytes_seen: 17 });
  assert.equal(multibyte.requestBytesStored, 17, 'the seventeen bytes the body encodes to, not its sixteen characters');
  assert.equal(d.bytesDropped(multibyte.requestBytesSeen, multibyte.requestBytesStored), 0);
  const cut = d.decodeBodies({ request_id: 'r-json', response: '{"content":[{"type":"text","text":"aaa', response_bytes_stored: 8_388_608, response_bytes_seen: 8_389_632, response_truncated: true });
  assert.equal(cut.responseTruncated, true);
  assert.equal(d.bytesDropped(cut.responseBytesSeen, cut.responseBytesStored), 1_024);
});

test('analytics groups and points keep the histograms and reasons whole', () => {
  const rows = fixture('analytics_breakdown').rows.map(d.decodeAnalyticsGroup);
  const desktop = rows.find((g) => g.key === 'desktop');
  assert.equal(desktop.requests, 3);
  const earlier = rows.find((g) => g.key.startsWith('desktop@'));
  assert.equal(earlier.label, 'desktop (deleted)');
  assert.equal(earlier.requests, 1);
  assert.equal(desktop.histograms.ttfbMs.length, 64);
  assert.equal(typeof desktop.percentiles.ttfbMs.p50, 'number');
  assert.equal(desktop.cacheMissReasons.none, 3);
  assert.equal(desktop.tokens.thinking, 120);
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
  assert.equal(all.requests, 4);
  assert.equal(all.succeeded, 3);
  assert.equal(all.failed, 1);
  assert.equal(all.attempts, 5);
  assert.equal(all.failovers, 1);
  assert.equal(all.tokens.cacheRead, 2000);
  assert.equal(all.histograms.ttfbMs.reduce((a, b) => a + b, 0), 4);
  assert.equal(all.cacheMissReasons.none, 4);
  assert.equal(all.probes.ok, 0);
  const captured = page.requests.find((r) => r.id === 'r-captured');
  const busy = d.foldGroups([{ ...captured, usage: { ...captured.usage, webSearch: 3, webFetch: 1 } }, captured], 'all')[0].tokens;
  assert.equal(busy.webSearch, 3 + captured.usage.webSearch);
  assert.equal(busy.webFetch, 1 + captured.usage.webFetch);
  const byKey = d.foldGroups(page.requests, 'key');
  assert.deepEqual(byKey.map((g) => g.label), ['desktop', 'desktop (deleted)'], 'a reused id is one group per holder, as the breakdown answers it');
  // No captured breakdown groups on effort, so the fold is where the dimension is proven.
  assert.deepEqual(d.foldGroups(page.requests, 'effort').map((g) => g.key), ['high']);
  assert.deepEqual(d.foldGroups([{ ...captured, shape: { ...captured.shape, effort: null } }], 'effort').map((g) => g.key), ['none'], 'a request that asked for no effort files under none, as the rollups file it');
  const oldest = page.requests.find((r) => r.id === 'r-failover');
  const newest = page.requests.find((r) => r.id === 'r-captured');
  const points = d.foldRequests(page.requests, 3_600_000, oldest.at, newest.at);
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

test('a cache-miss reason the console does not know keeps its own name on the attempt and in every sum', () => {
  const body = fixture('requests');
  const wire = body.requests.find((r) => r.id === 'r-captured');
  const row = d.decodeRequest({ ...wire, attempts: wire.attempts.map((a) => ({ ...a, cache_miss_reason: 'future_reason' })) });
  assert.ok(row.attempts.every((a) => a.cacheMissReason === 'future_reason'), 'the attempt answers the reason as sent rather than reading unavailable');
  assert.equal(d.decodeRequest({ ...wire, attempts: wire.attempts.map((a) => ({ ...a, cache_miss_reason: null })) }).attempts[0].cacheMissReason, null);
  const group = d.decodeAnalyticsGroup({ ...fixture('analytics_breakdown').rows[0], cache_miss_reasons: { future_reason: 3, none: 2 } });
  assert.equal(group.cacheMissReasons.future_reason, 3, 'the breakdown keeps the unknown key under its name');
  assert.equal(group.cacheMissReasons.none, 2);
  assert.equal(group.cacheMissReasons.not_requested, 0, 'every known reason stays present so each column draws');
  const [folded] = d.foldGroups([row], 'all');
  assert.equal(folded.cacheMissReasons.future_reason, 1, 'a session fold counts it under its name too');
  const summed = d.sumGroups([group, group, folded]);
  assert.equal(summed.cacheMissReasons.future_reason, 7);
  assert.equal(summed.cacheMissReasons.none, 4);
  assert.deepEqual(d.missReasonOrder([group, { cacheMissReasons: { a_reason: 1, none: 0 } }]), [...d.CACHE_MISS_REASONS, 'a_reason', 'future_reason'], 'the known eight in reading order, then the rest alphabetically');
  assert.equal(d.missLabel('future_reason'), 'future_reason');
  assert.equal(d.missLabel('messages_changed'), 'History edited');
});

test('the session fold buckets a range the way the analytics routes do', () => {
  const hour = 3_600_000;
  const day = 24 * hour;
  assert.equal(d.bucketSizeFor(48 * hour), hour);
  assert.equal(d.bucketSizeFor(48 * hour + 1), 6 * hour);
  assert.equal(d.bucketSizeFor(20 * day), 6 * hour, 'a twenty-day range buckets at six hours, as the route answers it');
  assert.equal(d.bucketSizeFor(30 * day), 6 * hour);
  assert.equal(d.bucketSizeFor(30 * day + 1), day);
});

test('a session fold marks what request rows cannot supply and a sum carries the gap rather than a number', () => {
  const rows = d.decodeRequestPage(fixture('requests')).requests;
  const [folded] = d.foldGroups(rows, 'all');
  assert.equal(folded.fromRequests, true);
  assert.equal(folded.savings, null);
  assert.equal(folded.keptWarm, null);
  assert.equal(folded.cost.cacheRead, null);
  assert.equal(folded.cost.cacheWrite, null);
  const route = d.decodeAnalyticsGroup(fixture('analytics_breakdown').rows[0]);
  assert.equal(route.fromRequests, undefined, 'a route group carries every figure');
  const mixed = d.sumGroups([route, folded]);
  assert.equal(mixed.cost.cacheRead, null, 'a null source makes the sum unavailable, never NaN');
  assert.equal(mixed.cost.cacheWrite, null);
  assert.equal(mixed.savings, null);
  assert.equal(mixed.keptWarm, null);
  assert.equal(mixed.fromRequests, true);
  assert.equal(mixed.costUsd, route.costUsd + folded.costUsd, 'the figures the rows do carry still sum');
  const routeOnly = d.sumGroups([route, route]);
  assert.equal(routeOnly.cost.cacheRead, route.cost.cacheRead * 2);
  assert.equal(routeOnly.savings.grossUsd, route.savings.grossUsd * 2);
  assert.equal(routeOnly.keptWarm.requests, route.keptWarm.requests * 2);
  assert.equal(routeOnly.fromRequests, undefined);
  const [bucket] = d.foldRequests(rows, 3_600_000, rows[rows.length - 1].at, rows[0].at);
  assert.ok(bucket.groups.every((g) => g.savings === null && g.cost.cacheRead === null), 'every folded bucket marks the same gaps');
});

test('a recipe held because every account of its pool was exhausted keeps that reason', () => {
  const [wire] = fixture('keepalive_recipes').recipes;
  const recipe = d.decodeRecipe({ ...wire, reason: 'pool_exhausted', state: 'held' });
  assert.equal(recipe.reason, 'pool_exhausted');
  assert.equal(recipe.state, 'held');
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
  assert.deepEqual(groups.map((g) => g.key).sort(), ['-', 'claude-cli/2.1.269 (external, cli)', 'phoebis-batch/0.9 (external, ci)'], 'a row that named no client falls under the marker the rollups file it as');
  assert.equal(groups.find((g) => g.key === '-').label, 'unattributed', 'which every route answers as unattributed');
  assert.deepEqual(groups.map((g) => g.requests), [1, 1, 1]);
  const named = d.foldGroups(rows, 'user_agent', { agentGroups: ['claude-cli/2.1.*', 'phoebis-batch/*'] });
  assert.deepEqual(named.map((g) => g.key).sort(), ['claude-cli/2.1.*', 'other', 'phoebis-batch/*'], 'the row that named no client falls under other, as the breakdown route files it');
  assert.deepEqual(named.map((g) => g.requests), [1, 1, 1]);
  assert.equal(d.foldGroups(rows, 'all', { agentPatterns: ['claude-cli/*'] })[0].requests, 1, 'the pattern keeps only the clients it admits');
});

test('a folded bucket carries one group per key the page groups on', () => {
  const [captured, failed] = d.decodeRequestPage(fixture('requests')).requests;
  const rows = [captured, { ...failed, keyId: 'laptop' }];
  const [bucket] = d.foldRequests(rows, 3_600_000, failed.at, captured.at, 'key');
  assert.deepEqual(bucket.groups.map((g) => g.label).sort(), ['desktop', 'laptop']);
  assert.deepEqual(d.foldRequests(rows, 3_600_000, failed.at, captured.at)[0].groups.map((g) => g.key), ['all'], 'the ungrouped default is unchanged');
  assert.equal(d.foldRequests(rows, 3_600_000, '1970-01-01T00:00:00.000Z', '1970-01-01T01:00:00.000Z').length, 1, 'a range ending on a bucket start adds no empty trailing bucket');
});

test('a session fold keeps two holders of one key id apart and labels the earlier one deleted', () => {
  const rows = d.decodeRequestPage(fixture('requests')).requests;
  const live = rows.find((r) => r.id === 'r-captured');
  const earlier = rows.find((r) => r.id === 'r-earlier-holder');
  const groups = d.foldGroups(rows, 'key');
  assert.deepEqual(
    groups.map((g) => [g.key, g.label, g.requests]),
    [[`desktop@${live.keyGeneration}`, 'desktop', 3], [`desktop@${earlier.keyGeneration}`, 'desktop (deleted)', 1]],
    'the key a group carries is the lifetime the requests route selects by',
  );
  assert.deepEqual(d.foldGroups([{ ...live, keyGeneration: 0, keyId: null }], 'key').map((g) => [g.key, g.label]), [['-@0', 'unattributed']]);
  const points = d.foldRequests([{ ...earlier, at: '1970-01-01T00:10:00.000Z' }, { ...live, at: '1970-01-01T01:10:00.000Z' }], 3_600_000, '1970-01-01T00:00:00.000Z', '1970-01-01T02:00:00.000Z', 'key');
  assert.deepEqual(points.map((p) => p.groups.map((g) => g.label)), [['desktop (deleted)'], ['desktop']], 'a bucket holding the earlier holder alone still labels it against the whole range');
});

test('a session view folds every page the cursor names', () => {
  const page = d.decodeRequestPage(fixture('requests'));
  const pages = [{ nextCursor: 'c1', requests: page.requests.slice(0, 2) }, { nextCursor: null, requests: page.requests.slice(2) }];
  let calls = 0;
  const load = async (cursor) => { calls += 1; return cursor === undefined ? pages[0] : pages[1]; };
  return d.loadSessionRequests(load, 20).then(({ partial, requests }) => {
    assert.equal(calls, 2);
    assert.equal(requests.length, 4, 'both pages folded');
    assert.equal(partial, false);
    return d.loadSessionRequests(async () => ({ nextCursor: 'more', requests: page.requests }), 2);
  }).then(({ partial, requests }) => {
    assert.equal(partial, true, 'the page bound marks the result partial');
    assert.equal(requests.length, 8);
  });
});

test('the dashboard reads the live pin count the route answers beside the rollups', () => {
  assert.equal(fixture('dashboard').sessions.active_pins, 0);
  assert.equal(d.decodeDashboard(fixture('dashboard')).activePins, 0);
  assert.equal(d.decodeDashboard({ ...fixture('dashboard'), sessions: { active_pins: 3 } }).activePins, 3);
});

test('the dashboard, sessions, recipes, and overview decode', () => {
  const o = d.decodeDashboard(fixture('dashboard'));
  assert.equal(o.hourly.length, 24);
  assert.equal(o.requests24h, 4);
  const sessions = fixture('sessions').sessions.map(d.decodeSession);
  const s1 = sessions.find((s) => s.sessionId === 's1');
  assert.ok(sessions.every((s) => typeof s.keyGeneration === 'number' && typeof s.historical === 'boolean'), 'every session says which generation of its key owns it and whether that holder is gone');
  assert.equal(sessions.find((s) => s.sessionId === 's0').historical, true, 'the row of an earlier holder of the same key id reads historical');
  assert.equal(s1.keyGeneration, fixture('sessions').sessions.find((s) => s.session_id === 's1').key_generation, 'the live holder names the generation its key row carries');
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

test('a settings answer with no compatibility block reads the defaults the router ships with', () => {
  const bare = { ...fixture('settings') };
  delete bare.compatibility;
  assert.deepEqual(d.decodeSettings(bare).compatibility, { cacheDiagnostics: false, outboundGzip: false, profile: 'claude-code-2.1.269-linux-x64', usageEntrypoint: 'cli' }, 'the profile falls back to the one the about block carries, and the rest to what the router does without the block');
  assert.deepEqual(d.decodeSettings({ ...bare, about: { ...bare.about, profile: 'claude-code-9.9.9-linux-x64' } }).compatibility.profile, 'claude-code-9.9.9-linux-x64');
});

test('settings decode the quota, routing, storage, and usage blocks the route answers', () => {
  const wire = fixture('settings');
  const s = d.decodeSettings(wire);
  assert.deepEqual(s.quota, { pollIntervalSeconds: wire.quota.poll_interval_seconds, sampleRetentionDays: wire.quota.sample_retention_days });
  assert.deepEqual(s.routing, { catalogSyncIntervalSeconds: wire.routing.catalog_sync_interval_seconds, cooldownFallbackSeconds: wire.routing.cooldown_fallback_seconds, maxAttempts: wire.routing.max_attempts });
  assert.deepEqual(s.storage, { ledgerPath: wire.storage.ledger_path, path: wire.storage.path, pricingPath: null }, 'no override file is a null path, never an empty one');
  assert.deepEqual(s.usage, { cacheTtlSeconds: wire.usage.cache_ttl_seconds, refreshConcurrency: wire.usage.refresh_concurrency });
  assert.equal(d.decodeSettings({ ...wire, storage: { ...wire.storage, pricing_path: './pricing.yaml' } }).storage.pricingPath, './pricing.yaml');
});

test('settings, pricing, the import preview, pairing, quota, and usage decode', () => {
  const s = d.decodeSettings(fixture('settings'));
  assert.equal(s.access.viewerConfigured, true);
  assert.equal(s.about.publicUrl, null);
  assert.equal(s.sessions.expireAfterSeconds, 86400);
  assert.equal(s.keepalive.maxWorkers, 4);
  assert.equal(s.listeners.metrics, '127.0.0.1:9464');
  assert.deepEqual(s.compatibility, { cacheDiagnostics: false, outboundGzip: false, profile: 'claude-code-2.1.269-linux-x64', usageEntrypoint: 'cli' });
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

test('a live request carries the client that sent it', () => {
  const started = fixture('live_messages').deltas.find((m) => m.delta?.type === 'started').delta.request;
  assert.equal(d.decodeLiveRequest({ ...started, user_agent: 'claude-cli/2.1.269 (external, cli)' }).userAgent, 'claude-cli/2.1.269 (external, cli)');
  assert.equal(d.decodeLiveRequest({ ...started, user_agent: null }).userAgent, null);
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
  const [life] = fixture('alert_lifecycle').deliveries.map(d.decodeLifecycleDelivery);
  assert.equal(life.event, 'session_started');
  assert.equal(life.detail.session, 'e2e-ui');
  assert.equal(life.kind, 'session_active');
  assert.equal(life.ruleId, fixture('alert_lifecycle').deliveries[0].rule_id, 'a lifecycle delivery names the rule that posted it, which no event row holds');
});

test('a question reads as redacted, expired, or missing from the wire detail and the page clock in milliseconds', () => {
  const [wire] = fixture('alert_events').events;
  const detail = { agent: null, key: 'desktop', question: null, question_expires_at: 1_000_000, session: 'sess-1' };
  const asked = d.decodeAlertEvent({ ...wire, detail, kind: 'ask_user_question' }).detail;
  assert.equal(asked.question_expires_at, 1_000_000, 'the deadline passes through as the number the route stores');
  assert.deepEqual(d.questionLine(asked, 1_000_000), { muted: true, text: 'question expired with its captured body' }, 'the route blanks the text at the deadline itself');
  assert.deepEqual(d.questionLine(asked, 999_999), { muted: true, text: 'question not captured' });
  assert.deepEqual(d.questionLine({ ...asked, question_expires_at: null }, 2_000_000), { muted: true, text: 'question not captured' });
  assert.deepEqual(d.questionLine({ ...asked, question_redacted: true }, 2_000_000), { muted: true, text: 'question is for administrators' }, 'a viewer is told who may read it, not that the capture failed');
  assert.deepEqual(d.questionLine({ ...asked, question: 'Which branch?' }, 2_000_000), { muted: false, text: 'asked: Which branch?' });
});

test('a lifecycle delivery whose event name this build does not know still decodes', () => {
  const [wire] = fixture('alert_lifecycle').deliveries;
  const later = d.decodeLifecycleDelivery({ ...wire, event: 'session_hibernated' });
  assert.equal(later.event, 'session_started', 'an unknown event name falls back to the first event of a session rather than throwing the log away');
  assert.equal(d.decodeLifecycleDelivery({ ...wire, status: null, error: 'refused' }).status, null);
});

test('a session lifecycle rule keeps its threshold in minutes and a lifecycle event keeps its detail', () => {
  const [wire] = fixture('alert_rules').rules;
  const idle = d.decodeAlertRule({ ...wire, kind: 'session_idle', threshold: 30, window: null });
  assert.equal(idle.kind, 'session_idle');
  assert.equal(idle.threshold, 30, 'an idle rule counts whole minutes, so the threshold arrives as the number the operator wrote');
  assert.equal(idle.windowId, null);
  const [wireEvent] = fixture('alert_events').events;
  const detail = { agent: 'sub-1', key: 'desktop', parent_agent: 'main', pool: 'main', session: 'e2e-ui' };
  const started = d.decodeAlertEvent({ ...wireEvent, detail, kind: 'subagent_active', subject: 'desktop, session e2e-ui, agent sub-1' });
  assert.equal(started.kind, 'subagent_active');
  assert.deepEqual(started.detail, detail, 'a detail is the free-form object the signal built and is passed through undecoded');
});
