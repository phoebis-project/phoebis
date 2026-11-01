import assert from 'node:assert/strict';
import { test } from 'node:test';
import { keyRankOf } from './dist/ui.js';

/** A key's lifetime sum, its plan rows' Pro-week figure null where `unpriced` holds, as the keys route answers it. */
const lifetime = ({ billedUsd = 0, cost = 0, sevenDay = 0, unpriced = false } = {}) => ({ billedUsd, cost, equivalent: unpriced ? null : sevenDay, requests: 12 });

test("a key's bar ranks by its figure under native and reads unknown where a plan row priced nothing", () => {
  assert.equal(keyRankOf('native', lifetime({ billedUsd: 4.91, cost: 9.4, sevenDay: 0.0241 })), 0.0241, 'the billed dollars read in the label alone');
  assert.equal(keyRankOf('native', lifetime({ billedUsd: 4.91, cost: 4.91 })), 0, 'billed rows alone rank at zero');
  assert.equal(keyRankOf('native', lifetime({ billedUsd: 4.91, cost: 9.4, sevenDay: 0.0137, unpriced: true })), null, 'the priced rows alone would draw a length the key does not have');
  assert.equal(keyRankOf('native', lifetime({ billedUsd: 4.91, cost: 4.91, unpriced: true })), null, 'a flagged key whose plan rows all priced nothing reads unknown as well');
});

test("a key's bar ranks by the position's dollars under a dollar position, the flag aside", () => {
  const flagged = lifetime({ billedUsd: 4, cost: 9, sevenDay: 0.0137, unpriced: true });
  assert.equal(keyRankOf('all', flagged), 9, 'every row');
  assert.equal(keyRankOf('usd', flagged), 4, 'the billed rows');
  assert.equal(keyRankOf('equivalent', flagged), 5, "the plan rows at the API price list's dollars");
});
