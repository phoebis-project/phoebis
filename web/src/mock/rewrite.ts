import { equivalentIn, modelWeeks, type Usage } from '@/lib/costs';
import { SHARED_WINDOWS } from './accounts';
import { MARKED_SESSION_IDS } from './requests';
import { TABLES, tablesFor } from './samples';

// The session rows and the routing log's rounds both price a pin's move here, so a row's move cost and a round's pick
// read one figure, and the rows need no import of the routing log, which imports them.

/** The plan windows a model draws: both shared windows, and a Fable model its week as well. */
export const drawnWindows = (model: string): string[] => [...SHARED_WINDOWS, ...modelWeeks(TABLES, model)];
/** The weekly window the pick prices a pin's rewrite in: the model's week where it draws one, else the 7-day window. */
export const rewriteWindowOf = (model: string): string => modelWeeks(TABLES, model)[0] ?? 'seven_day';
const proWeekOf = (usage: Usage, model: string, client: string | null): number | null => equivalentIn(tablesFor(client), model, usage, false);
/**
 * A rewrite of `tokens` as the Pro-week figure: the prompt written to the cache again, the Pro tier's share of its
 * 7-day window under the regime the pin's client label names, which no band weighs. Null for a model without a weight
 * of its own.
 */
export const proWeekRewriteOf = (tokens: number, model: string, client: string | null): number | null =>
  proWeekOf({ cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: tokens, input: 0, output: 0 }, model, client);
/** The same Pro-week figure of `tokens` as output, which the session pays again for the thinking a move drops. */
const proWeekOutputOf = (tokens: number, model: string, client: string | null): number | null =>
  proWeekOf({ cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: tokens }, model, client);

/**
 * The thinking the stretch round read as dropped where it planned its first marked pin's move: the blocks the pin's
 * account wrote, which the destination's binding drops. The round's record and the pin's session line read this one
 * seed, so the line answers the figures the round divided by.
 */
export const STRETCH_DROPPED_THINKING = { agentId: 'main', model: 'claude-sonnet-5-5', sessionId: MARKED_SESSION_IDS[1], tokens: 2_400 } as const;
/** The thinking a pin's move drops at its destination, null where the story learned no fact of the pair. */
export const droppedThinkingOf = (sessionId: string, agentId: string, model: string): number | null =>
  sessionId === STRETCH_DROPPED_THINKING.sessionId && agentId === STRETCH_DROPPED_THINKING.agentId && model === STRETCH_DROPPED_THINKING.model ? STRETCH_DROPPED_THINKING.tokens : null;
/**
 * The dropped thinking at the output's weight, in tokens: the count's share as output over its share as a cache write,
 * so the cost model's rows set the weight. Null where the move drops nothing and where either share is none.
 */
export const dropSizeOf = (dropped: number | null, model: string, client: string | null): number | null => {
  if (dropped === null || dropped <= 0) return null;
  const asOutput = proWeekOutputOf(dropped, model, client);
  const asWrite = proWeekRewriteOf(dropped, model, client);
  return asOutput === null || asWrite === null || asOutput <= 0 || asWrite <= 0 ? null : (dropped * asOutput) / asWrite;
};
/**
 * The prompt a move writes to the cache again at its destination: the last prompt less the thinking the move drops
 * there, since no destination of the story holds blocks of its own for the pin.
 */
export const rewriteTokensOf = (lastPrompt: number, dropped: number | null): number => Math.max(lastPrompt - (dropped ?? 0), 0);
/**
 * The size a round divides a pin's burn by and a move's cost prices: the rewrite at the destination plus the dropped
 * thinking at the output's weight, never under one token. Null where the pin holds no prompt size.
 */
export const moveSizeOf = (lastPrompt: number | null, dropped: number | null, model: string, client: string | null): number | null =>
  lastPrompt === null || lastPrompt === 0 ? null : Math.max(rewriteTokensOf(lastPrompt, dropped) + (dropSizeOf(dropped, model, client) ?? 0), 1);
