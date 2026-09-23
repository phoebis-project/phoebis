/**
 * A count per name the router passes through as sent, such as an error code. The map has no prototype and is read
 * by own key only, so a code named `__proto__` or `constructor` is a key like any other: it neither reads an
 * inherited member as its count nor, assigned, replaces the map's prototype.
 */
export const emptyCounts = (): Record<string, number> => Object.create(null) as Record<string, number>;

export const countOf = (counts: Record<string, number>, name: string): number => (Object.prototype.hasOwnProperty.call(counts, name) ? counts[name] : 0);
