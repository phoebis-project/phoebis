import { useId, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { api, ApiError } from '@/api';
import type { GuardrailInput } from '@/api/client';
import type { ApiKey, Condition, Guardrail, GuardrailAttachment, GuardrailMode, GuardrailPreset, GuardrailTestResult, Pool } from '@/api/types';
import { useSession } from '@/app/session';
import { blankCondition, ConditionsEditor } from '@/components/ConditionsEditor';
import { FilterBar } from '@/components/FilterBar';
import { OptionList, type Option, walkFocus } from '@/components/ui/OptionList';
import { Popover } from '@/components/ui/Popover';
import { IconChevron, IconInfo, IconPlus } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, EmptyState, ErrorState, Field, GroupedDialog, Input, Menu, PageLink, Refusal, Segmented, Select, Skeleton, Toggle, type DialogGroup } from '@/components/ui/primitives';
import { poolKeysHref, requestHref } from '@/lib/address';
import { effectiveCaptureHours } from '@/lib/capture';
import type { PauseCause } from '@/lib/follow';
import { fmtAbsolute, fmtInt, messageOf, noun } from '@/lib/format';
import { derivedId, refusalsView, sameMembers, sameValues, saveStatus } from '@/lib/forms';
import { guardrailRefusals, MODE_TONE, presetBadges, presetBadgeTone, verdictWords } from '@/lib/guardrails';
import { useAsync } from '@/lib/hooks';

const LEAD = 'A guardrail checks each request against its conditions before any account is selected. Observe records the matches, enforce refuses them with a 403.';
const NAME_HINT = "The guardrail's id is made from this name when the guardrail is created.";
const MODE_HINT = 'Observe records each match on the request. Enforce refuses a matching request with a 403 before any account is selected.';
const CONDITIONS_HINT = 'Every condition must hold. The router checks the cheap conditions first, whatever their order here, and the message text last.';
const APPLIES_HINT = "A pool's guardrails cover every key of the pool. A guardrail attached here joins the end of each pool's and key's order, which their own dialogs arrange.";
const DELETE_BODY = 'Removes the guardrail from every pool and key it is attached to. Requests it matched keep its id in the log.';
const MODES: { label: string; value: GuardrailMode }[] = [
  { label: 'Observe', value: 'observe' },
  { label: 'Enforce', value: 'enforce' },
];
const SAMPLE_SIZES = [50, 200, 500];
/** The route's own sample when the run names none. */
const DEFAULT_SAMPLE = 200;

type GuardrailGroup = 'applies' | 'conditions' | 'identity' | 'test';
const GROUP_LABEL: Record<GuardrailGroup, string> = { applies: 'Applies to', conditions: 'Conditions', identity: 'Identity', test: 'Test' };
/** The rail's order, which the dialog's work follows: what it is, what it checks, where it runs, how it would fare. */
const GROUPS: GuardrailGroup[] = ['identity', 'conditions', 'applies', 'test'];

const counted = (n: number, one: string, many: string) => `${fmtInt(n)} ${noun(n, one, many)}`;
const byPosition = (list: GuardrailAttachment[]) => [...list].sort((a, b) => a.position - b.position);

/** A create from a preset, as the dialog fills it: the preset's name, mode and conditions, and no description. */
const fromPreset = (p: GuardrailPreset): Guardrail => ({
  action: 'block',
  conditions: p.conditions.map((c) => ({ ...c })),
  createdAt: '',
  description: '',
  enabled: true,
  id: '',
  keys: [],
  mode: p.suggestedMode,
  name: p.name,
  pools: [],
  preset: { currentVersion: p.version, customized: false, id: p.id, version: p.version },
  revision: 0,
  updatedAt: '',
});

/** A copy that runs nowhere until attached, so it never runs twice on the source's keys, and still reads its preset. */
const duplicateOf = (g: Guardrail): Guardrail => ({ ...g, keys: [], name: `${g.name} copy`, pools: [] });

/** The "Create guardrail" menu: a blank guardrail, then one row per preset this build ships. */
function CreateMenu({ onBlank, onPreset, presets }: { onBlank: () => void; onPreset: (p: GuardrailPreset) => void; presets: GuardrailPreset[] }) {
  const menuId = useId();
  const [anchor, setAnchor] = useState<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  const choose = (act: () => void) => {
    setOpen(false);
    act();
  };
  // The menus' one key map, so the arrows wrap and Home and End jump to either end as the Menu primitive's items do.
  const walk = (e: KeyboardEvent<HTMLDivElement>) => {
    const items = [...e.currentTarget.querySelectorAll<HTMLElement>('[role="menuitem"]')];
    walkFocus(e, items, e.target);
  };
  return (
    <>
      <Button ref={setAnchor} variant="primary" icon={<IconPlus size={14} />} aria-haspopup="menu" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
        Create guardrail
      </Button>
      <Popover anchor={anchor} id={menuId} label="Create guardrail" onClose={() => setOpen(false)} open={open} role="menu">
        <div className="guardrails__create" onKeyDown={walk}>
          <button type="button" role="menuitem" className="mselect__option" onClick={() => choose(onBlank)}>
            Blank guardrail
          </button>
          {presets.length > 0 && (
            <div className="mselect__group" role="presentation">
              From a preset
            </div>
          )}
          {presets.map((p) => (
            <button key={p.id} type="button" role="menuitem" className="mselect__option" onClick={() => choose(() => onPreset(p))}>
              {p.name}
            </button>
          ))}
        </div>
      </Popover>
    </>
  );
}

export function GuardrailsPage(): JSX.Element {
  const { notify } = useSession();
  const [dialog, setDialog] = useState<{ existing: Guardrail | null; source?: Guardrail } | null>(null);
  const [deleting, setDeleting] = useState<Guardrail | null>(null);
  // The rows whose menu is open, by guardrail id: an answer landing under an open menu could move the rows beneath it.
  const [openMenus, setOpenMenus] = useState<ReadonlySet<string>>(() => new Set());
  const [intervalMs, setIntervalMs] = useState(5_000);
  const [barOpen, setBarOpen] = useState(false);
  // The dialog resets its fields when it opens, so a refresh landing under it would move the lists it picks from.
  const paused = dialog !== null || deleting !== null || openMenus.size > 0 || barOpen;
  const pauseCause: PauseCause | null = dialog !== null || deleting !== null ? 'dialog' : openMenus.size > 0 || barOpen ? 'menu' : null;
  const guardrails = useAsync(() => api.listGuardrails(), [], { paused, refreshMs: intervalMs });
  const counts = useAsync(() => api.getAnalyticsGuardrails('24h'), [], { paused, refreshMs: intervalMs });
  // The dialog's pickers alone draw the pools and the keys, so they read once per visit and again after a write, a
  // dialog close and Refresh, never at the interval.
  const pools = useAsync(() => api.listPools(), []);
  const keys = useAsync(() => api.listKeys(), []);
  const presets = useAsync(() => api.listGuardrailPresets(), []);
  const presetList = useMemo(() => presets.data ?? [], [presets.data]);
  const list = guardrails.data;

  const reload = () => {
    guardrails.reload();
    counts.reload();
    pools.reload();
    keys.reload();
  };
  /** One row's write, the list read again behind it. A refusal only toasts, leaving the list alone. */
  const run = async (action: () => Promise<unknown>, done?: string) => {
    try {
      await action();
    } catch (e) {
      notify(messageOf(e));
      return;
    }
    if (done) notify(done);
    reload();
  };
  const menuOpen = (id: string) => (open: boolean) =>
    setOpenMenus((s) => {
      const next = new Set(s);
      if (open) next.add(id);
      else next.delete(id);
      return next;
    });

  const row = (content: ReactNode, key: string | number = 'state') => (
    <tr key={key}>
      <td colSpan={7}>{content}</td>
    </tr>
  );
  const body = !list
    ? guardrails.error
      ? row(<ErrorState title="Could not load the guardrails" body={guardrails.error} retry={guardrails.reload} />)
      : [0, 1].map((i) => row(<Skeleton />, i))
    : list.length === 0
      ? row(
          <EmptyState
            title="No guardrails yet"
            action={
              <Button size="sm" onClick={() => setDialog({ existing: null })}>
                Create guardrail
              </Button>
            }
          />,
        )
      : list.map((g) => {
          const attachedPools = byPosition(g.pools);
          const attachedKeys = byPosition(g.keys);
          const nowhere = attachedPools.length === 0 && attachedKeys.length === 0;
          const where = `${counted(attachedPools.length, 'pool', 'pools')}, ${counted(attachedKeys.length, 'key', 'keys')}`;
          const names = [...attachedPools.map((p) => `pool ${p.label}`), ...attachedKeys.map((k) => `key ${k.label}`)].join(', ');
          const count = counts.data?.guardrails.find((c) => c.id === g.id);
          const presetName = g.preset === null ? null : `${presetList.find((p) => p.id === g.preset?.id)?.name ?? g.preset.id} v${g.preset.version}`;
          return (
            <tr key={g.id} id={g.id} className={g.enabled ? 'guardrail-row' : 'guardrail-row guardrail-row--off'}>
              <td className="col-fill" title={g.name}>
                <b>{g.name}</b>
                {g.description && (
                  <span className="sub muted" title={g.description}>
                    {g.description}
                  </span>
                )}
              </td>
              <td className="col-fit">
                <Badge tone={MODE_TONE[g.mode]}>{g.mode}</Badge>
              </td>
              <td className="col-fit">{g.enabled ? 'on' : <span className="muted">off</span>}</td>
              <td className="col-fit" title={nowhere ? undefined : names}>
                {nowhere ? <span className="muted">nowhere</span> : where}
              </td>
              <td className="right num col-fit">
                {counts.data === null ? (
                  <span className="muted">-</span>
                ) : (
                  <>
                    {fmtInt(count?.observed ?? 0)} observed
                    <span className="sub">{fmtInt(count?.blocked ?? 0)} blocked</span>
                  </>
                )}
              </td>
              <td className="col-fill" title={presetName ?? '-'}>
                {presetName === null ? (
                  <span className="muted">-</span>
                ) : (
                  <>
                    {presetName}
                    {presetBadges(g.preset).map((word) => (
                      <span key={word} className="sub">
                        <Badge tone={presetBadgeTone(word)}>{word}</Badge>
                      </span>
                    ))}
                  </>
                )}
              </td>
              <td className="right col-fit">
                <Menu
                  onOpenChange={menuOpen(g.id)}
                  items={[
                    { label: 'Edit', onClick: () => setDialog({ existing: g }) },
                    { label: 'Duplicate', onClick: () => setDialog({ existing: null, source: duplicateOf(g) }) },
                    { label: g.enabled ? 'Disable' : 'Enable', onClick: () => void run(() => api.updateGuardrail(g.id, { enabled: !g.enabled }, g.revision)) },
                    { label: g.mode === 'enforce' ? 'Switch to observe' : 'Switch to enforce', onClick: () => void run(() => api.updateGuardrail(g.id, { mode: g.mode === 'enforce' ? 'observe' : 'enforce' }, g.revision)) },
                    { label: 'Delete', danger: true, onClick: () => setDeleting(g) },
                  ]}
                />
              </td>
            </tr>
          );
        });

  return (
    <div className="stack">
      <FilterBar
        address="/guardrails"
        count={list === null ? '' : counted(list.length, 'guardrail', 'guardrails')}
        filters={{}}
        onOpenChange={setBarOpen}
        refresh={{ intervalMs, onIntervalChange: setIntervalMs, onRefresh: reload, paused: pauseCause, refreshTitle: 'Reads the list again now' }}
        update={() => undefined}
      >
        <CreateMenu presets={presetList} onBlank={() => setDialog({ existing: null })} onPreset={(p) => setDialog({ existing: null, source: fromPreset(p) })} />
      </FilterBar>
      <p className="page-head__meta guardrails__lead">{LEAD}</p>
      <Card padded={false} className="card--table">
        <div className="table-wrap">
          <table className="table guardrails__table">
            <thead>
              <tr>
                <th className="col-fill">Name</th>
                <th className="col-fit">Mode</th>
                <th className="col-fit">Enabled</th>
                <th className="col-fit">Applies to</th>
                <th className="right col-fit">Matches, 24h</th>
                <th className="col-fill">Preset</th>
                <th className="col-fit" />
              </tr>
            </thead>
            <tbody>{body}</tbody>
          </table>
        </div>
      </Card>
      <GuardrailDialog
        open={dialog !== null}
        existing={dialog?.existing ?? null}
        source={dialog?.source}
        existingIds={new Set((list ?? []).map((g) => g.id))}
        keys={keys.data ?? []}
        pools={pools.data ?? []}
        presets={presetList}
        // Closing reads the list again, which a stale save's Reload counts on whatever the auto reload is set to.
        onClose={() => {
          setDialog(null);
          reload();
        }}
        onSaved={() => {
          notify(dialog?.existing ? 'Guardrail saved' : 'Guardrail created');
          setDialog(null);
          reload();
        }}
      />
      <ConfirmDialog
        open={deleting !== null}
        onClose={() => setDeleting(null)}
        title={deleting ? `Delete guardrail ${deleting.name}` : ''}
        body={DELETE_BODY}
        confirmLabel="Delete"
        danger
        onConfirm={() => {
          const g = deleting;
          if (g) void run(() => api.deleteGuardrail(g.id), 'Guardrail deleted');
        }}
      />
    </div>
  );
}

interface GuardrailForm {
  conditions: Condition[];
  description: string;
  enabled: boolean;
  keys: string[];
  mode: GuardrailMode;
  name: string;
  pools: string[];
}

/** The form a dialog opens with: the guardrail it edits, else the source it copies, else a blank one with one row. */
function formOf(existing: Guardrail | null, source: Guardrail | undefined): GuardrailForm {
  const base = existing ?? source ?? null;
  return {
    conditions: base ? base.conditions.map((c) => ({ ...c })) : [blankCondition()],
    description: base?.description ?? '',
    enabled: base?.enabled ?? true,
    keys: byPosition(base?.keys ?? []).map((a) => a.id),
    mode: base?.mode ?? 'observe',
    name: base?.name ?? '',
    pools: byPosition(base?.pools ?? []).map((a) => a.id),
  };
}

/** A pool or key picker: the button reads the count and opens the filter bar's list, as Add filter opens its own. */
function Picker({ empty, label, many, one, onChange, options, value }: { empty: string; label: string; many: string; one: string; onChange: (next: string[]) => void; options: Option[]; value: string[] }) {
  const boxId = useId();
  const [anchor, setAnchor] = useState<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button ref={setAnchor} aria-haspopup="dialog" aria-expanded={open} aria-label={`${label}: ${value.length === 0 ? empty : counted(value.length, one, many)}`} onClick={() => setOpen((o) => !o)}>
        {value.length === 0 ? empty : counted(value.length, one, many)}
        <IconChevron size={12} strokeWidth={1.5} />
      </Button>
      <Popover anchor={anchor} id={boxId} label={label} onClose={() => setOpen(false)} open={open}>
        <OptionList autoFocus label={label} onChange={onChange} options={options} searchable value={value} />
      </Popover>
    </>
  );
}

/**
 * Create, edit or copy a guardrail. `existing` switches the dialog to edit mode. `source` opens a create filled from
 * another guardrail or a preset, whose stamp the create sends as `preset`. `existingIds` are the listed guardrails'
 * ids, which a create's derived id is checked against before the route's 409.
 */
export function GuardrailDialog({ existing, existingIds, keys, onClose, onSaved, open, pools, presets, source }: { existing: Guardrail | null; existingIds: ReadonlySet<string>; keys: ApiKey[]; onClose: () => void; onSaved: (g: Guardrail) => void; open: boolean; pools: Pool[]; presets: GuardrailPreset[]; source?: Guardrail }): JSX.Element {
  const [chosen, setChosen] = useState<GuardrailGroup>('identity');
  const [form, setForm] = useState<GuardrailForm>(() => formOf(null, undefined));
  // The values the dialog opened with, which the discard question compares the fields against.
  const [opened, setOpened] = useState<GuardrailForm>(() => formOf(null, undefined));
  // The preset version the update notice took, which the save sends as `preset_version`.
  const [presetVersion, setPresetVersion] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // A route refusal that names a condition, drawn on that row until the next edit.
  const [rowRefusal, setRowRefusal] = useState<{ index: number; text: string } | null>(null);
  const [stale, setStale] = useState<string | null>(null);
  const [side, setSide] = useState<'key' | 'pool'>('pool');
  const [subjectId, setSubjectId] = useState('');
  const [limit, setLimit] = useState(DEFAULT_SAMPLE);
  const [testing, setTesting] = useState(false);
  const [tested, setTested] = useState<{ result: GuardrailTestResult; side: 'key' | 'pool'; subjectId: string } | null>(null);
  const [testError, setTestError] = useState<string | null>(null);
  // A run's answer lands only while this still reads what it read at the start: a reset, a close and a change of the
  // inputs the run sent move it, so the panel never shows another guardrail's or an earlier form's figures.
  const runToken = useRef(0);
  const dropRun = () => {
    runToken.current += 1;
    setTesting(false);
    setTested(null);
    setTestError(null);
  };

  // Once per opening, in the render that brings it, so a list that lands later does not wipe the edits made meanwhile.
  const [resetFor, setResetFor] = useState<{ existing: Guardrail | null; source: Guardrail | undefined } | null>(null);
  const resetDue = open && (resetFor === null || resetFor.existing !== existing || resetFor.source !== source);
  const closing = !open && resetFor !== null;
  if (closing) setResetFor(null);
  if (resetDue) {
    const next = formOf(existing, source);
    setResetFor({ existing, source });
    setForm(next);
    setOpened(next);
    setChosen('identity');
    setPresetVersion(null);
    setBusy(false);
    setError(null);
    setRowRefusal(null);
    setStale(null);
    setSide(next.pools.length > 0 || next.keys.length === 0 ? 'pool' : 'key');
    setSubjectId(next.pools[0] ?? (next.keys.length > 0 ? next.keys[0] : (pools[0]?.id ?? '')));
    setLimit(DEFAULT_SAMPLE);
  }
  // A subject picked before the lists landed, or one the side does not hold, falls to the side's first.
  const sideIds = side === 'pool' ? pools.map((p) => p.id) : keys.map((k) => k.id);
  const fallback = open && !resetDue && !sideIds.includes(subjectId) && sideIds.length > 0;
  if (fallback) setSubjectId(sideIds[0]);
  // eslint-disable-next-line react-hooks/refs -- the token moves in the render that replaces the inputs, before an answer to the run it drops can land
  if (closing || resetDue || fallback) dropRun();

  const edit = (patch: Partial<GuardrailForm>) => {
    setForm({ ...form, ...patch });
    // The route's words are about the values just changed, so they leave with the edit.
    setError(null);
    setRowRefusal(null);
    if (patch.conditions !== undefined) dropRun();
  };

  const stamp = existing?.preset ?? source?.preset ?? null;
  const stampedPreset = stamp === null ? undefined : presets.find((p) => p.id === stamp.id);
  const name = form.name.trim();
  const draftId = existing ? existing.id : derivedId(name).id;
  const refusals = guardrailRefusals({ conditions: form.conditions, description: form.description, id: draftId, name }, existingIds, existing !== null);
  const { first, invalid, refusedGroups } = refusalsView(refusals);
  const rowRefusals = new Map<number, string>();
  for (const r of refusals) if (r.field.startsWith('Condition ')) rowRefusals.set(Number(r.field.slice('Condition '.length)) - 1, r.text);
  if (rowRefusal && !rowRefusals.has(rowRefusal.index)) rowRefusals.set(rowRefusal.index, rowRefusal.text);
  const conditionsHeld = refusals.some((r) => r.group === 'conditions');

  const submit = async () => {
    setBusy(true);
    setError(null);
    setRowRefusal(null);
    setStale(null);
    const fields = { conditions: form.conditions, description: form.description, enabled: form.enabled, mode: form.mode, name };
    try {
      let saved: Guardrail;
      if (existing) {
        // The guardrail's side appends a newly named subject and detaches one left out, so an untouched picker sends
        // nothing and leaves an attachment made meanwhile from the subject's side in place.
        const input: Partial<GuardrailInput> & { preset?: never } = {
          ...fields,
          ...(sameMembers(form.pools, opened.pools) ? {} : { pools: form.pools }),
          ...(sameMembers(form.keys, opened.keys) ? {} : { keys: form.keys }),
          ...(presetVersion === null ? {} : { presetVersion }),
        };
        saved = await api.updateGuardrail(existing.id, input, existing.revision);
      } else {
        saved = await api.createGuardrail({ ...fields, keys: form.keys, pools: form.pools, ...(source?.preset ? { preset: source.preset.id } : {}) });
      }
      setBusy(false);
      onSaved(saved);
    } catch (e) {
      setBusy(false);
      if (e instanceof ApiError && e.status === 409 && e.code === 'stale_edit') {
        // The row moved under the dialog, so the operator reloads and applies the edit again rather than saving over it.
        setStale(e.message);
        return;
      }
      const message = messageOf(e);
      const refused = /^conditions\[(\d+)\]: (.*)$/.exec(message);
      if (refused) {
        const index = Number(refused[1]);
        setRowRefusal({ index, text: `Condition ${index + 1}: ${refused[2]}` });
        setChosen('conditions');
      }
      setError(message);
    }
  };

  const runTest = async () => {
    const token = runToken.current;
    setTesting(true);
    setTestError(null);
    try {
      const result = await api.testGuardrail({ conditions: form.conditions, limit, ...(side === 'pool' ? { poolId: subjectId } : { keyId: subjectId }) });
      // A dropped run leaves the panel and the button to the run started after it.
      if (token !== runToken.current) return;
      setTested({ result, side, subjectId });
    } catch (e) {
      if (token !== runToken.current) return;
      setTested(null);
      setTestError(messageOf(e));
    }
    setTesting(false);
  };

  const poolName = (id: string) => pools.find((p) => p.id === id)?.name ?? id;
  const poolOptions: Option[] = pools.map((p) => ({ label: p.name, value: p.id }));
  const keyOptions: Option[] = keys.map((k) => ({ label: `${k.name} (${poolName(k.poolId)})`, value: k.id }));

  const emptySample = (run: { side: 'key' | 'pool'; subjectId: string; result: GuardrailTestResult }) => {
    const key = run.side === 'key' ? keys.find((k) => k.id === run.subjectId) : undefined;
    const pool = pools.find((p) => p.id === (key ? key.poolId : run.subjectId)) ?? null;
    const hours = key ? effectiveCaptureHours(key, pool) : (pool?.bodyCaptureHours ?? 0);
    if (hours > 0) return <p className="muted">No captured bodies for {key?.name ?? pool?.name ?? run.subjectId} in the last {hours} hours.</p>;
    const keysOfPool = poolKeysHref(pool?.id ?? run.subjectId);
    return (
      <p className="muted">
        Body capture is off, so there is nothing to test against. Turn it on in the key&apos;s Capture group. <PageLink to={keysOfPool}>The pool&apos;s keys</PageLink>
      </p>
    );
  };

  const bodies: Record<GuardrailGroup, ReactNode> = {
    identity: (
      <div className="cells">
        {stamp !== null && (
          <div className="guardrails__preset">
            <p>
              From the preset {stampedPreset?.name ?? stamp.id} v{stamp.version}
              {presetBadges(stamp).map((word) => (
                <Badge key={word} tone={presetBadgeTone(word)}>
                  {word}
                </Badge>
              ))}
            </p>
            {stampedPreset && <p className="muted">{stampedPreset.description}</p>}
            {existing && stamp.version < stamp.currentVersion && (
              <div className="callout callout--info" role="note">
                <IconInfo size={16} />
                <div>
                  This build ships version {stamp.currentVersion} of the preset.{' '}
                  {presetVersion === null ? (
                    <Button
                      size="sm"
                      disabled={stampedPreset === undefined || stampedPreset.version !== stamp.currentVersion}
                      onClick={() => {
                        if (!stampedPreset) return;
                        edit({ conditions: stampedPreset.conditions.map((c) => ({ ...c })) });
                        setPresetVersion(stampedPreset.version);
                      }}
                    >
                      Update to the current preset
                    </Button>
                  ) : (
                    <span className="muted">Saving takes version {presetVersion}.</span>
                  )}
                </div>
              </div>
            )}
          </div>
        )}
        <Field label="Name" className="cell--half" hint={`${NAME_HINT}${existing === null && name !== '' ? ` Id: ${draftId}` : ''}`}>
          <Input value={form.name} onChange={(e) => edit({ name: e.target.value })} placeholder="Subagent status summary" aria-invalid={invalid('Name')} />
        </Field>
        <Field label="Description" className="cell--half">
          <Input value={form.description} onChange={(e) => edit({ description: e.target.value })} placeholder="What the guardrail catches and why" aria-invalid={invalid('Description')} />
        </Field>
        <Field label="Enabled" beside hint="A disabled guardrail stays attached and is skipped.">
          <Toggle checked={form.enabled} onChange={(enabled) => edit({ enabled })} label="Enabled" />
        </Field>
        {/* Field's frame without its label element: a label forwards a click on its text to the first button inside. */}
        <Field label="Mode" plain hint={MODE_HINT}>
          <div>
            <Segmented<GuardrailMode> ariaLabel="Mode" value={form.mode} options={MODES} onChange={(mode) => edit({ mode })} />
          </div>
        </Field>
      </div>
    ),
    conditions: (
      <div className="cells">
        {/* Field's frame without its label element: a label forwards a click on its text to the first control inside. */}
        <Field label="Conditions" plain hint={CONDITIONS_HINT}>
          <ConditionsEditor value={form.conditions} refusals={rowRefusals} onChange={(conditions) => edit({ conditions })} />
        </Field>
      </div>
    ),
    applies: (
      <div className="cells">
        <p className="field__hint">{APPLIES_HINT}</p>
        <Field label="Pools" plain className="cell--half">
          <div>
            <Picker label="Pools" empty="No pools" one="pool" many="pools" options={poolOptions} value={form.pools} onChange={(next) => edit({ pools: next })} />
          </div>
        </Field>
        <Field label="Keys" plain className="cell--half">
          <div>
            <Picker label="Keys" empty="No keys" one="key" many="keys" options={keyOptions} value={form.keys} onChange={(next) => edit({ keys: next })} />
          </div>
        </Field>
      </div>
    ),
    test: (
      <div className="cells guardrails__test">
        <Field label="Sample from" plain className="cell--half">
          <div className="row">
            <Segmented<'key' | 'pool'>
              ariaLabel="Sample from"
              value={side}
              options={[
                { label: 'Pool', value: 'pool' },
                { label: 'Key', value: 'key' },
              ]}
              onChange={(next) => {
                setSide(next);
                const ids = next === 'pool' ? [...form.pools, ...pools.map((p) => p.id)] : [...form.keys, ...keys.map((k) => k.id)];
                setSubjectId(ids[0] ?? '');
                dropRun();
              }}
            />
            <Select
              aria-label={side === 'pool' ? 'Pool' : 'Key'}
              value={subjectId}
              onChange={(e) => {
                setSubjectId(e.target.value);
                dropRun();
              }}
            >
              {(side === 'pool' ? poolOptions : keyOptions).map((o) => (
                <option key={o.value} value={o.value}>
                  {o.label}
                </option>
              ))}
            </Select>
          </div>
        </Field>
        <Field label="Sample" className="cell--half">
          <Select
            value={limit}
            onChange={(e) => {
              setLimit(Number(e.target.value));
              dropRun();
            }}
          >
            {SAMPLE_SIZES.map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </Select>
        </Field>
        <div>
          <Button onClick={() => void runTest()} disabled={testing || subjectId === '' || conditionsHeld} title={conditionsHeld ? 'The conditions need fixing first' : undefined}>
            {testing ? 'Running…' : 'Run test'}
          </Button>
        </div>
        {testError && <Refusal>{testError}</Refusal>}
        {tested &&
          (tested.result.totals.sampled === 0 ? (
            emptySample(tested)
          ) : (
            <>
              <p className="guardrails__totals">
                {fmtInt(tested.result.totals.matched)} matched, {fmtInt(tested.result.totals.unmatched)} unmatched, {fmtInt(tested.result.totals.indeterminate)} indeterminate of {fmtInt(tested.result.totals.sampled)} sampled
              </p>
              <div className="table-wrap">
                <table className="table guardrails__sample">
                  <thead>
                    <tr>
                      <th className="col-fit">Time</th>
                      <th className="col-fill">Request</th>
                      <th className="col-fit">Verdict</th>
                    </tr>
                  </thead>
                  <tbody>
                    {/* The matched rows first, each side newest first as the route sampled them. */}
                    {[...tested.result.requests]
                      .sort((a, b) => Number(b.verdict === 'matched') - Number(a.verdict === 'matched'))
                      .map((r) => (
                        <tr key={r.id}>
                          <td className="col-fit">{fmtAbsolute(r.receivedAt)}</td>
                          <td className="col-fill mono" title={r.id}>
                            <PageLink to={requestHref(r.id)}>{r.id}</PageLink>
                          </td>
                          <td className="col-fit">{verdictWords(r)}</td>
                        </tr>
                      ))}
                  </tbody>
                </table>
              </div>
            </>
          ))}
      </div>
    ),
  };
  if (rowRefusal) refusedGroups.add('conditions');
  const groups: DialogGroup[] = GROUPS.map((id) => ({ body: bodies[id], id, label: GROUP_LABEL[id], refused: refusedGroups.has(id) }));
  const unsaved = presetVersion !== null || !sameValues(form, opened);
  const save = saveStatus({ error, first, stale, subject: existing ? `the guardrail ${existing.name}` : null });

  return (
    <GroupedDialog
      open={open}
      onClose={onClose}
      title={existing ? `Edit guardrail ${existing.name}` : 'Create guardrail'}
      unsaved={unsaved}
      railLabel="Guardrail settings"
      groups={groups}
      chosen={chosen}
      onChoose={(id) => setChosen(id as GuardrailGroup)}
      status={save.status}
      hold={save.hold ?? (rowRefusal && !stale ? 'conditions' : null)}
      held={save.held}
      notice={
        stale && (
          <Refusal>
            {stale}{' '}
            <Button variant="ghost" onClick={onClose}>
              Reload
            </Button>
          </Refusal>
        )
      }
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={busy || refusals.length > 0} onClick={() => void submit()}>
            {busy ? (existing ? 'Saving…' : 'Creating…') : existing ? 'Save guardrail' : 'Create guardrail'}
          </Button>
        </>
      }
    />
  );
}
