import type { KnownWindowId, WindowId } from '@/api/types';
import { lookup } from './counts';

/** A table over the known windows that a window id of any key reads, answering nothing for an unknown one. */
export type ByWindow<T> = Readonly<Record<KnownWindowId, T>> & Readonly<Partial<Record<string, T>>>;

export const WINDOW_HOURS: ByWindow<number> = { '5h': 5, '7d': 168, '7d_fable': 168 };
/** A window's wire key, the ledger's and the usage endpoint's spelling of it. */
export const WINDOW_KEY: ByWindow<string> = { '5h': 'five_hour', '7d': 'seven_day', '7d_fable': 'seven_day_fable' };
export const WINDOW_LABEL: ByWindow<string> = { '5h': '5-hour', '7d': '7-day', '7d_fable': '7-day Fable' };
/** A window's short word, for a cell too narrow for its label. */
export const WINDOW_SHORT: ByWindow<string> = { '5h': '5h', '7d': '7d', '7d_fable': '7-day Fable' };
/**
 * A window's tag in a cell or a chip: `5h`, `7d`, `Fable`. The preset preview's sentence reads `WINDOW_SHORT`, whose
 * Fable word is longer.
 */
const WINDOW_TAG: ByWindow<string> = { '5h': '5h', '7d': '7d', '7d_fable': 'Fable' };
/** A window as a hint's sentence names it. */
const WINDOW_HINT: ByWindow<string> = { '5h': 'the 5-hour window', '7d': 'the 7-day window', '7d_fable': 'the Fable week' };
/** The order the pages draw the windows in: the two shared windows, then the model-scoped one. */
export const WINDOW_ORDER: KnownWindowId[] = ['5h', '7d', '7d_fable'];
/** The wire keys of the three windows the router reads, in the order the pages draw them. */
export const WINDOW_KEYS: readonly string[] = WINDOW_ORDER.map((id) => WINDOW_KEY[id]);
/**
 * The three windows `keys` holds in `WINDOW_KEYS` order, then its other keys as listed. An alias spelling (`5h`,
 * `fable`) trails with the others, where a sort by `windowRank` would file it among the three.
 */
export const knownWindowsFirst = (keys: readonly string[]): string[] => [...WINDOW_KEYS.filter((k) => keys.includes(k)), ...keys.filter((k) => !WINDOW_KEYS.includes(k))];

/** The period of a window the pages do not know, read from its key's family: a week, five hours, or none. */
export function hoursOfKey(id: WindowId): number {
  if (id.startsWith('seven_day')) return 168;
  if (id.startsWith('five_hour')) return 5;
  return 0;
}

/**
 * The ledger, the request log and the usage endpoint spell a window by the endpoint's key, a request log attempt's
 * windows carry the stored keys, which no view reads, and the router's own windows name the 7-day Fable window
 * `fable`. Each spelling of a window is one window to the UI: `seven_day_fable`, `7d_oi` and `fable` all mean the
 * model-scoped week the UI draws third, and the quota history's decoder alone maps the head's stored key
 * `seven_day_overage_included` onto it. Any other key is a window of its own, kept under its name.
 */
const WINDOW_IDS = new Map<string, KnownWindowId>([
  ['5h', '5h'],
  ['7d', '7d'],
  ['7d_oi', '7d_fable'],
  ['fable', '7d_fable'],
  ['five_hour', '5h'],
  ['seven_day', '7d'],
  ['seven_day_fable', '7d_fable'],
]);
/** The UI's id for a window key: a known window's own id, any other key as it is, `null` for no key at all. */
export const windowKeyId = (id: string): WindowId | null => WINDOW_IDS.get(id) ?? (id === '' ? null : id);
/** A window table's entry under any of the window's spellings, nothing for a window the console does not know. */
export const windowEntry = <T>(table: ByWindow<T>, key: string): T | undefined => lookup(table, windowKeyId(key) ?? '');
/** A window's label under any of its spellings, a window the console does not know under its key. */
export const windowLabelOf = (key: string): string => windowEntry(WINDOW_LABEL, key) ?? key;
/** A window's word in a column head: "7-day", "Fable", "5-hour", a window the console does not know under its key. */
export const windowWord = (key: string): string => (windowKeyId(key) === '7d_fable' ? 'Fable' : windowLabelOf(key));
/** A known window's tag under any of its spellings, nothing for a window the console does not know. */
export const windowTagOf = (key: string): string | undefined => windowEntry(WINDOW_TAG, key);
/** A window's tag, a window the console does not know under its key. */
export const windowTag = (key: string): string => windowTagOf(key) ?? key;
/** A window's hint sentence, a window the console does not know under its key. */
export const windowHint = (key: string): string => windowEntry(WINDOW_HINT, key) ?? key;
/** A window key's place in the order the pages draw them, a window the pages do not know after every known one. */
export const windowRank = (key: string): number => {
  const at = (WINDOW_ORDER as readonly string[]).indexOf(windowKeyId(key) ?? '');
  return at === -1 ? WINDOW_ORDER.length : at;
};
/** The items in the order the pages draw their windows, then the items of any other window in arrival order. */
export function inWindowOrder<T>(items: readonly T[], keyOf: (item: T) => string): T[] {
  // A stable sort, so the ids the pages do not know keep their arrival order.
  return [...items].sort((x, y) => windowRank(keyOf(x)) - windowRank(keyOf(y)));
}
