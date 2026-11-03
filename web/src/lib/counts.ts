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

export const countOf = (counts: Record<string, number>, name: string): number => lookup(counts, name) ?? 0;
