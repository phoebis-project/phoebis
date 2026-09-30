import type { Account, FastLaneReading } from '@/api/types';
import { own } from '@/lib/analytics';
import { fmtAbsolute, fmtPct, fmtRelative } from '@/lib/format';

/** A fast cooldown's reason as its chip words it; a reason added later reads as the router sent it. */
const FAST_REASON_LABEL: Record<string, string> = { fast_credits: 'credits unavailable', fast_rate_limit: 'rate limited' };
const reasonLabel = (reason: string) => (own(FAST_REASON_LABEL, reason) ? FAST_REASON_LABEL[reason] : reason);

/** The later of the two resets: a fast request draws on both buckets, so that is when both have refilled. */
function laterReset(readings: (FastLaneReading | null)[]): string | null {
  const resets = readings.flatMap((r) => (r?.resetsAt ? [r.resetsAt] : []));
  return resets.length ? resets.reduce((a, b) => (Date.parse(b) > Date.parse(a) ? b : a)) : null;
}

/** An upstream's fast lane: the Fast mode line while the router holds a reading, and a chip per fast cooldown;
 *  nothing when it has neither. */
export function FastLane({ account, now }: { account: Account; now: number }): JSX.Element | null {
  const lane = account.fastLane;
  const cooling = account.cooldowns.filter((c) => c.scope === 'fast');
  if (lane === null && cooling.length === 0) return null;
  const used = lane ? ([['input', lane.input], ['output', lane.output]] as const).flatMap(([name, r]) => (r ? [`${name} ${fmtPct(r.utilization)} used`] : [])) : [];
  const reset = lane ? laterReset([lane.input, lane.output]) : null;
  return (
    <div className="chips fast-lane">
      {used.length > 0 && (
        <span className="chip fast-lane__line" title={reset ? fmtAbsolute(reset) : undefined}>
          <span className="chip__label">Fast mode</span>
          {[...used, ...(reset ? [`resets ${fmtRelative(reset, now)}`] : [])].join(', ')}
        </span>
      )}
      {cooling.map((c) => (
        <span key={c.reason} className="chip chip--serious fast-lane__cooldown" title={fmtAbsolute(c.until)}>
          <span className="chip__label">Fast mode</span>
          {reasonLabel(c.reason)}, until {fmtRelative(c.until, now)}
        </span>
      ))}
    </div>
  );
}
