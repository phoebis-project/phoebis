import { useState } from 'react';
import { Button, Field, GroupedDialog, Input, type DialogGroup } from '@/components/ui/primitives';

// The pool dialog's parents and groups, so the rail the browser pass measures wraps and stacks as the real one will.
const RAIL: [string, [string, string][]][] = [
  [
    'Pool',
    [
      ['identity', 'Identity'],
      ['members', 'Members'],
      ['keys', 'Keys'],
      ['models', 'Models'],
    ],
  ],
  [
    'Routing',
    [
      ['selection', 'Selection'],
      ['placement', 'Placement'],
      ['pinning', 'Pinning'],
    ],
  ],
  [
    'Load',
    [
      ['utilization-thresholds', 'Utilization thresholds'],
      ['load-shedding', 'Load shedding'],
      ['failover', 'Failover'],
      ['overage-and-plan-end', 'Overage and plan end'],
    ],
  ],
  [
    'Caching',
    [
      ['cache-ttl', 'Cache TTL'],
      ['keepalive', 'Keepalive'],
      ['probe-tuning', 'Probe tuning'],
    ],
  ],
  [
    'Visibility',
    [
      ['response-headers', 'Response headers'],
      ['body-capture', 'Body capture'],
      ['cache-diagnostics', 'Cache diagnostics'],
    ],
  ],
];

const GROUPS: DialogGroup[] = RAIL.flatMap(([parent, groups]) =>
  groups.map(([id, label]) => ({
    body: (
      <Field label={label}>
        <Input />
      </Field>
    ),
    id,
    label,
    parent,
  })),
);

export default function GroupedDialogFixture() {
  const [chosen, setChosen] = useState('identity');
  return (
    <GroupedDialog
      chosen={chosen}
      footer={<Button variant="primary">Save</Button>}
      groups={GROUPS}
      onChoose={setChosen}
      onClose={() => {}}
      open
      railLabel="Pool settings"
      status="Every group saves together."
      title="Edit pool"
    />
  );
}
