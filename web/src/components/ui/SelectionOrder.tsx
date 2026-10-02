import type { Account, Pool, Session } from '@/api/types';
import { GROUP_LABEL, INELIGIBLE_LABEL, LAST_RESORT_LABEL, RULE_NOTE, STRATEGY_RULE, kindOrdered, selectionOrder } from '@/lib/selection';
import { Badge } from './primitives';

/** The tiers the router walks for a fresh session, with the account the next one lands on marked. */
export function SelectionOrder({ pool, accounts, sessions }: { accounts: Account[]; pool: Pool; sessions: Session[] }) {
  const tiers = selectionOrder(pool, accounts, sessions);
  const grouped = kindOrdered(pool);
  return (
    <div className={`selection-order ${grouped ? 'selection-order--grouped' : ''}`}>
      {tiers.map((tier) => (
        <div key={`${tier.group}-${tier.priority}`} className="selection-order__tier">
          <span className="pool__prio num" title={grouped ? `${GROUP_LABEL[tier.group]} tier, priority ${tier.priority}` : 'Priority'}>
            {grouped ? `${GROUP_LABEL[tier.group]} ` : ''}P{tier.priority}
          </span>
          <span className="chips">
            {tier.placements.map((p) => {
              const isNext = tier.next.includes(p.account.id);
              const suffix = p.pins ? ` · ${p.pins} ${p.pins === 1 ? 'pin' : 'pins'}` : '';
              const title = p.ineligible
                ? `${p.account.label}: ${INELIGIBLE_LABEL[p.ineligible]}, skipped by selection`
                : isNext
                  ? `${p.account.label}: where the next fresh session lands`
                  : p.lastResort
                    ? `${p.account.label}: ${LAST_RESORT_LABEL[p.lastResort]}, taken only once no other account can take a fresh session`
                    : p.deferred
                      ? `${p.account.label}: in overage, taken after the plan-funded members of this tier`
                      : `${p.account.label}: rank ${p.rank} in this tier`;
              return (
                <Badge key={p.account.id} tone={isNext ? 'accent' : p.ineligible ? 'neutral' : p.lastResort ? 'warning' : 'good'} title={title}>
                  {isNext ? 'next ' : ''}
                  {p.account.label}
                  {p.ineligible ? ` (${INELIGIBLE_LABEL[p.ineligible]})` : p.lastResort ? ` (${LAST_RESORT_LABEL[p.lastResort]})` : p.deferred ? ' overage' : ''}
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
