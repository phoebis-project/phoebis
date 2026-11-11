import { useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { api } from '@/api';
import type { Account, AccountSlowDown, ExtraUsage, QuotaPoll, QuotaSnapshot, RateLimitWindow, Spend } from '@/api/types';
import { accountColor, accountSlots } from '@/components/charts/theme';
import { FilterBar } from '@/components/FilterBar';
import { SectionBar } from '@/components/SectionBar';
import { SortControl } from '@/components/SortControl';
import { useChartInk } from '@/components/charts/useChartInk';
import { SpeedChip } from '@/components/ui/SpeedChip';
import { IconChevronLeft, IconChevronRight, IconRefresh, IconReset, IconUser } from '@/components/ui/icons';
import { PopoverHost, useHoldHost } from '@/components/ui/Popover';
import { Badge, ErrorState, Meter, MeterAbsent, Pill, Segmented, Skeleton } from '@/components/ui/primitives';
import { QUOTA_CODEC, QUOTA_SORT_KEYS, sortDirOf, type QuotaAddress } from '@/lib/address';
import { CELL_MIN_PX, cellsFit, fmtAbsolute, fmtCredits, fmtDayShort, fmtEndsIn, fmtMinor, fmtMinute, fmtPct, fmtRelative, fmtShortHour, fmtShortTime, formatIn, getTimeFormat, noun, plural, upperFirst } from '@/lib/format';
import { useAsync, useHashSection, useNow, useQueryFilters, useResizeObserver } from '@/lib/hooks';
import { STATUS_LABEL, STATUS_TONE, burnWords, canceledEndOf, endOf, extraUsageState, hasSpendReading, isExpired, isQuotaListed, leadOf, meterTone, onTimeline, paceWords, pollReading, projectionWords, remainingOf, remainingTimeShare, resetOf, slowedOf, soonestResetAt, spendLeftOf, spendResetTitle, timelineLanes, windowOf, type ExtraUsageState } from '@/lib/quota';
import { readsFull, resetChip, resetMarks } from '@/lib/resets';
import { slowDownChip } from '@/lib/selection';
import { orderRows, type SortFigure } from '@/lib/sort';
import { DAY, HOUR } from '@/lib/time';
import { windowTag, windowWord } from '@/lib/windows';

const QUOTA_SECTIONS = [
  { id: 'accounts', label: 'Accounts' },
  { id: 'quota-windows', label: 'Quota windows' },
] as const;

export function QuotaPage() {
  const [intervalMs, setIntervalMs] = useState(5_000);
  const { cause: pauseCause, host } = useHoldHost();
  const accounts = useAsync(() => api.listAccounts(), [], { paused: pauseCause !== null, refreshMs: intervalMs });
  // The warning's days come with the settings and not with the list, so the page reads them once, and until they land,
  // or where the read fails, the Resets chip draws in the plain tone.
  const settings = useAsync(() => api.getSettings(), []);
  const warningDays = settings.data?.usage.limitResetWarningDays ?? null;
  const now = useNow();
  const { address, filters, update } = useQueryFilters(QUOTA_CODEC);
  const dir = sortDirOf(QUOTA_SORT_KEYS, filters);
  const [refreshing, setRefreshing] = useState(false);
  const [shown, choose] = useHashSection(QUOTA_SECTIONS, 'accounts');

  // An API-key account carries no quota window, and an account disconnected or awaiting re-authentication reports none,
  // so the page reads the other subscription accounts alone, whatever windows such a row still carries.
  const subscriptions = useMemo(() => (accounts.data ?? []).filter(isQuotaListed), [accounts.data]);
  // The slot map reads the whole registry, so a lane keeps the color its account has on the Dashboard.
  const slotOf = useMemo(() => accountSlots((accounts.data ?? []).map((a) => a.id)), [accounts.data]);
  // Each subscription row carries its poll block, so the list read is the one read of every card's standing.
  const snapById = useMemo(() => new Map(subscriptions.flatMap((a) => (a.usage ? [[a.id, a.usage] as const] : []))), [subscriptions]);

  const visible = useMemo(() => {
    const { sort } = filters;
    if (sort === 'default') return subscriptions;
    const figures: Record<Exclude<QuotaAddress['sort'], 'default'>, (a: Account) => SortFigure> = {
      pace5h: (a) => leadOf(a, '5h', now),
      pace7d: (a) => leadOf(a, '7d', now),
      paceFable: (a) => leadOf(a, '7d_fable', now),
      reset5h: (a) => resetOf(a, '5h', now),
      reset7d: (a) => resetOf(a, '7d', now),
      resetFable: (a) => resetOf(a, '7d_fable', now),
    };
    return orderRows(subscriptions, figures[sort], dir, (a) => a.label);
  }, [subscriptions, filters, dir, now]);

  const total = subscriptions.length;
  const attention = [...snapById.values()].filter((s) => Boolean(s.error) || needsAttention(s.poll)).length;

  const reload = () => accounts.reload();
  // The route reads through the poll's path, so a refresh inside the account's hold changes nothing.
  const refreshHint = 'reads the endpoint now unless the account is backing off, and records the reading as the scheduled poll does';
  const refreshAll = async () => {
    setRefreshing(true);
    // A credential the route refuses still reports through its snapshot, so one failure does not hold up the rest.
    await Promise.all(subscriptions.map((a) => api.refreshUsage(a.id).catch(() => undefined)));
    reload();
    setRefreshing(false);
  };

  const failure = accounts.error && !accounts.data ? <ErrorState title="Could not load the accounts" body={accounts.error} retry={reload} /> : null;

  return (
    <PopoverHost.Provider value={host}>
      <div className="stack">
        <FilterBar
          address={address}
          count={
            accounts.data === null ? (
              ''
            ) : (
              <>
                {plural(total, 'account')}
                {attention > 0 && <span className="filter-bar__count--critical">, {attention} {noun(attention, 'needs', 'need')} attention</span>}
              </>
            )
          }
          filters={filters}
          refresh={{ intervalMs, onIntervalChange: setIntervalMs, onRefresh: reload, paused: pauseCause, refreshTitle: 'Reads the list again now' }}
          update={update}
        >
          <SortControl keys={QUOTA_SORT_KEYS} onChange={update} value={filters} />
          <Pill onClick={() => void refreshAll()} disabled={refreshing} title={refreshHint} icon={<IconRefresh size={13} className={refreshing ? 'spin' : ''} />}>
            Refresh all
          </Pill>
        </FilterBar>

        <SectionBar sections={QUOTA_SECTIONS} shown={shown} onChoose={choose} />

        {shown === 'accounts' ? (
          <section className="quota-section" id="accounts" role="tabpanel" aria-labelledby="tab-accounts">
            {failure ?? (
              <div className="quota-grid">
                {!accounts.data
                  ? [0, 1, 2].map((i) => <QuotaCardSkeleton key={i} />)
                  : visible.map((a) => <QuotaCard key={a.id} account={a} snapshot={snapById.get(a.id)} now={now} refreshing={refreshing} refreshHint={refreshHint} onRefreshed={reload} warningDays={warningDays} />)}
              </div>
            )}
          </section>
        ) : (
          <section className="quota-section" id="quota-windows" role="tabpanel" aria-labelledby="tab-quota-windows">
            {failure ?? (accounts.data && <ResetTimeline accounts={subscriptions.filter(onTimeline)} now={now} slotOf={slotOf} />)}
          </section>
        )}
      </div>
    </PopoverHost.Provider>
  );
}

function QuotaCardSkeleton() {
  return (
    <article className="qcard">
      <header className="qcard__head">
        <Skeleton width={26} height={26} />
        <Skeleton width={120} />
      </header>
      <div className="qcard__body">
        <div className="qcard__skel">
          <Skeleton width="40%" />
          <Skeleton height={4} />
        </div>
        <div className="qcard__skel">
          <Skeleton width="55%" />
          <Skeleton height={4} />
        </div>
      </div>
    </article>
  );
}

/** A poll that last failed or is backing off wants a look even while the card still draws the windows it had. */
const needsAttention = (poll: QuotaPoll | null) => poll !== null && (poll.lastOutcome !== 'clean' || poll.backoffSince !== null);

/** What the Extra usage line says after its figures: the in-use flag's word, or a quiet state's phrase in its place. */
const EXTRA_USAGE_WORD: Record<ExtraUsageState, string> = { disabled: 'off', enabled: 'not in use', in_use: 'in use', unreported: 'not reported' };

/**
 * The Extra usage line, drawn on every card in one slot under the Fable row: in use reads in the serious tone with its
 * flag, and the quiet states read in the muted ink in the same box. The provider's reason sits inside the line, cut to
 * its width so it never grows the card, and the line's title carries it whole.
 */
function ExtraUsageLine({ extra }: { extra: ExtraUsage | null }) {
  const state = extraUsageState(extra);
  const reason = extra?.disabledReason ?? null;
  return (
    <div className="qcard__extra" title={reason ?? undefined}>
      <span className={`chip ${state === 'in_use' ? 'chip--serious' : 'qcard__extra-quiet'}`}>
        <span className="chip__label">Extra usage</span>
        {/* A reading a head wrote carries no amounts, which would read as $0.00 / $0.00. */}
        {extra && extra.enabled && extra.usedCredits !== null && extra.monthlyLimitCredits !== null && `${fmtCredits(extra.usedCredits)} / ${fmtCredits(extra.monthlyLimitCredits)}`}
        {state === 'in_use' ? <span className="chip__flag">{EXTRA_USAGE_WORD.in_use}</span> : <span>{EXTRA_USAGE_WORD[state]}</span>}
      </span>
      {reason && <span className="qcard__extra-reason ellipsis">{reason}</span>}
    </div>
  );
}

function QuotaCard({ account, snapshot, now, refreshing, refreshHint, onRefreshed, warningDays }: { account: Account; now: number; onRefreshed: () => void; refreshHint?: string; refreshing: boolean; snapshot?: QuotaSnapshot; warningDays: number | null }) {
  const soonest = soonestResetAt(account, now);
  const [localRefreshing, setLocalRefreshing] = useState(false);
  const busy = refreshing || localRefreshing;
  const onRefresh = async () => {
    setLocalRefreshing(true);
    // A refusal is worth reloading for too: the fresh snapshot carries the error this card draws in its place.
    await api.refreshUsage(account.id).catch(() => undefined);
    setLocalRefreshing(false);
    onRefreshed();
  };
  const w5 = windowOf(account, '5h');
  const w7 = windowOf(account, '7d');
  const end = endOf(account, now);
  const plan = account.plan;
  const trialEndsAt = account.subscription?.trialEndsAt ?? null;
  const endsAt = canceledEndOf(account);
  const soon = (at: string) => Date.parse(at) - now < 7 * DAY;
  const reading = pollReading(snapshot?.poll ?? null, snapshot?.fetchedAt ?? null, now);
  const chip = resetChip(account.limitResets, now, warningDays);
  const marks = resetMarks(account.limitResets);
  // Windows the card holds are drawn whatever the last read said. The error takes their place only with none, where a
  // spend reading with neither amounts nor a share counts as none.
  const failed = Boolean(snapshot?.error) && !w5 && !w7 && !account.fableWindow && !hasSpendReading(account.spend);
  return (
    <article className={`qcard ${failed ? 'is-error' : ''}`}>
      <header className="qcard__head">
        <span className="qcard__icon qcard__icon--oauth" title="Claude.ai subscription">
          <IconUser size={15} />
        </span>
        <span className="qcard__name ellipsis mono" title={account.email ?? undefined}>
          {account.label}
        </span>
        <Badge tone={STATUS_TONE[account.status]} dot>
          {STATUS_LABEL[account.status]}
        </Badge>
      </header>

      <div className="qcard__body">
        {failed ? (
          <div className="qcard__error" role="alert">
            <p>
              {snapshot?.error}
              {snapshot?.heldUntil && <>, next attempt {fmtMinute(snapshot.heldUntil)}</>}
            </p>
            <Pill onClick={() => void onRefresh()} disabled={busy} icon={<IconRefresh size={13} className={busy ? 'spin' : ''} />}>
              Retry
            </Pill>
          </div>
        ) : account.usageBased ? (
          !hasSpendReading(account.spend) ? (
            <p className="qcard__note muted">No spend reading yet.</p>
          ) : (
            <div className="qcard__windows">
              <SpendWindow spend={account.spend} now={now} soon={soonest.includes('spend')} />
            </div>
          )
        ) : !w5 && !w7 && !account.fableWindow ? (
          <p className="qcard__note muted">No quota windows reported yet.</p>
        ) : (
          <div className="qcard__windows">
            <QuotaWindow name={windowTag('5h')} absent={`no ${windowWord('5h')} window`} window={w5} end={end} now={now} resettable={marks.includes('5h')} soon={soonest.includes('5h')} slowDown={slowedOf(account, now)} />
            <QuotaWindow name={windowTag('7d')} absent={`no ${windowWord('7d')} window`} window={w7} end={end} now={now} resettable={marks.includes('7d')} soon={soonest.includes('7d')} />
            <QuotaWindow name={windowTag('7d_fable')} absent={`no ${windowWord('7d_fable')} window`} window={account.fableWindow} end={end} now={now} resettable={marks.includes('7d_fable')} soon={soonest.includes('7d_fable')} />
          </div>
        )}
      </div>

      {/* A usage-based seat's Spend row is that reading, so its Extra usage line would draw one figure twice. */}
      {!account.usageBased && <ExtraUsageLine extra={account.extraUsage} />}

      <div className="qcard__chips">
        {plan ? (
          <span className="chip" title={account.subscription?.rateLimitTier || undefined}>
            <span className="chip__label">Plan</span>
            {plan}
          </span>
        ) : (
          <span className="chip chip--empty" aria-hidden />
        )}
        {/* Unlabeled: the phrase itself says what ends, where a label would read "Ends ends in 12 days". */}
        {endsAt && (
          <span className={`chip ${soon(endsAt) ? 'chip--serious' : ''}`} title={fmtAbsolute(endsAt)}>
            {upperFirst(fmtEndsIn(endsAt, now))}
          </span>
        )}
        {trialEndsAt && (
          <span className={`chip ${soon(trialEndsAt) ? 'chip--serious' : ''}`} title={fmtAbsolute(trialEndsAt)}>
            <span className="chip__label">Trial</span>
            {fmtEndsIn(trialEndsAt, now)}
          </span>
        )}
        {/* Last, so the row gives up this chip's room before the plan's end. */}
        {chip && (
          <span className={`chip chip--resets ${chip.tone === 'warning' ? 'chip--serious' : ''}`} title={chip.title}>
            <span className="chip__label">Resets</span>
            <span className="chip__long">
              {chip.count} <span className={chip.usable ? undefined : 'muted'}>{chip.state}</span>
              {chip.ends && ` ${chip.ends}`}
            </span>
            <span className="chip__short">
              {chip.count} {chip.short}
            </span>
          </span>
        )}
      </div>

      <footer className="qcard__foot">
        <div className="qcard__status">
          {reading.lines.map((line, i) => (
            <span key={i} className={`qcard__poll ${line.tone === 'serious' ? 'qcard__backoff' : 'muted'}`} title={i === 0 && reading.title ? reading.title : undefined}>
              {line.text}
              {line.reason !== undefined && <span className="qcard__reason ellipsis">{line.reason}</span>}
            </span>
          ))}
        </div>
        <Pill onClick={() => void onRefresh()} disabled={busy} title={refreshHint} icon={<IconRefresh size={13} className={busy ? 'spin' : ''} />}>
          Refresh
        </Pill>
      </footer>
    </article>
  );
}

/** The short name and the remaining share, in the meter's own color. An expired window has no share to give, and a
 *  window the account does not carry names itself in the reset's cell, `absent` the chip's title. A `resettable` live
 *  window closes its share with the reset mark. */
function WindowChip({ absent, name, window: w, className, now, resettable }: { absent: string; className: string; name: string; now: number; resettable: boolean; window: RateLimitWindow | null }) {
  if (!w || isExpired(w, now)) {
    return (
      <span className={`${className} qchip qchip--absent`} title={w ? `${w.label}: expired, no reading since the reset` : absent}>
        <span className="qchip__name">{name}</span>
      </span>
    );
  }
  const remaining = remainingOf(w);
  // A window that prints as full has nothing a reset gives back, so it carries no mark.
  const marked = resettable && !readsFull(w, now);
  return (
    <span className={`${className} qchip qchip--${meterTone(remaining)}`} title={`${w.label}: ${fmtPct(remaining)} left${marked ? ', a reset can clear it now' : ''}`}>
      <span className="qchip__name">{name}</span>
      <span className="qchip__pct num">
        {fmtPct(remaining)} left
        {marked && <IconReset size={11} className="qchip__reset" />}
      </span>
    </span>
  );
}

/**
 * One window as three cells of the card grid: the chip, the meter, and the reset label, which carries the projection
 * at the window's horizon after the reset, breaking between the two where its column is too narrow, and names the burn
 * behind it on its hover. The grid sizes the chip and the reset to their content and hands the meter the rest, and
 * `qwin__chip` names the chip's cell for the checks that read a row by its cells. Under 900 px the reset label takes a
 * row of its own under the meter. A slowed account's 5-hour window, passed its `slowDown`, closes its reset label with
 * the speed chip. A window the account's poll does not carry keeps its row, so the cards align, with `absent`'s words
 * ("no 5-hour window") muted in the reset label's place. A window a reset usable now clears, passed `resettable`,
 * carries the reset mark in its chip.
 */
function QuotaWindow({ absent, end, name, window: w, now, resettable, slowDown = null, soon }: { absent: string; end: number | null; name: string; now: number; resettable: boolean; slowDown?: AccountSlowDown | null; soon: boolean; window: RateLimitWindow | null }) {
  // The last reading describes a window that has since reset, so its share says nothing until the next one.
  const live = w && !isExpired(w, now) ? w : null;
  const pace = live ? paceWords(live, now, end) : null;
  const reading = live ? `${live.label}: ${fmtPct(remainingOf(live))} left${pace ? `, ${pace}` : ''}` : '';
  const projection = live ? projectionWords(live, now) : null;
  const burn = live ? burnWords(live) : null;
  return (
    <>
      <WindowChip absent={absent} name={name} window={w} now={now} resettable={resettable} className="qwin__chip" />
      {live ? (
        <Meter value={remainingOf(live)} tone={meterTone(remainingOf(live))} thin label={reading} title={reading} mark={remainingTimeShare(live) ?? undefined} />
      ) : (
        <MeterAbsent />
      )}
      {live ? (
        <span className={`qwin__reset ${projection ? 'qwin__reset--split' : ''} ${soon ? 'is-soon' : ''}`} title={`${fmtAbsolute(live.resetsAt)}${soon ? ', the first of this account to recover' : ''}${burn ? `, ${burn}` : ''}`}>
          {projection ? (
            <>
              <span className="qwin__part">resets {fmtRelative(live.resetsAt, now)},</span> <span className="qwin__part">{projection}</span>
            </>
          ) : (
            `resets ${fmtRelative(live.resetsAt, now)}`
          )}
          {/* The space parts the chip from the label and lets a split label break before it. */}
          {slowDown && (
            <>
              {' '}
              <SpeedChip {...slowDownChip(slowDown, now)} />
            </>
          )}
        </span>
      ) : w ? (
        <span className="qwin__reset muted" title={`no reading since the reset, ${fmtAbsolute(w.resetsAt)}`}>
          expired {fmtRelative(w.resetsAt, now)}
        </span>
      ) : (
        <span className="qwin__reset muted">{absent}</span>
      )}
    </>
  );
}

/**
 * A usage-based seat's spend window as one row of the card grid, in a window row's shape: the chip with the share left,
 * the meter, and the reset label with the estimate's word where the router estimated it. It reads no pace, since the
 * router projects no burn on the seat. With no limit set the chip reads the amount spent in its currency and no meter
 * draws.
 */
function SpendWindow({ spend, now, soon }: { now: number; soon: boolean; spend: Spend }) {
  const { amounts } = spend;
  const unlimited = amounts !== null && amounts.limitMinor === null;
  const left = spendLeftOf(spend);
  const figures =
    amounts === null
      ? null
      : amounts.limitMinor === null
        ? `${fmtMinor(amounts.usedMinor, amounts.currency, amounts.exponent)} spent`
        : `${fmtMinor(amounts.usedMinor, amounts.currency, amounts.exponent)} of ${fmtMinor(amounts.limitMinor, amounts.currency, amounts.exponent)}`;
  const parts = [...(left === null ? [] : [`${fmtPct(left)} left`]), ...(figures === null ? [] : [figures])];
  const reading = parts.length === 0 ? 'Spend limit' : `Spend limit: ${parts.join(', ')}`;
  return (
    <>
      <span className={`qwin__chip qchip ${left === null ? 'qchip--absent' : `qchip--${meterTone(left)}`}`} title={reading}>
        <span className="qchip__name">Spend</span>
        <span className="qchip__pct num">{left !== null ? `${fmtPct(left)} left` : (figures ?? 'n/a')}</span>
      </span>
      {left !== null ? (
        <Meter value={left} tone={meterTone(left)} thin label={reading} title={reading} />
      ) : unlimited ? (
        // An empty cell keeps the reset in its column of the card's grid.
        <span aria-hidden="true" />
      ) : (
        <MeterAbsent />
      )}
      {/* The estimate's word is a part of its own, so a column too narrow for the line breaks before it, as a live
          window's label breaks before its projection. */}
      <span className={`qwin__reset ${spend.resetSource === 'estimated' ? 'qwin__reset--split' : ''} ${soon ? 'is-soon' : ''}`} title={`${spend.resetSource === 'estimated' ? spendResetTitle(spend) : fmtAbsolute(spend.resetsAt)}${soon ? ', the first of this account to recover' : ''}`}>
        {spend.resetSource === 'estimated' ? (
          <>
            <span className="qwin__part">resets {fmtRelative(spend.resetsAt, now)},</span> <span className="qwin__part muted">estimated</span>
          </>
        ) : (
          `resets ${fmtRelative(spend.resetsAt, now)}`
        )}
      </span>
    </>
  );
}

/* Reset timeline */

type Mode = 'session' | 'weekly';
/** A bar label's fallback: 0 the whole label, 1 the share alone, 2 none, the bar's title keeping it. */
type LabelStep = 0 | 1 | 2;

function spanFor(mode: Mode, offset: number, now: number, sessionCells: number) {
  const dayStart = new Date(now).setHours(0, 0, 0, 0);
  if (mode === 'session') {
    const start = dayStart - DAY + offset * 3 * DAY;
    return { startMs: start, endMs: start + 3 * DAY, cells: sessionCells, cellMs: (3 * DAY) / sessionCells };
  }
  const start = dayStart - 3 * DAY + offset * 14 * DAY;
  return { startMs: start, endMs: start + 14 * DAY, cells: 14, cellMs: DAY };
}

type DayCell = { at: number; weekend: boolean };
/** What every lane and bar of one draw reads, built once by the timeline, so all of them place against one width. */
interface LaneFrame {
  cells: DayCell[];
  now: number;
  nowPct: number | null;
  span: { endMs: number; startMs: number };
  stamp: (at: number) => string;
  steps: ReadonlyMap<string, LabelStep>;
  width: number;
}

/** A pill naming the window and how much of it is left, toned by its share as the card chips are. Without a share
 *  it sits on the grey surface. */
function LaneChip({ label, window: w, now }: { label: string; now: number; window: RateLimitWindow | null }) {
  if (!w) {
    return (
      <span className="tl-chip" title={`This plan reports no ${label.toLowerCase()}`}>
        {label} <b className="tl-chip__pct muted">n/a</b>
      </span>
    );
  }
  if (isExpired(w, now)) {
    return (
      <span className="tl-chip" title={`reset ${fmtAbsolute(w.resetsAt)}, no reading since`}>
        {label} <b className="tl-chip__pct muted">reset</b>
      </span>
    );
  }
  const remaining = remainingOf(w);
  return (
    <span className={`tl-chip tl-chip--${meterTone(remaining)}`} title={`${fmtPct(remaining)} of the ${label.toLowerCase()} left, resets ${fmtAbsolute(w.resetsAt)}`}>
      {label} <b className="tl-chip__pct num">{fmtPct(remaining)} left</b>
    </span>
  );
}

/**
 * A spend window's chip on its lane: the amount left of the limit in its currency, the amount spent alone with no limit
 * set, toned by the share left as a window's chip is, with the reset and the estimate's word on hover.
 */
function SpendChip({ spend }: { spend: Spend }) {
  const { amounts } = spend;
  const left = spendLeftOf(spend);
  const figure =
    amounts === null
      ? left === null
        ? 'n/a'
        : `${fmtPct(left)} left`
      : amounts.limitMinor === null
        ? `${fmtMinor(amounts.usedMinor, amounts.currency, amounts.exponent)} spent`
        : `${fmtMinor(Math.max(0, amounts.limitMinor - amounts.usedMinor), amounts.currency, amounts.exponent)} of ${fmtMinor(amounts.limitMinor, amounts.currency, amounts.exponent)} left`;
  return (
    <span className={`tl-chip lane__spend ${left === null ? '' : `tl-chip--${meterTone(left)}`}`} title={`${figure}, ${spendResetTitle(spend)}`}>
      Spend limit <b className="tl-chip__pct num">{figure}</b>
    </span>
  );
}

/**
 * The instant `months` calendar months from `ms` in UTC, the router's clock, a day past the month's end held to its
 * last.
 */
function addMonths(ms: number, months: number): number {
  const d = new Date(ms);
  const first = new Date(Date.UTC(d.getUTCFullYear(), d.getUTCMonth() + months, 1));
  const last = new Date(Date.UTC(first.getUTCFullYear(), first.getUTCMonth() + 1, 0)).getUTCDate();
  return Date.UTC(first.getUTCFullYear(), first.getUTCMonth(), Math.min(d.getUTCDate(), last), d.getUTCHours(), d.getUTCMinutes(), d.getUTCSeconds(), d.getUTCMilliseconds());
}

/**
 * A lane: the head with the account's dot, name, period word and chips, then the track with its piece of the day grid,
 * the bars, an optional end marker and the now line.
 */
function Lane({ account, children, chips, color, end, frame, period }: { account: Account; children: ReactNode; chips: ReactNode; color: string; end?: ReactNode; frame: LaneFrame; period: string }) {
  const { cells, nowPct } = frame;
  return (
    <div className="lane" style={{ ['--lane-color' as string]: color }}>
      <div className="lane__head">
        <div className="lane__id">
          <span className="lane__dot" />
          <span className="lane__name ellipsis" title={account.email ?? account.label}>
            {account.label}
          </span>
          <span className="lane__period">{period}</span>
        </div>
        <div className="lane__chips">{chips}</div>
      </div>
      <div className="lane__track">
        {/* First in the track, so the bars paint over it. */}
        <span className="lane__raster" aria-hidden>
          {rasterCells(cells)}
        </span>
        {children}
        {end}
        {nowPct !== null && <span className="lane__now" style={{ left: `${nowPct}%` }} aria-hidden />}
      </div>
    </div>
  );
}

/**
 * One window's bar from `start`, drawn to `stop` inside the span and titled to `end`, its reset. A live bar with a
 * `share` is filled by the window's used share, placed from the window's start up to `usedEnd` and cut to the drawn
 * part, so a fill past the now line reads as ahead of pace and one short of it as behind, the now line being the
 * elapsed mark. A bar without a share draws no fill. `extra` closes the title.
 */
function LaneBar({ account, end, extra = '', frame, share, start, stop, usedEnd }: { account: Account; end: number; extra?: string; frame: LaneFrame; share: string | null; start: number; stop: number; usedEnd: number }) {
  const { now, span, stamp, steps, width } = frame;
  const barKey = `${account.id} ${start}`;
  const step = steps.get(barKey) ?? 0;
  const state = now >= start && now < stop ? 'live' : stop <= now ? 'past' : 'next';
  const from = Math.max(start, span.startMs);
  const to = Math.min(stop, span.endMs);
  const filled = state === 'live' && share !== null;
  const used = filled ? Math.max(0, (Math.min(usedEnd, to) - from) / (to - from)) * 100 : 0;
  return (
    <div className={`lane__bar lane__bar--${state}`} style={{ left: `${((from - span.startMs) / width) * 100}%`, width: `${((to - from) / width) * 100}%` }} title={`${account.label}\n${fmtAbsolute(start)} to ${fmtAbsolute(end)}${filled ? `\n${share}` : ''}${extra}`}>
      {filled && (
        <span className="lane__clip">
          <span className="lane__used" style={{ width: `${used}%` }} />
        </span>
      )}
      {/* A live bar's label falls back to its share alone, and a reset stamp alone straight to the title. */}
      {filled ? (
        <span className="lane__label ellipsis" data-bar={barKey} hidden={step === 2}>
          {share}
          <span className="lane__label-reset" hidden={step !== 0}> · {stamp(end)}</span>
        </span>
      ) : (
        <span className="lane__label ellipsis" data-bar={barKey} hidden={step !== 0}>
          {stamp(end)}
        </span>
      )}
    </div>
  );
}

/**
 * A usage-based seat's lane: one chip for its spend limit and one bar per monthly spend window in the account's color.
 * The current window runs from one calendar month before the reading's reset to the reset, filled by the share spent
 * and labeled as a plan bar is, the upcoming one dashed where the span reaches it. With no limit set the current
 * window's outline stands alone, with no share to fill it.
 */
function SpendLane({ account, color, frame, spend }: { account: Account; color: string; frame: LaneFrame; spend: Spend }) {
  const { span } = frame;
  const reset = spend.resetsAt === null ? Number.NaN : Date.parse(spend.resetsAt);
  const unlimited = spend.amounts !== null && spend.amounts.limitMinor === null;
  const left = spendLeftOf(spend);
  const share = left === null ? null : `${fmtPct(left)} left`;
  // The reading's own window ends at its reset. Its share says nothing of any other, so only that one is filled.
  const bars: { current: boolean; end: number; start: number }[] = [];
  if (Number.isFinite(reset)) {
    let k = 0;
    while (addMonths(reset, k - 1) > span.startMs) k -= 1;
    for (; addMonths(reset, k - 1) < span.endMs; k += 1) {
      const [start, end] = [addMonths(reset, k - 1), addMonths(reset, k)];
      if (end <= span.startMs || (unlimited && k !== 0)) continue;
      bars.push({ current: k === 0, end, start });
    }
  }
  return (
    <Lane account={account} chips={<SpendChip spend={spend} />} color={color} frame={frame} period="month">
      {bars.map((b) => (
        <LaneBar
          key={b.start}
          account={account}
          end={b.end}
          frame={frame}
          share={b.current ? share : null}
          start={b.start}
          stop={b.end}
          usedEnd={b.start + Math.min(1, Math.max(0, spend.utilization ?? 0)) * (b.end - b.start)}
        />
      ))}
    </Lane>
  );
}

/**
 * A plan account's lane at the zoom's window: the three limit chips, and one bar per window projected on from the last
 * reset, the current one filled by the window's used share. A canceled account's saved end stops the bar that spans
 * it, opens no window after it and draws the end marker.
 */
function PlanLane({ account, color, frame, period, window: w }: { account: Account; color: string; frame: LaneFrame; period: string; window: RateLimitWindow }) {
  const { now, span } = frame;
  // The bars project the period on from the last reset, but an expired window's share is of the period before it, so
  // the current bar names no share: the previous window's use would fill a window the sample never read.
  const share = isExpired(w, now) ? null : `${fmtPct(remainingOf(w))} left`;
  const windowMs = w.periodHours * HOUR;
  const resetAt = Date.parse(w.resetsAt ?? '');
  // A canceled account's saved end: no window opens after it, and the one that spans it stops there.
  const endsAt = canceledEndOf(account);
  const endMs = endsAt ? Date.parse(endsAt) : Number.POSITIVE_INFINITY;
  const endPct = endMs >= span.startMs && endMs < span.endMs ? ((endMs - span.startMs) / frame.width) * 100 : null;
  const bars: { end: number; start: number; stop: number }[] = [];
  let start = resetAt - windowMs;
  while (start > span.startMs) start -= windowMs;
  for (; start < span.endMs; start += windowMs) {
    const end = start + windowMs;
    const stop = Math.min(end, endMs);
    // The drawn end decides, so a window that spans the period's start draws nothing once the plan ended at or
    // before that start.
    if (stop <= span.startMs) continue;
    if (start >= endMs) continue;
    bars.push({ start, end, stop });
  }
  return (
    <Lane
      account={account}
      chips={
        <>
          <LaneChip label="5-hour limit" window={windowOf(account, '5h')} now={now} />
          <LaneChip label="7-day limit" window={windowOf(account, '7d')} now={now} />
          <LaneChip label="Fable 7-day limit" window={account.fableWindow} now={now} />
        </>
      }
      color={color}
      end={endPct !== null && <span className="lane__end" style={{ left: `${endPct}%` }} title={`${account.label} ends ${fmtAbsolute(endsAt)}`} aria-hidden />}
      frame={frame}
      period={period}
    >
      {bars.map((b) => (
        <LaneBar
          key={b.start}
          account={account}
          end={b.end}
          extra={b.stop < b.end ? `\nends ${fmtAbsolute(endsAt)}` : ''}
          frame={frame}
          share={share}
          start={b.start}
          stop={b.stop}
          usedEnd={b.start + Math.min(1, w.utilization) * windowMs}
        />
      ))}
    </Lane>
  );
}

/** The day grid's columns, one builder for the chart's grid and each track's piece of it, so both shapes draw the same
 *  columns and weekend tint. */
function rasterCells(cells: DayCell[]) {
  return cells.map((c) => <span key={c.at} className={c.weekend ? 'is-weekend' : ''} />);
}

function ResetTimeline({ accounts, now, slotOf }: { accounts: Account[]; now: number; slotOf: (id: string) => number | null }) {
  const ink = useChartInk();
  const [mode, setMode] = useState<Mode>('weekly');
  const [offset, setOffset] = useState(0);
  // The cells row's width sets the 5-hour zoom's cell count and the short pair. A row no layout measured (jsdom) reads
  // 0 and keeps twelve cells with their full pair.
  const [rowWidth, setRowWidth] = useState(0);
  const cycle = getTimeFormat();
  const span = spanFor(mode, offset, now, rowWidth > 0 ? cellsFit(rowWidth, cycle) : 12);
  const width = span.endMs - span.startMs;
  const nowPct = now >= span.startMs && now < span.endMs ? ((now - span.startMs) / width) * 100 : null;
  const stamp = (at: number) => (mode === 'session' ? fmtShortTime(at) : `${fmtDayShort(at)} ${fmtShortTime(at)}`);

  // Each cell formats three or four labels, and the page re-renders on every accounts poll and clock tick, so the cells
  // rebuild only when the span, the zoom, the time format or the day moves. The span is a fresh object each render.
  const todayStart = new Date(now).setHours(0, 0, 0, 0);
  const cells = useMemo(
    () =>
      Array.from({ length: span.cells }, (_, i) => {
        const at = span.startMs + i * span.cellMs;
        const d = new Date(at);
        const dayStart = d.getHours() === 0;
        const head = mode === 'weekly' ? formatIn({ weekday: 'short' }, at) : dayStart ? fmtDayShort(at) : '';
        const day = formatIn({ day: 'numeric' }, at);
        const time = fmtShortTime(at);
        return {
          at,
          head,
          shortHead: mode === 'weekly' ? head.charAt(0) : dayStart ? day : '',
          shortSub: mode === 'weekly' ? day : fmtShortHour(at),
          sub: mode === 'weekly' ? fmtDayShort(at) : time,
          today: new Date(at).setHours(0, 0, 0, 0) === todayStart,
          weekend: [0, 6].includes(d.getDay()),
        };
      }),
    // eslint-disable-next-line react-hooks/exhaustive-deps -- the formatters read the time format the cycle names
    [cycle, mode, span.cellMs, span.cells, span.startMs, todayStart],
  );

  // A cell narrower than its full label prints its short pair with the full words on its title. A Weekly cell's labels
  // carry no hour, so that zoom reads the 24-hour figure under either setting.
  const cell = rowWidth / span.cells;
  const narrow = cell > 0 && cell < CELL_MIN_PX[mode === 'session' ? cycle : '24h'].full;
  // Each bar label's step, keyed by its account and its start, since two lanes can open a window at the same instant.
  const [steps, setSteps] = useState<ReadonlyMap<string, LabelStep>>(() => new Map());
  const chartRef = useRef<HTMLDivElement>(null);
  const cellsRef = useRef<HTMLDivElement>(null);
  // Every label is shown whole for the reading and its hidden parts restored before paint, so a label that fell back on
  // a narrow chart draws whole again once its bar holds it. A label no layout measured (jsdom) reads as fitting.
  const measure = () => {
    setRowWidth(cellsRef.current?.getBoundingClientRect().width ?? 0);
    const chart = chartRef.current;
    if (chart === null) return;
    // A live bar's label carries its reset as a part of its own, which the share-alone step hides.
    const labels = [...chart.querySelectorAll<HTMLElement>('.lane__label')].map((label) => ({ hidden: label.hidden, label, reset: label.querySelector<HTMLElement>('.lane__label-reset') }));
    const resetsHidden = labels.map(({ reset }) => reset?.hidden ?? false);
    const show = (withReset: boolean) => {
      for (const { label, reset } of labels) {
        label.hidden = false;
        if (reset !== null) reset.hidden = !withReset;
      }
    };
    const cut = ({ label }: { label: HTMLElement }) => label.scrollWidth > label.clientWidth;
    show(true);
    const wholeCut = labels.map(cut);
    show(false);
    const shareCut = labels.map(cut);
    labels.forEach(({ hidden, label, reset }, i) => {
      label.hidden = hidden;
      if (reset !== null) reset.hidden = resetsHidden[i];
    });
    const next = new Map<string, LabelStep>(labels.map(({ label, reset }, i) => [label.dataset.bar ?? '', !wholeCut[i] ? 0 : reset !== null && !shareCut[i] ? 1 : 2]));
    setSteps((was) => (was.size === next.size && [...next].every(([key, step]) => was.get(key) === step) ? was : next));
  };
  // Every draw can move a bar or change its words (a mode switch, a period paged, a new reading), so each one measures
  // again, and the observer covers a resize, since the tracks span the cells' width.
  useLayoutEffect(() => {
    measure();
  });
  useResizeObserver(cellsRef, measure);

  const lanes = timelineLanes(accounts, mode === 'session' ? '5h' : '7d');
  const frame: LaneFrame = { cells, now, nowPct, span, stamp, steps, width };

  return (
    <section className="timeline card card--padded">
      <header className="timeline__head">
        <div>
          <h2 className="card__title">Quota windows</h2>
          <p className="card__subtitle">
            {fmtDayShort(span.startMs)} to {fmtDayShort(span.endMs - DAY)} · {mode === 'weekly' ? 'two weeks' : 'three days'}
            {offset === 0 && ' · current'}
          </p>
        </div>
        <div className="timeline__controls">
          <div className="timeline__nav">
            <button type="button" onClick={() => setOffset((v) => v - 1)} aria-label="Previous period">
              <IconChevronLeft size={15} />
            </button>
            <button type="button" onClick={() => setOffset(0)} disabled={offset === 0}>
              Today
            </button>
            <button type="button" onClick={() => setOffset((v) => v + 1)} aria-label="Next period">
              <IconChevronRight size={15} />
            </button>
          </div>
          <Segmented<Mode>
            ariaLabel="Zoom"
            value={mode}
            onChange={(m) => {
              setMode(m);
              setOffset(0);
            }}
            options={[
              { value: 'weekly', label: 'Weekly' },
              { value: 'session', label: '5-hour' },
            ]}
          />
        </div>
      </header>

      <div ref={chartRef} className="timeline__chart">
        <div className="timeline__axis">
          <div className="timeline__axis-label">Account</div>
          <div ref={cellsRef} className="timeline__cells">
            {cells.map((c) => (
              <div key={c.at} className={`timeline__cell ${c.today ? 'is-today' : ''}`} title={narrow ? [c.head, c.sub].filter((part) => part !== '').join(' ') : undefined}>
                <span className="timeline__cell-head">{narrow ? c.shortHead : c.head}</span>
                <span className="timeline__cell-sub">{narrow ? c.shortSub : c.sub}</span>
              </div>
            ))}
            {/* The now line's pill in the stacked shape, in the axis row above every lane's head. */}
            {nowPct !== null && (
              <span className="timeline__now-pill" style={{ left: `${nowPct}%` }}>
                now
              </span>
            )}
          </div>
        </div>

        <div className="timeline__lanes">
          {/* One raster and one now line for the whole chart rather than a copy
              per lane, so both read as a single ruler down the rows. Under
              900 px the stacked heads cross the chart, so each track draws its
              own piece of the grid and of the line there (`lane__raster`,
              `lane__now`). */}
          <div className="timeline__raster" aria-hidden>
            {rasterCells(cells)}
          </div>
          {nowPct !== null && (
            <div className="timeline__now" style={{ left: `calc(var(--tl-pad) + var(--tl-left) + var(--tl-gap) + (100% - 2 * var(--tl-pad) - var(--tl-left) - var(--tl-gap)) * ${nowPct / 100})` }}>
              <span className="timeline__now-label">now</span>
            </div>
          )}

          {lanes.map(({ account, spend, window: w }) =>
            w === null ? (
              spend === null ? null : <SpendLane key={account.id} account={account} color={accountColor(ink, slotOf(account.id))} frame={frame} spend={spend} />
            ) : (
              <PlanLane key={account.id} account={account} color={accountColor(ink, slotOf(account.id))} frame={frame} period={mode === 'session' ? '5h' : '7d'} window={w} />
            ),
          )}
        </div>
      </div>

      <footer className="timeline__legend">
        <span>
          <i className="lane__swatch lane__swatch--live" /> current window
        </span>
        <span>
          <i className="lane__swatch lane__swatch--next" /> upcoming
        </span>
        <span>
          <i className="lane__swatch lane__swatch--used" /> used
        </span>
        <span>
          <i className="lane__swatch lane__swatch--past" /> past window
        </span>
        <span>
          <i className="lane__swatch lane__swatch--now" /> now
        </span>
        <span>
          <i className="lane__swatch lane__swatch--end" /> plan ends
        </span>
        <span className="timeline__note muted">Each bar is one full quota window, drawn from when it opened to when it resets, and labeled with the instant it resets. Lanes ending together compete for the same days.</span>
      </footer>
    </section>
  );
}
