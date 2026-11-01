import assert from 'node:assert/strict';
import { test } from 'node:test';
import { same, shapeOf } from './shape.mjs';

test('a nested field missing from a later mock row is a finding', () => {
  const want = shapeOf([{ attempts: [{ accountId: 'a', outcome: 'succeeded' }], id: 'r1' }]);
  const got = [{ attempts: [{ accountId: 'a', outcome: 'succeeded' }], id: 'r1' }, { attempts: [{ accountId: 'b' }], id: 'r2' }];
  assert.deepEqual(same(want, got), ['[1].attempts[0].outcome: only in the decoded fixture']);
  assert.deepEqual(same(shapeOf([{ accountId: 'a', id: 'r1' }]), [{ accountId: 5, extra: true, id: 'r1' }]), ['[0].accountId: number against string', '[0].extra: only in the mock']);
});

test('a required object answered as null is a finding, and a null in the fixture or inside a row asks nothing', () => {
  const want = shapeOf({ request: { id: 'r1' } });
  assert.deepEqual(same(want, { request: null }), ['.request: null in the mock where the fixture has a value']);
  assert.deepEqual(same(shapeOf({ request: null }), { request: { id: 'r1' } }), []);
  assert.deepEqual(same(shapeOf([{ id: 'r1', ttfbMs: 120 }]), [{ id: 'r1', ttfbMs: 120 }, { id: 'r2', ttfbMs: null }]), [], 'a failed request has no first byte');
});

test('a populated list answered empty is a finding, and an empty list in the fixture or inside a row asks nothing', () => {
  const want = shapeOf({ events: [{ kind: 'pin_migrated' }] });
  assert.deepEqual(same(want, { events: [] }), ['.events: an empty list in the mock where the fixture has rows']);
  assert.deepEqual(same(shapeOf({ events: [] }), { events: [{ kind: 'pin_migrated' }] }), []);
  assert.deepEqual(same(shapeOf([{ id: 'a', pools: [{ poolId: 'main' }] }]), [{ id: 'a', pools: [{ poolId: 'main' }] }, { id: 'mail', pools: [] }]), [], 'an account in no pool');
});

test('an opaque detail with different keys is no finding, and a detail that is not an object is one', () => {
  const want = shapeOf({ detail: { agent_id: 'main', epoch: 2, from_account: 'business', reason: 'cooldown' }, kind: 'pin_migrated' });
  const got = { detail: { agent_id: 'sub-1' }, kind: 'pin_released' };
  assert.deepEqual(same(want, got), []);
  assert.deepEqual(same(want, { detail: 'text', kind: 'pin_released' }), ['.detail: string where an object or null is expected']);
});

test('an open map only asks about entry types, but a sibling object with a different key set is a finding', () => {
  const want = shapeOf({ errors: { upstream_error: 1 }, labels: { a: 1 } });
  const got = { errors: { overloaded_error: 1, rate_limit_error: 2 }, labels: { b: 2 } };
  assert.deepEqual(same(want, got), ['.labels.a: only in the decoded fixture', '.labels.b: only in the mock']);
  // The two the map branch itself reports: a map answered as something other than a map, and an entry whose
  // value is not the type the fixture's entries carry.
  const rows = shapeOf([{ errors: { upstream_error: 1 } }]);
  assert.deepEqual(same(rows, [{ errors: [] }, { errors: { upstream_error: 'one' } }]), ['[0].errors: list where a map is expected', '[1].errors.upstream_error: string against number']);
});

test('a per-row pass-through path holds only to object-or-null, richer or not, the way a detail does', () => {
  const passthrough = (path) => path === '.body';
  const want = shapeOf({ body: { a: 1 } });
  assert.deepEqual(same(want, { body: { a: 1, b: 2, c: 3 } }, '', 0, passthrough), []);
  assert.deepEqual(same(want, { body: 'raw text' }, '', 0, passthrough), ['.body: string where an object or null is expected']);
});

test('a pass-through hook that names its kinds admits them, so a stored stream reads as text where a JSON answer reads as an object', () => {
  const passthrough = (path) => (path === '.body' ? ['object', 'null', 'string'] : false);
  const want = shapeOf({ body: { a: 1 } });
  assert.deepEqual(same(want, { body: 'event: message_start' }, '', 0, passthrough), []);
  assert.deepEqual(same(want, { body: null }, '', 0, passthrough), []);
  assert.deepEqual(same(want, { body: 7 }, '', 0, passthrough), ['.body: number where an object, null or a string is expected']);
});

test("a window-keyed map takes any window, a request row's share map a null entry, and an object entry is held to the first entry of the fixture", () => {
  const want = shapeOf({ costShares: { five_hour: 1 }, windows: { five_hour: { lead: 0.1, utilization: 0.5 } } });
  const got = { costShares: { five_hour: 1, seven_day_fable: 2 }, windows: { five_hour: { lead: 0.1, utilization: 0.5 }, seven_day: { lead: 0.2, utilization: 0.3 } } };
  assert.deepEqual(same(want, got), []);
  assert.deepEqual(same(want, { ...got, costShares: { five_hour: 'one' }, windows: { seven_day: { lead: 0.2 } } }), ['.costShares.five_hour: string against number', '.windows.seven_day.utilization: only in the decoded fixture']);
  // A request row carries an applicable window the cost model could not price as null, while a group's map drops it,
  // as the route does, so a null there is a finding.
  const rows = shapeOf({ requests: [{ costShares: { five_hour: 1 } }] });
  assert.deepEqual(same(rows, { requests: [{ costShares: { five_hour: 1, seven_day: null } }] }), []);
  assert.deepEqual(same(shapeOf({ request: { costShares: { five_hour: 1 } } }), { request: { costShares: { seven_day: null } } }), []);
  // The dashboard's recent rows are request rows too.
  assert.deepEqual(same(shapeOf({ recent: [{ costShares: { five_hour: 1 } }] }), { recent: [{ costShares: { five_hour: 1, seven_day: null } }] }), []);
  assert.deepEqual(same(want, { ...got, costShares: { five_hour: 1, seven_day: null } }), ['.costShares.seven_day: object against number']);
});
