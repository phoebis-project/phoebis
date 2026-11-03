import { CONDITION_OPERATOR_VALUES, CONDITION_SUBJECT_VALUES, MESSAGE_SELECTOR_VALUES, decodeGuardrailPreset } from '@/api/decode';
import type { Condition, Guardrail, GuardrailAttachment, GuardrailPreset, RequestLog, TestVerdict } from '@/api/types';
import { MAX_CONDITIONS, MAX_PATTERN_CHARS, parsesAsNumber, pathRefusal, subjectOf, takesValue } from '@/lib/guardrails';
import { FIXTURES } from './generated/fixtures';
import { daysAgo, hoursAgo } from './time';

/** The presets the binary ships, in the route's own words, as its presets route answers them. */
export const PRESETS: GuardrailPreset[] = ((FIXTURES.guardrail_presets.body as { presets: unknown[] }).presets ?? []).map(decodeGuardrailPreset);

const SUMMARY_PRESET = PRESETS.find((p) => p.id === 'subagent-status-summary');

const condition = (c: Pick<Condition, 'operator' | 'subject'> & Partial<Condition>): Condition => ({ name: undefined, path: undefined, selector: undefined, value: undefined, ...c });

/**
 * The story's two guardrails: the subagent summary fork's preset observed on the pool `all`, and a guardrail of the
 * operator's own enforced on the key `desktop`, which refuses an auxiliary request that asks for a long answer. The
 * mock stamps each revision as it loads.
 */
export const GUARDRAILS: Guardrail[] = [
  {
    action: 'block',
    conditions: [condition({ operator: 'equals', subject: 'request_class', value: 'auxiliary' }), condition({ operator: 'gt', path: 'max_tokens', subject: 'body', value: '4096' })],
    createdAt: daysAgo(2),
    description: 'Auxiliary requests that ask for more than 4096 output tokens.',
    enabled: true,
    id: 'large-auxiliary',
    keys: [{ id: 'desktop', label: 'desktop', position: 0 }],
    mode: 'enforce',
    name: 'Large auxiliary requests',
    pools: [],
    preset: null,
    revision: 0,
    updatedAt: hoursAgo(30),
  },
  {
    action: 'block',
    conditions: (SUMMARY_PRESET?.conditions ?? []).map((c) => ({ ...c })),
    createdAt: daysAgo(3),
    description: 'The status line fork the client sends for each running subagent.',
    enabled: true,
    id: 'subagent-status-summary',
    keys: [],
    mode: 'observe',
    name: SUMMARY_PRESET?.name ?? 'Subagent status summary',
    pools: [{ id: 'all', label: 'all', position: 0 }],
    // Stamped at the version its conditions are copied from, the presets route's current entry, so the stamp, the
    // conditions and `customized` read as the mock's own writes would read them whichever version the binary ships.
    preset: { currentVersion: SUMMARY_PRESET?.version ?? 1, customized: SUMMARY_PRESET === undefined, id: 'subagent-status-summary', version: SUMMARY_PRESET?.version ?? 1 },
    revision: 0,
    updatedAt: daysAgo(3),
  },
];

/** Whether two lists of conditions say the same, the route's test for a stamp it reads as customized. */
export const sameConditions = (a: Condition[], b: Condition[]): boolean =>
  a.length === b.length && a.every((c, i) => (['name', 'operator', 'path', 'selector', 'subject', 'value'] as const).every((k) => c[k] === b[i][k]));

/** A header name as `HeaderMap` takes it: 1 to 128 bytes of the token charset. */
const isHeaderToken = (name: string): boolean => /^[!#$%&'*+\-.^_`|~0-9A-Za-z]{1,128}$/.test(name);

/** The route's reason for one condition, the text after `conditions[{i}]: `, in the order its check reads the members. */
export function conditionReason(c: Condition): string | null {
  if (!CONDITION_SUBJECT_VALUES.some((s) => s === c.subject)) return `unknown subject ${c.subject}`;
  if (!CONDITION_OPERATOR_VALUES.some((o) => o === c.operator)) return `unknown operator ${c.operator}`;
  if (c.selector !== undefined && !MESSAGE_SELECTOR_VALUES.some((s) => s === c.selector)) return `unknown selector ${c.selector}`;
  const cell = subjectOf(c.subject).cell;
  if (c.name !== undefined && cell !== 'header') return `name does not apply to ${c.subject}`;
  if (c.name !== undefined && !isHeaderToken(c.name.toLowerCase())) return 'name is not a header token';
  if (c.name === undefined && cell === 'header') return 'name is required';
  if (c.path !== undefined && cell !== 'path') return `path does not apply to ${c.subject}`;
  // The console words the grammar's sentence with "the" ahead, the route without.
  const path = c.path === undefined ? null : pathRefusal(c.path);
  if (path !== null) return path.replace(/^the /, '');
  if (c.path === undefined && cell === 'path') return 'path is required';
  if (c.selector !== undefined && cell !== 'selector') return `selector does not apply to ${c.subject}`;
  if (c.selector === undefined && cell === 'selector') return 'selector is required';
  if (!subjectOf(c.subject).operators.includes(c.operator)) return `operator ${c.operator} does not apply to ${c.subject}`;
  if (!takesValue(c.operator)) return c.value === undefined ? null : `value does not apply to ${c.operator}`;
  if (c.value === undefined) return 'value is required';
  if ((c.operator === 'gt' || c.operator === 'lt') && !parsesAsNumber(c.value)) return 'value is not a number';
  if (c.operator === 'matches' && [...c.value].length > MAX_PATTERN_CHARS) return `pattern is longer than ${MAX_PATTERN_CHARS} characters`;
  if (c.subject === 'kind' && c.value !== 'inference' && c.value !== 'count_tokens') return 'kind is inference or count_tokens';
  return null;
}

/** The route's refusal of a whole list, the count without an index and a condition with its own. */
export function conditionsRefusal(conditions: Condition[] | undefined): string | null {
  const list = conditions ?? [];
  if (list.length === 0 || list.length > MAX_CONDITIONS) return `conditions holds 1 to ${MAX_CONDITIONS} entries`;
  for (const [i, c] of list.entries()) {
    const reason = conditionReason(c);
    if (reason !== null) return `conditions[${i}]: ${reason}`;
  }
  return null;
}

/**
 * The route's words, written out apart from the form's so a form that drifts from the route is refused here rather than
 * agreed with.
 */
export const GUARDRAIL_RESERVED_IDS: readonly string[] = ['presets', 'test'];
export const GUARDRAIL_STALE = 'the guardrail changed while it was being edited, so reload and try again';
export const GUARDRAIL_TAKEN = 'a guardrail with this id exists';

/** The guardrails one key holds at most through its pool and itself. */
const GUARDRAIL_BOUND = 64;

export const withAttachments = (g: Guardrail, kind: 'keys' | 'pools', list: GuardrailAttachment[]): Guardrail => (kind === 'keys' ? { ...g, keys: list } : { ...g, pools: list });

/**
 * The store's bound over the lists a write would leave: the pool's own list first, then each key's union with it, the
 * first key over the bound named.
 */
export function boundRefusal(poolId: string, poolList: string[], keys: [string, string[]][]): string | null {
  const pool = new Set(poolList);
  if (pool.size > GUARDRAIL_BOUND) return `pool ${poolId} would hold more than ${GUARDRAIL_BOUND} guardrails`;
  for (const [keyId, own] of keys) if (new Set([...own, ...pool]).size > GUARDRAIL_BOUND) return `key ${keyId} would hold more than ${GUARDRAIL_BOUND} guardrails`;
  return null;
}

/** The first id a list names twice, which the routes refuse before anything is written. */
export function repeatedId(ids: string[] | undefined): string | null {
  const seen = new Set<string>();
  for (const id of ids ?? []) {
    if (seen.has(id)) return id;
    seen.add(id);
  }
  return null;
}

/** The route's evaluation order: the cheap subjects first, the body walk, then the message regex. */
const RANK: Record<Condition['subject'], number> = { agent_type: 2, body: 7, client: 3, header: 6, kind: 0, message: 8, model: 4, request_class: 1, user_agent: 5 };

/** The headers a ledger row keeps, which the dry run reads. Every other header is not on the row. */
const KEPT_HEADERS: Record<string, (r: RequestLog) => string | null> = {
  'user-agent': (r) => r.userAgent,
  'x-claude-code-agent-type': (r) => r.agentType,
  'x-claude-code-compaction': (r) => r.compaction,
  'x-claude-code-context-compacted': (r) => r.contextCompacted,
  'x-claude-code-request-class': (r) => r.requestClass,
};

/** The fact each subject reads off the captured row itself. The record keeps the table total over the subjects. */
const FACT_OF: Record<Exclude<Condition['subject'], 'body' | 'header' | 'message'>, (r: RequestLog) => string | null> = {
  agent_type: (r) => r.agentType,
  client: (r) => r.client ?? 'other',
  kind: (r) => r.kind,
  model: (r) => r.canonicalModel,
  request_class: (r) => r.requestClass,
  user_agent: (r) => r.userAgent,
};

/** A pattern searched unanchored, as the route searches. The dialect's leading `(?i)` reads as the flag it sets. */
function searches(pattern: string, text: string): boolean {
  const insensitive = pattern.startsWith('(?i)');
  try {
    return new RegExp(insensitive ? pattern.slice(4) : pattern, insensitive ? 'iu' : 'u').test(text);
  } catch {
    return false;
  }
}

/** One fact against one operator: whether it holds, absent reading null. */
function holds(c: Condition, fact: string | null): boolean {
  switch (c.operator) {
    case 'absent':
      return fact === null;
    case 'present':
      return fact !== null;
    case 'equals':
      return fact !== null && fact === c.value;
    case 'matches':
      return fact !== null && searches(c.value ?? '', fact);
    default:
      return false;
  }
}

/**
 * The dry run's verdict on one captured row. The facts the row keeps evaluate exactly, a header the row does not keep
 * cannot be answered, and a body or message condition reads the story's own word, since the mock ships no body
 * evaluator: `matched` names a row the story's guardrail rows hold. The walk runs in the route's cost order and names
 * a condition by its written index.
 */
export function dryRunVerdict(conditions: Condition[], r: RequestLog, matched: boolean): { condition: number | null; verdict: TestVerdict } {
  const order = conditions.map((c, i) => ({ c, i })).sort((a, b) => RANK[a.c.subject] - RANK[b.c.subject]);
  let unknown: number | null = null;
  for (const { c, i } of order) {
    const read = c.subject === 'header' ? KEPT_HEADERS[(c.name ?? '').toLowerCase()] : undefined;
    const ok = c.subject === 'body' || c.subject === 'message' ? matched : c.subject === 'header' ? (read === undefined ? null : holds(c, read(r))) : holds(c, FACT_OF[c.subject](r));
    if (ok === false) return { condition: i, verdict: 'unmatched' };
    if (ok === null && unknown === null) unknown = i;
  }
  return unknown === null ? { condition: null, verdict: 'matched' } : { condition: unknown, verdict: 'indeterminate' };
}
