import type { KnownWindowId, WindowId } from '@/api/types';

/** A table over the known windows that a window id of any key reads, answering nothing for an unknown one. */
export type ByWindow<T> = Readonly<Record<KnownWindowId, T>> & Readonly<Partial<Record<string, T>>>;

export const WINDOW_HOURS: ByWindow<number> = { '5h': 5, '7d': 168, '7d_fable': 168 };
export const WINDOW_LABEL: ByWindow<string> = { '5h': '5-hour', '7d': '7-day', '7d_fable': '7-day Fable' };
/** The order the pages draw the windows in: the two shared windows, then the model-scoped one. */
export const WINDOW_ORDER: KnownWindowId[] = ['5h', '7d', '7d_fable'];
/** The wire keys of the three windows the router reads, in the order the pages draw them. */
export const WINDOW_KEYS: readonly string[] = ['five_hour', 'seven_day', 'seven_day_fable'];

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
