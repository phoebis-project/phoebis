import type { Account, MemberEligibility, Pool, PoolStrategy, Session } from '@/api/types';
import { windowOf } from '@/lib/quota';

/** One sentence per strategy, shown in the dialog's hint and the card; a later strategy adds its entry here. */
export const STRATEGY_RULE: Record<PoolStrategy, string> = {
  earliest_refresh: 'Within the first tier that has an eligible upstream, the one whose 7-day window resets soonest; ties are random.',
  least_utilized: 'Within the first tier that has an eligible upstream, the one with the lowest 5-hour utilization; ties are random.',
  priority_random: 'Within the first tier that has an eligible upstream, any of them at random.',
};
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

/** The order the router walks for a fresh session of this pool, from the data the routes already serve. */
export function selectionOrder(pool: Pool, accounts: Account[], sessions: Session[]): Tier[] {
  const byId = new Map(accounts.map((a) => [a.id, a]));
  const pinsOf = (id: string) => sessions.filter((s) => s.pinned && s.accountId === id).length;
  const priorities = [...new Set(pool.members.map((m) => m.priority))].sort((a, b) => a - b);
  let nextFound = false;
  return priorities.map((priority) => {
    const placements: Placement[] = pool.members
      .filter((m) => m.priority === priority)
      .flatMap((m) => {
        const account = byId.get(m.accountId);
        if (!account) return [];
        return [
          {
            account,
            deferred: pool.overagePolicy === 'prefer_plan' && Boolean(account.extraUsage?.inUse),
            ineligible: m.eligible === 'eligible' ? null : m.eligible,
            pins: pinsOf(m.accountId),
            rank: null,
          },
        ];
      });
    // The router's tier is (priority, deferred): plan-funded members rank ahead of deferred ones, then the
    // strategy orders inside each half; equal scores share a rank because the router draws among them at random.
    const eligible = placements
      .filter((p) => p.ineligible === null)
      .sort((a, b) => Number(a.deferred) - Number(b.deferred) || keyOf(a.account, pool.strategy) - keyOf(b.account, pool.strategy) || a.account.label.localeCompare(b.account.label));
    let rank = 0;
    let previous: { deferred: boolean; score: number } | null = null;
    for (const p of eligible) {
      const score = keyOf(p.account, pool.strategy);
      if (!previous || previous.deferred !== p.deferred || previous.score !== score) rank += 1;
      p.rank = rank;
      previous = { deferred: p.deferred, score };
    }
    const ordered = [...eligible, ...placements.filter((p) => p.ineligible !== null).sort((a, b) => a.account.label.localeCompare(b.account.label))];
    const next = !nextFound && eligible.length > 0 ? eligible.filter((p) => p.rank === 1).map((p) => p.account.id) : [];
    if (next.length) nextFound = true;
    return { next, placements: ordered, priority };
  });
}
