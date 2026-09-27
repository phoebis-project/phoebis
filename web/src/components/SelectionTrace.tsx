import type { SKIP_OBSTACLE_VALUES } from '@/api/decode';
import type { Placement, PoolStrategy, SelectionTrace as Trace, Usage } from '@/api/types';
import { Badge } from '@/components/ui/primitives';
import { moveLabel, own } from '@/lib/analytics';
import { fmtCompact, placementHit } from '@/lib/format';
import { STRATEGY_LABEL } from '@/lib/selection';

type Obstacle = (typeof SKIP_OBSTACLE_VALUES)[number];

/** A hover hint per obstacle the router names; a name added later shows as sent, without one. */
const OBSTACLE_HINT: Record<Obstacle, string> = {
  above_ceiling: 'above the utilization ceiling',
  at_cap: 'at its in-flight cap',
  attempted: 'already tried by this request',
  budget: 'at its spend budget for the period',
  cooldown: 'in a cooldown',
  floor: 'at the Pace floor',
  operator: 'held by an operator',
  overage: 'in overage',
  ranked: 'ranked behind the upstream chosen',
  spent: 'its window spent',
  subscription_ended: 'its subscription ended',
  winding_down: 'winding down toward its subscription end',
};

const strategyLabel = (s: string) => (own(STRATEGY_LABEL, s) ? STRATEGY_LABEL[s as PoolStrategy] : s);
const obstacleHint = (o: string) => (own(OBSTACLE_HINT, o) ? OBSTACLE_HINT[o as Obstacle] : undefined);

const PLACED: Record<Placement['rung'], (depth: number) => string> = {
  content: (depth) => `placed by its messages, depth ${depth}`,
  static: (depth) => `placed by its static prefix, depth ${depth}`,
  with_main: () => 'placed with the main conversation',
};
const HOLD_HINT = 'Every upstream the placement could land on was at its in-flight cap, so the request waited first';
const PROMISE_HINT: Record<Placement['rung'], string> = {
  content: 'A placement by its messages hits when the cache reads the whole prefix it was promised',
  static: 'A placement by its static prefix hits when the cache reads anything',
  with_main: 'A placement beside the main conversation hits when the cache reads anything',
};

/**
 * Why one attempt landed where it did: the strategy and the tier it chose in, whether it continued the session's
 * pin, moved it or placed it afresh (by a warm prefix, or beside the main conversation), any hold it waited
 * through, every member it passed over, and whether the cache answered it. The cache read is folded into the trace
 * of the attempt that answered, whose tokens are the request's usage; a placement reads it against the prefix it
 * was promised.
 */
export function SelectionTrace({ trace, usage }: { trace: Trace; usage: Usage }) {
  const input = usage.input + usage.cacheRead + usage.cacheWrite5m + usage.cacheWrite1h;
  const placed = trace.placement ? PLACED[trace.placement.rung](trace.placement.depth) : null;
  const moved = trace.moved !== null ? `moved: ${moveLabel(trace.moved)}` : null;
  const placement = trace.renewed ? 'continued, pin renewed' : (moved ?? placed ?? 'fresh placement');
  // The cohort's account names where the hold was placed; a hold on the strategy's choice names the last one waited on.
  const waited = trace.cohortHold?.waitedOn ?? [];
  const hold = trace.cohortHold ? (trace.cohortHold.cohort ?? waited[waited.length - 1] ?? null) : null;
  // `with_main` names no entry and so promises no size; its verdict, any read, is the plain cache line's own.
  const promised = trace.placement !== null && trace.placement.rung !== 'with_main' ? trace.placement : null;
  const hit = promised !== null ? placementHit(promised, trace.cacheRead) : null;
  return (
    <div className="selection-trace">
      <div className="selection-trace__row">
        <span className="muted">Selection</span>
        <span>{strategyLabel(trace.strategy)}</span>
        {trace.tier && (
          <span className="num" title="The tier the choice was made in: the group, the priority, and whether the upstream was in overage">
            group {trace.tier[0]}, priority {trace.tier[1]}
            {trace.tier[2] === 1 ? ', overage' : ''}
          </span>
        )}
        <span>{placement}</span>
        {placed !== null && moved !== null && <span>{placed}</span>}
        {hold !== null && <span title={HOLD_HINT}>held on {hold} before placing</span>}
        {promised !== null && hit !== null && trace.cacheRead !== null ? (
          <span className="num" title={PROMISE_HINT[promised.rung]}>
            {hit ? 'hit' : 'miss'}, {fmtCompact(trace.cacheRead)} of {fmtCompact(promised.prefixTokens)} promised tokens
            read
          </span>
        ) : (
          trace.cacheRead !== null && (
            <span className="num">
              {trace.cacheRead > 0 ? 'hit' : 'miss'}, {fmtCompact(trace.cacheRead)} of {fmtCompact(input)} input tokens read from cache
            </span>
          )
        )}
      </div>
      {trace.skipped.length > 0 && (
        <div className="selection-trace__row">
          <span className="muted">Skipped</span>
          {trace.skipped.map((s) => (
            <Badge key={`${s.accountId}-${s.obstacle}`} title={obstacleHint(s.obstacle)}>
              {s.accountId}: {s.obstacle}
            </Badge>
          ))}
        </div>
      )}
    </div>
  );
}
