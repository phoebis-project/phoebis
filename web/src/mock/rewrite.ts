import { shareIn, weightOf } from '@/lib/costs';
import { SHARED_WINDOWS } from './accounts';
import { TABLES, tablesFor } from './samples';

// The session rows and the routing log's rounds both price a pin's move here, so a row's move cost and a round's pick
// read one figure, and the rows need no import of the routing log, which imports them.

/** The plan windows a model draws: both shared windows, and a Fable model its week as well. */
export const drawnWindows = (model: string): string[] => (model.includes('fable') ? [...SHARED_WINDOWS, 'seven_day_fable'] : [...SHARED_WINDOWS]);
/** The weekly window the pick prices a pin's rewrite in: the model's week where it draws one, else the 7-day window. */
export const rewriteWindowOf = (model: string): string => (drawnWindows(model).includes('seven_day_fable') ? 'seven_day_fable' : 'seven_day');
/**
 * A rewrite of `tokens` as the Pro-week figure: the prompt written to the cache again, the Pro tier's share of its
 * 7-day window under the regime the pin's client label names, which no band weighs. Null for a model without a weight
 * of its own.
 */
export const proWeekRewriteOf = (tokens: number, model: string, client: string | null): number | null =>
  weightOf(TABLES, model, 'seven_day') !== null ? shareIn(tablesFor(client), 'seven_day', 'default_claude_ai', model, { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: tokens, input: 0, output: 0 }, false) : null;
