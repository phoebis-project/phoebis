import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { api } from '@/api';
import type { Account, AccountSlowDown, ExtraUsage, QuotaPoll, QuotaSnapshot, RateLimitWindow } from '@/api/types';
import { accountColor, accountSlots } from '@/components/charts/theme';
import { FilterBar } from '@/components/FilterBar';
import { SectionBar } from '@/components/SectionBar';
import { useChartInk } from '@/components/charts/useChartInk';
import { SpeedChip } from '@/components/ui/SpeedChip';
import { IconChevronLeft, IconChevronRight, IconRefresh, IconUser } from '@/components/ui/icons';
import { Badge, ErrorState, Meter, Pill, Segmented, Select, Skeleton } from '@/components/ui/primitives';
import { CELL_MIN_PX, cellsFit, fmtAbsolute, fmtCredits, fmtDayShort, fmtEndsIn, fmtMinute, fmtPct, fmtRelative, fmtShortHour, fmtShortTime, getTimeFormat, upperFirst } from '@/lib/format';
import { useAsync, useNow } from '@/lib/hooks';
import { STATUS_LABEL, STATUS_TONE, burnWords, endOf, extraUsageState, isExpired, isSubscription, meterTone, paceWords, pollReading, projectionWords, remainingOf, remainingTimeShare, slowedOf, soonestReset, sortByPace, windowOf, type ExtraUsageState } from '@/lib/quota';
import { speedHover } from '@/lib/selection';

type Sort = 'default' | 'pace5h' | 'pace7d' | 'paceWeek' | 'soonest';

const QUOTA_SECTIONS = [
  { id: 'accounts', label: 'Accounts' },
  { id: 'quota-windows', label: 'Quota windows' },
] as const;
type QuotaSectionId = (typeof QUOTA_SECTIONS)[number]['id'];

export function QuotaPage() {
  const [intervalMs, setIntervalMs] = useState(5_000);
  const [barOpen, setBarOpen] = useState(false);
  const accounts = useAsync(() => api.listAccounts(), [], { paused: barOpen, refreshMs: intervalMs });
  const now = useNow();
  const [sort, setSort] = useState<Sort>('default');
  const [refreshing, setRefreshing] = useState(false);
  const location = useLocation();
  const navigate = useNavigate();
  // The hash is the section, as on the Analytics page: a link or a reload lands on the one it names, and a choice
  // replaces the entry with the search kept, so Back leaves the page rather than walking the sections.
  const shown = useMemo<QuotaSectionId>(() => {
    const id = location.hash.slice(1);
    return QUOTA_SECTIONS.some((s) => s.id === id) ? (id as QuotaSectionId) : 'accounts';
  }, [location.hash]);
  const choose = (id: QuotaSectionId) => {
    navigate({ hash: `#${id}`, pathname: location.pathname, search: location.search }, { replace: true });
    // jsdom implements no `scrollTo`, so the call is guarded.
    document.scrollingElement?.scrollTo?.({ top: 0 });
  };

  // An API-key account carries no quota window, so the page reads the subscription accounts alone.
  const subscriptions = useMemo(() => (accounts.data ?? []).filter(isSubscription), [accounts.data]);
  // The slot map reads the whole registry, so a lane keeps the color its account has on the Dashboard.
  const slotOf = useMemo(() => accountSlots((accounts.data ?? []).map((a) => a.id)), [accounts.data]);
  // Each subscription row carries its poll block, so the list read is the one read of every card's standing.
  const snapById = useMemo(() => new Map(subscriptions.flatMap((a) => (a.usage ? [[a.id, a.usage] as const] : []))), [subscriptions]);

  const visible = useMemo(() => {
    if (sort === 'pace5h') return sortByPace(subscriptions, '5h', now);
    if (sort === 'pace7d') return sortByPace(subscriptions, '7d', now);
    if (sort === 'paceWeek') return sortByPace(subscriptions, '7d_fable', now);
    if (sort === 'soonest') {
      const nextReset = (a: Account) => {
        const future = a.windows.flatMap((w) => (w.resetsAt === null ? [] : [Date.parse(w.resetsAt)])).filter((t) => t > now);
        return future.length ? Math.min(...future) : Number.POSITIVE_INFINITY;
      };
      return [...subscriptions].sort((a, b) => nextReset(a) - nextReset(b));
    }
    return subscriptions;
  }, [subscriptions, sort, now]);

  const total = subscriptions.length;
  const attention = [...snapById.values()].filter((s) => s.status === 'error' || needsAttention(s.poll)).length;

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

  const failure = accounts.error && !accounts.data ? <ErrorState title="Could not load the accounts" body={accounts.error} action={<Pill onClick={reload}>Try again</Pill>} /> : null;

  return (
    <div className="stack">
      <FilterBar
        address="/quota"
        count={
          accounts.data === null ? (
            ''
          ) : (
            <>
              {total} {total === 1 ? 'account' : 'accounts'}
              {attention > 0 && <span className="filter-bar__count--critical">, {attention} {attention === 1 ? 'needs' : 'need'} attention</span>}
            </>
          )
        }
        filters={{}}
        onOpenChange={setBarOpen}
        refresh={{ intervalMs, onIntervalChange: setIntervalMs, onRefresh: reload, paused: barOpen ? 'menu' : null, refreshTitle: 'Reads the list again now' }}
        update={() => undefined}
      >
        <Pill onClick={() => void refreshAll()} disabled={refreshing} title={refreshHint} icon={<IconRefresh size={13} className={refreshing ? 'spin' : ''} />}>
          Refresh all
        </Pill>
      </FilterBar>

      <SectionBar sections={QUOTA_SECTIONS} shown={shown} onChoose={choose} />

      {shown === 'accounts' ? (
        <section className="quota-section" id="accounts" role="tabpanel" aria-labelledby="tab-accounts">
          <div className="quota-sort">
            <Select value={sort} onChange={(e) => setSort(e.target.value as Sort)} aria-label="Sort">
              <option value="default">Default order</option>
              <option value="soonest">Soonest recovery</option>
              <option value="pace5h">5-hour pace</option>
              <option value="pace7d">7-day pace</option>
              <option value="paceWeek">7-day Fable pace</option>
            </Select>
          </div>
          {failure ?? (
            <div className="quota-grid">
              {!accounts.data
                ? [0, 1, 2].map((i) => <QuotaCardSkeleton key={i} />)
                : visible.map((a) => <QuotaCard key={a.id} account={a} snapshot={snapById.get(a.id)} now={now} refreshing={refreshing} refreshHint={refreshHint} onRefreshed={reload} />)}
            </div>
          )}
        </section>
      ) : (
        <section className="quota-section" id="quota-windows" role="tabpanel" aria-labelledby="tab-quota-windows">
          {failure ?? (accounts.data && <ResetTimeline accounts={subscriptions.filter((a) => a.windows.length > 0)} now={now} slotOf={slotOf} />)}
        </section>
      )}
    </div>
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
        {extra && extra.enabled && `${fmtCredits(extra.usedCredits)} / ${fmtCredits(extra.monthlyLimitCredits)}`}
        {state === 'in_use' ? <span className="chip__flag">{EXTRA_USAGE_WORD.in_use}</span> : <span>{EXTRA_USAGE_WORD[state]}</span>}
      </span>
      {reason && <span className="qcard__extra-reason">{reason}</span>}
    </div>
  );
}

function QuotaCard({ account, snapshot, now, refreshing, refreshHint, onRefreshed }: { account: Account; now: number; onRefreshed: () => void; refreshHint?: string; refreshing: boolean; snapshot?: QuotaSnapshot }) {
  const status = snapshot?.status ?? 'success';
  const soonest = soonestReset(account, now);
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
  const endsAt = account.cancelled ? (account.subscription?.endsAt ?? null) : null;
  const soon = (at: string) => Date.parse(at) - now < 7 * DAY;
  const reading = pollReading(snapshot?.poll ?? null, snapshot?.fetchedAt ?? null, now);
  // Windows the card holds are drawn whatever the last read said; the error takes their place only with none.
  const failed = status === 'error' && !w5 && !w7 && !account.fableWindow;
  return (
    <article className={`qcard ${failed ? 'is-error' : ''}`}>
      <header className="qcard__head">
        <span className="qcard__icon qcard__icon--oauth" title="Claude.ai subscription">
          <IconUser size={15} />
        </span>
        <span className="qcard__name mono" title={account.email ?? undefined}>
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
        ) : !w5 && !w7 && !account.fableWindow ? (
          <p className="qcard__note muted">No quota windows reported yet.</p>
        ) : (
          <div className="qcard__windows">
            <QuotaWindow name="5h" window={w5} end={end} now={now} soon={soonest === '5h'} slowDown={slowedOf(account, now)} />
            <QuotaWindow name="7d" window={w7} end={end} now={now} soon={soonest === '7d'} />
            <QuotaWindow name="Fable" window={account.fableWindow} end={end} now={now} soon={soonest === '7d_fable'} />
          </div>
        )}
      </div>

      <ExtraUsageLine extra={account.extraUsage} />

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
      </div>

      <footer className="qcard__foot">
        <div className="qcard__status">
          {reading.lines.map((line, i) => (
            <span key={i} className={`qcard__poll ${line.tone === 'serious' ? 'qcard__backoff' : 'muted'}`} title={i === 0 && reading.title ? reading.title : undefined}>
              {line.text}
              {line.reason !== undefined && <span className="qcard__reason">{line.reason}</span>}
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

/** The short name and the remaining share, in the meter's own color; an expired window has no share to give. */
function WindowChip({ name, window: w, className, now }: { className: string; name: string; now: number; window: RateLimitWindow | null }) {
  if (!w) {
    return (
      <span className={`${className} qchip qchip--absent`} title={`This plan reports no ${name} window`}>
        <span className="qchip__name">{name}</span>
        <span className="qchip__pct">n/a</span>
      </span>
    );
  }
  if (isExpired(w, now)) {
    return (
      <span className={`${className} qchip qchip--absent`} title={`${w.label}: expired, no reading since the reset`}>
        <span className="qchip__name">{name}</span>
      </span>
    );
  }
  const remaining = remainingOf(w);
  return (
    <span className={`${className} qchip qchip--${meterTone(remaining)}`} title={`${w.label}: ${fmtPct(remaining)} left`}>
      <span className="qchip__name">{name}</span>
      <span className="qchip__pct num">{fmtPct(remaining)} left</span>
    </span>
  );
}

/**
 * One window as three cells of the card grid: the chip, the meter, and the reset label, which carries the projection
 * at the window's horizon after the reset, breaking between the two where its column is too narrow, and names the burn
 * behind it on its hover. The grid sizes the chip and the reset to their content and hands the meter the rest, and
 * `qwin__chip` names the chip's cell for the checks that read a row by its cells. Under 900 px the reset label takes a
 * row of its own under the meter. A slowed account's 5-hour window, passed its `slowDown`, closes its reset label with
 * the speed chip.
 */
function QuotaWindow({ end, name, window: w, now, slowDown = null, soon }: { end: number | null; name: string; now: number; slowDown?: AccountSlowDown | null; soon: boolean; window: RateLimitWindow | null }) {
  // The last reading describes a window that has since reset, so its share says nothing until the next one.
  const live = w && !isExpired(w, now) ? w : null;
  const pace = live ? paceWords(live, now, end) : null;
  const reading = live ? `${live.label}: ${fmtPct(remainingOf(live))} left${pace ? `, ${pace}` : ''}` : '';
  const projection = live ? projectionWords(live, now) : null;
  const burn = live ? burnWords(live) : null;
  return (
    <>
      <WindowChip name={name} window={w} now={now} className="qwin__chip" />
      {live ? (
        <Meter value={remainingOf(live)} tone={meterTone(remainingOf(live))} thin label={reading} title={reading} mark={remainingTimeShare(live) ?? undefined} />
      ) : (
        <div className="meter meter--thin meter--absent" role="presentation">
          <div className="meter__fill" style={{ width: '100%' }} />
        </div>
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
              <SpeedChip factor={slowDown.factor} hover={speedHover(slowDown, now)} />
            </>
          )}
        </span>
      ) : w ? (
        <span className="qwin__reset muted" title={`no reading since the reset, ${fmtAbsolute(w.resetsAt)}`}>
          expired {fmtRelative(w.resetsAt, now)}
        </span>
      ) : (
        <span className="qwin__reset muted">not reported</span>
      )}
    </>
  );
}

/* Reset timeline */

type Mode = 'session' | 'weekly';
/** A bar label's fallback: 0 the whole label, 1 the share alone, 2 none, the bar's title keeping it. */
type LabelStep = 0 | 1 | 2;
const HOUR = 3_600_000;
const DAY = 24 * HOUR;

function spanFor(mode: Mode, offset: number, now: number, sessionCells: number) {
  const dayStart = new Date(now).setHours(0, 0, 0, 0);
  if (mode === 'session') {
    const start = dayStart - DAY + offset * 3 * DAY;
    return { startMs: start, endMs: start + 3 * DAY, cells: sessionCells, cellMs: (3 * DAY) / sessionCells };
  }
  const start = dayStart - 3 * DAY + offset * 14 * DAY;
  return { startMs: start, endMs: start + 14 * DAY, cells: 14, cellMs: DAY };
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

/** The day grid's columns, one builder for the chart's grid and each track's piece of it, so both shapes draw the same
 *  columns and weekend tint. */
function rasterCells(cells: { at: number; weekend: boolean }[]) {
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
  const stamp = (at: number) => (mode === 'session' ? fmtShortTime(new Date(at).toISOString()) : `${fmtDayShort(at)} ${fmtShortTime(new Date(at).toISOString())}`);

  const cells = Array.from({ length: span.cells }, (_, i) => {
    const at = span.startMs + i * span.cellMs;
    const d = new Date(at);
    const dayStart = d.getHours() === 0;
    const head = mode === 'weekly' ? d.toLocaleDateString('en-US', { weekday: 'short' }) : dayStart ? fmtDayShort(at) : '';
    const day = d.toLocaleDateString('en-US', { day: 'numeric' });
    const time = fmtShortTime(new Date(at).toISOString());
    return {
      at,
      head,
      shortHead: mode === 'weekly' ? head.charAt(0) : dayStart ? day : '',
      shortSub: mode === 'weekly' ? day : fmtShortHour(new Date(at).toISOString()),
      sub: mode === 'weekly' ? fmtDayShort(at) : time,
      today: new Date(at).setHours(0, 0, 0, 0) === new Date(now).setHours(0, 0, 0, 0),
      weekend: [0, 6].includes(d.getDay()),
    };
  });

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
  const measureRef = useRef(measure);
  // Every draw can move a bar or change its words (a mode switch, a period paged, a new reading), so each one measures
  // again, and the observer covers a resize, since the tracks span the cells' width.
  useLayoutEffect(() => {
    measureRef.current = measure;
    measure();
  });
  useEffect(() => {
    const row = cellsRef.current;
    if (row === null || typeof ResizeObserver !== 'function') return;
    const observer = new ResizeObserver(() => measureRef.current());
    observer.observe(row);
    return () => observer.disconnect();
  }, []);

  const lanes = accounts
    .map((a) => ({ account: a, window: windowOf(a, mode === 'session' ? '5h' : '7d') }))
    .filter((l): l is { account: Account; window: RateLimitWindow } => l.window !== null);

  return (
    <section className="timeline card">
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

          {lanes.map(({ account, window: w }) => {
            // The bars project the period on from the last reset, but an expired window's share is of the period
            // before it, so the current bar names no share.
            const share = isExpired(w, now) ? null : `${fmtPct(remainingOf(w))} left`;
            const period = w.periodHours * HOUR;
            const resetAt = Date.parse(w.resetsAt ?? '');
            // A cancelled account's saved end: no window opens after it, and the one that spans it stops there.
            const endsAt = account.cancelled ? (account.subscription?.endsAt ?? null) : null;
            const endMs = endsAt ? Date.parse(endsAt) : Number.POSITIVE_INFINITY;
            const endPct = endMs >= span.startMs && endMs < span.endMs ? ((endMs - span.startMs) / width) * 100 : null;
            const bars: { end: number; start: number; state: 'live' | 'next' | 'past'; stop: number }[] = [];
            let start = resetAt - period;
            while (start > span.startMs) start -= period;
            for (; start < span.endMs; start += period) {
              const end = start + period;
              const stop = Math.min(end, endMs);
              // The drawn end decides, so a window that spans the period's start draws nothing once the plan ended
              // at or before that start.
              if (stop <= span.startMs) continue;
              if (start >= endMs) continue;
              bars.push({ start, end, state: now >= start && now < stop ? 'live' : stop <= now ? 'past' : 'next', stop });
            }
            return (
              <div key={account.id} className="lane" style={{ ['--lane-color' as string]: accountColor(ink, slotOf(account.id)) }}>
                <div className="lane__head">
                  <div className="lane__id">
                    <span className="lane__dot" />
                    <span className="lane__name" title={account.email ?? account.label}>
                      {account.label}
                    </span>
                    <span className="lane__period">{mode === 'session' ? '5h' : '7d'}</span>
                  </div>
                  <div className="lane__chips">
                    <LaneChip label="5-hour limit" window={windowOf(account, '5h')} now={now} />
                    <LaneChip label="7-day limit" window={windowOf(account, '7d')} now={now} />
                    <LaneChip label="Fable 7-day limit" window={account.fableWindow} now={now} />
                  </div>
                </div>
                <div className="lane__track">
                  {/* First in the track, so the bars paint over it. */}
                  <span className="lane__raster" aria-hidden>
                    {rasterCells(cells)}
                  </span>
                  {bars.map((b) => {
                    const from = Math.max(b.start, span.startMs);
                    const to = Math.min(b.stop, span.endMs);
                    const wPct = ((to - from) / width) * 100;
                    const key = `${account.id} ${b.start}`;
                    const step = steps.get(key) ?? 0;
                    // The fill is the window's used share placed from the window's start and cut to the drawn part,
                    // so a fill past the now line reads as ahead of pace and one short of it as behind. It draws only
                    // while the sample is current: an expired sample's bar is projected past its reset, and the
                    // previous window's use would fill a window the sample never read. The now line is the elapsed
                    // mark.
                    const usedEnd = b.start + Math.min(1, w.utilization) * period;
                    const used = b.state === 'live' && share !== null ? Math.max(0, (Math.min(usedEnd, to) - from) / (to - from)) * 100 : 0;
                    return (
                      <div
                        key={b.start}
                        className={`lane__bar lane__bar--${b.state}`}
                        style={{ left: `${((from - span.startMs) / width) * 100}%`, width: `${wPct}%` }}
                        title={`${account.label}\n${fmtAbsolute(new Date(b.start).toISOString())} to ${fmtAbsolute(new Date(b.end).toISOString())}${b.state === 'live' && share ? `\n${share}` : ''}${b.stop < b.end ? `\nends ${fmtAbsolute(endsAt)}` : ''}`}
                      >
                        {b.state === 'live' && share !== null && (
                          <span className="lane__clip">
                            <span className="lane__used" style={{ width: `${used}%` }} />
                          </span>
                        )}
                        {/* A live bar's label falls back to its share alone, and a reset stamp alone straight to the title. */}
                        {b.state === 'live' && share !== null ? (
                          <span className="lane__label" data-bar={key} hidden={step === 2}>
                            {share}
                            <span className="lane__label-reset" hidden={step !== 0}> · {stamp(b.end)}</span>
                          </span>
                        ) : (
                          <span className="lane__label" data-bar={key} hidden={step !== 0}>
                            {stamp(b.end)}
                          </span>
                        )}
                      </div>
                    );
                  })}
                  {endPct !== null && <span className="lane__end" style={{ left: `${endPct}%` }} title={`${account.label} ends ${fmtAbsolute(endsAt)}`} aria-hidden />}
                  {nowPct !== null && <span className="lane__now" style={{ left: `${nowPct}%` }} aria-hidden />}
                </div>
              </div>
            );
          })}
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
