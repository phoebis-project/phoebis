import { FEATURE_WORD_VALUES, type ApiKey, type FeatureWord, type Pool } from '@/api/types';

export type AccountUse =
  | { feature: FeatureWord; keyId: string; keyName: string; kind: 'key_feature' }
  | { kind: 'member'; poolId: string; poolName: string }
  | { feature: FeatureWord; kind: 'pool_feature'; poolId: string; poolName: string };

const byName = <T extends { name: string }>(rows: readonly T[]): T[] => [...rows].sort((a, b) => a.name.localeCompare(b.name));

/** Where an account is in use: the pool it is a member of, each pool that selects it for a feature and each
 * key that names it for one. A key that inherits its pool's account is no use of its own. */
export function accountUses(accountId: string, pools: readonly Pool[], keys: readonly ApiKey[]): AccountUse[] {
  const sorted = byName(pools);
  const member = sorted.filter((p) => p.members.some((m) => m.accountId === accountId)).map((p): AccountUse => ({ kind: 'member', poolId: p.id, poolName: p.name }));
  const poolFeatures = sorted.flatMap((p) => FEATURE_WORD_VALUES.filter((f) => p.featureAccounts[f] === accountId).map((feature): AccountUse => ({ feature, kind: 'pool_feature', poolId: p.id, poolName: p.name })));
  // The key's own choice and never its resolved value, which names the pool's account under inherit.
  const keyFeatures = byName(keys).flatMap((k) =>
    FEATURE_WORD_VALUES.filter((f) => k.featureAccounts[f].mode === 'account' && k.featureAccounts[f].accountId === accountId).map((feature): AccountUse => ({ feature, keyId: k.id, keyName: k.name, kind: 'key_feature' })),
  );
  return [...member, ...poolFeatures, ...keyFeatures];
}
