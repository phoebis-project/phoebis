import assert from 'node:assert/strict';
import { test } from 'node:test';
import { fundedRankOf } from './dist/api.js';

/** A key's lifetime sum, its plan rows' Pro-week figure null where `unpriced` holds, as the keys route answers it. */
const lifetime = ({ billedUsd = 0, cost = 0, sevenDay = 0, unpriced = false } = {}) => ({ billedUsd, cost, equivalent: unpriced ? null : sevenDay, requests: 12 });

test("a key's bar ranks by its dollars at the API price list under native", () => {
  assert.equal(fundedRankOf('native', lifetime({ billedUsd: 4.91, cost: 9.4, sevenDay: 0.0241 })), 9.4, "every row's dollars, the Pro-week figure read in the label alone");
  assert.equal(fundedRankOf('native', lifetime({ billedUsd: 1129.29, cost: 1129.29 })), 1129.29, 'billed rows alone rank by their dollars, where an idle key reads zero');
  assert.equal(fundedRankOf('native', lifetime({ billedUsd: 4.91, cost: 9.4, sevenDay: 0.0137, unpriced: true })), 9.4, "a plan row that priced nothing leaves the rank on every row's dollars");
  assert.equal(fundedRankOf('native', lifetime({ billedUsd: 4.91, cost: 4.91, unpriced: true })), 4.91, "a key whose plan rows all priced nothing ranks by its dollars, here its billed rows' alone");
});

test("a key's bar ranks by the position's dollars under a dollar position, the flag aside", () => {
  const flagged = lifetime({ billedUsd: 4, cost: 9, sevenDay: 0.0137, unpriced: true });
  assert.equal(fundedRankOf('all', flagged), 9, 'every row');
  assert.equal(fundedRankOf('usd', flagged), 4, 'the billed rows');
  assert.equal(fundedRankOf('equivalent', flagged), 5, "the plan rows at the API price list's dollars");
});
