import type { AnalyticsGroupBy } from '@/api/types';
import { OTHER_DIMENSION, foldRequestClass } from './dimensions';

export interface RequestsLinkInput {
  /** The reason the count is under, any name the beta sent. */
  reason: string;
  count: number;
  /** The panel's window in milliseconds and the page's now; `exact` for a session view, whose log read the exact
   * window, else the rollups' whole hours apply. */
  window: { fromMs: number; toMs: number; nowMs: number; exact: boolean };
  /** The Analytics page's filters the Requests page reads the same way. */
  filters: {
    accountIds: string[];
    clientPattern: string;
    includeProbes: boolean;
    /** The page's key selection, or a session view's key lifetime alone (`linkedKeyIds`). */
    keyIds: string[];
    models: string[];
    poolIds: string[];
    sessionId: string | null;
    userAgentPattern: string;
  };
  /** The breakdown row's own group; absent for a totals bar. */
  group?: { by: AnalyticsGroupBy; key: string };
}

export interface RequestsLinkTarget {
  /** The Requests address, or null where the page cannot list exactly the count's requests. */
  href: string | null;
  /** Why the count links, or why it stays plain. */
  title: string;
}

const HOUR = 3_600_000;

/** The rollups file a request under the hour it arrived in, and the breakdown reads the hours before the hour ceiling
 * of the end, so an end exactly on the hour leaves that hour out. The end is capped at now, since the route's `to` is
 * inclusive and nothing arrived after now. */
const hourWindow = ({ fromMs, toMs, nowMs, exact }: RequestsLinkInput['window']) =>
  exact ? { from: fromMs, to: toMs } : { from: Math.floor(fromMs / HOUR) * HOUR, to: Math.min(Math.ceil(toMs / HOUR) * HOUR - 1, nowMs) };

/** The rollups fold a client class past 64 bytes or outside the identifier characters into `other`, as they fold a
 * request class, beside the clients the relay labels `other` itself, while the request log matches the raw class. */
const unlistableClient = (key: string) => key === OTHER_DIMENSION || foldRequestClass(key) !== key;

/** The Requests filter each grouping's row writes; the user agent grouping's counts stay plain. */
const GROUP_PARAM: Record<Exclude<AnalyticsGroupBy, 'all' | 'user_agent'>, string> = {
  account: 'accounts',
  client: 'client',
  effort: 'efforts',
  key: 'keys',
  kind: 'kind',
  model: 'models',
  pool: 'pools',
  request_class: 'request_classes',
  speed: 'speeds',
};

const LISTED = 'List these requests on the Requests page';
// The Requests page lists probes under its Probe kind alone.
const LISTED_WITHOUT_PROBES = `${LISTED}. The list leaves the probes out`;
const NOTHING_TO_LIST = 'No requests to list';
const AGENT_PATTERN_PLAIN = 'Not linked: the user agent pattern matches the folded product token here, and the request log matches the raw header';
const AGENT_GROUP_PLAIN = 'Not linked: this grouping keys a user agent by its folded product token, and the request log matches the raw header';
const CLIENT_PLAIN = 'Not linked: the request log matches the raw client class, and the rollups file a long or unusual class under other';

/**
 * The key selectors a link carries: a session view's key lifetime alone while the page reads the session under one,
 * since the page keeps that lifetime's rows alone, else the page's key selection.
 */
export function linkedKeyIds(selected: string[], sessionKey: string | null): string[] {
  return sessionKey === null ? selected : [sessionKey];
}

/**
 * The Requests address for a count of the cache miss reasons panel: `cache_miss_reasons` set to the reason, the window
 * as two ISO instants (the start of the hour holding the panel's start to one millisecond before the hour ceiling of
 * its end, capped at now, or the exact window for a session view), the page's selections as `accounts`, `keys`,
 * `models` and `pools`, the class pattern as `client`, a session id as `search`, and a breakdown row's group as its
 * own filter: `accounts`, `client`, `efforts`, `keys`, `kind`, `models`, `pools`, `request_classes` or `speeds`. A
 * client group keyed `other`, or keyed with a class the rollups would fold, answers no address.
 */
export function requestsLink({ count, filters, group, reason, window }: RequestsLinkInput): RequestsLinkTarget {
  if (count === 0) return { href: null, title: NOTHING_TO_LIST };
  if (filters.userAgentPattern !== '') return { href: null, title: AGENT_PATTERN_PLAIN };
  if (group?.by === 'user_agent') return { href: null, title: AGENT_GROUP_PLAIN };
  if (group?.by === 'client' && unlistableClient(group.key)) return { href: null, title: CLIENT_PLAIN };
  const { from, to } = hourWindow(window);
  const params = new URLSearchParams();
  const put = (name: string, value: string) => {
    if (value) params.set(name, value);
  };
  put('cache_miss_reasons', reason);
  put('from', new Date(from).toISOString());
  put('to', new Date(to).toISOString());
  put('accounts', filters.accountIds.join(','));
  put('keys', filters.keyIds.join(','));
  put('models', filters.models.join(','));
  put('pools', filters.poolIds.join(','));
  put('client', filters.clientPattern);
  put('search', filters.sessionId ?? '');
  // The row is one value inside the page's selection of its own dimension, so it takes that selection's place.
  if (group !== undefined && group.by !== 'all') put(GROUP_PARAM[group.by], group.key);
  const probes = group?.by === 'kind' && group.key === 'probe';
  return { href: `/requests?${params}`, title: filters.includeProbes && !probes ? LISTED_WITHOUT_PROBES : LISTED };
}
