import assert from 'node:assert/strict';
import { test } from 'node:test';

const d = await import('./dist/api.js');

test("the created row's guard keeps the id where the read ends before the address names it, and lets it go once the address leaves it", () => {
  // The router commits the confirm's navigation in a transition, so the failed read can end first. The page renders
  // again at once after each step, so the unlisted phase meets an address that names no id yet.
  const reading = d.nextCreatedRow(d.confirmedRow('solo-loft'), { accountId: undefined, listed: false, loading: true });
  const unlisted = d.nextCreatedRow(reading, { accountId: undefined, listed: false, loading: false });
  const waiting = d.nextCreatedRow(unlisted, { accountId: undefined, listed: false, loading: false });
  assert.deepEqual([reading?.phase, unlisted?.phase, waiting?.phase], ['reading', 'unlisted', 'unlisted']);
  const named = d.nextCreatedRow(waiting, { accountId: 'solo-loft', listed: false, loading: false });
  assert.deepEqual([named?.id, named?.phase], ['solo-loft', 'unlisted']);
  assert.equal(d.nextCreatedRow(named, { accountId: 'primary', listed: false, loading: false }), null, 'an address that named the id and then left it lets the id go');
});
