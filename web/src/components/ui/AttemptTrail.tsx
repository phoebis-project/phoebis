import type { AttemptOutcome } from '@/api/types';
import { fmtAbsolute } from '@/lib/format';
import type { BadgeTone } from '@/lib/quota';
import { useDescription } from './primitives';

/** The shape shared by a ledger attempt and a live one; the live one has no status until its head lands. */
export interface TrailAttempt {
  accountId: string;
  cooldownSet: { reason: string; until: string } | null;
  index: number;
  outcome: AttemptOutcome | 'in_flight';
  upstreamStatus: number | null;
}

const OUTCOME_LABEL: Record<AttemptOutcome | 'in_flight', string> = {
  auth_failed: 'auth failed',
  canceled: 'canceled',
  in_flight: 'in flight',
  rate_limited: 'rate limited',
  succeeded: 'succeeded',
  transport_error: 'transport error',
  upstream_error: 'upstream error',
};

const toneOf = (status: number | null): BadgeTone => (status === null ? 'neutral' : status === 200 ? 'good' : status === 429 ? 'warning' : status === 529 ? 'serious' : 'critical');

const lineFor = (a: TrailAttempt) =>
  [`${a.index + 1}. ${a.accountId}: ${OUTCOME_LABEL[a.outcome]}`, a.upstreamStatus === null ? 'no status yet' : `HTTP ${a.upstreamStatus}`, a.cooldownSet ? `cooldown set, ${a.cooldownSet.reason}, until ${fmtAbsolute(a.cooldownSet.until)}` : 'no cooldown set'].join(', ');

/** One badge per attempt in the order they were tried, so a failover reads as "a 429, b 200". */
export function AttemptTrail({ attempts }: { attempts: TrailAttempt[] }) {
  const title = attempts.map(lineFor).join('\n');
  const described = useDescription(title);
  if (attempts.length === 0) return <span className="muted">none</span>;
  return (
    <>
      <span className="trail" title={title} aria-describedby={described?.id}>
        {attempts.map((a) => (
          <span key={a.index} className={`trail__step badge badge--${toneOf(a.upstreamStatus)}`}>
            {a.accountId} <span className="num">{a.upstreamStatus ?? '…'}</span>
          </span>
        ))}
      </span>
      {described?.sibling && (
        <span id={described.id} className="sr-only" data-descriptions="">
          {title}
        </span>
      )}
    </>
  );
}
