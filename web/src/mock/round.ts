/** `x` to `places` decimals, as the story writes its figures. A power of ten up to 10^5 is exact, so a figure rounds as
 *  it does over the written-out multiplier. */
export const round = (x: number, places: number): number => Math.round(x * 10 ** places) / 10 ** places;
