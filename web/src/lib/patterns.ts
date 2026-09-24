import type { ModelRule, ModelRulesMode } from '@/api/types';

/** One entry of a processed list, in the order it applies, with the level it came from. */
export interface LevelledRule extends ModelRule {
  level: 'key' | 'pool';
}

/** The longest pattern the matcher walks and the longest name it walks against; either past its bound answers
 * no match, mirroring the Rust matcher's own bound. */
const MAX_PATTERN_CHARS = 256;
const MAX_NAME_CHARS = 256;

type Token = { char: string; kind: 'literal' } | { kind: 'anyOne' } | { kind: 'anyPath' } | { kind: 'anyRun' };

/** ASCII-only fold, matching the Rust matcher's `eq_ignore_ascii_case`: a non-ASCII letter keeps its case. */
const foldAscii = (c: string): string => (c >= 'A' && c <= 'Z' ? String.fromCharCode(c.charCodeAt(0) + 32) : c);

function tokenize(pattern: string): Token[] {
  const chars = [...pattern];
  const tokens: Token[] = [];
  let i = 0;
  while (i < chars.length) {
    const c = chars[i];
    if (c === '\\') {
      tokens.push({ char: chars[i + 1] ?? '\\', kind: 'literal' });
      i += 2;
    } else if (c === '*' && chars[i + 1] === '*') {
      tokens.push({ kind: 'anyPath' });
      i += 2;
    } else if (c === '*') {
      tokens.push({ kind: 'anyRun' });
      i += 1;
    } else if (c === '?') {
      tokens.push({ kind: 'anyOne' });
      i += 1;
    } else {
      tokens.push({ char: c, kind: 'literal' });
      i += 1;
    }
  }
  return tokens;
}

/** The refusal text the route answers for a pattern, or null: empty, longer than 256 characters, an unquoted
 *  trailing backslash, three stars in a row. The same four checks, the same words. */
export function checkPattern(pattern: string): string | null {
  const chars = [...pattern];
  if (chars.length === 0) return 'pattern is empty';
  if (chars.length > MAX_PATTERN_CHARS) return 'pattern is longer than 256 characters';
  // An odd run of trailing backslashes leaves the last one with nothing to quote.
  let trailing = 0;
  while (trailing < chars.length && chars[chars.length - 1 - trailing] === '\\') trailing += 1;
  if (trailing % 2 === 1) return 'pattern ends in an unquoted backslash';
  let run = 0;
  for (let i = 0; i < chars.length; i += 1) {
    if (chars[i] === '\\') {
      i += 1;
      run = 0;
    } else if (chars[i] === '*') {
      run += 1;
      if (run >= 3) return 'pattern holds three stars in a row';
    } else {
      run = 0;
    }
  }
  return null;
}

/**
 * The path-style grammar of the model rules over a folded name: `*` stops at a slash, `**` crosses it, `?`
 * takes one character other than a slash, a backslash quotes the next; ASCII case folds. The table walk of the
 * Rust matcher, tokens then a boolean table.
 */
export function matchesPattern(pattern: string, name: string): boolean {
  const tokens = tokenize(pattern);
  const chars = [...name];
  if (tokens.length > MAX_PATTERN_CHARS || chars.length > MAX_NAME_CHARS) return false;
  // table[t][c]: whether tokens[t..] matches chars[c..].
  const table: boolean[][] = Array.from({ length: tokens.length + 1 }, () => new Array<boolean>(chars.length + 1).fill(false));
  table[tokens.length][chars.length] = true;
  for (let t = tokens.length - 1; t >= 0; t -= 1) {
    for (let c = chars.length; c >= 0; c -= 1) {
      const next = chars[c] as string | undefined;
      const token = tokens[t];
      if (token.kind === 'anyPath') {
        table[t][c] = table[t + 1][c] || (next !== undefined && table[t][c + 1]);
      } else if (token.kind === 'anyRun') {
        table[t][c] = table[t + 1][c] || (next !== undefined && next !== '/' && table[t][c + 1]);
      } else if (token.kind === 'anyOne') {
        table[t][c] = next !== undefined && next !== '/' && table[t + 1][c + 1];
      } else {
        table[t][c] = next !== undefined && foldAscii(next) === foldAscii(token.char) && table[t + 1][c + 1];
      }
    }
  }
  return table[0][0];
}

/**
 * The ordered list a key is judged by: its own entries under `key`, then the pool's under `pool` when the mode
 * is `extend`, nothing more under `replace`.
 */
export function processRules(keyRules: ModelRule[], mode: ModelRulesMode, poolRules: ModelRule[]): LevelledRule[] {
  const key: LevelledRule[] = keyRules.map((r) => ({ not: r.not, pattern: r.pattern, level: 'key' }));
  if (mode === 'replace') return key;
  return [...key, ...poolRules.map((r): LevelledRule => ({ not: r.not, pattern: r.pattern, level: 'pool' }))];
}

/**
 * Top to bottom, first match wins: a matching `not` entry denies, a matching plain entry allows, and a name no
 * entry matches is allowed.
 */
export function allowsModel(processed: LevelledRule[], name: string): boolean {
  for (const entry of processed) {
    if (matchesPattern(entry.pattern, name)) return !entry.not;
  }
  return true;
}
