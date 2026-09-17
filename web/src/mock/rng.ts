// mulberry32: small, fast, and deterministic for a given seed so the fixture
// renders the same shapes on every reload.
export interface Rng {
  chance(probability: number): boolean;
  float(min: number, max: number): number;
  int(min: number, max: number): number;
  next(): number;
  pick<T>(items: readonly T[]): T;
  uuid(): string;
  weighted<T>(items: readonly { value: T; weight: number }[]): T;
}

export function createRng(seed: number): Rng {
  let state = seed >>> 0;
  const next = () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const hex = (n: number) => {
    let out = '';
    for (let i = 0; i < n; i += 1) out += Math.floor(next() * 16).toString(16);
    return out;
  };
  return {
    chance: (p) => next() < p,
    float: (min, max) => min + next() * (max - min),
    int: (min, max) => Math.floor(min + next() * (max - min + 1)),
    next,
    pick: (items) => items[Math.floor(next() * items.length)],
    uuid: () => `${hex(8)}-${hex(4)}-4${hex(3)}-${(8 + Math.floor(next() * 4)).toString(16)}${hex(3)}-${hex(12)}`,
    weighted: (items) => {
      const total = items.reduce((sum, item) => sum + item.weight, 0);
      let roll = next() * total;
      for (const item of items) {
        roll -= item.weight;
        if (roll <= 0) return item.value;
      }
      return items[items.length - 1].value;
    },
  };
}
