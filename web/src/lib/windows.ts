import type { WindowId } from '@/api/types';

export const WINDOW_HOURS: Record<WindowId, number> = { '5h': 5, '7d': 168, '7d_fable': 168 };
export const WINDOW_LABEL: Record<WindowId, string> = { '5h': '5-hour', '7d': '7-day', '7d_fable': '7-day Fable' };
/** The order the pages draw the windows in: the two shared windows, then the model-scoped one. */
export const WINDOW_ORDER: WindowId[] = ['5h', '7d', '7d_fable'];
