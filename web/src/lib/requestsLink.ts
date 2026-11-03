import { iso } from '@/api/decode';
import type { AnalyticsGroupBy, RequestLog } from '@/api/types';
import { type RangeValue, REQUESTS_DEFAULTS, requestsHref, type RequestsAddress } from './address';
import { OTHER_DIMENSION, foldRequestClass } from './dimensions';
import { matchesPattern } from './pattern';
import { aroundRequest, aroundSubject, clockMinute, type LogWindow } from './routingLog';
import { HOUR } from './time';

export interface RequestsLinkInput {
  /** The reason the count is under, any name the beta sent. */
  reason?: string;
  /** The category of the upstream's refusals the count is under, `-` for a refusal with none. */
  refusalCategory?: string;
  count: number;
  /** The panel's window in milliseconds and the page's now. `exact` keeps the window as given, for a session view whose
   * log read it and a refusal count whose route counted it, and otherwise the rollups' whole hours apply. */
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
  /** The breakdown row's own group. A totals bar has none. */
  group?: { by: AnalyticsGroupBy; key: string };
}

export interface RequestsLinkTarget {
  /** The Requests address, or null where the page cannot list exactly the count's requests. */
  href: string | null;
  /** Why the count links, or why it stays plain. */
  title: string;
}

/** The rollups file a request under the hour it arrived in, and the breakdown reads the hours before the hour ceiling
 * of the end, so an end exactly on the hour leaves that hour out. The end is capped at now, since the route's `to` is
 * inclusive and nothing arrived after now. */
const hourWindow = ({ fromMs, toMs, nowMs, exact }: RequestsLinkInput['window']) =>
  exact ? { from: fromMs, to: toMs } : { from: Math.floor(fromMs / HOUR) * HOUR, to: Math.min(Math.ceil(toMs / HOUR) * HOUR - 1, nowMs) };

/** The rollups fold a client class past 64 bytes or outside the identifier characters into `other`, as they fold a
 * request class, beside the clients the relay labels `other` itself, while the request log matches the raw class. */
const unlistableClient = (key: string) => key === OTHER_DIMENSION || foldRequestClass(key) !== key;

/** A class pattern that matches the rollups' `other` counts the long or unusual classes folded into it, which the
 * request log tests raw and leaves out. A pattern of stars alone matches the raw and the folded alike. */
const unlistableClientPattern = (pattern: string) => pattern !== '' && !/^\*+$/.test(pattern) && matchesPattern(pattern, OTHER_DIMENSION);

/** The Requests filter each grouping's row writes. The user agent grouping's counts stay plain. */
const GROUP_PARAM: Record<Exclude<AnalyticsGroupBy, 'all' | 'user_agent'>, 'accounts' | 'classifiers' | 'client' | 'efforts' | 'keys' | 'kind' | 'models' | 'pools' | 'requestClasses' | 'speeds'> = {
  account: 'accounts',
  classifier: 'classifiers',
  client: 'client',
  effort: 'efforts',
  key: 'keys',
  kind: 'kind',
  model: 'models',
  pool: 'pools',
  request_class: 'requestClasses',
  speed: 'speeds',
};

const LISTED = 'List these requests on the Requests page';
// The Requests page lists probes under its Probe kind alone.
const LISTED_WITHOUT_PROBES = `${LISTED}. The list leaves the probes out`;
const NOTHING_TO_LIST = 'No requests to list';
const AGENT_PATTERN_PLAIN = 'Not linked: the user agent pattern matches the folded product token here, and the request log matches the raw header';
const AGENT_GROUP_PLAIN = 'Not linked: this grouping keys a user agent by its folded product token, and the request log matches the raw header';
const CLIENT_PLAIN = 'Not linked: the request log matches the raw client class, and the rollups file a long or unusual class under other';
const CLIENT_PATTERN_PLAIN = 'Not linked: the client pattern matches other here, where the rollups file a long or unusual class, and the request log matches the raw client class';
/**
 * The hover of a count the Analytics page draws under several client patterns: the request log's client filter takes
 * one pattern, so a link could list fewer rows than the count holds or, with the patterns dropped, more.
 */
export const CLIENT_PATTERNS_PLAIN = 'Several client patterns are set, and the request log takes one';

/**
 * The key selectors a link carries: a session view's key lifetime alone while the page reads the session under one,
 * since the page keeps that lifetime's rows alone, else the page's key selection.
 */
export function linkedKeyIds(selected: string[], sessionKey: string | null): string[] {
  return sessionKey === null ? selected : [sessionKey];
}

/**
 * The Requests address for a count of the cache miss reasons panel or the refusals card: `cache_miss_reasons` set to
 * the reason where one is given, `stop_reasons` set to `refusal` and `refusal_categories` to the category where one is
 * given, the window as two ISO instants (the start of the hour holding the panel's start to one millisecond before the
 * hour ceiling of its end, capped at now, or the exact window where `exact` is set), the page's selections as
 * `accounts`, `keys`, `models` and `pools`, the class pattern as `client`, a session id as `session`, and a breakdown
 * row's group as its own filter: `accounts`, `classifiers`, `client`, `efforts`, `keys`, `kind`, `models`, `pools`,
 * `request_classes` or `speeds`. A client group keyed `other`, or keyed with a class the rollups would fold, answers
 * no address, and outside the client grouping neither does a class pattern that matches `other`, unless it is stars
 * alone, which matches the raw and the folded alike.
 */
export function requestsLink({ count, filters, group, reason, refusalCategory, window }: RequestsLinkInput): RequestsLinkTarget {
  if (count === 0) return { href: null, title: NOTHING_TO_LIST };
  if (filters.userAgentPattern !== '') return { href: null, title: AGENT_PATTERN_PLAIN };
  if (group?.by === 'user_agent') return { href: null, title: AGENT_GROUP_PLAIN };
  if (group?.by === 'client' && unlistableClient(group.key)) return { href: null, title: CLIENT_PLAIN };
  // A client row's own class takes the pattern's place in the address, so the pattern decides nothing there.
  if (group?.by !== 'client' && unlistableClientPattern(filters.clientPattern)) return { href: null, title: CLIENT_PATTERN_PLAIN };
  const { from, to } = hourWindow(window);
  const address: RequestsAddress = {
    ...REQUESTS_DEFAULTS,
    accounts: filters.accountIds,
    cacheMissReasons: reason === undefined ? [] : [reason],
    client: filters.clientPattern,
    keys: filters.keyIds,
    models: filters.models,
    pools: filters.poolIds,
    range: { from: iso(from), preset: 'custom', to: iso(to) },
    refusalCategories: refusalCategory === undefined ? [] : [refusalCategory],
    session: filters.sessionId ?? '',
    stopReasons: refusalCategory === undefined ? [] : ['refusal'],
  };
  // The row is one value inside the page's selection of its own dimension, so it takes that selection's place.
  if (group !== undefined && group.by !== 'all') {
    const field = GROUP_PARAM[group.by];
    if (field === 'client') address.client = group.key;
    else if (field === 'kind') address.kind = group.key as RequestsAddress['kind'];
    else address[field] = [group.key];
  }
  const probes = group?.by === 'kind' && group.key === 'probe';
  return { href: requestsHref(address), title: filters.includeProbes && !probes ? LISTED_WITHOUT_PROBES : LISTED };
}

/** A count of the refusals card: its category's refused answers over the exact window the route counted, since the
 *  route reads the attempts at the request rather than the rollups' whole hours, with no session, which the route does
 *  not read. The two patterns pass through as the route reads them, so the count reads plain under a user agent pattern
 *  or a client pattern the request log cannot list, as the reasons panel's counts do, and its link never lists more
 *  rows than the bar counts. */
export function refusalLink(input: Omit<RequestsLinkInput, 'count' | 'group' | 'reason' | 'refusalCategory'>, category: string, count: number): RequestsLinkTarget {
  return requestsLink({ ...input, count, filters: { ...input.filters, sessionId: null }, refusalCategory: category, window: { ...input.window, exact: true } });
}

/** One link of the drawer's "Widen to" line. */
export interface Widening {
  href: string;
  label: 'this account' | 'this minute' | 'this session';
  /** The session or account id the link widens to, none for the minute. */
  title: string | null;
}

/**
 * The drawer's three widenings in the order narrow to wide, each a fresh Requests address from the codec's defaults
 * with the page size kept: this minute over `clockMinute`, this session over `aroundSubject` with the session, this
 * account over `aroundRequest` with the account. The windows are the Routing log's own, so one widening reads one span
 * on either view. A request without a session or an account omits that link, and one still running reads as a moment.
 * Each window's end is capped at `now`, or at the request's end where that is later.
 */
export function requestsWiden(request: Pick<RequestLog, 'accountId' | 'at' | 'durationMs' | 'id' | 'sessionId'>, now: number, rows: string): Widening[] {
  const at = Date.parse(request.at);
  const finished = request.durationMs === null ? null : at + request.durationMs;
  // A row the list read after the page's clock sample is newer than it, and a window capped there would leave the
  // request out, or put this minute's end before its start.
  const cap = Math.max(now, finished ?? at);
  const defaults = { ...REQUESTS_DEFAULTS, rows };
  const over = (link: LogWindow): RangeValue => ('range' in link ? { from: '', preset: link.range, to: '' } : { from: iso(link.from), preset: 'custom', to: iso(link.to) });
  return [
    { href: requestsHref({ ...defaults, range: over(clockMinute(at, cap)) }), label: 'this minute', title: null },
    ...(request.sessionId === null ? [] : [{ href: requestsHref({ ...defaults, range: over(aroundSubject(at, cap)), session: request.sessionId }), label: 'this session' as const, title: request.sessionId }]),
    ...(request.accountId === null ? [] : [{ href: requestsHref({ ...defaults, accounts: [request.accountId], range: over(aroundRequest(at, finished, cap)) }), label: 'this account' as const, title: request.accountId }]),
  ];
}
