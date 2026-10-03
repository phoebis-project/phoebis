import { useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { Button, Expert, Field, GroupedDialog, GroupLink, Input, type DialogGroup } from '@/components/ui/primitives';
import { POOL_GROUPS, type PoolGroup } from '@/lib/forms';
import { POOL_RAIL } from '@/lib/poolRail';

const list = (value: string | null) => (value ?? '').split(',').filter(Boolean);
// A copy of the pool dialog's create marks, so the fixture draws the page's words without importing the page.
const CREATE_MARKS: Partial<Record<PoolGroup, string>> = { identity: 'needed', keys: 'after saving', members: 'needed' };

export default function GroupedDialogFixture() {
  // `?refused=<id>,<id>` marks groups refused, since the dot widens a rail entry and the pass measures that state;
  // `?expert=<id>,<id>` wraps one more field per listed group, and `?expertOnly=<id>,<id>` replaces a group's plain
  // field by a wrapped one and declares the group expert only, so the checks reach the line and the rail rule now.
  // `?create=1` sets the create marks, which the pass measures with and without the refusal dots.
  const [params] = useSearchParams();
  const create = params.get('create') === '1';
  const refused = list(params.get('refused'));
  const expert = list(params.get('expert'));
  const expertOnly = list(params.get('expertOnly'));
  const target = expertOnly.find((id) => id !== 'identity');
  // The pool dialog's own rail table, so the rail the browser pass measures wraps and stacks as the real one does.
  const groups: DialogGroup[] = POOL_GROUPS.map((id) => {
    const { label, parent } = POOL_RAIL[id];
    const only = expertOnly.includes(id);
    // In the dialogs' cell grid: an `?expert=` group pairs its plain field with the wrapped one as two half cells, and
    // an `?expertOnly=` group's wrapped field spans the grid, so the pass measures both widths through the wrapper.
    const half = expert.includes(id) && !only ? 'cell--half' : undefined;
    return {
      body: (
        <div className="cells">
          {!only && (
            <Field label={label} className={half}>
              <Input aria-label={label} />
            </Field>
          )}
          {(only || expert.includes(id)) && (
            <Expert refused={refused.includes(id)}>
              <Field label={`${label} (expert)`} className={half}>
                <Input aria-label={`${label} expert`} />
              </Field>
            </Expert>
          )}
          {id === 'identity' && target !== undefined && (
            <p className="field__hint">
              Opens <GroupLink id={target}>{POOL_RAIL[target as PoolGroup].label}</GroupLink>.
            </p>
          )}
        </div>
      ),
      expertOnly: only,
      id,
      label,
      mark: create ? CREATE_MARKS[id] : undefined,
      parent,
      refused: refused.includes(id),
    };
  });
  // `?extra=<n>` adds n groups under a parent of their own, so the pass can grow the rail past the height it holds.
  const extra = Array.from({ length: Number(params.get('extra') ?? 0) }, (_, i): DialogGroup => {
    const label = `Extra ${i + 1}`;
    return {
      body: (
        <Field label={label}>
          <Input />
        </Field>
      ),
      id: `extra-${i + 1}`,
      label,
      parent: 'More',
    };
  });
  const [chosen, setChosen] = useState('identity');
  return (
    <GroupedDialog
      chosen={chosen}
      footer={<Button variant="primary">Save</Button>}
      groups={[...groups, ...extra].map((g) => (refused.includes(g.id) ? { ...g, refused: true } : g))}
      hold={refused[0] ?? null}
      onChoose={setChosen}
      onClose={() => {}}
      open
      railLabel="Pool settings"
      status={refused.length ? 'Save is held.' : 'Every group saves together.'}
      title="Edit pool"
    />
  );
}
