import { useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { Button, Field, GroupedDialog, Input, type DialogGroup } from '@/components/ui/primitives';
import { POOL_GROUPS } from '@/lib/forms';
import { POOL_RAIL } from '@/lib/poolRail';

// The pool dialog's own rail table, so the rail the browser pass measures wraps and stacks as the real one does.
const GROUPS: DialogGroup[] = POOL_GROUPS.map((id) => ({
  body: (
    <Field label={POOL_RAIL[id].label}>
      <Input />
    </Field>
  ),
  id,
  ...POOL_RAIL[id],
}));

export default function GroupedDialogFixture() {
  // `?refused=<id>,<id>` marks groups refused, since the dot widens a rail entry and the pass measures that state.
  const [params] = useSearchParams();
  const refused = (params.get('refused') ?? '').split(',').filter(Boolean);
  const [chosen, setChosen] = useState('identity');
  return (
    <GroupedDialog
      chosen={chosen}
      footer={<Button variant="primary">Save</Button>}
      groups={GROUPS.map((g) => (refused.includes(g.id) ? { ...g, refused: true } : g))}
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
