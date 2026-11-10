import { decodePricing } from '@/api/decode';
import type { CatalogModel, ModelPrice, PriceCells } from '@/api/types';
import { revisionAt, type Usage } from '@/lib/costs';
import { undatedModel } from '@/lib/format';
import { FIXTURES } from './generated/fixtures';

// Stored out of id order, so a page that renders it verbatim rather than sorting it fails the smoke check
// that reads the catalog popup as sorted by id. The effort levels are the ones the API documents for each model.
export const CATALOG: CatalogModel[] = [
  { id: 'claude-fable-5-1', displayName: 'Claude Fable 5.1', createdAt: '2026-08-01', effortLevels: ['low', 'medium', 'high', 'xhigh', 'max'], maxInputTokens: 1_000_000, maxTokens: 128_000 },
  { id: 'claude-sonnet-5', displayName: 'Claude Sonnet 5', createdAt: '2026-05-01', effortLevels: ['low', 'medium', 'high', 'xhigh', 'max'], maxInputTokens: 1_000_000, maxTokens: 128_000 },
  { id: 'claude-sonnet-5-5', displayName: 'Claude Sonnet 5.5', createdAt: '2026-06-01', effortLevels: ['low', 'medium', 'high', 'xhigh', 'max'], maxInputTokens: 1_000_000, maxTokens: 128_000 },
  { id: 'claude-haiku-4-5-20251001', displayName: 'Claude Haiku 4.5', createdAt: '2025-10-01', effortLevels: [], maxInputTokens: 200_000, maxTokens: 64_000 },
  { id: 'claude-opus-5', displayName: 'Claude Opus 5', createdAt: '2026-06-01', effortLevels: ['low', 'medium', 'high', 'xhigh', 'max'], maxInputTokens: 1_000_000, maxTokens: 128_000 },
  { id: 'claude-opus-5-5', displayName: 'Claude Opus 5.5', createdAt: '2026-07-01', effortLevels: ['low', 'medium', 'high', 'xhigh', 'max'], maxInputTokens: 1_000_000, maxTokens: 128_000 },
];

interface EntryWire {
  content: Record<string, unknown> & { effective: string };
  effective: string;
}
interface EnvelopeWire extends EntryWire {
  revisions: EntryWire[];
  source: string;
}

/** The dates the story gives the shipped revisions, in their order: the story runs the month before its clock, so a
 *  shipped date would put every request before the first revision. The last price date lies after the clock, since the
 *  captured fixture holds that entry as the next one (the capture's clock sits before its shipped date), so the mock
 *  holds it as the next one too. */
const STORY_PRICE_DATES = ['2026-09-01', '2026-09-08', '2026-10-09'];
const STORY_COST_DATES = ['2026-09-01'];

/** The envelope with each revision's date, and the entry in force's, moved to the story's own, the tables untouched.
 *  A revision count the dates do not match fails the mock build rather than dating an entry by guess. */
function redated(envelope: EnvelopeWire, dates: string[]): EnvelopeWire {
  if (envelope.revisions.length !== dates.length) throw new Error(`the story dates ${dates.length} revisions, and the fixture lists ${envelope.revisions.length}`);
  const moved = new Map(envelope.revisions.map((r, i) => [r.effective, dates[i]]));
  const move = <T extends EntryWire>(entry: T): T => ({ ...entry, content: { ...entry.content, effective: moved.get(entry.content.effective) ?? entry.content.effective }, effective: moved.get(entry.effective) ?? entry.effective });
  return { ...move(envelope), revisions: envelope.revisions.map(move) };
}

const settingsBody = FIXTURES.settings.body as { api_prices: EnvelopeWire; subscription_costs: EnvelopeWire };
/** The shipped price table as the settings route answers it, dated into the story's past. */
export const API_PRICES_WIRE = redated(settingsBody.api_prices, STORY_PRICE_DATES);
/** The shipped cost model as the settings route answers it, dated into the story's past. */
export const SUBSCRIPTION_COSTS_WIRE = redated(settingsBody.subscription_costs, STORY_COST_DATES);
const PRICE_REVISIONS = decodePricing(API_PRICES_WIRE).revisions;

/** The row the entry in force at `at` prices `model` at, a dated id at its undated row as the ledger reads it. Null for
 *  a model the entry lacks, which the ledger prices at nothing. */
export function priceFor(model: string, at: string | number): ModelPrice | null {
  const models = revisionAt(PRICE_REVISIONS, at)?.models ?? [];
  return models.find((p) => p.model === model) ?? models.find((p) => p.model === undatedModel(model)) ?? null;
}

/** A request's prompt as the ledger sizes it and a pin's last turn counts it: its uncached input, cache reads and cache
 *  writes together. */
export const promptOf = (u: Pick<Usage, 'cacheRead' | 'cacheWrite1h' | 'cacheWrite5m' | 'input'>): number => u.input + u.cacheRead + u.cacheWrite5m + u.cacheWrite1h;

/**
 * The cells an answer prices at, as the ledger selects them: the fast cells for a fast answer on a row that has
 * them, else the long-context cells when the prompt (uncached input, cache reads and cache writes together) is
 * over the threshold, else the standard cells. Null for a model the entry in force lacks.
 */
export function cellsFor(model: string, usage: Pick<Usage, 'cacheRead' | 'cacheWrite1h' | 'cacheWrite5m' | 'input'>, fast: boolean, at: string | number): PriceCells | null {
  const row = priceFor(model, at);
  if (row === null) return null;
  if (fast && row.fast) return row.fast;
  return row.longContext && promptOf(usage) > row.longContext.over ? row.longContext : row;
}
