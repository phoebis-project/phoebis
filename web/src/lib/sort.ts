import type { SortDir } from './address';

/** A row's figure under a key: a number, a word compared by locale, or null for a row that holds none. */
export type SortFigure = number | string | null;

/**
 * The rows under a key: a row whose figure is null after every row with one under either direction, the rows with a
 * figure flipped by the direction, and a tie by `nameOf` ascending under both directions.
 */
export function orderRows<T>(rows: readonly T[], keyOf: (row: T) => SortFigure, dir: SortDir, nameOf: (row: T) => string): T[] {
  const compare = (a: SortFigure, b: SortFigure): number => (typeof a === 'number' && typeof b === 'number' ? a - b : String(a).localeCompare(String(b)));
  return [...rows].sort((x, y) => {
    const [a, b] = [keyOf(x), keyOf(y)];
    if (a === null || b === null) return a === b ? nameOf(x).localeCompare(nameOf(y)) : a === null ? 1 : -1;
    const order = compare(a, b);
    return (dir === 'desc' ? -order : order) || nameOf(x).localeCompare(nameOf(y));
  });
}
