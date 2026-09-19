import type { Account, ApiKeySubStrategy, MemberEligibility, Pool, PoolStrategy, Session } from '@/api/types';
import { windowOf } from '@/lib/quota';

/** One sentence per strategy, shown in the dialog's hint and the card; a later strategy adds its entry here. */
export const STRATEGY_RULE: Record<PoolStrategy, string> = {
  earliest_refresh: 'Subscription upstreams with plan quota first, then those in overage, then API keys; within a tier, the one whose 7-day window resets soonest; ties are random.',
  least_utilized: 'Subscription upstreams with plan quota first, then those in overage, then API keys; within a tier, the one with the lowest 5-hour utilization; ties are random.',
  priority_random: 'Within the first tier that has an eligible upstream, any of them at random.',
  round_robin: 'Within the first tier that has an eligible upstream, each in turn; a restart starts the round again.',
};
export const STRATEGY_LABEL: Record<PoolStrategy, string> = { earliest_refresh: 'Earliest refresh', least_utilized: 'Least utilized', priority_random: 'Priority, then random', round_robin: 'Round robin' };
export const SUB_STRATEGY_LABEL: Record<ApiKeySubStrategy, string> = { priority_random: 'Priority, then random', round_robin: 'Round robin' };
/** The kind tiers, in the order selection walks them. */
export const GROUP_LABEL = ['plan', 'overage', 'keys'] as const;

/** The strategies that order by kind whatever the flag holds: an API key carries no usage sample to rank it by. */
export const sampleDriven = (strategy: PoolStrategy) => strategy === 'earliest_refresh' || strategy === 'least_utilized';
/** Whether selection walks the kind tiers for this pool, which is the flag the route resolved, not the stored one. */
export const kindOrdered = (pool: Pool) => pool.preferSubscriptionEffective;
export const RULE_NOTE = 'A request for a Fable model is placed on the Fable window instead, which this card does not draw.';

/** Why the router skips a member: its own verdict, as the pool view reports it. */
export type Ineligible = Exclude<MemberEligibility, 'eligible'>;
export const INELIGIBLE_LABEL: Record<Ineligible, string> = { cooldown: 'in cooldown', overage: 'in overage', unavailable: 'not available' };

/** `deferred`: eligible but in overage under prefer_plan, so ranked after the plan-funded members of its tier. */
export interface Placement {
  account: Account;
  deferred: boolean;
  ineligible: Ineligible | null;
  pins: number;
  rank: number | null;
}
export interface Tier {
  group: 0 | 1 | 2;
  next: string[];
  placements: Placement[];
  priority: number;
}

/** The sort key the strategy ranks an eligible account by; lower first. `priority_random` scores nothing. */
function keyOf(account: Account, strategy: PoolStrategy): number {
  if (strategy === 'least_utilized') return windowOf(account, '5h')?.utilization ?? 0;
  if (strategy === 'earliest_refresh') {
    const resetsAt = windowOf(account, '7d')?.resetsAt;
    return resetsAt ? Date.parse(resetsAt) : Number.POSITIVE_INFINITY;
  }
  return 0;
}

/** Which kind tier an upstream sits in; a member the accounts list does not carry has no kind, so it leads. */
const groupOf = (account: Account | undefined): 0 | 1 | 2 => (account?.kind === 'api_key' ? 2 : account?.extraUsage?.inUse ? 1 : 0);

/** The order the router walks for a fresh session of this pool, from the data the routes already serve. */
export function selectionOrder(pool: Pool, accounts: Account[], sessions: Session[]): Tier[] {
  const byId = new Map(accounts.map((a) => [a.id, a]));
  const pinsOf = (id: string) => sessions.filter((s) => s.pinned && s.accountId === id).length;
  const grouped = kindOrdered(pool);
  const groupFor = (accountId: string) => (grouped ? groupOf(byId.get(accountId)) : 0);
  const tiers = [...new Map(pool.members.map((m) => [`${groupFor(m.accountId)}:${m.priority}`, { group: groupFor(m.accountId), priority: m.priority }])).values()].sort(
    (a, b) => a.group - b.group || a.priority - b.priority,
  );
  let nextFound = false;
  return tiers.map(({ group, priority }) => {
    const placements: Placement[] = pool.members
      .filter((m) => m.priority === priority && groupFor(m.accountId) === group)
      .flatMap((m) => {
        const account = byId.get(m.accountId);
        if (!account) return [];
        return [
          {
            account,
            // Under the kind order an upstream in overage already has a tier of its own, so deferring it inside
            // one would rank it behind its own siblings twice.
            deferred: !grouped && pool.overagePolicy === 'prefer_plan' && Boolean(account.extraUsage?.inUse),
            ineligible: m.eligible === 'eligible' ? null : m.eligible,
            pins: pinsOf(m.accountId),
            rank: null,
          },
        ];
      });
    // A sample-driven strategy has nothing to rank an API key by, so the key tier falls back to its own order.
    const strategy: PoolStrategy = group === 2 && sampleDriven(pool.strategy) ? pool.apiKeySubStrategy : pool.strategy;
    // The router's tier is (priority, deferred): plan-funded members rank ahead of deferred ones, then the
    // strategy orders inside each half; equal scores share a rank because the router draws among them at random.
    const eligible = placements
      .filter((p) => p.ineligible === null)
      .sort((a, b) => Number(a.deferred) - Number(b.deferred) || keyOf(a.account, strategy) - keyOf(b.account, strategy) || a.account.label.localeCompare(b.account.label));
    let rank = 0;
    let previous: { deferred: boolean; score: number } | null = null;
    for (const p of eligible) {
      const score = keyOf(p.account, strategy);
      if (!previous || previous.deferred !== p.deferred || previous.score !== score) rank += 1;
      // Round robin and priority_random both score every member 0, so the rank only moves across the deferred
      // boundary: each member of a subtier is equally the next, and the router rotates the plan-funded subtier
      // on its own while it lasts, so an overage sibling ranks behind it rather than beside it.
      p.rank = rank;
      previous = { deferred: p.deferred, score };
    }
    const ordered = [...eligible, ...placements.filter((p) => p.ineligible !== null).sort((a, b) => a.account.label.localeCompare(b.account.label))];
    const next = !nextFound && eligible.length > 0 ? eligible.filter((p) => p.rank === 1).map((p) => p.account.id) : [];
    if (next.length) nextFound = true;
    return { group, next, placements: ordered, priority };
  });
}
