import type { AttentionItem } from '@/api/types';
import { accountById } from './accounts';
import { KEYS, POOLS } from './pools';
import { ATTENTION_ITEM_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, NOW, ago } from './time';

const ms = (at: string | null | undefined) => (at ? Date.parse(at) : null);

/** The account as the route names it, with the pools it is a member of, which a pool card filters by. */
const ofAccount = (id: string): Pick<AttentionItem, 'link' | 'pools' | 'subject'> => ({
  link: { id, page: 'accounts' },
  pools: POOLS.filter((p) => p.members.some((m) => m.accountId === id)).map((p) => p.id),
  subject: { id, label: accountById.get(id)?.label ?? id, type: 'account' },
});

const secondaryPoll = accountById.get('secondary')?.usage?.poll ?? null;

/**
 * One item per kind but a failed certificate reload, on the story's accounts, pools, keys and proxy listener, in the
 * route's order: codex exhausted since the runner's key last asked it, a client build refused on the laptop key, the
 * routing state writer keeping its writes in memory, legacy's lapsed credential, the Overflow workspace out of funding
 * as its account reads, legacy's refreshes failing, the proxy listener's certificate 12 days from its end, a priced
 * record of codex's one member parked in the ledger file, and the backoff secondary's poll is in.
 */
export const ATTENTION: AttentionItem[] = [
  {
    ...ATTENTION_ITEM_SAMPLE,
    detail: { key: 'ci', pool: 'codex' },
    kind: 'pool_exhausted',
    link: { id: 'codex', page: 'pools' },
    pools: ['codex'],
    since: ago(2 * DAY),
    subject: { id: 'codex', label: 'codex', type: 'pool' },
  },
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
    detail: { accounts: ['research'], count: 1, pool: 'codex' },
    kind: 'ledger_record_parked',
    link: { id: 'codex', page: 'pools' },
    pools: ['codex'],
    since: ago(HOUR),
    subject: { id: 'codex', label: 'codex', type: 'pool' },
  },
  {
    ...ATTENTION_ITEM_SAMPLE,
    ...ofAccount('secondary'),
    detail: { account: 'secondary', backoff_since: ms(secondaryPoll?.backoffSince), next_attempt_at: ms(secondaryPoll?.nextAttemptAt), rate_limited_in_a_row: secondaryPoll?.rateLimitedInARow ?? 0 },
    kind: 'poll_backoff',
    since: secondaryPoll?.backoffSince ?? ago(42 * MINUTE),
  },
];
