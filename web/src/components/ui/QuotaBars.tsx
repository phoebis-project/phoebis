import type { Account, RateLimitWindow } from '@/api/types';
import { fmtAbsolute, fmtPct, fmtRelative } from '@/lib/format';
import { meterTone, remainingOf } from '@/lib/quota';
import { WINDOW_ORDER } from '@/lib/windows';
import { Meter } from './primitives';

export function WindowBar({ window: w, now, soon = false, showLabel = true, compact = false }: { compact?: boolean; now: number; showLabel?: boolean; soon?: boolean; window: RateLimitWindow }) {
  const remaining = remainingOf(w);
  return (
    <div className="wbar">
      <div className="wbar__head">
        {showLabel && <span className="wbar__label">{w.label}</span>}
        <span className="wbar__pct num">{fmtPct(remaining)}</span>
      </div>
      <Meter value={remaining} tone={meterTone(remaining)} thin label={`${w.label} remaining`} />
      {/* Three of these share the width of one card, so the compact form drops
          the word and leans on the title for the absolute instant. */}
      <div className={`wbar__reset ${soon ? 'is-soon' : ''}`} title={`resets ${fmtAbsolute(w.resetsAt)}`}>
        {compact ? fmtRelative(w.resetsAt, now) : `resets ${fmtRelative(w.resetsAt, now)}`}
      </div>
    </div>
  );
}

/** Placeholder bar for a window the provider does not report on this plan. */
export function WindowBarAbsent({ label }: { label: string }) {
  return (
    <div className="wbar wbar--absent" title="no Fable window">
      <div className="wbar__head">
        <span className="wbar__label">{label}</span>
        <span className="wbar__pct muted">n/a</span>
      </div>
      <div className="meter meter--thin meter--absent" role="presentation">
        <div className="meter__fill" style={{ width: '100%' }} />
      </div>
      <div className="wbar__reset muted">not reported</div>
    </div>
  );
}

/**
 * The three windows every subscription account chip shows, in one row: 5h, 7d,
 * and Fable 7d. Accounts without a Fable window keep the third slot as a greyed
 * placeholder so chips line up across the list. With `others`, every further
 * window the account carries follows under its own key, as the detail card
 * draws them.
 */
export function WindowTriple({ account, now, compact = false, others = false }: { account: Account; compact?: boolean; now: number; others?: boolean }) {
  const w5 = account.windows.find((w) => w.id === '5h');
  const w7 = account.windows.find((w) => w.id === '7d');
  const known: readonly string[] = WINDOW_ORDER;
  const rest = others ? account.windows.filter((w) => !known.includes(w.id)) : [];
  if (!w5 && !w7 && !account.fableWindow && rest.length === 0) return <div className="wbar__none muted">No quota windows</div>;
  // One word each: "Fable 7d" wraps in the narrow compact columns, which drops
  // that meter a line below its two neighbors.
  const labels = compact ? { '5h': '5h', '7d': '7d', fable: 'Fable' } : { '5h': '5-hour', '7d': '7-day', fable: 'Fable' };
  return (
    <div className={`wbars wbars--3 ${compact ? 'wbars--compact' : ''}`}>
      {w5 ? <WindowBar window={{ ...w5, label: labels['5h'] }} now={now} compact={compact} /> : <WindowBarAbsent label={labels['5h']} />}
      {w7 ? <WindowBar window={{ ...w7, label: labels['7d'] }} now={now} compact={compact} /> : <WindowBarAbsent label={labels['7d']} />}
      {account.fableWindow ? <WindowBar window={{ ...account.fableWindow, label: labels.fable }} now={now} compact={compact} /> : <WindowBarAbsent label={labels.fable} />}
      {rest.map((w) => (
        <WindowBar key={w.id} window={w} now={now} compact={compact} />
      ))}
    </div>
  );
}
