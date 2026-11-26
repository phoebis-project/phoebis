import type { Account, ApiKey, AttentionItem, FeatureWord, Pool, Reconciliation, RoutingEvent } from '@/api/types';
import { ATTENTION_KIND_VALUES } from '@/api/types';
import { keyFeaturesNaming, poolFeaturesNaming } from '@/lib/accountUses';
import { MIN_RISE, ratioOff } from '@/lib/analytics';
import { lookup } from '@/lib/counts';
import { byteOrder, tokensTotal, undatedModel } from '@/lib/format';
import { featureObstacleOf } from '@/lib/forms';
import { readWindows } from '@/lib/quota';
import { WINDOW_KEY, hoursOfKey, windowEntry } from '@/lib/windows';
import { ACCOUNTS, accountById } from './accounts';
import { RECONCILIATIONS, termEndOf } from './analytics';
import { ceilingSettingOf } from './fit';
import { EXPIRED_BETWEEN_TURNS, KEEPALIVE_OVERVIEW, SETTINGS_WIRE } from './operations';
import { KEYS, POOLS, keyById, poolById } from './pools';
import { REQUESTS, planPaid, warmsCaches } from './requests';
import { RESET_BLOCKS, resetEnding, wallReset } from './resets';
// The walls condition names its shedding round `round`, so the helper reads under another name here.
import { round as roundTo } from './round';
import { ROUTING_EVENTS } from './routingLog';
import { ATTENTION_ITEM_SAMPLE, TABLES } from './samples';
import { DAY, HOUR, MINUTE, NOW, ago, iso } from './time';

const ms = (at: string | null | undefined) => (at ? Date.parse(at) : null);

const poolsOf = (accountId: string): string[] => POOLS.filter((p) => p.members.some((m) => m.accountId === accountId)).map((p) => p.id);

/** The account as the route names it, with the pools it is a member of, which a pool card filters by. */
const ofAccount = (id: string, label = accountById.get(id)?.label ?? id): Pick<AttentionItem, 'link' | 'pools' | 'subject'> => ({
  link: { id, page: 'accounts' },
  pools: poolsOf(id),
  subject: { id, label, type: 'account' },
});
const ofRouter = (id: string, label: string): Pick<AttentionItem, 'link' | 'pools' | 'subject'> => ({ link: null, pools: [], subject: { id, label, type: 'router' } });

const secondaryPoll = accountById.get('secondary')?.usage?.poll ?? null;

/** The reader's lead before a refresh token's fixed end, in days. */
const PAIRING_WARNING_DAYS = 3;

/** The route's order inside a kind: by when each item began, then by subject. */
const bySinceThenSubject = (x: AttentionItem, y: AttentionItem) => Date.parse(x.since) - Date.parse(y.since) || x.subject.id.localeCompare(y.subject.id);
const KIND_RANKS: readonly string[] = ATTENTION_KIND_VALUES;
/** The route's order of the list: by kind, then by when each item began, then by subject. */
export const byKindThenSince = (x: AttentionItem, y: AttentionItem) => KIND_RANKS.indexOf(x.kind) - KIND_RANKS.indexOf(y.kind) || bySinceThenSubject(x, y);

/**
 * The `pairing_expiring` items of these accounts at an instant, by the reader's rule and in the route's order: an item
 * for every OAuth account whose stored end lies no more than three days ahead of `at` or behind it, dated from three
 * days before the end. The route holds it until a re-pair stores an end more than three days away, or until a
 * disconnect or the account's deletion, so legacy holds it past its end and spare, disconnected, holds none. The mock
 * reads it at `NOW`, since reserve's end lies two days after it to the millisecond and a later read of the clock floors
 * to 1. The route orders a kind's items by start, then subject.
 */
export const pairingExpiringOf = (accounts: readonly Account[], at: number): AttentionItem[] =>
  accounts
    .flatMap((a) => {
      const end = a.kind === 'oauth' ? ms(a.credential.refreshTokenExpiresAt) : null;
      if (end === null) return [];
      const since = end - PAIRING_WARNING_DAYS * DAY;
      if (at < since) return [];
      return [
        {
          ...ATTENTION_ITEM_SAMPLE,
          // The row's own label, as the detail reads it, so a renamed account's item names it as the row does.
          ...ofAccount(a.id, a.label),
          detail: { account: a.id, days_left: Math.floor((end - at) / DAY), expires: end, label: a.label },
          kind: 'pairing_expiring' as const,
          since: iso(since),
        },
      ];
    })
    .sort(bySinceThenSubject);

/** One feature selection that names an account, as the reader's detail lists it. */
interface FeatureSelection {
  feature: FeatureWord;
  key: string | null;
  pool: string;
}

/** A pool's own selection, which names no key, ahead of its keys'. */
const keyOrder = (x: string | null, y: string | null): number => (x === y ? 0 : x === null ? -1 : y === null ? 1 : byteOrder(x, y));

/**
 * The `feature_account_unavailable` items of the registry at an instant, by the reader's rule and in the route's
 * order: one for each OAuth account that a pool's own selection or a key's own names and that `featureObstacleOf`
 * refuses at `at`, the rule the dialogs' disabled option reads. A key that inherits its pool's account selects nothing
 * of its own. The selections run by pool, a pool's own ahead of its keys', then by key and feature, and their pools are
 * the cards, a member's pools or not. A signed-out account dates from the story's re-auth item of the account where one
 * stands, since the console's row holds no instant of the change, else from the read. One past its end dates from the
 * earliest of its ends, as the reader's `term::end_of` does.
 */
export const featureAccountUnavailableOf = (
  registry: { accounts: readonly Account[]; keys: readonly Pick<ApiKey, 'featureAccounts' | 'id' | 'poolId'>[]; pools: readonly Pick<Pool, 'featureAccounts' | 'id'>[] },
  at: number,
): AttentionItem[] =>
  registry.accounts
    .flatMap((a) => {
      const named: FeatureSelection[] = [
        ...registry.pools.flatMap((p) => poolFeaturesNaming(p, a.id).map((feature) => ({ feature, key: null, pool: p.id }))),
        ...registry.keys.flatMap((k) => keyFeaturesNaming(k, a.id).map((feature) => ({ feature, key: k.id, pool: k.poolId }))),
      ].sort((x, y) => byteOrder(x.pool, y.pool) || keyOrder(x.key, y.key) || byteOrder(x.feature, y.feature));
      if (named.length === 0) return [];
      const obstacle = a.kind === 'oauth' ? featureObstacleOf(a, at) : null;
      if (obstacle === null) return [];
      return [
        {
          ...ATTENTION_ITEM_SAMPLE,
          ...ofAccount(a.id, a.label),
          detail: { account: a.id, label: a.label, obstacle, selections: named },
          kind: 'feature_account_unavailable' as const,
          pools: [...new Set(named.map((s) => s.pool))],
          since: obstacle === 'ended' ? iso(termEndOf(a, at) ?? at) : (ATTENTION.find((i) => i.kind === 'reauth_required' && i.subject.id === a.id)?.since ?? iso(at)),
        },
      ];
    })
    .sort(bySinceThenSubject);

/**
 * A pool's exhaustion as the reader dates it from the rows: the first exhausted answer after the pool's last success,
 * under that request's key, since a success clears the condition. Null when no request since the last success was
 * answered exhausted.
 */
function exhaustion(poolId: string): Pick<AttentionItem, 'detail' | 'since'> | null {
  // The rows run newest first.
  const rows = REQUESTS.filter((r) => r.poolId === poolId);
  const lastSuccess = rows.findIndex((r) => r.outcome === 'succeeded');
  const exhausted = (lastSuccess < 0 ? rows : rows.slice(0, lastSuccess)).filter((r) => r.outcome === 'exhausted');
  const first = exhausted[exhausted.length - 1];
  return first ? { detail: { key: first.keyId, pool: poolId }, since: iso(Date.parse(first.at) + (first.durationMs ?? 0)) } : null;
}

const jobsExhausted = exhaustion('jobs');

/**
 * The day's expiries between turns a probe could have prevented, as the reader counts them for its item: those on a
 * pool whose keepalive policy is on, under a key that holds the keepalive permission. The keepalive overview's count
 * keeps every pool's. The rows run newest first.
 */
const preventable = EXPIRED_BETWEEN_TURNS.filter((r) => r.poolId !== null && r.keyId !== null && keyById.has(r.keyId) && warmsCaches(r.keyId, r.poolId));

/** The trailing day's events of a kind, oldest first, which the ledger folds the routing conditions from. */
const lastDay = (kind: string) => ROUTING_EVENTS.filter((e) => e.kind === kind && Date.parse(e.at) >= NOW - DAY);
const sorted = (values: unknown[]) => [...new Set(values.map(String))].sort();

/** `lead`, then the ceiling setting of each window `windows` lists, each once and the 5-hour one first, as the fold
 *  names them. */
const ceilingSettings = (lead: string[], windows: string[]): string[] => [...lead, ...(['five_hour_ceiling', 'weekly_ceiling'] as const).filter((setting) => windows.some((w) => ceilingSettingOf(w) === setting))];

/** A pool's value of each setting a condition names, as the reader joins it. */
const settingValues = (poolId: string, names: string[]): Record<string, number> | null => {
  const pool = poolById.get(poolId);
  if (!pool) return null;
  const value: Record<string, number> = { five_hour_ceiling: pool.fiveHourCeiling, shed_cap: pool.shedCap, slow_down_stretch_seconds: pool.slowDownStretchSeconds, weekly_ceiling: pool.weeklyCeiling };
  return Object.fromEntries(
    names.flatMap((name) => {
      const v = lookup(value, name);
      return v === undefined ? [] : [[name, v]];
    }),
  );
};

/**
 * The pool whose values a wall or a spent destination of the account reads: its one pool where the account stands in
 * exactly one, none where it stands in several, since the reader would pick among them in no fixed order.
 */
const onePoolOf = (accountId: string): string | null => {
  const pools = poolsOf(accountId);
  return pools.length === 1 ? pools[0] : null;
};

/** A condition's settings and their values on `poolId`, with no values where it names no pool the registry holds. */
const settingsOf = (poolId: string | null, names: string[]) => {
  const values = poolId === null ? null : settingValues(poolId, names);
  return { setting: names, ...(values === null ? {} : { setting_values: values }) };
};

/** An account condition's settings with the values of its one pool, where it stands in exactly one. */
const accountSettings = (accountId: string, names: string[]) => settingsOf(onePoolOf(accountId), names);

/**
 * A model the cost model prices nothing for as the ledger counts it: the requests on it that a plan funded and that
 * carried usage, each with no Pro-week figure, the run dated from the first of them. Null when the story holds none.
 */
function unpriced(model: string): Pick<AttentionItem, 'detail' | 'since'> | null {
  // The rows run newest first.
  const rows = REQUESTS.filter((r) => undatedModel(r.canonicalModel) === model && planPaid(r) && r.costEquivalent === null && tokensTotal(r.usage) > 0);
  const first = rows[rows.length - 1];
  return first ? { detail: { model, requests: rows.length }, since: first.at } : null;
}

const sonnetUnpriced = unpriced('claude-sonnet-4-5');

/** The windows an account's reading holds at their limit, in the router's names, as the wall's join reads the poll. */
const atLimit = (accountId: string): string[] => {
  const a = accountById.get(accountId);
  return (a ? readWindows(a) : []).filter((w) => w.utilization !== null && w.utilization >= 1).map((w) => windowEntry(WINDOW_KEY, w.id) ?? w.id);
};

/**
 * An account's walls over the day as the reader dates them, read by the newest round on the newest wall's window
 * inside that window's length before it: after a projection round the error of its projection, after a slow-down round
 * how long before the reset its `window_spent` names the wall came, 0 at or after it. Null where no window of the
 * account was spent.
 */
function walls(accountId: string): Pick<AttentionItem, 'detail' | 'since'> | null {
  const spent = lastDay('window_spent').filter((e) => e.accountId === accountId);
  const newest = spent[spent.length - 1];
  if (!newest) return null;
  const window = String(newest.detail.window);
  const wallAt = Date.parse(newest.at);
  const length = hoursOfKey(window) * HOUR;
  const round = ROUTING_EVENTS.filter((e) => e.kind === 'shedding_round' && e.accountId === accountId && e.detail.window === window && Date.parse(e.at) <= wallAt && wallAt - Date.parse(e.at) <= length).pop();
  // A slow-down round's projection is the demand's at a moved horizon and measures nothing at the reset.
  const slowed = round?.detail.trigger === 'slow_down';
  const projection = !slowed && typeof round?.detail.projection === 'number' ? round.detail.projection : null;
  const reset = typeof newest.detail.reset === 'number' ? newest.detail.reset : null;
  // From the block and the present reading, as the reader joins it: the row's windows name the day's spent ones.
  const usable = wallReset(lookup(RESET_BLOCKS, accountId), atLimit(accountId), NOW);
  const windows = sorted(spent.map((e) => e.detail.window));
  return {
    detail: {
      early_ms: slowed && reset !== null ? Math.max(0, reset - wallAt) : null,
      projection_error: projection === null ? null : roundTo(1 - projection, 3),
      ...(usable === null ? {} : { reset: usable }),
      ...accountSettings(accountId, ceilingSettings(['shed_cap'], windows)),
      walls: spent.length,
      windows,
    },
    since: spent[0].at,
  };
}

/** The moves that landed on an account whose window was spent within the hour after, as the reader counts them. */
function spentDestination(accountId: string): Pick<AttentionItem, 'detail' | 'since'> | null {
  const spent = lastDay('window_spent').filter((e) => e.accountId === accountId);
  const after = (move: (typeof spent)[number]) => spent.filter((w) => Date.parse(w.at) >= Date.parse(move.at) && Date.parse(w.at) - Date.parse(move.at) <= HOUR);
  const moves = lastDay('pin_migrated').filter((e) => e.accountId === accountId && after(e).length > 0);
  if (moves.length === 0) return null;
  const fits: Record<string, number> = {};
  for (const m of moves) fits[String(m.detail.fit ?? 'unknown')] = (fits[String(m.detail.fit ?? 'unknown')] ?? 0) + 1;
  const windows = sorted(moves.flatMap((m) => after(m).map((w) => w.detail.window)));
  return { detail: { fits, moves: moves.length, ...accountSettings(accountId, ceilingSettings([], windows)), windows }, since: moves[0].at };
}

/**
 * A pool's rounds that marked nothing for want of a destination with runway, with both ceilings the pool holds, since
 * a round reads a sibling's runway over windows of both kinds.
 */
function roundsWithoutRunway(poolId: string): Pick<AttentionItem, 'detail' | 'since'> | null {
  const rounds = lastDay('shedding_round').filter((e) => e.poolId === poolId && e.detail.outcome === 'nothing_to_mark' && e.detail.reason === 'no_runway');
  if (rounds.length === 0) return null;
  return { detail: { accounts: sorted(rounds.map((e) => e.accountId)), rounds: rounds.length, ...settingsOf(poolId, ['five_hour_ceiling', 'weekly_ceiling']) }, since: rounds[0].at };
}

/** A routing log event as the slowed-account fold reads it. */
export type SpanRow = Pick<RoutingEvent, 'accountId' | 'at' | 'detail' | 'kind' | 'poolId'>;

/** The slowed-account bar in stretch thresholds and its floor, the reader's own. */
const SLOWED_LONG_STRETCHES = 2;
const SLOWED_LONG_FLOOR_MS = HOUR;

/**
 * The day's slow-down holds per account, the hourly rollups' held time and held requests the ledger sums, which the
 * story's rows cannot carry: they hold only the turns the drawer reads, studio's two held rows of 36 s and 42 s among
 * its 72. Each hold stays under work's 120 s bound.
 */
const HELD_TODAY: Readonly<Record<string, { held: number; heldMs: number }>> = { studio: { held: 72, heldMs: 7_700_000 } };

/**
 * An account's slow-down spans over the day as the reader pairs them: the spans standing at the day's cutoff, then each
 * start with the next end of its account and pool or the next restart, whichever comes first, a start with neither open
 * to now, each span clamped to the day and one with no part inside it not counted. It holds once the account's held
 * time of the day passes the bar, twice the stretch threshold the newest span's start row read and at least an hour,
 * from the own start of the oldest span, with the day's held time, the held count and the stretch threshold of the
 * newest span's pool. Null where no span falls inside the day or it does not hold. `held` is the day's held count and
 * held time, which the ledger sums from the hourly rollups.
 */
function slowedLongOf(rows: readonly SpanRow[], accountId: string, now: number, held: { held: number; heldMs: number }): Pick<AttentionItem, 'detail' | 'since'> | null {
  const from = now - DAY;
  const spans: { open: boolean; pool: string; since: number; thresholdMs: number | null }[] = [];
  const count = (pool: string, since: number, end: number, open: boolean, thresholdMs: number | null) => {
    const slowedMs = Math.min(end, now) - Math.max(since, from);
    if (slowedMs > 0) spans.push({ open, pool, since, thresholdMs });
  };
  const thresholdOf = (e: SpanRow) => (typeof e.detail.threshold_ms === 'number' ? e.detail.threshold_ms : null);
  const own = rows.filter((e) => e.kind === 'router_started' || (e.accountId === accountId && e.poolId !== null)).sort((a, b) => Date.parse(a.at) - Date.parse(b.at));
  // A span that started before the cutoff and ends inside the day names no start the day holds, so the state at the
  // cutoff opens it: per pool the newest start or end before the cutoff, where that row is a start newer than the
  // newest restart before it.
  const before = own.filter((e) => Date.parse(e.at) < from);
  const restart = Math.max(Number.NEGATIVE_INFINITY, ...before.filter((e) => e.kind === 'router_started').map((e) => Date.parse(e.at)));
  const lastBefore = new Map<string, SpanRow>();
  for (const e of before) if (e.kind === 'slow_down_started' || e.kind === 'slow_down_ended') lastBefore.set(e.poolId ?? '', e);
  const standing = new Map<string, { since: number; thresholdMs: number | null }>();
  for (const [pool, e] of lastBefore) if (e.kind === 'slow_down_started' && Date.parse(e.at) > restart) standing.set(pool, { since: Date.parse(e.at), thresholdMs: thresholdOf(e) });
  for (const e of own.filter((x) => Date.parse(x.at) >= from && Date.parse(x.at) <= now)) {
    const pool = e.poolId ?? '';
    if (e.kind === 'router_started') {
      for (const [open, span] of standing) count(open, span.since, Date.parse(e.at), false, span.thresholdMs);
      standing.clear();
    } else if (e.kind === 'slow_down_started') {
      if (!standing.has(pool)) standing.set(pool, { since: Date.parse(e.at), thresholdMs: thresholdOf(e) });
    } else if (e.kind === 'slow_down_ended') {
      const span = standing.get(pool);
      if (!span) continue;
      standing.delete(pool);
      const since = typeof e.detail.since === 'number' ? e.detail.since : span.since;
      const end = typeof e.detail.duration_ms === 'number' ? since + e.detail.duration_ms : Date.parse(e.at);
      count(pool, since, end, false, span.thresholdMs);
    }
  }
  for (const [pool, span] of standing) count(pool, span.since, now, true, span.thresholdMs);
  if (spans.length === 0) return null;
  const newest = spans.reduce((a, b) => (b.since > a.since ? b : a));
  const barMs = Math.max(SLOWED_LONG_STRETCHES * (newest.thresholdMs ?? 0), SLOWED_LONG_FLOOR_MS);
  if (held.heldMs <= barMs) return null;
  return {
    detail: {
      bar_ms: barMs,
      held: held.held,
      held_ms: held.heldMs,
      open: spans.some((x) => x.open),
      ...settingsOf(newest.pool, ['slow_down_stretch_seconds']),
      spans: spans.length,
    },
    since: iso(Math.min(...spans.map((x) => x.since))),
  };
}

/**
 * An account's meter drift as the fold reads the reconciled windows: per window the newest row whose rise reaches
 * `MIN_RISE`, holding while any of them reads off the band, with the off ones in window order and dated from the
 * oldest one's end. Null where every such row reads inside.
 */
function drift(accountId: string): Pick<AttentionItem, 'detail' | 'since'> | null {
  // The rows run newest first, so the first of a window is its newest.
  const newest = new Map<string, Reconciliation>();
  for (const r of RECONCILIATIONS) if (r.accountId === accountId && r.observedRise >= MIN_RISE && !newest.has(r.window)) newest.set(r.window, r);
  const off = [...newest.values()].filter((r) => ratioOff(r)).sort((x, y) => x.window.localeCompare(y.window));
  if (off.length === 0) return null;
  return {
    detail: {
      effective: TABLES.effective,
      source: 'shipped',
      windows: off.map((r) => ({ observed_rise: r.observedRise, predicted_share: r.predictedShare, ratio: r.ratio, requests: r.requests, window: r.window, window_end: Date.parse(r.windowEnd) })),
    },
    since: iso(Math.min(...off.map((r) => Date.parse(r.windowEnd)))),
  };
}

/** The conditions the ledger folds per subscription account, each kind beside its fold. */
const ACCOUNT_CONDITIONS: [AttentionItem['kind'], (accountId: string) => Pick<AttentionItem, 'detail' | 'since'> | null][] = [
  ['meter_drift', drift],
  ['wall_reached', walls],
  ['destination_spent', spentDestination],
  ['slowed_long', (id) => slowedLongOf(ROUTING_EVENTS, id, NOW, lookup(HELD_TODAY, id) ?? { held: 0, heldMs: 0 })],
];

/**
 * The meter drift and the four routing conditions as the reader builds them, one per subject the fold holds: every
 * OAuth account for the meter drift, the walls, the spent destinations and the slow-downs, every pool for the rounds
 * without runway. The route lists them by kind, then by when each began, then by subject.
 */
const CONDITIONS: AttentionItem[] = [
  // Built account by account: the sort orders the items by kind, start and subject, and no two share all three.
  ...ACCOUNTS.filter((a) => a.kind === 'oauth').flatMap((a) =>
    ACCOUNT_CONDITIONS.flatMap(([kind, fold]) => {
      const held = fold(a.id);
      return held ? [{ ...ATTENTION_ITEM_SAMPLE, ...ofAccount(a.id), ...held, kind }] : [];
    }),
  ),
  ...POOLS.flatMap((p) => {
    const held = roundsWithoutRunway(p.id);
    return held ? [{ ...ATTENTION_ITEM_SAMPLE, ...held, kind: 'shed_no_runway' as const, link: { id: p.id, page: 'pools' as const }, pools: [p.id], subject: { id: p.id, label: p.name, type: 'pool' as const } }] : [];
  }),
].sort(byKindThenSince);

/**
 * One item per account whose stored block holds an unused reset that ends within the settings' days, read from the
 * blocks as the reader reads the stored ones, listed by when each began, then by subject.
 */
const RESET_ITEMS: AttentionItem[] = ACCOUNTS.flatMap((a) => {
  const held = resetEnding(a.id, lookup(RESET_BLOCKS, a.id), NOW, SETTINGS_WIRE.usage.limit_reset_warning_days);
  return held ? [{ ...ATTENTION_ITEM_SAMPLE, ...ofAccount(a.id), ...held, kind: 'limit_reset_ending' as const }] : [];
}).sort(bySinceThenSubject);

/**
 * The session engine's two caps, each under the config key that sets it. The route raises one item per cap that
 * stands at 90 percent of its max or more.
 */
const SESSION_CAPS = [
  { cap: 'sessions', key: 'sessions.max_sessions', limit: KEEPALIVE_OVERVIEW.limits.sessions },
  { cap: 'agents', key: 'sessions.max_agents', limit: KEEPALIVE_OVERVIEW.limits.agents },
] as const;

/**
 * One item per kind but a failed certificate reload, a refresh token near its end and a feature account that cannot
 * serve, which `pairingExpiringOf` and `featureAccountUnavailableOf` read off the live registry, a certificate near its
 * end, which holds on the proxy listener and the feature CA, and a wall, which holds on three, on the story's accounts,
 * pools, keys, proxy listener, feature CA, models, session engine and keepalive engine, in the route's order: jobs
 * exhausted from the runner's first request after its one member spent the day's budget, a client build refused on the
 * laptop key, the routing state writer keeping its writes in memory, legacy's lapsed credential, the Overflow workspace
 * out of funding as its account reads, legacy's refreshes failing, the proxy listener's certificate 12 days from its
 * end, the feature CA in the data directory of the story's storage path 20 days from its end, nightly's reset 4 days
 * from its end, a priced record of the jobs pool's one member parked in the ledger file, the keepalive engine's bus
 * gaps, the backoff secondary's poll is in, Sonnet 4.5 priced nothing, nightly's 5-hour window charged twice the table
 * up to its 13:11 reset, nightly's wall at 12:01, secondary's after the 11:00 round, which secondary's reset can clear,
 * and studio's at 14:20 after its stretch round, nightly spent after the least-bad move onto it, the all pool's round
 * that found no destination with runway, studio slowed in work for 2 h 8 min of the day, the session engine's sessions
 * at 94 percent of their cap, the cache warming's retained prompts at 92 percent of their cap, and the day's expiries
 * between turns a probe could have prevented. The meter drift reads the reconciled windows, the four routing conditions
 * the routing log's events, as the ledger folds them, the reset item and the wall's reset the stored blocks, the bus
 * gaps and the two cap items the overview's figures, and the expiries the ledger rows of the day.
 */
export const ATTENTION: AttentionItem[] = [
  ...(jobsExhausted
    ? [
        {
          ...ATTENTION_ITEM_SAMPLE,
          ...jobsExhausted,
          kind: 'pool_exhausted' as const,
          link: { id: 'jobs', page: 'pools' as const },
          pools: ['jobs'],
          subject: { id: 'jobs', label: 'jobs', type: 'pool' as const },
        },
      ]
    : []),
  {
    ...ATTENTION_ITEM_SAMPLE,
    detail: { client_version: '2.1.271', key: 'laptop', trigger: 'hash' },
    kind: 'client_drift',
    link: { id: 'laptop', page: 'keys' },
    pools: KEYS.filter((k) => k.id === 'laptop').map((k) => k.poolId),
    since: ago(25 * MINUTE),
    subject: { id: '2.1.271', label: '2.1.271', type: 'client' },
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofRouter('routing', 'routing'),
    detail: { component: 'routing' },
    kind: 'writer_degraded',
    since: ago(12 * MINUTE),
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofAccount('legacy'),
    detail: { account: 'legacy', label: 'legacy' },
    kind: 'reauth_required',
    since: ago(2 * DAY + 2 * HOUR),
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofAccount('overflow'),
    detail: { account: 'overflow', label: 'Overflow workspace' },
    kind: 'funding_required',
    since: ago(3 * HOUR),
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofAccount('legacy'),
    detail: { account: 'legacy', failures: 5, last_error: 'refresh_token rejected: invalid_grant', outcome: 'failed' },
    kind: 'refresh_failed',
    since: ago(2 * DAY + 2 * HOUR + 50 * MINUTE),
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofRouter('proxy.tls', 'proxy.tls'),
    detail: { cert_file: '/etc/phoebis/proxy.pem', days_left: 12, expires: NOW + 12 * DAY },
    kind: 'certificate_expiring',
    since: ago(18 * DAY),
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofRouter('features.ca', 'features.ca'),
    detail: { cert_file: './data/feature-ca.pem', days_left: 20, expires: NOW + 20 * DAY },
    kind: 'certificate_expiring',
    since: ago(10 * DAY),
  },
  ...RESET_ITEMS,
  {
    ...ATTENTION_ITEM_SAMPLE,
    detail: { accounts: ['research'], count: 1, pool: 'jobs' },
    kind: 'ledger_record_parked',
    link: { id: 'jobs', page: 'pools' },
    pools: ['jobs'],
    since: ago(HOUR),
    subject: { id: 'jobs', label: 'jobs', type: 'pool' },
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofRouter('keepalive', 'Keepalive'),
    detail: { gaps: KEEPALIVE_OVERVIEW.busGaps, last_at: ms(KEEPALIVE_OVERVIEW.lastBusGapAt) },
    kind: 'keepalive_bus_gaps',
    since: ago(5 * HOUR + 40 * MINUTE),
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofAccount('secondary'),
    detail: { account: 'secondary', backoff_since: ms(secondaryPoll?.backoffSince), next_attempt_at: ms(secondaryPoll?.nextAttemptAt), rate_limited_in_a_row: secondaryPoll?.rateLimitedInARow ?? 0 },
    kind: 'poll_backoff',
    since: secondaryPoll?.backoffSince ?? ago(42 * MINUTE),
  },
  ...(sonnetUnpriced
    ? [
        {
          ...ATTENTION_ITEM_SAMPLE,
          ...sonnetUnpriced,
          kind: 'model_unpriced' as const,
          link: null,
          pools: [],
          subject: { id: 'claude-sonnet-4-5', label: 'claude-sonnet-4-5', type: 'model' as const },
        },
      ]
    : []),
  ...CONDITIONS,
  ...SESSION_CAPS.filter(({ limit }) => limit.max > 0 && limit.count * 100 >= limit.max * 90).map(({ cap, key, limit }) => ({
    ...ATTENTION_ITEM_SAMPLE,
    ...ofRouter(key, 'Sessions'),
    detail: { cap, count: limit.count, max: limit.max, pct: limit.count / limit.max },
    kind: 'session_cap' as const,
    since: ago(50 * MINUTE),
  })),
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofRouter('keepalive.retained', 'Keepalive'),
    detail: { cap: 'retained', count: KEEPALIVE_OVERVIEW.limits.retainedBytes.count, max: KEEPALIVE_OVERVIEW.limits.retainedBytes.max, pct: roundTo(KEEPALIVE_OVERVIEW.limits.retainedBytes.count / KEEPALIVE_OVERVIEW.limits.retainedBytes.max, 2) },
    kind: 'keepalive_cap',
    since: ago(3 * HOUR + 15 * MINUTE),
  },
  ...(preventable.length > 0
    ? [
        {
          ...ATTENTION_ITEM_SAMPLE,
          ...ofRouter('keepalive', 'Keepalive'),
          detail: { expired: preventable.length },
          kind: 'expired_between_turns' as const,
          since: preventable[preventable.length - 1].at,
        },
      ]
    : []),
];
