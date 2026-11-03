import { useId } from 'react';
import type { Condition, ConditionSubject, MessageSelector } from '@/api/types';
import { IconPlus } from '@/components/ui/icons';
import { Button, Input, RowActions, Select } from '@/components/ui/primitives';
import { MAX_CONDITIONS, OPERATOR_LABEL, SELECTOR_LABEL, SUBJECTS, subjectOf, takesValue } from '@/lib/guardrails';
import { useKeyedRows } from '@/lib/hooks';

const SELECTORS = Object.keys(SELECTOR_LABEL) as MessageSelector[];

/**
 * A row moved to `subject`: the operator back to the subject's first, and every cell the subject or that operator does
 * not take cleared. A subject with a closed set of words keeps a value among them and starts any other on its default.
 */
function withSubject(c: Condition, subject: ConditionSubject): Condition {
  const words = subjectOf(subject);
  const operator = words.operators[0];
  const value = !takesValue(operator) ? undefined : words.values === undefined || words.values.some((v) => v.value === c.value) ? c.value : words.defaultValue;
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
  const refusalId = useId();
  // A row added by the button takes the focus on its subject, so the operator picks it straight away.
  const { add, idOf, keys, move, remove, update } = useKeyedRows(value, onChange, blankCondition);

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
                  <Select id={idOf(key)} aria-label={`Condition ${n} subject`} value={c.subject} onChange={(e) => update(i, withSubject(c, e.target.value as ConditionSubject))}>
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
                    (words.values ? (
                      <Select aria-label={`Condition ${n} value`} value={c.value ?? ''} onChange={(e) => update(i, { ...c, value: e.target.value })}>
                        {words.values.map((k) => (
                          <option key={k.value} value={k.value}>
                            {k.label}
                          </option>
                        ))}
                      </Select>
                    ) : (
                      <Input className="mono" aria-label={`Condition ${n} value`} placeholder={c.operator === 'matches' ? '^Describe' : 'Value'} spellCheck={false} autoComplete="off" value={c.value ?? ''} aria-invalid={invalid} aria-describedby={described} onChange={(e) => update(i, { ...c, value: e.target.value })} />
                    ))}
                  <span className="conditions-editor__actions">
                    <RowActions count={value.length} index={i} noun="condition" onMove={move} onRemove={remove} upClass="conditions-editor__up" />
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
