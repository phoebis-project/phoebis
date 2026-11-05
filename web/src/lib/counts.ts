/**
 * A count per name the router passes through as sent, such as an error code. The map has no prototype and is read
 * by own key only, so a code named `__proto__` or `constructor` is a key like any other: it neither reads an
 * inherited member as its count nor, assigned, replaces the map's prototype.
 */
export const emptyCounts = (): Record<string, number> => Object.create(null) as Record<string, number>;

/**
 * Whether a map holds the name as its own key. The names are the router's data or the address's, and one could name
 * a member every object inherits.
 */
export const has = (map: Readonly<Record<string, unknown>>, key: string): boolean => Object.hasOwn(map, key);

/** The map's value under its own key, undefined for a name it does not hold, an inherited member's name among them. */
export const lookup = <T>(map: Readonly<Record<string, T>>, key: string): T | undefined => (Object.hasOwn(map, key) ? map[key] : undefined);

/** The items under their keys, the keys in first-insertion order and each list in the items' order. An item whose key
 *  is null is left out. */
export function groupInto<K, T>(items: Iterable<T>, keyOf: (item: T) => K | null): Map<K, T[]> {
  const groups = new Map<K, T[]>();
  for (const item of items) {
    const key = keyOf(item);
    if (key === null) continue;
    const list = groups.get(key);
    if (list) list.push(item);
    else groups.set(key, [item]);
  }
  return groups;
}

export const countOf = (counts: Record<string, number>, name: string): number => lookup(counts, name) ?? 0;
