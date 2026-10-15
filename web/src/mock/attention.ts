import type { AttentionItem } from '@/api/types';
import { latestWeighted, modelTierOf } from '@/lib/costs';
import { undatedModel } from '@/lib/format';
import { ACCOUNTS, accountById } from './accounts';
import { KEYS, POOLS } from './pools';
import { REQUESTS } from './requests';
import { ROUTING_EVENTS } from './routingLog';
import { ATTENTION_ITEM_SAMPLE, TABLES } from './samples';
import { DAY, HOUR, MINUTE, NOW, ago, iso } from './time';

const ms = (at: string | null | undefined) => (at ? Date.parse(at) : null);

/** The account as the route names it, with the pools it is a member of, which a pool card filters by. */
const ofAccount = (id: string): Pick<AttentionItem, 'link' | 'pools' | 'subject'> => ({
  link: { id, page: 'accounts' },
  pools: POOLS.filter((p) => p.members.some((m) => m.accountId === id)).map((p) => p.id),
  subject: { id, label: accountById.get(id)?.label ?? id, type: 'account' },
});

const secondaryPoll = accountById.get('secondary')?.usage?.poll ?? null;

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

/** The trailing day's events of a kind, oldest first, which the ledger folds the routing conditions from. */
const lastDay = (kind: string) => ROUTING_EVENTS.filter((e) => e.kind === kind && Date.parse(e.at) >= NOW - DAY);
const sorted = (values: unknown[]) => [...new Set(values.map(String))].sort();

/**
 * A model the cost model does not weight as the ledger counts it: the requests on it that a plan funded and that
 * carried usage, priced at its model tier's latest weighted model, the run dated from the first of them. Null when the
 * story holds none.
 */
function unweighted(model: string): Pick<AttentionItem, 'detail' | 'since'> | null {
  // The rows run newest first.
  const rows = REQUESTS.filter((r) => undatedModel(r.canonicalModel) === model && r.estimated && r.usage.input + r.usage.output + r.usage.cacheRead + r.usage.cacheWrite5m + r.usage.cacheWrite1h > 0);
  const first = rows[rows.length - 1];
  const tier = modelTierOf(TABLES, model);
  return first ? { detail: { model, requests: rows.length, tier, weight_of: tier === null ? null : latestWeighted(TABLES, tier) }, since: first.at } : null;
}

const sonnetUnweighted = unweighted('claude-sonnet-4-6');

/**
 * An account's walls over the day as the reader dates them, with the error of the newest round's projection on the
 * newest wall's window inside that window's length before it. Null where no window of the account was spent.
 */
function walls(accountId: string): Pick<AttentionItem, 'detail' | 'since'> | null {
  const spent = lastDay('window_spent').filter((e) => e.accountId === accountId);
  const newest = spent[spent.length - 1];
  if (!newest) return null;
  const window = String(newest.detail.window);
  const wallAt = Date.parse(newest.at);
  const length = window === 'five_hour' ? 5 * HOUR : 7 * DAY;
  const round = ROUTING_EVENTS.filter((e) => e.kind === 'shedding_round' && e.accountId === accountId && e.detail.window === window && Date.parse(e.at) <= wallAt && wallAt - Date.parse(e.at) <= length).pop();
  const projection = typeof round?.detail.projection === 'number' ? round.detail.projection : null;
  return {
    detail: { projection_error: projection === null ? null : Math.round((1 - projection) * 1_000) / 1_000, setting: ['shed_cap', 'window_ceiling'], walls: spent.length, windows: sorted(spent.map((e) => e.detail.window)) },
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
  return { detail: { fits, moves: moves.length, setting: ['window_ceiling'], windows: sorted(moves.flatMap((m) => after(m).map((w) => w.detail.window))) }, since: moves[0].at };
}

/** A pool's rounds that marked nothing for want of a destination with runway, with the ceiling the pool holds. */
function roundsWithoutRunway(poolId: string): Pick<AttentionItem, 'detail' | 'since'> | null {
  const rounds = lastDay('shedding_round').filter((e) => e.poolId === poolId && e.detail.outcome === 'nothing_to_mark' && e.detail.reason === 'no_runway');
  if (rounds.length === 0) return null;
  const ceiling = POOLS.find((p) => p.id === poolId)?.windowCeiling ?? null;
  return { detail: { accounts: sorted(rounds.map((e) => e.accountId)), rounds: rounds.length, setting: ['window_ceiling'], setting_values: { window_ceiling: ceiling } }, since: rounds[0].at };
}

/** The slowed time past which an account's slow-down is the pool's load rather than one turn's, the reader's own. */
const SLOWED_LONG_MS = HOUR;

/**
 * An account's slow-down spans over the day as the reader pairs them: each start with the next end of its account and
 * pool or the next restart, whichever comes first, a start with neither open to now, each span clamped to the day. It
 * holds once the spans sum past an hour, from the own start of the oldest, with the lowest factor any span read and
 * the stretch threshold of the newest span's pool. Null where it does not hold.
 */
function slowed(accountId: string): Pick<AttentionItem, 'detail' | 'since'> | null {
  const from = NOW - DAY;
  const spans: { factor: number | null; open: boolean; pool: string; since: number; slowedMs: number }[] = [];
  const count = (pool: string, since: number, end: number, open: boolean, factor: number | null) => spans.push({ factor, open, pool, since, slowedMs: Math.max(0, end - Math.max(since, from)) });
  const lowest = (a: number | null, b: unknown) => (typeof b !== 'number' ? a : a === null ? b : Math.min(a, b));
  const standing = new Map<string, { factor: number | null; since: number }>();
  const rows = ROUTING_EVENTS.filter((e) => Date.parse(e.at) >= from && Date.parse(e.at) <= NOW && (e.kind === 'router_started' || (e.accountId === accountId && e.poolId !== null)));
  for (const e of rows) {
    const pool = e.poolId ?? '';
    if (e.kind === 'router_started') {
      for (const [open, span] of standing) count(open, span.since, Date.parse(e.at), false, span.factor);
      standing.clear();
    } else if (e.kind === 'slow_down_started') {
      const span = standing.get(pool);
      standing.set(pool, span ? { ...span, factor: lowest(span.factor, e.detail.factor) } : { factor: lowest(null, e.detail.factor), since: Date.parse(e.at) });
    } else if (e.kind === 'slow_down_ended') {
      const span = standing.get(pool);
      if (!span) continue;
      standing.delete(pool);
      const since = typeof e.detail.since === 'number' ? e.detail.since : span.since;
      const end = typeof e.detail.duration_ms === 'number' ? since + e.detail.duration_ms : Date.parse(e.at);
      count(pool, since, end, false, typeof e.detail.factor_min === 'number' ? e.detail.factor_min : span.factor);
    }
  }
  for (const [pool, span] of standing) count(pool, span.since, NOW, true, span.factor);
  const slowedMs = spans.reduce((n, x) => n + x.slowedMs, 0);
  if (slowedMs <= SLOWED_LONG_MS) return null;
  const newest = spans.reduce((a, b) => (b.since > a.since ? b : a));
  const stretch = POOLS.find((p) => p.id === newest.pool)?.slowDownStretchSeconds;
  return {
    detail: {
      factor_min: spans.reduce<number | null>((n, x) => lowest(n, x.factor), null),
      open: spans.some((x) => x.open),
      setting: ['slow_down_stretch_seconds'],
      ...(stretch === undefined ? {} : { setting_values: { slow_down_stretch_seconds: stretch } }),
      slowed_ms: slowedMs,
      spans: spans.length,
    },
    since: iso(Math.min(...spans.map((x) => x.since))),
  };
}

const CONDITION_ORDER: AttentionItem['kind'][] = ['wall_reached', 'destination_spent', 'shed_no_runway', 'slowed_long'];

/**
 * The four routing conditions as the reader builds them, one per subject the fold holds: every OAuth account for the
 * walls, the spent destinations and the slow-downs, every pool for the rounds without runway. The route lists them by
 * kind, then by when each began, then by subject.
 */
const CONDITIONS: AttentionItem[] = [
  ...ACCOUNTS.filter((a) => a.kind === 'oauth').flatMap((a) => {
    const held = walls(a.id);
    return held ? [{ ...ATTENTION_ITEM_SAMPLE, ...ofAccount(a.id), ...held, kind: 'wall_reached' as const }] : [];
  }),
  ...ACCOUNTS.filter((a) => a.kind === 'oauth').flatMap((a) => {
    const held = spentDestination(a.id);
    return held ? [{ ...ATTENTION_ITEM_SAMPLE, ...ofAccount(a.id), ...held, kind: 'destination_spent' as const }] : [];
  }),
  ...POOLS.flatMap((p) => {
    const held = roundsWithoutRunway(p.id);
    return held ? [{ ...ATTENTION_ITEM_SAMPLE, ...held, kind: 'shed_no_runway' as const, link: { id: p.id, page: 'pools' as const }, pools: [p.id], subject: { id: p.id, label: p.name, type: 'pool' as const } }] : [];
  }),
  ...ACCOUNTS.filter((a) => a.kind === 'oauth').flatMap((a) => {
    const held = slowed(a.id);
    return held ? [{ ...ATTENTION_ITEM_SAMPLE, ...ofAccount(a.id), ...held, kind: 'slowed_long' as const }] : [];
  }),
].sort((x, y) => CONDITION_ORDER.indexOf(x.kind) - CONDITION_ORDER.indexOf(y.kind) || Date.parse(x.since) - Date.parse(y.since) || x.subject.id.localeCompare(y.subject.id));

/**
 * One item per kind but a failed certificate reload and a wall, which holds on two accounts, on the story's accounts,
 * pools, keys, proxy listener and models, in the route's order: jobs exhausted from the runner's first request after
 * its one member spent the day's budget, a client build refused on the laptop key, the routing state writer keeping
 * its writes in memory, legacy's lapsed credential, the Overflow workspace out of funding as its account reads,
 * legacy's refreshes failing, the proxy listener's certificate 12 days from its end, a priced record of the jobs pool's
 * one member parked in the ledger file, the backoff secondary's poll is in, Sonnet 4.6 priced at its model tier's
 * weight, nightly's wall at 12:01 and secondary's after the 11:00 round, nightly spent after the least-bad move onto
 * it, the all pool's round that found no destination with runway, and studio slowed in work for nearly four hours of
 * the day. The four routing conditions read the routing log's events as the ledger folds them.
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
    detail: { component: 'routing' },
    kind: 'writer_degraded',
    link: null,
    pools: [],
    since: ago(12 * MINUTE),
    subject: { id: 'routing', label: 'routing', type: 'router' },
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
    detail: { cert_file: '/etc/phoebis/proxy.pem', days_left: 12, expires: NOW + 12 * DAY },
    kind: 'certificate_expiring',
    link: null,
    pools: [],
    since: ago(18 * DAY),
    subject: { id: 'proxy.tls', label: 'proxy.tls', type: 'router' },
  },
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
    ...ofAccount('secondary'),
    detail: { account: 'secondary', backoff_since: ms(secondaryPoll?.backoffSince), next_attempt_at: ms(secondaryPoll?.nextAttemptAt), rate_limited_in_a_row: secondaryPoll?.rateLimitedInARow ?? 0 },
    kind: 'poll_backoff',
    since: secondaryPoll?.backoffSince ?? ago(42 * MINUTE),
  },
  ...(sonnetUnweighted
    ? [
        {
          ...ATTENTION_ITEM_SAMPLE,
          ...sonnetUnweighted,
          kind: 'model_unweighted' as const,
          link: { id: 'claude-sonnet-4-6', page: 'models' as const },
          pools: [],
          subject: { id: 'claude-sonnet-4-6', label: 'claude-sonnet-4-6', type: 'model' as const },
        },
      ]
    : []),
  ...CONDITIONS,
];
