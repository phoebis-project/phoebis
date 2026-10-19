// A type-only import, so the API bundle the unit tests load stays free of React.
import type { Option } from '@/components/ui/OptionList';
import { NO_CLASS } from './dimensions';
import { kindsChipText } from './routingLog';

export type DimensionKind = 'choice' | 'flag' | 'id' | 'list' | 'pattern';

/** One filter of a view: the bar derives the Add filter list, the chips and the popover's steps from these entries. */
export interface Dimension {
  /** A choice's first option, which removes the chip ("Any kind", "All statuses"). */
  anyLabel?: string;
  /** A choice's value that means none, which hasValue, Remove, Clear all and the any row read and write: the empty string unless set, `all` on the Keys Status. */
  defaultValue?: string;
  /** A list whose empty value means every option, which its values step draws with every row checked. */
  emptyMeansAll?: true;
  /** The codec field the dimension reads and writes. */
  field: string;
  /** The Add filter list's group heading. */
  group: string;
  kind: DimensionKind;
  /** The Add filter list's label ("Cache miss reason"). */
  label: string;
  /** The chip's values and the option rows in the mono face. */
  mono?: boolean;
  /** A list that opens on its search field however few its options, where a list draws one past twelve alone. */
  searchable?: true;
  /** The chip's text, title and values for a value, in place of chipText's forms. The chip draws `values` as given, whatever `text` reads. */
  text?: (value: unknown, options: Option[]) => ChipText;
  /** The word before the colon: the label in lower case unless named ("kinds", "client"). */
  word: string;
}

export interface ChipText {
  text: string;
  title: string;
  /** The text past the word and its colon, which the chip draws in the value ink. Null for a flag, whose text is the word alone. */
  values: string | null;
}

/** A standing filter as the chip line draws it. `label` is the dimension's chip word, which the x and the fold name. */
export interface Chip {
  field: string;
  label: string;
  mono: boolean;
  text: string;
  title: string;
  values: string | null;
}

/** The values a list chip prints before it counts the rest. */
const LISTED = 3;
/** The chips the line shows before it folds the rest. */
const SHOWN = 5;
/** The cut an id chip prints, enough to tell two sessions or requests apart. */
const ID_SHOWN = 8;

const labelOf = (value: string, options: Option[]) => options.find((o) => o.value === value)?.label ?? value;
const listed = (words: string[]) => `${words.slice(0, LISTED).join(', ')}${words.length > LISTED ? ` +${words.length - LISTED}` : ''}`;
const worded = (word: string, values: string, title = values): ChipText => ({ text: `${word}: ${values}`, title, values });

/** A non-empty list or text, a true flag, a choice off its `defaultValue` (the empty string unless set). */
export function hasValue(dimension: Dimension, value: unknown): boolean {
  switch (dimension.kind) {
    case 'choice':
      return typeof value === 'string' && value !== (dimension.defaultValue ?? '');
    case 'flag':
      return value === true;
    case 'id':
    case 'pattern':
      return typeof value === 'string' && value !== '';
    case 'list':
      return Array.isArray(value) && value.length > 0;
  }
}

/** The value that removes the dimension's chip, which a chip's x, Remove and Clear all write. */
export function emptyValue(dimension: Dimension): unknown {
  switch (dimension.kind) {
    case 'choice':
      return dimension.defaultValue ?? '';
    case 'flag':
      return false;
    case 'id':
    case 'pattern':
      return '';
    case 'list':
      return [];
  }
}

/** A list as up to three option labels then "+N" with every label in the title, a pattern as itself, an id as its first eight characters and an ellipsis with the whole id in the title, a choice as its option's label, a flag as the word alone with no colon. */
export function chipText(dimension: Dimension, value: unknown, options: Option[] = []): ChipText {
  const { word } = dimension;
  switch (dimension.kind) {
    case 'choice':
      return worded(word, labelOf(value as string, options));
    case 'flag':
      return { text: word, title: word, values: null };
    case 'id': {
      const id = value as string;
      return worded(word, id.length > ID_SHOWN ? `${id.slice(0, ID_SHOWN)}…` : id, id);
    }
    case 'list': {
      // A value no option names prints itself, so an earlier key holder or a status code only a loaded row carries
      // still reads.
      const labels = (value as string[]).map((v) => labelOf(v, options));
      return worded(word, listed(labels), labels.join(', '));
    }
    case 'pattern':
      return worded(word, value as string);
  }
}

/** The view's chips in the dimension list's order, one per dimension that has a value. */
export function chipsOf(dimensions: Dimension[], filters: Record<string, unknown>, options: Record<string, Option[]>): Chip[] {
  return dimensions
    .filter((d) => hasValue(d, filters[d.field]))
    .map((d) => {
      const value = filters[d.field];
      const known = options[d.field] ?? [];
      const { text, title, values } = d.text ? d.text(value, known) : chipText(d, value, known);
      return { field: d.field, label: d.word, mono: d.mono ?? false, text, title, values };
    });
}

/** At most five chips, fewer when `fits(count)` says so, the rest folded: the fold's title names the folded chips' labels. */
export function foldChips(chips: Chip[], fits: (count: number) => boolean): { folded: Chip[]; shown: Chip[]; title: string } {
  let count = Math.min(SHOWN, chips.length);
  while (count > 1 && !fits(count)) count -= 1;
  const folded = chips.slice(count);
  return { folded, shown: chips.slice(0, count), title: folded.length > 0 ? `Folded: ${folded.map((c) => c.label).join(', ')}` : '' };
}

const ACCOUNT: Dimension = { field: 'accounts', group: 'Routing', kind: 'list', label: 'Account', word: 'account' };
const KEY: Dimension = { field: 'keys', group: 'Routing', kind: 'list', label: 'Key', mono: true, word: 'key' };
const MODEL: Dimension = { field: 'models', group: 'Routing', kind: 'list', label: 'Model', mono: true, word: 'model' };
/** The whole catalog, which Requests and the Routing log offer, so the list opens on its search field. */
const CATALOG_MODEL: Dimension = { ...MODEL, searchable: true };
const POOL: Dimension = { field: 'pools', group: 'Routing', kind: 'list', label: 'Pool', mono: true, word: 'pool' };
const USER_AGENT: Dimension = { field: 'agent', group: 'Request', kind: 'pattern', label: 'User agent', mono: true, word: 'user agent' };

/**
 * A dimension's disabled reason while its option list did not load, `{ [field]: "The key list did not load: ..." }`,
 * else nothing. A list that loaded keeps it through a refresh that fails.
 */
export function notLoadedReason(field: string, name: string, read: { data: unknown; error: string | null }): Record<string, string> {
  return read.data === null && read.error !== null ? { [field]: `The ${name} list did not load: ${read.error}` } : {};
}

/** Routing, Outcome, Request, Subject. A status chip prints the codes alone, since an option's label adds the code's name. */
export const REQUESTS_DIMENSIONS: Dimension[] = [
  ACCOUNT,
  KEY,
  CATALOG_MODEL,
  POOL,
  { field: 'cacheMissReasons', group: 'Outcome', kind: 'list', label: 'Cache miss reason', word: 'cache miss reason' },
  { field: 'guardrails', group: 'Outcome', kind: 'list', label: 'Guardrail', word: 'guardrail' },
  { field: 'held', group: 'Outcome', kind: 'flag', label: 'Held only', word: 'held only' },
  { field: 'outcomes', group: 'Outcome', kind: 'list', label: 'Outcome', word: 'outcome' },
  {
    field: 'statuses',
    group: 'Outcome',
    kind: 'list',
    label: 'Status',
    text: (value, options) => {
      const codes = value as string[];
      return worded('status', listed(codes), codes.map((code) => labelOf(code, options)).join(', '));
    },
    word: 'status',
  },
  { field: 'agentTypes', group: 'Request', kind: 'list', label: 'Agent type', mono: true, word: 'agent type' },
  {
    field: 'classifiers',
    group: 'Request',
    kind: 'list',
    label: 'Classifier',
    // The option `client classifier` would read the word twice after `classifier:`, so the chip prints the standing.
    text: (value, options) => {
      const standings = value as string[];
      return worded('classifier', listed(standings.map((v) => (v === NO_CLASS ? 'none' : v))), standings.map((v) => labelOf(v, options)).join(', '));
    },
    word: 'classifier',
  },
  { field: 'client', group: 'Request', kind: 'pattern', label: 'Client', mono: true, word: 'client' },
  { field: 'efforts', group: 'Request', kind: 'list', label: 'Effort', mono: true, word: 'effort' },
  { anyLabel: 'Any kind', field: 'kind', group: 'Request', kind: 'choice', label: 'Kind', word: 'kind' },
  { field: 'requestClasses', group: 'Request', kind: 'list', label: 'Request class', mono: true, word: 'request class' },
  { field: 'speeds', group: 'Request', kind: 'list', label: 'Speed', mono: true, word: 'speed' },
  { field: 'turnOrigins', group: 'Request', kind: 'list', label: 'Turn origin', mono: true, word: 'turn origin' },
  USER_AGENT,
  { field: 'request', group: 'Subject', kind: 'id', label: 'Request', mono: true, word: 'request' },
  { field: 'session', group: 'Subject', kind: 'id', label: 'Session', mono: true, word: 'session' },
];

/** Kinds, Routing, Subject: the kinds lead, since their presets are the way to narrow this view. */
export const ROUTING_LOG_DIMENSIONS: Dimension[] = [
  { emptyMeansAll: true, field: 'kinds', group: 'Kinds', kind: 'list', label: 'Kinds', text: (value) => kindsChipText(value as string[]), word: 'kinds' },
  ACCOUNT,
  KEY,
  CATALOG_MODEL,
  POOL,
  { field: 'request', group: 'Subject', kind: 'id', label: 'Request', mono: true, word: 'request' },
  { field: 'session', group: 'Subject', kind: 'id', label: 'Session', mono: true, word: 'session' },
];

/** Routing, Request, Subject. The client pattern travels as `class` and reads "client", as on Requests. */
export const ANALYTICS_DIMENSIONS: Dimension[] = [
  { ...ACCOUNT, mono: true },
  KEY,
  MODEL,
  POOL,
  { field: 'class', group: 'Request', kind: 'pattern', label: 'Client', mono: true, word: 'client' },
  { field: 'probes', group: 'Request', kind: 'flag', label: 'Include probes', word: 'include probes' },
  USER_AGENT,
  { field: 'session', group: 'Subject', kind: 'id', label: 'Session', mono: true, word: 'session' },
];

/** Keys. The status choice's none is `all`, the codec's default. */
export const KEYS_DIMENSIONS: Dimension[] = [
  { ...POOL, group: 'Keys' },
  { anyLabel: 'All statuses', defaultValue: 'all', field: 'status', group: 'Keys', kind: 'choice', label: 'Status', word: 'status' },
];

/** Accounts. The status choice's none is `all`, the codec's default. */
export const ACCOUNTS_DIMENSIONS: Dimension[] = [{ anyLabel: 'All statuses', defaultValue: 'all', field: 'status', group: 'Accounts', kind: 'choice', label: 'Status', word: 'status' }];

/** The codec keys that are no dimension: `range`, `rows` and `search` everywhere, and per view the keys named here. */
export const NOT_DIMENSIONS: Record<'accounts' | 'analytics' | 'keys' | 'requests' | 'routingLog', readonly string[]> = {
  accounts: ['range', 'rows', 'search', 'sort'],
  analytics: ['by', 'groups', 'key', 'range', 'rows', 'search', 'unit'],
  keys: ['range', 'rows', 'search'],
  requests: ['range', 'rows', 'search'],
  routingLog: ['range', 'rows', 'search'],
};
