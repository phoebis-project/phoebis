import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ATTENTION_LABEL, ATTENTION_TONE, sortAttention } from './dist/api.js';

const item = (kind, since, id = kind) => ({ detail: `${kind} detail`, kind, since, subject: { id, kind: 'account', label: id } });

test('the attention rows sort worst first, the oldest first within a kind', () => {
  const items = [
    item('client_drift', '2026-09-18T10:00:00Z'),
    item('poll_backoff', '2026-09-18T09:00:00Z'),
    item('funding_required', '2026-09-18T08:00:00Z'),
    item('pool_exhausted', '2026-09-18T07:00:00Z'),
    item('refresh_failed', '2026-09-18T11:00:00Z', 'newer'),
    item('refresh_failed', '2026-09-18T06:00:00Z', 'older'),
    item('reauth_required', '2026-09-18T05:00:00Z'),
  ];
  const sorted = sortAttention(items);
  assert.deepEqual(
    sorted.map((i) => i.kind),
    ['reauth_required', 'refresh_failed', 'refresh_failed', 'pool_exhausted', 'funding_required', 'poll_backoff', 'client_drift'],
  );
  assert.deepEqual(
    sorted.filter((i) => i.kind === 'refresh_failed').map((i) => i.subject.id),
    ['older', 'newer'],
  );
  assert.equal(items[0].kind, 'client_drift', 'the sort leaves the given list as it was');
});

test('each attention kind has its word and its tone', () => {
  assert.deepEqual(ATTENTION_LABEL, {
    client_drift: 'Client drift',
    funding_required: 'Funding required',
    poll_backoff: 'Usage poll backing off',
    pool_exhausted: 'Pool exhausted',
    reauth_required: 'Re-auth required',
    refresh_failed: 'Refresh failed',
  });
  const byTone = (tone) => Object.keys(ATTENTION_TONE).filter((kind) => ATTENTION_TONE[kind] === tone).sort();
  assert.deepEqual(byTone('critical'), ['pool_exhausted', 'reauth_required', 'refresh_failed']);
  assert.deepEqual(byTone('serious'), ['funding_required']);
  assert.deepEqual(byTone('warning'), ['client_drift', 'poll_backoff']);
});
