import type { SKIP_OBSTACLE_VALUES } from '@/api/decode';
import type { PoolStrategy, SelectionTrace as Trace, Usage } from '@/api/types';
import { Badge } from '@/components/ui/primitives';
import { moveLabel, own } from '@/lib/analytics';
import { fmtCompact } from '@/lib/format';
import { STRATEGY_LABEL } from '@/lib/selection';

type Obstacle = (typeof SKIP_OBSTACLE_VALUES)[number];

/** A hover hint per obstacle the router names; a name added later shows as sent, without one. */
const OBSTACLE_HINT: Record<Obstacle, string> = {
  above_ceiling: 'above the utilization ceiling',
  at_cap: 'at its in-flight cap',
  attempted: 'already tried by this request',
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

/**
 * Why one attempt landed where it did: the strategy and the tier it chose in, whether it continued the session's
 * pin, moved it or placed it afresh, every member it passed over, and whether the cache answered it. The cache read
 * is folded into the trace of the attempt that answered, whose tokens are the request's usage.
 */
export function SelectionTrace({ trace, usage }: { trace: Trace; usage: Usage }) {
  const input = usage.input + usage.cacheRead + usage.cacheWrite5m + usage.cacheWrite1h;
  const placement = trace.renewed ? 'continued, pin renewed' : trace.moved !== null ? `moved: ${moveLabel(trace.moved)}` : 'fresh placement';
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
        {trace.cacheRead !== null && (
          <span className="num">
            {trace.cacheRead > 0 ? 'hit' : 'miss'}, {fmtCompact(trace.cacheRead)} of {fmtCompact(input)} input tokens read from cache
          </span>
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
