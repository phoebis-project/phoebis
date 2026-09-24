import { useEffect, useId, useRef, useState } from 'react';
import { api } from '@/api';
import type { ModelRule } from '@/api/types';
import { IconChevron, IconPlus, IconX } from '@/components/ui/icons';
import { Button, Input, Segmented } from '@/components/ui/primitives';
import { checkPattern } from '@/lib/patterns';

type Effect = 'allow' | 'deny';

const EFFECTS: { label: string; value: Effect }[] = [
  { label: 'Allow', value: 'allow' },
  { label: 'Deny', value: 'deny' },
];

/** Long enough that ticking several members in a row costs one catalog read rather than one per tick. */
const SUGGESTION_DELAY_MS = 300;

/**
 * The model ids the named accounts' live catalogs hold, read again once the set has stopped changing for a
 * moment. Nothing is read while `enabled` is off, so a closed dialog asks no upstream for its catalog.
 */
export function useCatalogSuggestions(accountIds: readonly string[], enabled: boolean): string[] {
  // Account ids are slugs, so a newline never occurs inside one.
  const named = [...accountIds].sort().join('\n');
  const [ids, setIds] = useState<string[]>([]);
  useEffect(() => {
    if (!enabled) return;
    let alive = true;
    const timer = window.setTimeout(() => {
      if (!named) {
        setIds([]);
        return;
      }
      api.listCatalogOf(named.split('\n')).then(
        (models) => {
          if (alive) setIds(models.map((m) => m.id));
        },
        () => {
          if (alive) setIds([]);
        },
      );
    }, SUGGESTION_DELAY_MS);
    return () => {
      alive = false;
      window.clearTimeout(timer);
    };
  }, [enabled, named]);
  return ids;
}

export function PatternEditor({ inherited = [], label, onChange, suggestions, value }: {
  /** The pool's entries a key under `extend` inherits, shown read-only in grey after the own entries. */
  inherited?: ModelRule[];
  label: string;
  onChange: (rules: ModelRule[]) => void;
  /** Ids the level can see: the union of the pool's members' live catalogs. */
  suggestions: string[];
  value: ModelRule[];
}): JSX.Element {
  const listId = useId();
  const rowId = useId();
  const summaryId = useId();
  const refusalId = useId();
  // A key per row that travels with it, so a row moved up or down keeps its node and the button keeps focus.
  const keys = useRef<number[]>([]);
  const nextKey = useRef(0);
  const focusKey = useRef<number | null>(null);
  while (keys.current.length < value.length) keys.current.push(nextKey.current++);
  keys.current.length = value.length;
  // A row added by the button takes the focus once it is drawn, so the operator types the pattern straight away.
  useEffect(() => {
    if (focusKey.current === null) return;
    const input = document.getElementById(`${rowId}-${focusKey.current}`);
    if (!input) return;
    focusKey.current = null;
    input.focus();
  });

  const commit = (rules: ModelRule[], order: number[]) => {
    keys.current = order;
    onChange(rules);
  };
  const update = (i: number, rule: ModelRule) => onChange(value.map((r, j) => (j === i ? rule : r)));
  const move = (i: number, j: number) => {
    const rules = [...value];
    const order = [...keys.current];
    [rules[i], rules[j]] = [rules[j], rules[i]];
    [order[i], order[j]] = [order[j], order[i]];
    commit(rules, order);
  };
  const remove = (i: number) => commit(value.filter((_, j) => j !== i), keys.current.filter((_, j) => j !== i));
  const add = () => {
    const key = nextKey.current++;
    focusKey.current = key;
    commit([...value, { not: false, pattern: '' }], [...keys.current, key]);
  };

  const refusals = value.map((r) => checkPattern(r.pattern));
  const first = refusals.findIndex((r) => r !== null);
  const count = `${value.length} ${value.length === 1 ? 'rule' : 'rules'}${inherited.length ? `, then ${inherited.length} from pool` : ''}`;
  const summary = first < 0 ? count : `${count}; rule ${first + 1}: ${refusals[first]}`;

  return (
    <div className="pattern-editor" role="group" aria-label={label} aria-describedby={summaryId}>
      <span id={summaryId} className="sr-only">
        {summary}
      </span>
      {(value.length > 0 || inherited.length > 0) && (
        <ol className="pattern-editor__rows">
          {value.map((rule, i) => {
            const key = keys.current[i];
            const refusal = refusals[i];
            const n = i + 1;
            return (
              <li key={key} className={`pattern-editor__row ${refusal ? 'is-refused' : ''}`}>
                <div className="pattern-editor__line">
                  <Segmented<Effect> ariaLabel={`Rule ${n} allows or denies`} value={rule.not ? 'deny' : 'allow'} options={EFFECTS} onChange={(v) => update(i, { ...rule, not: v === 'deny' })} />
                  <Input
                    id={`${rowId}-${key}`}
                    className="mono"
                    value={rule.pattern}
                    list={listId}
                    placeholder="claude-sonnet-*"
                    spellCheck={false}
                    autoComplete="off"
                    aria-label={`Rule ${n} pattern`}
                    aria-invalid={refusal ? true : undefined}
                    aria-describedby={refusal ? `${refusalId}-${n}` : undefined}
                    onChange={(e) => update(i, { ...rule, pattern: e.target.value })}
                  />
                  <span className="pattern-editor__actions">
                    <Button size="sm" variant="ghost" className="pattern-editor__up" icon={<IconChevron size={14} />} aria-label={`Move rule ${n} up`} disabled={i === 0} onClick={() => move(i, i - 1)} />
                    <Button size="sm" variant="ghost" icon={<IconChevron size={14} />} aria-label={`Move rule ${n} down`} disabled={i === value.length - 1} onClick={() => move(i, i + 1)} />
                    <Button size="sm" variant="ghost" icon={<IconX size={14} />} aria-label={`Remove rule ${n}`} onClick={() => remove(i)} />
                  </span>
                </div>
                {refusal && (
                  <p id={`${refusalId}-${n}`} className="pattern-editor__refusal">
                    {refusal}
                  </p>
                )}
              </li>
            );
          })}
          {inherited.map((rule, i) => (
            <li key={`inherited-${i}`} className="pattern-editor__row pattern-editor__row--inherited">
              <span className="pattern-editor__effect">{rule.not ? 'Deny' : 'Allow'}</span>
              <span className="pattern-editor__pattern mono">{rule.pattern}</span>
              <span className="pattern-editor__source">from pool</span>
            </li>
          ))}
        </ol>
      )}
      <div>
        <Button size="sm" icon={<IconPlus size={13} />} onClick={add}>
          Add rule
        </Button>
      </div>
      <datalist id={listId}>
        {suggestions.map((id) => (
          <option key={id} value={id} />
        ))}
      </datalist>
    </div>
  );
}
