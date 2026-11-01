import type { Account, AccountSlowDown, RateLimitWindow, Spend } from '@/api/types';
import { fmtAbsolute, fmtMinor, fmtPct, fmtRelative } from '@/lib/format';
import { hasSpendReading, isExpired, isSubscription, meterTone, planEndedSentence, projectionParts, remainingOf, slowedOf, spendResetTitle } from '@/lib/quota';
import { speedHover } from '@/lib/selection';
import { WINDOW_ORDER } from '@/lib/windows';
import { Meter } from './primitives';
import { SpeedChip } from './SpeedChip';

/**
 * `slowDown` is a slowed account's slow-down, which the whole 5-hour bar alone reads: its speed chip follows the label.
 */
export function WindowBar({ window: w, now, soon = false, showLabel = true, compact = false, slowDown = null }: { compact?: boolean; now: number; showLabel?: boolean; slowDown?: AccountSlowDown | null; soon?: boolean; window: RateLimitWindow }) {
  const remaining = remainingOf(w);
  const reset = compact ? fmtRelative(w.resetsAt, now) : `resets ${fmtRelative(w.resetsAt, now)}`;
  // A window past its reset says nothing until its next reading, as the Quota cell's expired line does.
  const projection = isExpired(w, now) ? null : projectionParts(w, now, compact);
  return (
    <div className="wbar">
      <div className="wbar__head">
        {showLabel && <span className="wbar__label">{w.label}</span>}
        {slowDown && <SpeedChip factor={slowDown.factor} hover={speedHover(slowDown, now)} />}
        <span className="wbar__pct num">{fmtPct(remaining)}</span>
      </div>
      <Meter value={remaining} tone={meterTone(remaining)} thin label={`${w.label} remaining`} />
      {/* Three of these share the width of one card, so the compact form drops
          the word and leans on the title for the absolute instant, and a
          window that projects shows the projection in the reset's place, since
          a third of a card holds one of the two, and breaks it between its
          share and its horizon where even that is too wide. A whole bar keeps
          both and breaks between them where it is too narrow. */}
      <div className={`wbar__reset ${projection !== null ? 'wbar__reset--split' : ''} ${soon ? 'is-soon' : ''}`} title={`resets ${fmtAbsolute(w.resetsAt)}`}>
        {projection === null ? (
          reset
        ) : compact ? (
          <>
            <span className="wbar__part">{projection[0]}</span> <span className="wbar__part">{projection[1]}</span>
          </>
        ) : (
          <>
            <span className="wbar__part">{reset},</span> <span className="wbar__part">{projection.join(' ')}</span>
          </>
        )}
      </div>
    </div>
  );
}

/**
 * A usage-based seat's one window in the window bars' shape: the amount spent of the monthly limit in its currency in
 * the head, a meter of the share spent toned by the share left, and the reset line, `estimated` in the muted ink where
 * no head has named the reset yet. With no limit set the head reads the amount spent alone and no meter draws, and a
 * reading without a share draws its amounts with no meter.
 */
export function SpendBar({ spend, now, compact = false }: { compact?: boolean; now: number; spend: Spend }) {
  const { amounts, utilization } = spend;
  const label = compact ? 'Spend' : 'Spend limit';
  const used = amounts === null ? null : fmtMinor(amounts.usedMinor, amounts.currency, amounts.exponent);
  const limited = amounts !== null && amounts.limitMinor !== null;
  const head = amounts === null ? (utilization === null ? null : `${fmtPct(utilization)} spent`) : limited ? `${used} of ${fmtMinor(amounts.limitMinor ?? 0, amounts.currency, amounts.exponent)}` : `${used} spent`;
  return (
    <div className="wbar wbar--spend">
      <div className="wbar__head">
        <span className="wbar__label">{label}</span>
        {head !== null && <span className="wbar__pct num">{head}</span>}
      </div>
      {utilization !== null && (amounts === null || limited) && <Meter value={Math.min(1, Math.max(0, utilization))} tone={meterTone(1 - utilization)} thin label={`${label} used`} />}
      <div className="wbar__reset" title={spendResetTitle(spend)}>
        resets {fmtRelative(spend.resetsAt, now)}
        {spend.resetSource === 'estimated' && (
          <>
            , <span className="muted">estimated</span>
          </>
        )}
      </div>
    </div>
  );
}

/** Where a compact slot names a window it lacks: the slot's label says which, and the title carries the words. */
const COMPACT_ABSENT = 'none';

/**
 * The slot of a window the account does not carry: `words` muted in the share's place, in the head beside the label
 * ("no 5-hour window", "no Fable window"), so the triple keeps its three slots aligned across the list. A compact slot
 * is too narrow for the words beside its label, so it draws a short form in their place and keeps them on its title.
 */
export function WindowBarAbsent({ compact = false, label, words }: { compact?: boolean; label: string; words: string }) {
  return (
    <div className="wbar wbar--absent" title={words}>
      <div className="wbar__head">
        <span className="wbar__label">{label}</span>
        <span className="wbar__pct muted">{compact ? COMPACT_ABSENT : words}</span>
      </div>
      <div className="meter meter--thin meter--absent" role="presentation">
        <div className="meter__fill" style={{ width: '100%' }} />
      </div>
      <div className="wbar__reset muted">not reported</div>
    </div>
  );
}

/**
 * The three windows every subscription account chip shows, in one row: 5h, 7d, and Fable 7d. A window the account's
 * poll does not carry keeps its slot as a greyed placeholder naming it, so chips line up across the list: an approved
 * Ultra account reads "no 5-hour window" beside its 7-day bar, "none" on a compact card. With `others`, every further
 * window the account carries follows under its own key, as the detail card draws them.
 */
export function WindowTriple({ account, now, compact = false, others = false }: { account: Account; compact?: boolean; now: number; others?: boolean }) {
  // A usage-based seat has no plan window, so its one spend window takes the triple's place, full width.
  if (account.usageBased) {
    if (!hasSpendReading(account.spend)) return <div className={`wbar__none muted ${compact ? 'wbar__none--compact' : ''}`}>No spend reading yet</div>;
    return (
      <div className={`wbars wbars--1 ${compact ? 'wbars--compact' : ''}`}>
        <SpendBar spend={account.spend} now={now} compact={compact} />
      </div>
    );
  }
  const w5 = account.windows.find((w) => w.id === '5h');
  const w7 = account.windows.find((w) => w.id === '7d');
  const known: readonly string[] = WINDOW_ORDER;
  const rest = others ? account.windows.filter((w) => !known.includes(w.id)) : [];
  // An API-key account carries no quota window by nature, so only a subscription account's empty triple waits on a
  // read, which a compact card writes short with the sentence on its title.
  if (!w5 && !w7 && !account.fableWindow && rest.length === 0) {
    const unread = account.planEndedAt === null && isSubscription(account);
    return (
      <div className={`wbar__none muted ${compact ? 'wbar__none--compact' : ''}`} title={unread && compact ? 'Not read yet' : undefined}>
        {account.planEndedAt !== null ? planEndedSentence(account.planEndedAt, now, compact) : unread ? (compact ? 'not read' : 'Not read yet') : 'No quota windows'}
      </div>
    );
  }
  // One word each: "Fable 7d" wraps in the narrow compact columns, which drops
  // that meter a line below its two neighbors. Those columns have no room for a
  // speed chip beside the 5-hour label either, so a compact card draws it in its head.
  const labels = compact ? { '5h': '5h', '7d': '7d', fable: 'Fable' } : { '5h': '5-hour', '7d': '7-day', fable: 'Fable' };
  return (
    <div className={`wbars wbars--3 ${compact ? 'wbars--compact' : ''}`}>
      {w5 ? <WindowBar window={{ ...w5, label: labels['5h'] }} now={now} compact={compact} slowDown={compact ? null : slowedOf(account, now)} /> : <WindowBarAbsent compact={compact} label={labels['5h']} words="no 5-hour window" />}
      {w7 ? <WindowBar window={{ ...w7, label: labels['7d'] }} now={now} compact={compact} /> : <WindowBarAbsent compact={compact} label={labels['7d']} words="no 7-day window" />}
      {account.fableWindow ? <WindowBar window={{ ...account.fableWindow, label: labels.fable }} now={now} compact={compact} /> : <WindowBarAbsent compact={compact} label={labels.fable} words="no Fable window" />}
      {rest.map((w) => (
        <WindowBar key={w.id} window={w} now={now} compact={compact} />
      ))}
    </div>
  );
}
