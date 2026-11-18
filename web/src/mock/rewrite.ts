import { equivalentIn, modelWeeks, type Usage } from '@/lib/costs';
import { servingAttempt } from '@/lib/dimensions';
import { SHARED_WINDOWS } from './accounts';
import { REQUESTS, bindsThinking } from './requests';
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

/** A pin by its key and the account it stands on. */
interface PinOn {
  accountId: string;
  agentId: string;
  keyId: string;
  model: string;
  sessionId: string;
}
/**
 * The thinking a pin's move to another account drops by `at`, as the router reads it: the pin's sum for its own account
 * where the pair reads bound, 0 with no sum, and null where the pair reads kept. The sum is the thinking of the pin
 * key's answered turns that account served since the key's last compaction, which sets it to 0. The router learns a
 * pair from the first answer behind a move and reads a pair it holds no row for as bound once one pair of the model
 * reads bound. The upstream binds the blocks of each model `bindsThinking` names, and the story's forced move at 11:01
 * teaches the router that for Sonnet 5.5 before any round or session line reads the figure. A pair reads bound on
 * those models and kept on every other. The round's record and the pin's session line both read this figure, so the
 * line answers the figures the round divided by.
 */
export const droppedThinkingOf = (pin: PinOn, at: number): number | null => {
  if (!bindsThinking(pin.model)) return null;
  const turns = REQUESTS.filter((r) => r.keyId === pin.keyId && r.sessionId === pin.sessionId && r.agentId === pin.agentId && r.canonicalModel === pin.model && Date.parse(r.at) < at);
  let sum = 0;
  for (const r of turns.sort((a, b) => Date.parse(a.at) - Date.parse(b.at))) {
    if (r.contextCompacted !== null) sum = 0;
    const serving = servingAttempt(r);
    if (serving?.upstreamStatus === 200 && serving.accountId === pin.accountId) sum += r.usage.thinking;
  }
  return sum;
};
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
