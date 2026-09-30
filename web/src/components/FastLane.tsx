import type { Account, FastLaneReading } from '@/api/types';
import { own } from '@/lib/analytics';
import { fmtAbsolute, fmtPct, fmtRelative } from '@/lib/format';
import { isFastScope } from '@/lib/quota';

/** A fast cooldown's reason as its chip words it; a reason added later reads as the router sent it. */
const FAST_REASON_LABEL: Record<string, string> = { fast_credits: 'credits unavailable', fast_rate_limit: 'rate limited' };
const reasonLabel = (reason: string) => (own(FAST_REASON_LABEL, reason) ? FAST_REASON_LABEL[reason] : reason);

/** The later of the two resets: a fast request draws on both buckets, so that is when both have refilled. */
function laterReset(readings: (FastLaneReading | null)[]): string | null {
  const resets = readings.flatMap((r) => (r?.resetsAt ? [r.resetsAt] : []));
  return resets.length ? resets.reduce((a, b) => (Date.parse(b) > Date.parse(a) ? b : a)) : null;
}

const refilled = (r: FastLaneReading, now: number) => r.resetsAt !== null && Date.parse(r.resetsAt) <= now;

/** An upstream's fast lane: the Fast mode line while the router holds a reading, and a chip per fast cooldown;
 *  nothing when it has neither. */
export function FastLane({ account, now }: { account: Account; now: number }): JSX.Element | null {
  const lane = account.fastLane;
  const cooling = account.cooldowns.filter((c) => isFastScope(c.scope));
  if (lane === null && cooling.length === 0) return null;
  // The router keeps the last answer's pair in memory until the next fast answer, while the buckets reset within
  // minutes, so a reading past its reset reads as the refilled bucket the router itself routes on.
  const used = lane ? ([['input', lane.input], ['output', lane.output]] as const).flatMap(([name, r]) => (r ? [`${name} ${fmtPct(refilled(r, now) ? 0 : r.utilization)} used`] : [])) : [];
  const readings = lane ? [lane.input, lane.output].filter((r): r is FastLaneReading => r !== null) : [];
  const reset = lane ? laterReset([lane.input, lane.output]) : null;
  // A reading with no reset may still be spent, so the lane reads refilled only when every reading has refilled; a
  // later reset already past beside such a reading says nothing of when.
  const when = reset === null ? [] : Date.parse(reset) > now ? [`resets ${fmtRelative(reset, now)}`] : readings.every((r) => refilled(r, now)) ? [`refilled ${fmtRelative(reset, now)}`] : [];
  return (
    <div className="chips fast-lane">
      {used.length > 0 && (
        <span className="fast-lane__line" title={reset !== null && when.length > 0 ? fmtAbsolute(reset) : undefined}>
          <span className="fast-lane__label">Fast mode</span>
          {[...used, ...when].join(', ')}
        </span>
      )}
      {cooling.map((c) => (
        <span key={c.scope} className="chip chip--serious fast-lane__cooldown" title={fmtAbsolute(c.until)}>
          <span className="chip__label">Fast mode</span>
          {reasonLabel(c.reason)}, lifts {fmtRelative(c.until, now)}
        </span>
      ))}
    </div>
  );
}
