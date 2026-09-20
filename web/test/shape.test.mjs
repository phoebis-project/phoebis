import assert from 'node:assert/strict';
import { test } from 'node:test';
import { same, shapeOf } from './shape.mjs';

test('a nested field missing from a later mock row is a finding', () => {
  const want = shapeOf([{ attempts: [{ accountId: 'a', outcome: 'succeeded' }], id: 'r1' }]);
  const got = [{ attempts: [{ accountId: 'a', outcome: 'succeeded' }], id: 'r1' }, { attempts: [{ accountId: 'b' }], id: 'r2' }];
  assert.deepEqual(same(want, got), ['[1].attempts[0].outcome: only in the decoded fixture']);
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
  const want = shapeOf({ detail: { client_version: '2.1.0', key: 'desktop', trigger: 'drift' }, kind: 'client_drift' });
  const got = { detail: { agent: null, key: 'desktop', question: 'Continue?', session: 's1' }, kind: 'ask_user_question' };
  assert.deepEqual(same(want, got), []);
  assert.deepEqual(same(want, { detail: 'text', kind: 'ask_user_question' }), ['.detail: string where an object or null is expected']);
});
