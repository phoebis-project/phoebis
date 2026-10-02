import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

/** A tab's session storage and its reload, recorded. */
const tab = (stored = {}) => {
  const items = new Map(Object.entries(stored));
  const env = {
    reloads: 0,
    reload: () => {
      env.reloads += 1;
    },
    storage: () => ({ getItem: (k) => items.get(k) ?? null, removeItem: (k) => items.delete(k), setItem: (k, v) => items.set(k, String(v)) }),
  };
  return { env, items };
};
const settled = (promise) => Promise.race([promise.then(() => 'resolved', () => 'rejected'), new Promise((r) => setTimeout(() => r('pending'), 20))]);
const missing = () => Promise.reject(new TypeError('Failed to fetch dynamically imported module'));

test('a page chunk that fails to load reloads the console once and leaves the page loading', async () => {
  const { env, items } = tab();
  const load = d.reloadOnChunkFailure(missing, env);
  assert.equal(await settled(load()), 'pending', 'the fallback stays drawn until the reload replaces the page');
  assert.equal(env.reloads, 1);
  assert.equal(items.size, 1, 'the reload is marked in the tab');
});

test('a chunk that still fails after the reload is answered as the error, with no second reload', async () => {
  const first = tab();
  await settled(d.reloadOnChunkFailure(missing, first.env)());
  const { env } = tab(Object.fromEntries(first.items));
  await assert.rejects(d.reloadOnChunkFailure(missing, env)(), TypeError);
  assert.equal(env.reloads, 0);
});

test('a chunk that loads clears the mark, so a later failure may reload again', async () => {
  const first = tab();
  await settled(d.reloadOnChunkFailure(missing, first.env)());
  const { env, items } = tab(Object.fromEntries(first.items));
  const page = { default: 'page' };
  assert.equal(await d.reloadOnChunkFailure(() => Promise.resolve(page), env)(), page);
  assert.equal(items.size, 0);
  assert.equal(await settled(d.reloadOnChunkFailure(missing, env)()), 'pending');
  assert.equal(env.reloads, 1);
});

test('a tab without session storage never reloads, since nothing could stop a loop', async () => {
  const env = { reloads: 0, reload: () => (env.reloads += 1), storage: () => { throw new Error('storage is disabled'); } };
  await assert.rejects(d.reloadOnChunkFailure(missing, env)(), TypeError);
  assert.equal(env.reloads, 0);
});
