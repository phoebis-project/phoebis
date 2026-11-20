import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const inherit = { accountId: null, mode: 'inherit' };

test("the deletion's warning names each use", () => {
  // `a` serves model requests in `main`, which also selects it for connectors, and `spare` selects it for both
  // features. `k2` inherits `spare`'s choice for both features, which is no use of the key's own.
  const pools = [
    { featureAccounts: { artifacts: null, connectors: 'a' }, id: 'main', members: [{ accountId: 'a' }], name: 'Main' },
    { featureAccounts: { artifacts: 'a', connectors: 'a' }, id: 'spare', members: [], name: 'Spare' },
  ];
  const keys = [
    { featureAccounts: { artifacts: inherit, connectors: { accountId: 'a', mode: 'account' } }, id: 'k1', name: 'Desk', poolId: 'main' },
    { featureAccounts: { artifacts: inherit, connectors: inherit }, id: 'k2', name: 'Runner', poolId: 'spare' },
  ];
  assert.deepEqual(
    d.accountUses('a', pools, keys),
    [
      { kind: 'member', poolId: 'main', poolName: 'Main' },
      { feature: 'connectors', kind: 'pool_feature', poolId: 'main', poolName: 'Main' },
      { feature: 'artifacts', kind: 'pool_feature', poolId: 'spare', poolName: 'Spare' },
      { feature: 'connectors', kind: 'pool_feature', poolId: 'spare', poolName: 'Spare' },
      { feature: 'connectors', keyId: 'k1', keyName: 'Desk', kind: 'key_feature' },
    ],
    'the membership, both features of each pool that selects the account and the key that names it',
  );
  assert.deepEqual(d.accountUses('z', pools, keys), [], 'an account no pool and no key names is in no use');
});
