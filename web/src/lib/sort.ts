import type { SortDir } from './address';

/** A row's figure under a key: a number, a word compared by locale, or null for a row that holds none. */
export type SortFigure = number | string | null;

/** Two figures in `dir`: a null after every figure under either direction, two nulls equal. */
export function compareFigures(a: SortFigure, b: SortFigure, dir: SortDir): number {
  if (a === null || b === null) return a === b ? 0 : a === null ? 1 : -1;
  const order = typeof a === 'number' && typeof b === 'number' ? a - b : String(a).localeCompare(String(b));
  return dir === 'desc' ? -order : order;
}

/**
 * The rows under a key: a row whose figure is null after every row with one under either direction, the rows with a
 * figure flipped by the direction, and a tie by `nameOf` ascending under both directions.
 */
export function orderRows<T>(rows: readonly T[], keyOf: (row: T) => SortFigure, dir: SortDir, nameOf: (row: T) => string): T[] {
  return rows
    .map((row) => ({ key: keyOf(row), name: nameOf(row), row }))
    .sort((x, y) => compareFigures(x.key, y.key, dir) || x.name.localeCompare(y.name))
    .map((entry) => entry.row);
}
