import { useEffect, useId, useRef, useState } from 'react';
import type { Condition, ConditionSubject, MessageSelector } from '@/api/types';
import { IconChevron, IconPlus, IconX } from '@/components/ui/icons';
import { Button, Input, Select } from '@/components/ui/primitives';
import { KIND_VALUES, MAX_CONDITIONS, OPERATOR_LABEL, SELECTOR_LABEL, SUBJECTS, subjectOf, takesValue } from '@/lib/guardrails';

const SELECTORS = Object.keys(SELECTOR_LABEL) as MessageSelector[];
const KIND_WORDS = KIND_VALUES.map((k) => k.value);

/**
 * A row moved to `subject`: the operator back to the subject's first, and every cell the subject or that operator does
 * not take cleared. A `kind` row compares against one of its two words, so it starts on Inference.
 */
function withSubject(c: Condition, subject: ConditionSubject): Condition {
  const words = subjectOf(subject);
  const operator = words.operators[0];
  const value = !takesValue(operator) ? undefined : subject === 'kind' ? (KIND_WORDS.includes(c.value ?? '') ? c.value : 'inference') : c.value;
  return {
    name: words.cell === 'header' ? c.name : undefined,
    operator,
    path: words.cell === 'path' ? c.path : undefined,
    selector: words.cell === 'selector' ? c.selector : undefined,
    subject,
    value,
  };
}

/** A blank row, the first subject's: what "Add condition" appends. */
export const blankCondition = (): Condition => withSubject({ operator: 'absent', subject: 'agent_type' }, SUBJECTS[0].value);

/**
 * The conditions in their written order, one row each: the subject, the subject's own cell, the operator its row
 * offers, and the value the operator compares against, with the move and remove actions of the pattern editor.
 * `refusals` holds a row's sentence by its index, the console's own or the route's, drawn under the row.
 */
export function ConditionsEditor({ onChange, refusals, value }: { onChange: (c: Condition[]) => void; refusals: ReadonlyMap<number, string>; value: Condition[] }): JSX.Element {
  const rowId = useId();
  const refusalId = useId();
  // A key per row that travels with it, so a row moved up or down keeps its node and the button keeps focus.
  const [rows, setRows] = useState(() => ({ keys: value.map((_, i) => i), next: value.length }));
  const focusKey = useRef<number | null>(null);
  let keys = rows.keys;
  if (keys.length !== value.length) {
    let next = rows.next;
    keys = keys.slice(0, value.length);
    while (keys.length < value.length) keys.push(next++);
    setRows({ keys, next });
  }
  // A row added by the button takes the focus once it is drawn, so the operator picks its subject straight away.
  useEffect(() => {
    if (focusKey.current === null) return;
    const select = document.getElementById(`${rowId}-${focusKey.current}`);
    if (!select) return;
    focusKey.current = null;
    select.focus();
  });

  const commit = (next: Condition[], order: number[], nextKey = rows.next) => {
    setRows({ keys: order, next: nextKey });
    onChange(next);
  };
  const update = (i: number, c: Condition) => onChange(value.map((x, j) => (j === i ? c : x)));
  const move = (i: number, j: number) => {
    const next = [...value];
    const order = [...keys];
    [next[i], next[j]] = [next[j], next[i]];
    [order[i], order[j]] = [order[j], order[i]];
    commit(next, order);
  };
  const remove = (i: number) => commit(value.filter((_, j) => j !== i), keys.filter((_, j) => j !== i));
  const add = () => {
    const key = rows.next;
    focusKey.current = key;
    commit([...value, blankCondition()], [...keys, key], key + 1);
  };

  return (
    <div className="conditions-editor" role="group" aria-label="Conditions">
      {value.length > 0 && (
        <ol className="conditions-editor__rows">
          {value.map((c, i) => {
            const key = keys[i];
            const n = i + 1;
            const words = subjectOf(c.subject);
            const refusal = refusals.get(i) ?? null;
            const invalid = refusal === null ? undefined : true;
            const described = refusal === null ? undefined : `${refusalId}-${n}`;
            return (
              <li key={key} className={`conditions-editor__row ${refusal ? 'is-refused' : ''}`}>
                <div className="conditions-editor__line">
                  <Select id={`${rowId}-${key}`} aria-label={`Condition ${n} subject`} value={c.subject} onChange={(e) => update(i, withSubject(c, e.target.value as ConditionSubject))}>
                    {SUBJECTS.map((s) => (
                      <option key={s.value} value={s.value}>
                        {s.label}
                      </option>
                    ))}
                  </Select>
                  {words.cell === 'header' && (
                    <Input className="mono" aria-label={`Condition ${n} header name`} placeholder="x-claude-code-agent-type" spellCheck={false} autoComplete="off" value={c.name ?? ''} aria-invalid={invalid} aria-describedby={described} onChange={(e) => update(i, { ...c, name: e.target.value })} />
                  )}
                  {words.cell === 'path' && (
                    <Input className="mono" aria-label={`Condition ${n} path`} placeholder="tools[*].name" spellCheck={false} autoComplete="off" value={c.path ?? ''} aria-invalid={invalid} aria-describedby={described} onChange={(e) => update(i, { ...c, path: e.target.value })} />
                  )}
                  {words.cell === 'selector' && (
                    <Select aria-label={`Condition ${n} selector`} value={c.selector ?? ''} aria-invalid={invalid} aria-describedby={described} onChange={(e) => update(i, { ...c, selector: e.target.value === '' ? undefined : (e.target.value as MessageSelector) })}>
                      {c.selector === undefined && <option value="">Choose a message</option>}
                      {SELECTORS.map((s) => (
                        <option key={s} value={s}>
                          {SELECTOR_LABEL[s]}
                        </option>
                      ))}
                    </Select>
                  )}
                  <Select aria-label={`Condition ${n} operator`} value={c.operator} onChange={(e) => {
                    const operator = e.target.value as Condition['operator'];
                    update(i, { ...c, operator, value: takesValue(operator) ? (c.value ?? '') : undefined });
                  }}>
                    {words.operators.map((o) => (
                      <option key={o} value={o}>
                        {OPERATOR_LABEL[o]}
                      </option>
                    ))}
                  </Select>
                  {takesValue(c.operator) &&
                    (c.subject === 'kind' ? (
                      <Select aria-label={`Condition ${n} value`} value={c.value ?? ''} onChange={(e) => update(i, { ...c, value: e.target.value })}>
                        {KIND_VALUES.map((k) => (
                          <option key={k.value} value={k.value}>
                            {k.label}
                          </option>
                        ))}
                      </Select>
                    ) : (
                      <Input className="mono" aria-label={`Condition ${n} value`} placeholder={c.operator === 'matches' ? '^Describe' : 'Value'} spellCheck={false} autoComplete="off" value={c.value ?? ''} aria-invalid={invalid} aria-describedby={described} onChange={(e) => update(i, { ...c, value: e.target.value })} />
                    ))}
                  <span className="conditions-editor__actions">
                    <Button size="sm" variant="ghost" className="conditions-editor__up" icon={<IconChevron size={14} />} aria-label={`Move condition ${n} up`} disabled={i === 0} onClick={() => move(i, i - 1)} />
                    <Button size="sm" variant="ghost" icon={<IconChevron size={14} />} aria-label={`Move condition ${n} down`} disabled={i === value.length - 1} onClick={() => move(i, i + 1)} />
                    <Button size="sm" variant="ghost" icon={<IconX size={14} />} aria-label={`Remove condition ${n}`} onClick={() => remove(i)} />
                  </span>
                </div>
                {refusal && (
                  <p id={`${refusalId}-${n}`} className="conditions-editor__refusal">
                    {refusal}
                  </p>
                )}
              </li>
            );
          })}
        </ol>
      )}
      <div className="conditions-editor__foot">
        <Button size="sm" icon={<IconPlus size={13} />} onClick={add} disabled={value.length >= MAX_CONDITIONS}>
          Add condition
        </Button>
        <span className="muted">
          {value.length} of {MAX_CONDITIONS}
        </span>
      </div>
    </div>
  );
}
