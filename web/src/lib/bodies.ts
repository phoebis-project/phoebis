/**
 * One frame of a server-sent event stream: its `event:` name, its `data:` lines joined by newline, and the data
 * parsed as JSON where it parses.
 */
export interface SseFrame {
  data: string;
  event: string | null;
  json: unknown | undefined;
}

/** The field prefixes a stream's lines may carry, a comment's lone colon among them. */
const SSE_LINE = /^(?:event:|data:|id:|retry:|:)/;

/**
 * The frames of a stored stream capture, or null for text that is not one or holds no frame: a capture is SSE text
 * when every non-empty line of it starts with `event:`, `data:`, `id:`, `retry:` or `:`, frames parting at a blank
 * line, the lines ending LF, CRLF or bare CR alike. A capture cut mid-frame keeps its last frame as the text it has:
 * the ledger's cut falls anywhere, so a last line without a line break after it is read as data whatever its shape,
 * once a field line before it has shown the text to be a stream.
 */
export function sseFrames(text: string): SseFrame[] | null {
  // The relay keeps the captured bytes as they came, and its parser takes all three endings.
  const lines = text.split(/\r\n|\r|\n/);
  const frames: SseFrame[] = [];
  let event: string | null = null;
  let data: string[] = [];
  let sawField = false;
  const close = () => {
    if (event !== null || data.length > 0) {
      const joined = data.join('\n');
      frames.push({ data: joined, event, json: parsed(joined) });
    }
    event = null;
    data = [];
  };
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (line === '') {
      close();
      continue;
    }
    const last = i === lines.length - 1;
    if (!SSE_LINE.test(line)) {
      if (!last || !sawField) return null;
      data.push(line);
      continue;
    }
    sawField = true;
    if (line.startsWith('event:')) event = afterPrefix(line, 6);
    else if (line.startsWith('data:')) data.push(afterPrefix(line, 5));
  }
  close();
  return frames.length === 0 ? null : frames;
}

/** The field's value: the prefix gone and one space after it, as the SSE grammar strips it. */
const afterPrefix = (line: string, at: number) => (line[at] === ' ' ? line.slice(at + 1) : line.slice(at));

const parsed = (data: string): unknown | undefined => {
  try {
    return JSON.parse(data) as unknown;
  } catch {
    return undefined;
  }
};

/**
 * The text a body block shows: a JSON value indented, a stream as its events (`event: name` over its data, indented
 * where it is JSON) and any other string as itself.
 */
export function bodyText(value: unknown): string {
  if (typeof value === 'string') {
    const frames = sseFrames(value);
    return frames === null ? value : frames.map(frameText).join('\n\n');
  }
  return JSON.stringify(value, null, 2);
}

const frameText = (frame: SseFrame): string => {
  const lines: string[] = [];
  if (frame.event !== null) lines.push(`event: ${frame.event}`);
  const data = frame.json === undefined ? frame.data : JSON.stringify(frame.json, null, 2);
  if (data !== '') lines.push(data);
  return lines.join('\n');
};

/**
 * The text's lines, split on `\n` once. A trailing newline yields no extra empty row, and an empty text one empty row.
 */
export function bodyLines(text: string): string[] {
  const lines = text.split('\n');
  if (text !== '' && text.endsWith('\n')) lines.pop();
  return lines;
}

const NOT_ASCII = /[\u0080-\uffff]/;

/**
 * The text lowered one code point at a time, each to the first code point of its lower case, and kept as it is where
 * that point is not as long, so an offset found in it is an offset in the text as drawn. A whole-string lowering moves
 * every offset after a character whose lower case is longer (U+0130 lowers to two code points), and a lowering by code
 * unit leaves a capital outside the basic plane (Deseret, Osage, Adlam) unmatched by its lower case.
 */
function lowered(text: string): string {
  if (!NOT_ASCII.test(text)) return text.toLowerCase();
  let out = '';
  for (const char of text) {
    const [first] = char.toLowerCase();
    out += first.length === char.length ? first : char;
  }
  return out;
}

/**
 * The indexes of the lines that hold `term` as case-insensitive plain text, in order and one per line. An empty term
 * has none.
 */
export function findHits(lines: readonly string[], term: string): number[] {
  if (term === '') return [];
  const needle = lowered(term);
  const hits: number[] = [];
  lines.forEach((line, i) => {
    if (lowered(line).includes(needle)) hits.push(i);
  });
  return hits;
}

/**
 * The offsets at which `term` starts in `line` as case-insensitive plain text, left to right and none overlapping the
 * one before. An empty term has none.
 */
export function findOffsets(line: string, term: string): number[] {
  if (term === '') return [];
  const needle = lowered(term);
  const text = lowered(line);
  const starts: number[] = [];
  for (let at = text.indexOf(needle); at >= 0; at = text.indexOf(needle, at + needle.length)) starts.push(at);
  return starts;
}

export type TokenClass = 'tok-event' | 'tok-key' | 'tok-literal' | 'tok-number' | 'tok-punct' | 'tok-string';

export interface Token {
  cls: TokenClass | null;
  text: string;
}

const NUMBER = /-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/y;
const LITERAL = /(?:true|false|null)\b/y;
const WORD = /\w/;
const PUNCT = new Set(['{', '}', '[', ']', ':', ',']);

/**
 * The text split into spans by what each run is: an object key, a string, a number, `true`, `false` or `null`,
 * punctuation, a line that starts `event:`, and the rest unclassed. The tokens concatenate back to the text.
 */
export function highlightJson(text: string): Token[] {
  const tokens: Token[] = [];
  let plain = '';
  const flush = () => {
    if (plain !== '') tokens.push({ cls: null, text: plain });
    plain = '';
  };
  const push = (cls: TokenClass, run: string) => {
    flush();
    tokens.push({ cls, text: run });
  };
  const n = text.length;
  let i = 0;
  let lineStart = true;
  while (i < n) {
    if (lineStart && text.startsWith('event:', i)) {
      const end = text.indexOf('\n', i);
      const stop = end < 0 ? n : end;
      push('tok-event', text.slice(i, stop));
      i = stop;
      lineStart = false;
      continue;
    }
    lineStart = false;
    const c = text[i];
    if (c === '"') {
      // A backslash skips the next character, and a string the cut left open runs to the text's end.
      let j = i + 1;
      while (j < n && text[j] !== '"') j += text[j] === '\\' ? 2 : 1;
      const end = Math.min(j + 1, n);
      let k = end;
      while (k < n && (text[k] === ' ' || text[k] === '\t' || text[k] === '\n' || text[k] === '\r')) k += 1;
      push(text[k] === ':' ? 'tok-key' : 'tok-string', text.slice(i, end));
      i = end;
      continue;
    }
    if (c === '-' || (c >= '0' && c <= '9')) {
      NUMBER.lastIndex = i;
      const m = NUMBER.exec(text);
      if (m !== null) {
        push('tok-number', m[0]);
        i += m[0].length;
        continue;
      }
    }
    if ((c === 't' || c === 'f' || c === 'n') && (i === 0 || !WORD.test(text[i - 1]))) {
      LITERAL.lastIndex = i;
      const m = LITERAL.exec(text);
      if (m !== null) {
        push('tok-literal', m[0]);
        i += m[0].length;
        continue;
      }
    }
    if (PUNCT.has(c)) {
      push('tok-punct', c);
      i += 1;
      continue;
    }
    plain += c;
    if (c === '\n') lineStart = true;
    i += 1;
  }
  flush();
  return tokens;
}
