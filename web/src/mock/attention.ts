import type { AttentionItem } from '@/api/types';
import { latestWeighted, modelTierOf } from '@/lib/costs';
import { undatedModel } from '@/lib/format';
import { accountById } from './accounts';
import { KEYS, POOLS } from './pools';
import { REQUESTS } from './requests';
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
 * One item per kind but a failed certificate reload, on the story's accounts, pools, keys, proxy listener and models,
 * in the route's order: jobs exhausted from the runner's first request after its one member spent the day's budget, a
 * client build refused on the laptop key, the routing state writer keeping its writes in memory, legacy's lapsed
 * credential, the Overflow workspace out of funding as its account reads, legacy's refreshes failing, the proxy
 * listener's certificate 12 days from its end, a priced record of the jobs pool's one member parked in the ledger file,
 * the backoff secondary's poll is in, and Sonnet 4.6 priced at its model tier's weight.
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
];
