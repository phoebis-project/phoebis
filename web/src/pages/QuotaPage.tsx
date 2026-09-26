import { useMemo, useState } from 'react';
import { api } from '@/api';
import type { Account, QuotaSnapshot, RateLimitWindow } from '@/api/types';
import { accountColor } from '@/components/charts/theme';
import { useChartInk } from '@/components/charts/useChartInk';
import { IconAnthropic, IconChevronLeft, IconChevronRight, IconRefresh, IconServer, IconWarning } from '@/components/ui/icons';
import { Badge, ErrorState, Meter, Pill, Segmented, Select, Skeleton, Tabs } from '@/components/ui/primitives';
import { fmtAbsolute, fmtCredits, fmtDate, fmtDayShort, fmtEndsIn, fmtPct, fmtRelative, fmtShortTime } from '@/lib/format';
import { useAsync, useNow } from '@/lib/hooks';
import { STATUS_LABEL, STATUS_TONE, meterTone, remainingOf, soonestReset, windowOf } from '@/lib/quota';

type Tab = 'all' | 'api_key' | 'oauth';
type Sort = 'default' | 'soonest';

const capitalize = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

export function QuotaPage() {
  const accounts = useAsync(() => api.listAccounts(), []);
  const snapshots = useAsync(() => api.listQuotaSnapshots(), []);
  const settings = useAsync(() => api.getSettings(), []);
  const now = useNow();
  const [tab, setTab] = useState<Tab>('all');
  const [sort, setSort] = useState<Sort>('default');
  const [refreshing, setRefreshing] = useState(false);

  const snapById = useMemo(() => new Map((snapshots.data ?? []).map((s) => [s.accountId, s])), [snapshots.data]);

  const visible = useMemo(() => {
    const list = (accounts.data ?? []).filter((a) => tab === 'all' || a.kind === tab);
    if (sort === 'soonest') {
      const nextReset = (a: Account) => {
        const future = a.windows.flatMap((w) => (w.resetsAt === null ? [] : [Date.parse(w.resetsAt)])).filter((t) => t > now);
        return future.length ? Math.min(...future) : Number.POSITIVE_INFINITY;
      };
      return [...list].sort((a, b) => nextReset(a) - nextReset(b));
    }
    return list;
  }, [accounts.data, tab, sort, now]);

  const total = accounts.data?.length ?? 0;
  const loaded = (snapshots.data ?? []).filter((s) => s.status !== 'loading').length;
  const attention = (snapshots.data ?? []).filter((s) => s.status === 'error').length;
  const counts = { all: total, api_key: (accounts.data ?? []).filter((a) => a.kind === 'api_key').length, oauth: (accounts.data ?? []).filter((a) => a.kind === 'oauth').length };

  const reload = () => {
    accounts.reload();
    snapshots.reload();
  };
  // The route hands back a cached snapshot inside this TTL, which is why a refresh can change nothing.
  const refreshHint = settings.data ? `a snapshot younger than ${settings.data.usage.cacheTtlSeconds} seconds is served as is` : undefined;
  const refreshAll = async () => {
    setRefreshing(true);
    // A credential the route refuses still reports through its snapshot, so one failure does not hold up the rest.
    await Promise.all((accounts.data ?? []).filter((a) => a.kind === 'oauth').map((a) => api.refreshUsage(a.id).catch(() => undefined)));
    reload();
    setRefreshing(false);
  };

  return (
    <div className="stack">
      <div className="page-head">
        <div>
          <p className="page-head__meta quota-meta">
            <span>{total} {total === 1 ? 'credential' : 'credentials'}</span>
            <span className="quota-meta__sep" aria-hidden />
            <span className={loaded ? '' : 'muted'}>{loaded} loaded</span>
            {attention > 0 && (
              <>
                <span className="quota-meta__sep" aria-hidden />
                <span className="quota-meta__attention">{attention} {attention === 1 ? 'needs' : 'need'} attention</span>
              </>
            )}
          </p>
        </div>
        <Pill onClick={() => void refreshAll()} disabled={refreshing} title={refreshHint} icon={<IconRefresh size={13} className={refreshing ? 'spin' : ''} />}>
          Refresh all
        </Pill>
      </div>

      <div className="quota-toolbar">
        <Tabs<Tab>
          value={tab}
          onChange={setTab}
          tabs={[
            { value: 'all', label: 'All', count: counts.all },
            { value: 'oauth', label: 'OAuth', count: counts.oauth },
            { value: 'api_key', label: 'API key', count: counts.api_key },
          ]}
        />
        <Select value={sort} onChange={(e) => setSort(e.target.value as Sort)} aria-label="Sort">
          <option value="default">Default order</option>
          <option value="soonest">Soonest recovery</option>
        </Select>
      </div>

      {accounts.error && !accounts.data ? (
        <ErrorState title="Could not load the upstreams" body={accounts.error} action={<Pill onClick={reload}>Try again</Pill>} />
      ) : (
        <div className="quota-grid">
          {!accounts.data
            ? [0, 1, 2].map((i) => <QuotaCardSkeleton key={i} />)
            : visible.map((a) => <QuotaCard key={a.id} account={a} snapshot={snapById.get(a.id)} now={now} refreshing={refreshing} refreshHint={refreshHint} onRefreshed={reload} />)}
        </div>
      )}

      {accounts.data && <ResetTimeline accounts={accounts.data.filter((a) => a.kind === 'oauth' && a.windows.length > 0)} now={now} />}
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
  const extra = account.extraUsage;
  const since = account.subscription?.since ?? null;
  const trialEndsAt = account.subscription?.trialEndsAt ?? null;
  const endsAt = account.cancelled ? (account.subscription?.endsAt ?? null) : null;
  const soon = (at: string) => Date.parse(at) - now < 7 * DAY;
  return (
    <article className={`qcard ${status === 'error' ? 'is-error' : ''}`}>
      <header className="qcard__head">
        <span className={`qcard__icon qcard__icon--${account.kind}`} title={account.kind === 'oauth' ? 'Claude.ai subscription' : 'API key upstream'}>
          {account.kind === 'oauth' ? <IconAnthropic size={15} /> : <IconServer size={15} />}
        </span>
        <span className="qcard__name mono" title={account.email ?? undefined}>
          {account.label}
        </span>
        <Badge tone={STATUS_TONE[account.status]} dot>
          {STATUS_LABEL[account.status]}
        </Badge>
      </header>

      <div className="qcard__body">
        {status === 'loading' ? (
          <div className="qcard__skel-wrap" aria-busy="true">
            <span className="sr-only">Loading quota</span>
            <div className="qcard__skel">
              <Skeleton width="40%" />
              <Skeleton height={4} />
            </div>
            <div className="qcard__skel">
              <Skeleton width="55%" />
              <Skeleton height={4} />
            </div>
          </div>
        ) : status === 'error' ? (
          <div className="qcard__error" role="alert">
            <p>{snapshot?.error}</p>
            <Pill onClick={() => void onRefresh()} disabled={busy} icon={<IconRefresh size={13} className={busy ? 'spin' : ''} />}>
              Retry
            </Pill>
          </div>
        ) : !w5 && !w7 && !account.fableWindow ? (
          <p className="qcard__note muted">No quota windows. API-key upstreams bill per token and are limited only by the workspace tier.</p>
        ) : (
          <div className="qcard__windows">
            <QuotaWindow name="5h" window={w5} now={now} soon={soonest === '5h'} />
            <QuotaWindow name="7d" window={w7} now={now} soon={soonest === '7d'} />
            <QuotaWindow name="Fable" window={account.fableWindow} now={now} soon={soonest === '7d_fable'} />
          </div>
        )}
      </div>

      {status !== 'loading' && (account.plan || endsAt || trialEndsAt || extra?.enabled) && (
        <div className="qcard__chips">
          {account.plan && (
            <span className="chip" title={account.subscription?.rateLimitTier || undefined}>
              <span className="chip__label">Plan</span>
              {account.plan}
              {since && ` since ${fmtDate(since)}`}
            </span>
          )}
          {/* Unlabelled: the phrase itself says what ends, where a label would read "Ends ends in 12 days". */}
          {endsAt && (
            <span className={`chip ${soon(endsAt) ? 'chip--serious' : ''}`} title={fmtAbsolute(endsAt)}>
              {capitalize(fmtEndsIn(endsAt, now))}
            </span>
          )}
          {trialEndsAt && (
            <span className={`chip ${soon(trialEndsAt) ? 'chip--serious' : ''}`} title={fmtAbsolute(trialEndsAt)}>
              <span className="chip__label">Trial</span>
              {fmtEndsIn(trialEndsAt, now)}
            </span>
          )}
          {extra?.enabled && (
            <span className={`chip ${extra.inUse ? 'chip--serious' : ''}`} title={extra.disabledReason ?? undefined}>
              <span className="chip__label">Extra usage</span>
              {fmtCredits(extra.usedCredits)} / {fmtCredits(extra.monthlyLimitCredits)}
              {extra.inUse && <span className="chip__flag">in use</span>}
            </span>
          )}
        </div>
      )}

      {status !== 'loading' && extra?.disabledReason && (
        <div className="callout callout--warning qcard__flag">
          <IconWarning size={14} />
          <div>{extra.disabledReason}</div>
        </div>
      )}

      {status !== 'loading' && (
        <footer className="qcard__foot">
          <span className="muted" title={snapshot?.fetchedAt ? fmtAbsolute(snapshot.fetchedAt) : undefined}>
            {snapshot?.fetchedAt ? `checked ${fmtRelative(snapshot.fetchedAt, now)}` : ''}
          </span>
          <Pill onClick={() => void onRefresh()} disabled={busy} title={refreshHint} icon={<IconRefresh size={13} className={busy ? 'spin' : ''} />}>
            Refresh
          </Pill>
        </footer>
      )}
    </article>
  );
}

/** The short name and the remaining share, in the meter's own color. */
function WindowChip({ name, window: w, className }: { className: string; name: string; window: RateLimitWindow | null }) {
  if (!w) {
    return (
      <span className={`${className} qchip qchip--absent`} title={`This plan reports no ${name} window`}>
        <span className="qchip__name">{name}</span>
        <span className="qchip__pct">n/a</span>
      </span>
    );
  }
  const remaining = remainingOf(w);
  return (
    <span className={`${className} qchip qchip--${meterTone(remaining)}`} title={`${w.label}: ${fmtPct(remaining)} of the window left`}>
      <span className="qchip__name">{name}</span>
      <span className="qchip__pct num">{fmtPct(remaining)}</span>
    </span>
  );
}

/**
 * One window as three cells of the card grid: the chip, the meter, and the
 * reset label. `qwin__chip` marks the cell the fixed first column sizes, so
 * every row of every card hands its meter the same starting x.
 */
function QuotaWindow({ name, window: w, now, soon }: { name: string; now: number; soon: boolean; window: RateLimitWindow | null }) {
  return (
    <>
      <WindowChip name={name} window={w} className="qwin__chip" />
      {w ? (
        <Meter value={remainingOf(w)} tone={meterTone(remainingOf(w))} thin label={`${w.label} remaining`} />
      ) : (
        <div className="meter meter--thin meter--absent" role="presentation">
          <div className="meter__fill" style={{ width: '100%' }} />
        </div>
      )}
      {w ? (
        <span className={`qwin__reset ${soon ? 'is-soon' : ''}`} title={`${fmtAbsolute(w.resetsAt)}${soon ? ', the first of this upstream to recover' : ''}`}>
          resets {fmtRelative(w.resetsAt, now)}
        </span>
      ) : (
        <span className="qwin__reset muted">not reported</span>
      )}
    </>
  );
}

/* Reset timeline */

type Mode = 'session' | 'weekly';
const HOUR = 3_600_000;
const DAY = 24 * HOUR;

function spanFor(mode: Mode, offset: number, now: number) {
  const dayStart = new Date(now).setHours(0, 0, 0, 0);
  if (mode === 'session') {
    const start = dayStart - DAY + offset * 3 * DAY;
    return { startMs: start, endMs: start + 3 * DAY, cells: 12, cellMs: 6 * HOUR };
  }
  const start = dayStart - 3 * DAY + offset * 14 * DAY;
  return { startMs: start, endMs: start + 14 * DAY, cells: 14, cellMs: DAY };
}

/** A muted pill: what the window is, and how much of it is left in the meter tone. */
function LaneChip({ label, window: w }: { label: string; window: RateLimitWindow | null }) {
  if (!w) {
    return (
      <span className="tl-chip" title={`This plan reports no ${label.toLowerCase()}`}>
        {label} <b className="tl-chip__pct muted">n/a</b>
      </span>
    );
  }
  const remaining = remainingOf(w);
  return (
    <span className="tl-chip" title={`${fmtPct(remaining)} of the ${label.toLowerCase()} left, resets ${fmtAbsolute(w.resetsAt)}`}>
      {label} <b className={`tl-chip__pct num tl-chip__pct--${meterTone(remaining)}`}>{fmtPct(remaining)}</b>
    </span>
  );
}

function ResetTimeline({ accounts, now }: { accounts: Account[]; now: number }) {
  const ink = useChartInk();
  const [mode, setMode] = useState<Mode>('weekly');
  const [offset, setOffset] = useState(0);
  const span = spanFor(mode, offset, now);
  const width = span.endMs - span.startMs;
  const nowPct = now >= span.startMs && now < span.endMs ? ((now - span.startMs) / width) * 100 : null;
  const stamp = (at: number) => (mode === 'session' ? fmtShortTime(new Date(at).toISOString()) : `${fmtDayShort(at)} ${fmtShortTime(new Date(at).toISOString())}`);

  const cells = Array.from({ length: span.cells }, (_, i) => {
    const at = span.startMs + i * span.cellMs;
    const d = new Date(at);
    const dayStart = d.getHours() === 0;
    return {
      at,
      head: mode === 'weekly' ? d.toLocaleDateString('en-US', { weekday: 'short' }) : dayStart ? fmtDayShort(at) : '',
      sub: mode === 'weekly' ? fmtDayShort(at) : fmtShortTime(new Date(at).toISOString()),
      today: new Date(at).setHours(0, 0, 0, 0) === new Date(now).setHours(0, 0, 0, 0),
      weekend: [0, 6].includes(d.getDay()),
    };
  });

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

      <div className="timeline__chart">
        <div className="timeline__axis">
          <div className="timeline__axis-label">Credential</div>
          <div className="timeline__cells">
            {cells.map((c) => (
              <div key={c.at} className={`timeline__cell ${c.today ? 'is-today' : ''}`}>
                <span className="timeline__cell-head">{c.head}</span>
                <span className="timeline__cell-sub">{c.sub}</span>
              </div>
            ))}
          </div>
        </div>

        <div className="timeline__lanes">
          {/* One raster and one now line for the whole chart rather than a copy
              per lane, so both read as a single ruler down the rows. */}
          <div className="timeline__raster" aria-hidden>
            {cells.map((c) => (
              <span key={c.at} className={c.weekend ? 'is-weekend' : ''} />
            ))}
          </div>
          {nowPct !== null && (
            <div className="timeline__now" style={{ left: `calc(var(--tl-left) + var(--tl-gap) + (100% - var(--tl-left) - var(--tl-gap)) * ${nowPct / 100})` }}>
              <span className="timeline__now-label">now</span>
            </div>
          )}

          {lanes.map(({ account, window: w }) => {
            const period = w.periodHours * HOUR;
            const resetAt = Date.parse(w.resetsAt ?? '');
            const bars: { end: number; start: number; state: 'live' | 'next' | 'past' }[] = [];
            let start = resetAt - period;
            while (start > span.startMs) start -= period;
            for (; start < span.endMs; start += period) {
              const end = start + period;
              if (end < span.startMs) continue;
              bars.push({ start, end, state: now >= start && now < end ? 'live' : end <= now ? 'past' : 'next' });
            }
            return (
              <div key={account.id} className="lane" style={{ ['--lane-color' as string]: accountColor(ink, account.id) }}>
                <div className="lane__head">
                  <div className="lane__id">
                    <span className="lane__dot" />
                    <span className="lane__name" title={account.email ?? account.label}>
                      {account.label}
                    </span>
                    <span className="lane__period">{mode === 'session' ? '5h' : '7d'}</span>
                  </div>
                  <div className="lane__chips">
                    <LaneChip label="5-hour limit" window={windowOf(account, '5h')} />
                    <LaneChip label="7-day limit" window={windowOf(account, '7d')} />
                    <LaneChip label="Fable 7-day limit" window={account.fableWindow} />
                  </div>
                </div>
                <div className="lane__track">
                  {bars.map((b) => {
                    const from = Math.max(b.start, span.startMs);
                    const to = Math.min(b.end, span.endMs);
                    const wPct = ((to - from) / width) * 100;
                    const showLabel = wPct > (mode === 'session' ? 5 : 9);
                    // The elapsed share is of the drawn bar, which the range may
                    // have clipped, not of the whole window.
                    const elapsed = b.state === 'live' ? ((Math.min(now, to) - from) / (to - from)) * 100 : 0;
                    return (
                      <div
                        key={b.start}
                        className={`lane__bar lane__bar--${b.state}`}
                        style={{ left: `${((from - span.startMs) / width) * 100}%`, width: `${wPct}%` }}
                        title={`${account.label}\n${fmtAbsolute(new Date(b.start).toISOString())} to ${fmtAbsolute(new Date(b.end).toISOString())}${b.state === 'live' ? `\n${fmtPct(w.utilization)} used` : ''}`}
                      >
                        {b.state === 'live' && <span className="lane__elapsed" style={{ width: `${elapsed}%` }} />}
                        {b.end <= span.endMs && <span className="lane__reset-mark" aria-hidden />}
                        {showLabel && <span className="lane__label">{b.state === 'live' ? `${fmtPct(w.utilization)} used · ${stamp(b.end)}` : stamp(b.end)}</span>}
                      </div>
                    );
                  })}
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
          <i className="lane__swatch lane__swatch--past" /> elapsed
        </span>
        <span>
          <i className="lane__swatch lane__swatch--now" /> now
        </span>
        <span className="timeline__note muted">Each bar is one full quota window, drawn from when it opened to when it resets, and labelled with the instant it resets. Lanes ending together compete for the same days.</span>
      </footer>
    </section>
  );
}
