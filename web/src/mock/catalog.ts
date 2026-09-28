import type { CatalogModel, ModelPrice } from '@/api/types';

// Stored out of id order, so a page that renders it verbatim rather than sorting it fails the smoke check
// that reads the catalog popup as sorted by id.
export const CATALOG: CatalogModel[] = [
  { id: 'claude-fable-5-1', displayName: 'Claude Fable 5.1', createdAt: '2026-08-01', maxInputTokens: 1_000_000, maxTokens: 128_000 },
  { id: 'claude-sonnet-5', displayName: 'Claude Sonnet 5', createdAt: '2026-05-01', maxInputTokens: 1_000_000, maxTokens: 128_000 },
  { id: 'claude-haiku-4-5-20251001', displayName: 'Claude Haiku 4.5', createdAt: '2025-10-01', maxInputTokens: 200_000, maxTokens: 64_000 },
  { id: 'claude-opus-5', displayName: 'Claude Opus 5', createdAt: '2026-06-01', maxInputTokens: 1_000_000, maxTokens: 128_000 },
];

/**
 * The shipped Anthropic price table, USD per million tokens, grouped by model
 * class with the newest generation of each first. This is what the cost
 * reference shows, so it holds Anthropic models only.
 */
export const PRICES: ModelPrice[] = [
  { model: 'claude-fable-5-1', inputPerM: 10, outputPerM: 50, cacheReadPerM: 0.25, cacheWrite5mPerM: 12.5, cacheWrite1hPerM: 20 },
  { model: 'claude-fable-5', inputPerM: 10, outputPerM: 50, cacheReadPerM: 1, cacheWrite5mPerM: 12.5, cacheWrite1hPerM: 20 },
  { model: 'claude-opus-5', inputPerM: 5, outputPerM: 25, cacheReadPerM: 0.5, cacheWrite5mPerM: 6.25, cacheWrite1hPerM: 10 },
  { model: 'claude-opus-4-8', inputPerM: 5, outputPerM: 25, cacheReadPerM: 0.5, cacheWrite5mPerM: 6.25, cacheWrite1hPerM: 10 },
  { model: 'claude-opus-4-7', inputPerM: 5, outputPerM: 25, cacheReadPerM: 0.5, cacheWrite5mPerM: 6.25, cacheWrite1hPerM: 10 },
  { model: 'claude-sonnet-5', inputPerM: 2, outputPerM: 10, cacheReadPerM: 0.2, cacheWrite5mPerM: 2.5, cacheWrite1hPerM: 4 },
  { model: 'claude-sonnet-4-6', inputPerM: 3, outputPerM: 15, cacheReadPerM: 0.3, cacheWrite5mPerM: 3.75, cacheWrite1hPerM: 6 },
  { model: 'claude-sonnet-4-5', inputPerM: 3, outputPerM: 15, cacheReadPerM: 0.3, cacheWrite5mPerM: 3.75, cacheWrite1hPerM: 6 },
  { model: 'claude-haiku-4-5-20251001', inputPerM: 1, outputPerM: 5, cacheReadPerM: 0.1, cacheWrite5mPerM: 1.25, cacheWrite1hPerM: 2 },
];

const priceIndex = new Map(PRICES.map((p) => [p.model, p]));
const FALLBACK = priceIndex.get('claude-sonnet-5')!;

export function priceFor(model: string): ModelPrice {
  return priceIndex.get(model) ?? FALLBACK;
}
