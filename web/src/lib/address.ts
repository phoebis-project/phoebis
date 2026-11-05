import { iso } from '@/api/decode';
import type { Account, BurnGroup, KeyStatus, RequestKind, RequestOutcome } from '@/api/types';
import { REQUEST_KIND_VALUES, REQUEST_OUTCOME_VALUES } from '@/api/types';
import { clock } from './clock';
import { ACCOUNT_STATUSES } from './quota';
import { DAY, HOUR } from './time';

/** One preset of the range control's one table: the name the address carries, the width and the label. */
export interface RangePreset {
  label: string;
  ms: number;
  name: string;
}

/** The one preset table of every ranged view: the name the address carries, the width and the label for the titles. */
export const RANGE_PRESETS = [
  { label: 'Last hour', ms: HOUR, name: '1h' },
  { label: 'Last 6 hours', ms: 6 * HOUR, name: '6h' },
  { label: 'Last 24 hours', ms: DAY, name: '24h' },
  { label: 'Last 7 days', ms: 7 * DAY, name: '7d' },
  { label: 'Last 30 days', ms: 30 * DAY, name: '30d' },
] as const satisfies readonly RangePreset[];

/** A view's window as the address carries it: a preset by name with the instants empty, or `custom` with both instants. */
export interface RangeValue {
  from: string;
  preset: string;
  to: string;
}

export type KeySpec =
  /** A value outside `values` reads as the default. */
  | { kind: 'choice'; default: string; name: string; values: readonly string[] }
  /** A number among `values`, written as its digits. A value outside them reads as the default. */
  | { kind: 'count'; default: number; name: string; values: readonly number[] }
  /** Written as `1`, read as true on `1`. */
  | { kind: 'flag'; name: string }
  /** Comma-joined ids. An entry `accepts` refuses leaves the list, so it leaves the address on the next write. */
  | { accepts?: (value: string) => boolean; kind: 'list'; name: string }
  /** Patterns joined by a newline, as the wire joins them, so a comma inside one rides. */
  | { kind: 'patterns'; name: string }
  /** The keys `range`, `from` and `to`. */
  | { kind: 'range'; default: string }
  | { kind: 'text'; name: string };

export type SortDir = 'asc' | 'desc';
export const SORT_DIRS = ['asc', 'desc'] as const;

/** One key of a view's sort control: its address value, its label and the direction a pick of it starts in. */
export interface SortKey<K extends string = string> {
  defaultDir: SortDir;
  id: K;
  label: string;
}

/**
 * A view's `sort` and `dir` keys: `choice` keys with the view's default key as `sort`'s default and the empty value
 * as `dir`'s, which stands for the picked key's own default direction, so the address carries `dir` only where it
 * differs from that direction and `?sort=usage` alone keeps its meaning. `sort` comes first, since `codecOf` writes
 * the keys in the specification's order, so a spread writes `sort=...&dir=...`.
 */
export const sortSpecs = <K extends string>(keys: readonly SortKey<K>[], defaultKey: K): { sort: KeySpec; dir: KeySpec } => ({
  sort: { default: defaultKey, kind: 'choice', name: 'sort', values: keys.map((k) => k.id) },
  dir: { default: '', kind: 'choice', name: 'dir', values: SORT_DIRS },
});

/** The direction an address means: its `dir` where it names one, else the sort key's own default. */
export const sortDirOf = <K extends string>(keys: readonly SortKey<K>[], address: { dir: '' | SortDir; sort: K }): SortDir =>
  address.dir !== '' ? address.dir : (keys.find((k) => k.id === address.sort)?.defaultDir ?? 'asc');

/** The address fields of a pick: `dir` empty where it is the key's own default, so the address omits it. */
export const sortAddressOf = <K extends string>(keys: readonly SortKey<K>[], next: { dir: SortDir; sort: K }): { dir: '' | SortDir; sort: K } => ({
  dir: next.dir === keys.find((k) => k.id === next.sort)?.defaultDir ? '' : next.dir,
  sort: next.sort,
});

export type Codec<F> = {
  /** The address with every field at its default, for a link that names one field and leaves the rest as the view's. */
  defaults: F;
  fromParams(params: URLSearchParams, now?: number): F;
  toParams(filters: F): URLSearchParams;
};

/** The instant a window key carries, or empty: any RFC 3339 instant parses, a bare integer reads as milliseconds. */
export const instantOf = (value: string | null): string => {
  if (!value) return '';
  const at = new Date(/^\d+$/.test(value) ? Number(value) : Date.parse(value));
  // An integer past the dates a `Date` holds reads as empty, since its ISO form would throw.
  return Number.isFinite(at.getTime()) ? at.toISOString() : '';
};

function readKey(key: KeySpec, params: URLSearchParams, now: number): unknown {
  switch (key.kind) {
    case 'choice': {
      const value = params.get(key.name);
      return value !== null && key.values.includes(value) ? value : key.default;
    }
    case 'count': {
      const value = params.get(key.name);
      return key.values.find((v) => String(v) === value) ?? key.default;
    }
    case 'flag':
      return params.get(key.name) === '1';
    case 'list':
      return (params.get(key.name) ?? '').split(',').filter((v) => v !== '' && (key.accepts?.(v) ?? true));
    case 'patterns':
      return (params.get(key.name) ?? '').split('\n').filter(Boolean);
    case 'range': {
      const from = instantOf(params.get('from'));
      const asked = instantOf(params.get('to'));
      // The end is capped before the pair is judged, as the range control caps it, so a pair wholly after now reads
      // as one that ends before it starts.
      const to = asked !== '' && Date.parse(asked) > now ? iso(now) : asked;
      // A pair is a span only while `from` precedes `to`, so a half, backward or unreadable pair reads as the preset.
      if (from !== '' && to !== '' && Date.parse(from) < Date.parse(to)) return { from, preset: 'custom', to } satisfies RangeValue;
      const name = params.get('range');
      return { from: '', preset: RANGE_PRESETS.some((p) => p.name === name) && name !== null ? name : key.default, to: '' } satisfies RangeValue;
    }
    case 'text':
      return params.get(key.name) ?? '';
  }
}

function writeKey(key: KeySpec, value: unknown, params: URLSearchParams): void {
  switch (key.kind) {
    case 'choice':
      if (value !== key.default) params.set(key.name, value as string);
      return;
    case 'count':
      if (value !== key.default) params.set(key.name, String(value));
      return;
    case 'flag':
      if (value === true) params.set(key.name, '1');
      return;
    case 'list':
    case 'patterns':
      if ((value as string[]).length > 0) params.set(key.name, (value as string[]).join(key.kind === 'list' ? ',' : '\n'));
      return;
    case 'range': {
      const range = value as RangeValue;
      if (range.preset === 'custom') {
        if (range.from) params.set('from', range.from);
        if (range.to) params.set('to', range.to);
      } else if (range.preset !== key.default) params.set('range', range.preset);
      return;
    }
    case 'text':
      if (value) params.set(key.name, value as string);
  }
}

/**
 * The grammar every filtered view reads its address through. A filter at its default leaves the address, and an
 * unknown key is dropped on the next write. A window's `from` and `to` are RFC 3339 UTC instants at millisecond
 * precision (`toISOString()`), read through `Date.parse`. A bare integer reads as Unix milliseconds, a form no page
 * writes. A `to` past `now`, the console clock's unless given, reads as `now`. The address lists the keys in the
 * specification's order.
 */
export function codecOf<F extends object>(spec: { [K in keyof F]: KeySpec }): Codec<F> {
  const keys = Object.entries(spec) as [string, KeySpec][];
  const fromParams = (params: URLSearchParams, now = clock.now()): F => Object.fromEntries(keys.map(([field, key]) => [field, readKey(key, params, now)])) as F;
  return {
    // An empty address names no instant, so the clock the range is capped at does not enter it.
    defaults: fromParams(new URLSearchParams()),
    fromParams,
    toParams(filters) {
      const params = new URLSearchParams();
      for (const [field, key] of keys) writeKey(key, (filters as Record<string, unknown>)[field], params);
      return params;
    },
  };
}

/** A preset's window from `now`, or a custom span's own instants. Named apart from quota.ts's windowOf, which the test entry also star-exports. */
export function addressWindow(range: RangeValue, fallback: string, now: number): { from: number; to: number } {
  if (range.preset === 'custom') {
    const from = Date.parse(range.from);
    const to = Date.parse(range.to);
    if (from < to) return { from, to };
  }
  const preset = RANGE_PRESETS.find((p) => p.name === range.preset) ?? RANGE_PRESETS.find((p) => p.name === fallback);
  return { from: now - (preset?.ms ?? 0), to: now };
}

const list = (name: string): KeySpec => ({ kind: 'list', name });
const text = (name: string): KeySpec => ({ kind: 'text', name });

export interface RequestsAddress {
  accounts: string[];
  /** The user agent pattern. */
  agent: string;
  agentId: string;
  agentTypes: string[];
  cacheMissReasons: string[];
  /** `client`, `-` for a request the marker does not hold. */
  classifiers: string[];
  client: string;
  efforts: string[];
  /** Guardrail ids, `-` for a request no guardrail matched. */
  guardrails: string[];
  held: boolean;
  keys: string[];
  kind: '' | RequestKind;
  models: string[];
  outcomes: RequestOutcome[];
  pools: string[];
  range: RangeValue;
  /** The serving attempt's refusal category, `-` for none. */
  refusalCategories: string[];
  request: string;
  requestClasses: string[];
  /** The page size. */
  rows: number;
  search: string;
  session: string;
  speeds: string[];
  statuses: string[];
  /** The serving attempt's stop reason, `-` for none. */
  stopReasons: string[];
  turnOrigins: string[];
}

/** The ledger's words for an outcome, so one the address carries that names none leaves it. */
const isOutcome = (value: string): boolean => REQUEST_OUTCOME_VALUES.some((o) => o === value);
/** A status code is digits alone. */
const isStatus = (value: string): boolean => /^\d+$/.test(value);

const REQUEST_KINDS: readonly ('' | RequestKind)[] = ['', ...REQUEST_KIND_VALUES];

export const REQUESTS_CODEC = codecOf<RequestsAddress>({
  search: text('search'),
  request: text('request'),
  session: text('session'),
  agentId: text('agent_id'),
  client: text('client'),
  agent: text('agent'),
  agentTypes: list('agent_types'),
  turnOrigins: list('turn_origins'),
  efforts: list('efforts'),
  requestClasses: list('request_classes'),
  classifiers: list('classifiers'),
  speeds: list('speeds'),
  held: { kind: 'flag', name: 'held' },
  keys: list('keys'),
  pools: list('pools'),
  accounts: list('accounts'),
  models: list('models'),
  statuses: { accepts: isStatus, kind: 'list', name: 'statuses' },
  outcomes: { accepts: isOutcome, kind: 'list', name: 'outcomes' },
  cacheMissReasons: list('cache_miss_reasons'),
  guardrails: list('guardrails'),
  refusalCategories: list('refusal_categories'),
  stopReasons: list('stop_reasons'),
  kind: { default: '', kind: 'choice', name: 'kind', values: REQUEST_KINDS },
  rows: { default: 25, kind: 'count', name: 'rows', values: [10, 25, 50, 100] },
  range: { default: '24h', kind: 'range' },
});
export const requestsHref = (address: RequestsAddress): string => `/requests?${REQUESTS_CODEC.toParams(address)}`;

export const accountHref = (id: string): string => `/accounts/${encodeURIComponent(id)}`;
/** One request's own page, where `requestsHref` opens the filtered list. */
export const requestHref = (id: string): string => `/requests/${encodeURIComponent(id)}`;

export interface AnalyticsAddress {
  accounts: string[];
  /** The group-by, the Burn section's own `session` among its values. */
  by: BurnGroup;
  /** The client patterns, any of which a request's client matches. */
  clients: string[];
  /** The user agent groups, one pattern per line. */
  groups: string;
  /** A session's key lifetime, `<key id>@<generation>`. */
  key: string;
  keys: string[];
  models: string[];
  pools: string[];
  probes: boolean;
  range: RangeValue;
  session: string;
  /** A shared link's unit position, which the page reads once into the console-wide setting and drops. */
  unit: string;
  /** The user agent patterns, any of which a request's user agent matches. */
  userAgents: string[];
}

// A key table rather than a list, so a grouping the burn route gains fails to compile until it is listed here. The
// burn route's groupings are the breakdown's and its own `session`.
const GROUP_BY_KEYS = { account: true, classifier: true, client: true, effort: true, key: true, kind: true, model: true, pool: true, request_class: true, session: true, speed: true, user_agent: true } satisfies Record<BurnGroup, true>;
const GROUP_BYS = Object.keys(GROUP_BY_KEYS) as BurnGroup[];

export const ANALYTICS_CODEC = codecOf<AnalyticsAddress>({
  session: text('session'),
  key: text('key'),
  accounts: list('accounts'),
  by: { default: 'key', kind: 'choice', name: 'by', values: GROUP_BYS },
  clients: { kind: 'patterns', name: 'clients' },
  groups: text('groups'),
  keys: list('keys'),
  models: list('models'),
  pools: list('pools'),
  probes: { kind: 'flag', name: 'probes' },
  unit: text('unit'),
  userAgents: { kind: 'patterns', name: 'user_agents' },
  range: { default: '24h', kind: 'range' },
});

/** The routing log's filters as the address holds them. */
export interface LogFilters {
  accounts: string[];
  agentId: string;
  keys: string[];
  kinds: string[];
  models: string[];
  pools: string[];
  range: RangeValue;
  request: string;
  session: string;
}

/**
 * The routing log's filters to and from its address. A kind this build does not know stays, so the route's refusal
 * reaches the page where a link from a newer router would otherwise read as every kind.
 */
export const ROUTING_LOG_CODEC = codecOf<LogFilters>({
  accounts: list('accounts'),
  agentId: text('agent_id'),
  keys: list('keys'),
  kinds: list('kinds'),
  models: list('models'),
  pools: list('pools'),
  request: text('request'),
  session: text('session'),
  range: { default: '1h', kind: 'range' },
});

/** The Meter view's one filter. */
export interface MeterAddress {
  accounts: string[];
}

export const METER_CODEC = codecOf<MeterAddress>({ accounts: list('accounts') });
/** The Meter view on one account's groups. */
export const accountMeterHref = (accountId: string): string => `/meter?${METER_CODEC.toParams({ accounts: [accountId] })}`;

export interface KeysAddress {
  pools: string[];
  search: string;
  status: 'all' | KeyStatus;
}

const KEY_STATUSES: readonly ('all' | KeyStatus)[] = ['all', 'active', 'disabled', 'expired', 'revoked'];

export const KEYS_CODEC = codecOf<KeysAddress>({
  pools: list('pools'),
  search: text('search'),
  status: { default: 'all', kind: 'choice', name: 'status', values: KEY_STATUSES },
});
/** The Keys view on one pool's keys, every other filter at its default. */
export const poolKeysHref = (poolId: string): string => `/keys?${KEYS_CODEC.toParams({ ...KEYS_CODEC.defaults, pools: [poolId] })}`;

export const EDIT_PARAM = 'edit';
export const GROUP_PARAM = 'group';
export const EDIT_PARAMS = [EDIT_PARAM, GROUP_PARAM] as const;
/** The view at `path` with the dialog of item `id` opened once on `group`, which `useAddressEdit` reads. */
export const editHref = (path: string, id: string, group: string): string => `${path}?${new URLSearchParams({ [EDIT_PARAM]: id, [GROUP_PARAM]: group })}`;

export interface AccountsAddress {
  /** `asc`, `desc` or empty for the sort key's own default direction, which the address omits. */
  dir: '' | SortDir;
  search: string;
  /** The list's order: `name` the default, `reset` the soonest reset, `usage` the most used, `status`. */
  sort: (typeof ACCOUNT_SORT_KEYS)[number]['id'];
  /** `all` the default, else an account status. */
  status: 'all' | Account['status'];
}

export const ACCOUNT_SORT_KEYS = [
  { defaultDir: 'asc', id: 'name', label: 'Name' },
  { defaultDir: 'asc', id: 'reset', label: 'Soonest reset' },
  { defaultDir: 'desc', id: 'usage', label: 'Most used' },
  { defaultDir: 'asc', id: 'status', label: 'Status' },
] as const satisfies readonly SortKey[];

export const ACCOUNTS_CODEC = codecOf<AccountsAddress>({
  search: text('search'),
  ...sortSpecs(ACCOUNT_SORT_KEYS, 'name'),
  status: { default: 'all', kind: 'choice', name: 'status', values: ['all', ...ACCOUNT_STATUSES] },
});

// These views have no filter, so their codecs read and write nothing: the Pools page keeps its hash and its one-shot
// `edit` and `group`, the Quota page's hash is its section, and a filter either view gains later lands here.
export const POOLS_CODEC = codecOf<Record<string, never>>({});
export const QUOTA_CODEC = codecOf<Record<string, never>>({});

export interface SessionsAddress {
  /** Companion only: the rows whose companion reports or went silent. */
  companion: boolean;
  /** `asc`, `desc` or empty for the sort key's own default direction, which the address omits. */
  dir: '' | SortDir;
  /** Show finished sessions. */
  finished: boolean;
  /** Show released agents on a live session. */
  released: boolean;
  /** The rows' order: `created` the default, the session's creation time, or `activity`, its last activity. */
  sort: (typeof SESSION_SORT_KEYS)[number]['id'];
}

export const SESSION_SORT_KEYS = [
  { defaultDir: 'desc', id: 'created', label: 'Created' },
  { defaultDir: 'desc', id: 'activity', label: 'Last activity' },
] as const satisfies readonly SortKey[];

// The codec's literal keeps the bar's order: codecOf writes the address's keys in its specification's order.
export const SESSIONS_CODEC = codecOf<SessionsAddress>({
  companion: { kind: 'flag', name: 'companion' },
  finished: { kind: 'flag', name: 'finished' },
  released: { kind: 'flag', name: 'released' },
  ...sortSpecs(SESSION_SORT_KEYS, 'created'),
});
