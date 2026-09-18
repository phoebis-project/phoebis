import type { Account, Pool, Session } from '@/api/types';
import { INELIGIBLE_LABEL, RULE_NOTE, STRATEGY_RULE, selectionOrder } from '@/lib/selection';
import { Badge } from './primitives';

/** The tiers the router walks for a fresh session, with the upstream the next one lands on marked. */
export function SelectionOrder({ pool, accounts, sessions, now }: { accounts: Account[]; now: number; pool: Pool; sessions: Session[] }) {
  const tiers = selectionOrder(pool, accounts, sessions, now);
  return (
    <div className="selection-order">
      {tiers.map((tier) => (
        <div key={tier.priority} className="selection-order__tier">
          <span className="pool__prio num" title="Priority">
            P{tier.priority}
          </span>
          <span className="chips">
            {tier.placements.map((p) => {
              const isNext = tier.next.includes(p.account.id);
              const suffix = p.pins ? ` · ${p.pins} ${p.pins === 1 ? 'pin' : 'pins'}` : '';
              const title = p.ineligible
                ? `${p.account.label}: ${INELIGIBLE_LABEL[p.ineligible]}, skipped by selection`
                : isNext
                  ? `${p.account.label}: where the next fresh session lands`
                  : p.deferred
                    ? `${p.account.label}: in overage, taken after the plan-funded members of this tier`
                    : `${p.account.label}: rank ${p.rank} in this tier`;
              return (
                <Badge key={p.account.id} tone={isNext ? 'accent' : p.ineligible ? 'neutral' : 'good'} title={title}>
                  {isNext ? 'next ' : ''}
                  {p.account.label}
                  {p.ineligible ? ` (${INELIGIBLE_LABEL[p.ineligible]})` : p.deferred ? ' overage' : ''}
                  {suffix}
                </Badge>
              );
            })}
          </span>
        </div>
      ))}
      <p className="muted selection-order__rule" title={RULE_NOTE}>
        {STRATEGY_RULE[pool.strategy]}
      </p>
    </div>
  );
}
