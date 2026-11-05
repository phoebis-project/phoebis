import { DAY, HOUR } from './time';

/**
 * The bucket width the analytics routes answer a range in (`bucket_seconds` in the ledger's query module), so a
 * session view folded from request rows draws the same buckets as the route view beside it.
 */
export const bucketSizeFor = (spanMs: number): number => (spanMs <= 48 * HOUR ? HOUR : spanMs <= 30 * DAY ? 6 * HOUR : DAY);
