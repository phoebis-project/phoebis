import type { Condition, ConditionOperator, ConditionSubject, Guardrail, GuardrailPresetState, GuardrailTestResult, MessageSelector, RequestLog } from '@/api/types';
import type { Option } from '@/components/ui/OptionList';

/** The route's bounds: the conditions one guardrail holds and the characters of one pattern. */
export const MAX_CONDITIONS = 16;
export const MAX_PATTERN_CHARS = 2048;
/** The name's and the description's bound, in bytes as the route counts them. */
const TEXT_MAX_BYTES = 256;
const PATH_MAX_CHARS = 256;
const PATH_MAX_SEGMENTS = 32;
/** The ids of the routes beside `GET /admin/guardrails/{id}`, which no guardrail can take. */
const RESERVED_IDS: readonly string[] = ['presets', 'test'];

/**
 * A subject's words: the extra cell it takes (a header name, a body path, a message selector or none) and the operators
 * it offers in the route's order, the first of which a subject change resets the row to.
 */
export interface SubjectWords {
  cell: 'header' | 'none' | 'path' | 'selector';
  label: string;
  operators: ConditionOperator[];
  value: ConditionSubject;
}

export const SUBJECTS: SubjectWords[] = [
  { cell: 'none', label: 'Agent type', operators: ['absent', 'equals', 'matches'], value: 'agent_type' },
  { cell: 'path', label: 'Body', operators: ['absent', 'equals', 'gt', 'lt', 'matches', 'present'], value: 'body' },
  { cell: 'none', label: 'Client', operators: ['equals', 'matches'], value: 'client' },
  { cell: 'header', label: 'Header', operators: ['absent', 'equals', 'matches', 'present'], value: 'header' },
  { cell: 'none', label: 'Kind', operators: ['equals'], value: 'kind' },
  // The regex runs over the selected entries' text blocks, and no other test reads a message.
  { cell: 'selector', label: 'Message', operators: ['matches'], value: 'message' },
  { cell: 'none', label: 'Model', operators: ['equals', 'matches'], value: 'model' },
  { cell: 'none', label: 'Request class', operators: ['absent', 'equals', 'matches'], value: 'request_class' },
  { cell: 'none', label: 'User agent', operators: ['absent', 'equals', 'matches'], value: 'user_agent' },
];

export const subjectOf = (value: ConditionSubject): SubjectWords => SUBJECTS.find((s) => s.value === value) ?? SUBJECTS[0];

export const OPERATOR_LABEL: Record<ConditionOperator, string> = {
  absent: 'is absent',
  equals: 'equals',
  gt: 'is greater than',
  lt: 'is less than',
  matches: 'matches',
  present: 'is present',
};

export const SELECTOR_LABEL: Record<MessageSelector, string> = {
  any: 'any message',
  any_assistant: 'any assistant message',
  any_user: 'any user message',
  first_user: 'first user message',
  last: 'last message',
  last_assistant: 'last assistant message',
  last_user: 'last user message',
};

/** The two words a `kind` condition compares against, as the route spells them. */
export const KIND_VALUES: { label: string; value: string }[] = [
  { label: 'Count tokens', value: 'count_tokens' },
  { label: 'Inference', value: 'inference' },
];

/** Whether the operator compares against a value, so the row draws the value cell. */
export const takesValue = (operator: ConditionOperator): boolean => operator !== 'absent' && operator !== 'present';

/** The route counts characters as Unicode scalars, which a JavaScript string's length does not. */
const chars = (text: string): number => [...text].length;
const bytes = (text: string): number => new TextEncoder().encode(text).length;

/**
 * A decimal as Rust parses an `f64` and the route then requires finite: an optional sign, digits with an optional
 * fraction or a fraction alone, an optional exponent, and no space anywhere.
 */
export const parsesAsNumber = (text: string): boolean => /^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/.test(text) && Number.isFinite(Number(text));

/** One index as the grammar reads it: `*`, a decimal from zero, or `-` and a decimal of at least 1. */
const isIndex = (text: string): boolean => text === '*' || /^\d+$/.test(text) || (/^-\d+$/.test(text) && Number(text.slice(1)) >= 1);

/**
 * The route's body path grammar, its sentences each read with "the" ahead: the path's length, its first character, then
 * each segment in turn, and the segment count last, as the route reads them.
 */
export function pathRefusal(path: string): string | null {
  if (path === '') return 'the path is empty';
  if (chars(path) > PATH_MAX_CHARS) return `the path is longer than ${PATH_MAX_CHARS} characters`;
  if (path.startsWith('[')) return 'the path starts with an index';
  let segments = 0;
  let rest = path;
  let memberNext = true;
  for (;;) {
    if (memberNext) {
      const found = rest.search(/[.[\]]/);
      const end = found < 0 ? rest.length : found;
      if (end === 0) return `the path has an empty member at segment ${segments + 1}`;
      segments += 1;
      rest = rest.slice(end);
    }
    if (rest === '') break;
    const separator = rest[0];
    if (separator === '.') {
      rest = rest.slice(1);
      memberNext = true;
      continue;
    }
    if (separator !== '[') return `the path has a malformed index at segment ${segments + 1}`;
    const at = segments + 1;
    const inner = rest.slice(1);
    const close = inner.indexOf(']');
    const dot = inner.indexOf('.');
    if (close < 0 || (dot >= 0 && dot < close)) return `the path has an unclosed index at segment ${at}`;
    if (!isIndex(inner.slice(0, close))) return `the path has a malformed index at segment ${at}`;
    segments += 1;
    rest = inner.slice(close + 1);
    if (rest !== '' && !rest.startsWith('.') && !rest.startsWith('[')) return `the path has a malformed index at segment ${at}`;
    memberNext = false;
  }
  return segments > PATH_MAX_SEGMENTS ? `the path holds more than ${PATH_MAX_SEGMENTS} segments` : null;
}

/**
 * The structural rules a row breaks, named by its row number (`index + 1`), or null. The pattern is the route's to
 * compile, since the console has no engine of Rust's dialect, so the bound alone is checked here. An empty value on
 * `equals` is a value, the empty string, as the route reads it.
 */
export function conditionRefusal(condition: Condition, index: number): string | null {
  const row = `Condition ${index + 1}: `;
  const subject = subjectOf(condition.subject);
  if (subject.cell === 'header' && !condition.name) return `${row}needs a header name`;
  if (subject.cell === 'path') {
    if (!condition.path) return `${row}needs a path`;
    const path = pathRefusal(condition.path);
    if (path !== null) return `${row}${path}`;
  }
  if (subject.cell === 'selector' && condition.selector === undefined) return `${row}needs a selector`;
  if (!takesValue(condition.operator)) return null;
  const value = condition.value;
  if (value === undefined || (value === '' && (condition.operator === 'gt' || condition.operator === 'lt'))) return `${row}needs a value`;
  if ((condition.operator === 'gt' || condition.operator === 'lt') && !parsesAsNumber(value)) return `${row}${value} is not a number`;
  if (condition.operator === 'matches' && chars(value) > MAX_PATTERN_CHARS) return `${row}the pattern is longer than ${MAX_PATTERN_CHARS} characters`;
  return null;
}

/** A refusal the dialog holds the save on: its control's accessible name, its group and its sentence. */
export interface GuardrailRefusal {
  field: string;
  group: 'conditions' | 'identity';
  text: string;
}

/**
 * Every structural rule the form breaks, in the dialog's order: the name, the id a create derives from it, the two
 * bounds, the count, then each row. `id` is the id the guardrail has or a create derives, which an edit keeps, so an
 * edit reads no taken and no reserved id.
 */
export function guardrailRefusals(form: { conditions: Condition[]; description: string; id: string; name: string }, existingIds: ReadonlySet<string>, editing: boolean): GuardrailRefusal[] {
  const out: GuardrailRefusal[] = [];
  const identity = (field: string, text: string) => out.push({ field, group: 'identity', text });
  if (form.name === '') identity('Name', 'The guardrail needs a name.');
  else if (!editing && RESERVED_IDS.includes(form.id)) identity('Name', `The id ${form.id} is reserved.`);
  else if (!editing && existingIds.has(form.id)) identity('Name', `Another guardrail already has the id ${form.id}.`);
  if (bytes(form.name) > TEXT_MAX_BYTES) identity('Name', `The name is at most ${TEXT_MAX_BYTES} bytes.`);
  if (bytes(form.description) > TEXT_MAX_BYTES) identity('Description', `The description is at most ${TEXT_MAX_BYTES} bytes.`);
  if (form.conditions.length === 0 || form.conditions.length > MAX_CONDITIONS) out.push({ field: 'conditions', group: 'conditions', text: `A guardrail holds 1 to ${MAX_CONDITIONS} conditions.` });
  form.conditions.forEach((c, i) => {
    const text = conditionRefusal(c, i);
    if (text !== null) out.push({ field: `Condition ${i + 1}`, group: 'conditions', text });
  });
  return out;
}

/** The guardrail's name from the list, the id marked deleted where the list lacks it, the id alone until it is read. */
export function guardrailLabel(id: string, list: Pick<Guardrail, 'id' | 'name'>[] | null): string {
  if (list === null) return id;
  return list.find((g) => g.id === id)?.name ?? `${id} (deleted)`;
}

/**
 * The Guardrail facet's options: the registry's guardrails by name, then the ids the loaded rows and the filter carry
 * that the registry lacks, which matched before a delete, then `none` for a row no guardrail matched.
 */
export function guardrailOptions(list: Pick<Guardrail, 'id' | 'name'>[] | null, rows: Pick<RequestLog, 'guardrails'>[], selected: string[]): Option[] {
  const listed = new Set((list ?? []).map((g) => g.id));
  const unknown = new Set([...rows.flatMap((r) => r.guardrails.map((m) => m.id)), ...selected.filter((id) => id !== '-')].filter((id) => !listed.has(id)));
  return [
    ...(list ?? []).map((g) => ({ label: g.name, value: g.id })),
    ...[...unknown].sort().map((id) => ({ label: guardrailLabel(id, list), value: id })),
    { label: 'none', value: '-' },
  ];
}

/** A dry run's verdict on one request, the condition named by the row number the editor shows. */
export function verdictWords(r: GuardrailTestResult['requests'][number]): string {
  if (r.verdict === 'matched') return 'matched';
  const n = (r.condition ?? 0) + 1;
  return r.verdict === 'unmatched' ? `failed at condition ${n}` : `condition ${n} needs the live header`;
}

/** The stamp's badges as the route reads it: the conditions changed since the stamp, and a newer version shipped. */
export function presetBadges(preset: GuardrailPresetState | null): string[] {
  if (preset === null) return [];
  return [...(preset.customized ? ['customized'] : []), ...(preset.version < preset.currentVersion ? ['update available'] : [])];
}
