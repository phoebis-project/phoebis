import type { KnownWindowId, WindowId } from '@/api/types';

/** A table over the known windows that a window id of any key reads, answering nothing for an unknown one. */
export type ByWindow<T> = Readonly<Record<KnownWindowId, T>> & Readonly<Partial<Record<string, T>>>;

export const WINDOW_HOURS: ByWindow<number> = { '5h': 5, '7d': 168, '7d_fable': 168 };
export const WINDOW_LABEL: ByWindow<string> = { '5h': '5-hour', '7d': '7-day', '7d_fable': '7-day Fable' };
/** The order the pages draw the windows in: the two shared windows, then the model-scoped one. */
export const WINDOW_ORDER: KnownWindowId[] = ['5h', '7d', '7d_fable'];

/** The period of a window the pages do not know, read from its key's family: a week, five hours, or none. */
export function hoursOfKey(id: WindowId): number {
  if (id.startsWith('seven_day')) return 168;
  if (id.startsWith('five_hour')) return 5;
  return 0;
}
