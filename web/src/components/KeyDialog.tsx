import { useId, useState, type ReactNode } from 'react';
import { api } from '@/api';
import { isStaleEdit, slug } from '@/api/http';
import type { Account, ApiKey, CreatedApiKey, Guardrail, GuardrailRef, KeyModels, KeyStatus, ModelRulesMode, Pool } from '@/api/types';
import { FEATURE_WORD_VALUES } from '@/api/types';
import { PatternEditor, useCatalogSuggestions } from '@/components/PatternEditor';
import { OptionList } from '@/components/ui/OptionList';
import { Popover } from '@/components/ui/Popover';
import { IconInfo, IconPlus } from '@/components/ui/icons';
import { Badge, Button, CopyButton, Dialog, ErrorState, Expert, Field, GroupedDialog, Input, NumberInput, PageLink, Refusal, RowActions, Segmented, Select, Skeleton, StaleRefusal, Toggle, Warning, type DialogGroup } from '@/components/ui/primitives';
import { clock } from '@/lib/clock';
import { messageOf, plural } from '@/lib/format';
import { FEATURE_LABEL, FEATURE_NONE, featureAccountOptions, featureAccountValue, featureStandingLine, guardrailsChange, KEY_GROUPS, keyFeatureChoice, keyFeatureValue, keyFormOf, keyInputOf, keyRefusals, refusalsView, sameValues, saveStatus, type AutoModeOverride, type KeyForm, type KeyGroup } from '@/lib/forms';
import { MODE_TONE } from '@/lib/guardrails';
import { failedFirst, useAsync, useNow, type Loaded } from '@/lib/hooks';

export const KEY_STATUS_TONE: Record<KeyStatus, 'critical' | 'good' | 'neutral' | 'warning'> = { active: 'good', disabled: 'warning', expired: 'neutral', revoked: 'critical' };
const MODES: { label: string; value: ModelRulesMode }[] = [
  { label: 'Extend pool rules', value: 'extend' },
  { label: 'Replace pool rules', value: 'replace' },
];

/** A new or rotated key's secret, shown this once, since the router stores only its hash. */
export function SecretDialog({ children, onClose, secret, title }: { children?: ReactNode; onClose: () => void; secret: string | null; title: string }) {
  return (
    <Dialog
      open={secret !== null}
      onClose={onClose}
      title={title}
      subtitle="Copy the secret now. The router stores only a hash, so it cannot be shown again."
      footer={
        <Button variant="primary" onClick={onClose}>
          Done
        </Button>
      }
    >
      {secret !== null && (
        <>
          <div className="secret">
            <code>{secret}</code>
            <CopyButton text={secret} size="md" />
          </div>
          {children}
        </>
      )}
    </Dialog>
  );
}

/**
 * What the key dialog reads of a pool: its auto mode and capture defaults, the rules, guardrails and feature accounts a
 * key inherits, and whose catalogs to suggest from.
 */
export type KeyDialogPool = Pick<Pool, 'autoModeServer' | 'bodyCaptureHours' | 'featureAccounts' | 'guardrails' | 'id' | 'modelRules' | 'name'> & { members: Pick<Pool['members'][number], 'accountId'>[] };

const KEY_GROUP_LABEL: Record<KeyGroup, string> = { access: 'Access', capture: 'Capture', features: 'claude.ai features', guardrails: 'Guardrails', key: 'Key', models: 'Models', sessions: 'Sessions' };
const KEY_GUARDRAILS_HINT = "The pool's guardrails run first and cannot be dropped here. This key's own run after them.";

/** A row of a guardrail list: the route's ref, or the registry's row for one attached since the dialog opened. */
type ListedGuardrail = Pick<GuardrailRef, 'enabled' | 'id' | 'name'> & { mode: GuardrailRef['mode'] | null };

/**
 * A row's name, the view's word for a switched-off guardrail, and its mode badge in the Guardrails view's tones,
 * drawn last so it ends at the trailing slot in every row.
 */
function GuardrailWords({ guardrail: g }: { guardrail: ListedGuardrail }) {
  return (
    <>
      <span className="pattern-editor__pattern">{g.name}</span>
      {!g.enabled && <span className="muted">off</span>}
      {g.mode !== null && <Badge tone={MODE_TONE[g.mode]}>{g.mode}</Badge>}
    </>
  );
}

/**
 * A pool's or a key's guardrails in the order they run, each with move up, move down and remove, and "Attach guardrail"
 * offering the registry's guardrails that `inherited` does not hold, since one on both runs once at its pool position,
 * the list's own ones among them and checked. `inherited` draws a key's pool's rows first, read-only. `refs` are the
 * record's own rows as the route reported them, which name a row before the registry answers. `pending` is the sentence
 * a subject not saved yet reads in place of its own list and the control, since the create routes take no attachment.
 */
export function GuardrailList({ inherited = [], onChange, pending, refs, registry, value }: { inherited?: GuardrailRef[]; onChange: (ids: string[]) => void; pending?: string; refs: GuardrailRef[]; registry: Loaded<Guardrail[] | null>; value: string[] }): JSX.Element {
  const boxId = useId();
  const [anchor, setAnchor] = useState<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  const fromPool = [...inherited].sort((a, b) => a.position - b.position);
  const own = pending === undefined ? value : [];
  const listed = (id: string): ListedGuardrail => refs.find((r) => r.id === id) ?? registry.data?.find((g) => g.id === id) ?? { enabled: true, id, mode: null, name: id };
  const options = (registry.data ?? []).filter((g) => !fromPool.some((r) => r.id === g.id)).map((g) => ({ label: g.name, value: g.id }));
  const held = registry.data === null ? (registry.error ? `The guardrail list did not load: ${registry.error}` : 'Reading the guardrail list') : options.length === 0 ? 'No other guardrail is left to attach' : undefined;
  const move = (i: number, j: number) => {
    const next = [...value];
    [next[i], next[j]] = [next[j], next[i]];
    onChange(next);
  };
  // A pick attaches at once and lands at the end of the order, where the guardrail dialog's attach lands too, and an
  // uncheck removes its guardrail, so the box stays open while the operator picks.
  const pick = (next: string[]) => {
    const kept = value.filter((id) => next.includes(id));
    const added = options.map((o) => o.value).filter((id) => next.includes(id) && !value.includes(id));
    onChange([...kept, ...added]);
  };
  // The Popover hands the focus back to the anchor on Escape itself, and a focus move here would also pull the focus
  // back when it leaves the box for another control.
  const close = () => setOpen(false);
  return (
    <div className="pattern-editor guardrail-list" role="group" aria-label="Guardrails">
      {fromPool.length + own.length > 0 && (
        <ol className="plain-list pattern-editor__rows">
          {fromPool.map((r) => (
            <li key={`inherited-${r.id}`} className="pattern-editor__row pattern-editor__row--inherited">
              <GuardrailWords guardrail={r} />
              <span className="pattern-editor__source guardrail-row__trailing">inherited</span>
            </li>
          ))}
          {own.map((id, i) => {
            const g = listed(id);
            return (
              <li key={id} className="pattern-editor__row">
                <div className="pattern-editor__line">
                  <GuardrailWords guardrail={g} />
                  <span className="pattern-editor__actions guardrail-row__trailing">
                    <RowActions count={own.length} index={i} name={g.name} noun="guardrail" onMove={move} onRemove={() => onChange(value.filter((x) => x !== id))} />
                  </span>
                </div>
              </li>
            );
          })}
        </ol>
      )}
      {pending !== undefined ? (
        <p className="muted">{pending}</p>
      ) : (
        <>
          {fromPool.length + own.length === 0 && <p className="muted">No guardrails attached.</p>}
          <div>
            <Button ref={setAnchor} size="sm" icon={<IconPlus size={13} />} aria-haspopup="dialog" aria-expanded={open} disabled={held !== undefined} title={held} onClick={() => setOpen((o) => !o)}>
              Attach guardrail
            </Button>
          </div>
          {/* Each pick adds an own row above the anchor and each uncheck removes one, so the box is placed again. */}
          <Popover anchor={anchor} id={boxId} label="Attach guardrail" onClose={close} open={open} placeKey={value.length}>
            <OptionList autoFocus label="Guardrails" onChange={pick} options={options} searchable value={value} />
          </Popover>
        </>
      )}
    </div>
  );
}

/**
 * Create or edit a key. `existing` switches the dialog to edit mode. `initialPoolId` with `lockPool` pins the pool when
 * the dialog is opened from inside a pool's own dialog. `existingIds` are the listed keys' ids, which a new key's
 * derived id is checked against before the create.
 */
export function KeyDialog({ open, onClose, onCreated, onReload, onUpdated, pools, captureMaxHours, existing = null, existingIds, initialGroup = null, initialPoolId, lockPool = false }: { /** The bootstrap's body capture limit from the host page's settings read, undefined while that read is out. */ captureMaxHours?: number; existing?: ApiKey | null; existingIds: ReadonlySet<string>; initialGroup?: KeyGroup | null; initialPoolId?: string; lockPool?: boolean; onClose: () => void; onCreated: (c: CreatedApiKey) => void; /** The stale save's Reload: closes the dialog and reads the keys again. */ onReload?: () => void; onUpdated?: (k: ApiKey) => void; open: boolean; pools: KeyDialogPool[] }) {
  const [chosen, setChosen] = useState<KeyGroup>('key');
  const [form, setForm] = useState<KeyForm>(() => keyFormOf(null, ''));
  // A field written again unchanged, as the override's focus writes its mode, keeps the form, so it keeps the server's
  // words too.
  const set = <K extends keyof KeyForm>(key: K, value: KeyForm[K]) => setForm((f) => (f[key] === value ? f : { ...f, [key]: value }));
  const { allowance, autoModeServer, captureHours, captureMode, enabled, expiry, featureAccounts, guardrails, keepalive, longContextTwins, modelRules, modelRulesMode, name, notes, poolId } = form;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [stale, setStale] = useState<string | null>(null);
  const now = useNow(undefined, open);
  // Once per opening, a closed dialog answering null with no request, so a reopening reads the list anew.
  const accountsRead = useAsync(() => (open ? api.listAccounts() : Promise.resolve(null)), [open]);
  const accounts = accountsRead.current ?? [];
  const accountsFailed = failedFirst(accountsRead) ? accountsRead.error : null;
  const pool = pools.find((p) => p.id === poolId) ?? null;
  const poolName = pool?.name ?? poolId;
  const poolHours = pool?.bodyCaptureHours ?? 0;
  const effectiveHours = captureMode === 'inherit' ? poolHours : captureMode === 'off' ? 0 : (captureHours ?? 0);
  const suggestions = useCatalogSuggestions(pool?.members.map((m) => m.accountId) ?? [], open);
  const refusals = keyRefusals({ captureHours, captureMode, modelRules, name, poolId }, { identity: { existingIds, isNew: existing === null }, maxHours: captureMaxHours });
  const { first, invalid, refusalOf, refusedGroups } = refusalsView(refusals);
  const rulesRefusal = refusalOf('Model rules');
  const processed = useAsync(() => (open && existing ? api.getKeyModels(existing.id) : Promise.resolve(null)), [open, existing?.id]);
  // Once per opening of a saved key, since a create takes no attachment and offers nothing to attach.
  const registry = useAsync(() => (open && existing ? api.listGuardrails() : Promise.resolve(null)), [open, existing?.id]);
  // The values the dialog opened with, which the discard question compares the fields against.
  const [opened, setOpened] = useState(() => keyFormOf(null, ''));

  // Once per opening, in the render that brings it: the address can open the dialog before the pool list lands, and a
  // reset on its arrival would discard every edit made meanwhile and put back the group the dialog opened on, the
  // focus left in a hidden one.
  const [resetFor, setResetFor] = useState<{ existing: ApiKey | null; initialGroup: KeyGroup | null; initialPoolId: string | undefined } | null>(null);
  const resetDue = open && (resetFor === null || resetFor.existing !== existing || resetFor.initialGroup !== initialGroup || resetFor.initialPoolId !== initialPoolId);
  if (!open && resetFor !== null) setResetFor(null);
  if (resetDue) {
    // A key cannot exist without a pool, so the first pool stands in until one is picked.
    const next = keyFormOf(existing, initialPoolId ?? pools[0]?.id ?? '');
    setResetFor({ existing, initialGroup, initialPoolId });
    setOpened(next);
    setForm(next);
    setChosen(initialGroup ?? 'key');
    setBusy(false);
    setError(null);
    setStale(null);
  }
  // A new key opened before the pools landed has no pool yet, so the first pool fills it once they do, after the
  // render that resets. The fill is no edit of the operator's, so the opening values take it too where they hold no
  // pool yet.
  if (open && !resetDue && existing === null && poolId === '' && pools.length > 0) {
    set('poolId', pools[0].id);
    if (opened.poolId === '') setOpened({ ...opened, poolId: pools[0].id });
  }

  const moving = existing !== null && poolId !== existing.poolId;
  // The server's words leave with the next edit to any field. A failed save changes none, so they stand until then.
  const [edited, setEdited] = useState(form);
  if (edited !== form) {
    setEdited(form);
    setError(null);
  }

  const submit = async () => {
    setBusy(true);
    setError(null);
    setStale(null);
    const input = keyInputOf(form, clock.now());
    try {
      if (existing) {
        const updated = await api.updateKey(existing.id, { ...input, ...guardrailsChange(guardrails, opened.guardrails) }, existing.revision);
        setBusy(false);
        onUpdated?.(updated);
        return;
      }
      const created = await api.createKey(input);
      setBusy(false);
      onCreated(created);
    } catch (e) {
      // Stated in the dialog rather than a toast, so the form the operator filled in is still on screen beside it.
      setBusy(false);
      if (isStaleEdit(e)) {
        // The row moved under the dialog: the edit is about values that are gone, so the operator reloads and
        // re-applies it rather than saving over what someone else wrote.
        setStale(e.message);
        return;
      }
      setError(messageOf(e));
    }
  };

  // The dialog holds no member list, so an option names the pool the account is stored in.
  const poolNameOf = (account: Account) => {
    const poolOf = account.pool?.poolId;
    return poolOf === undefined ? null : (pools.find((p) => p.id === poolOf)?.name ?? poolOf);
  };

  // Keyed by group, so a group without a body fails to compile. `KEY_GROUPS` sets the rail order.
  const bodiesOf = (): Record<KeyGroup, ReactNode> => ({
    key: (
      <>
        {moving && (
          <div className="callout callout--warning" role="status">
            <IconInfo size={16} />
            <div>Moving this key ends its current session pins.</div>
          </div>
        )}
        <div className="cells">
          <Field label="Name" className="cell--half" hint={!existing && name.trim() ? `Id: ${slug(name.trim())}` : undefined}>
            <Input value={name} onChange={(e) => set('name', e.target.value)} placeholder="workstation" autoFocus aria-invalid={invalid('Name')} />
          </Field>
          <Field
            label="Pool"
            className="cell--half"
            hint={
              lockPool ? (
                `Created in pool ${poolName}`
              ) : (
                <>
                  A key reaches only its pool&apos;s members, and moving it ends its session pins. Members and rules are on the <PageLink to="/pools">Pools page</PageLink>.
                </>
              )
            }
          >
            <Select value={poolId} onChange={(e) => set('poolId', e.target.value)} disabled={lockPool} aria-invalid={invalid('Pool')}>
              {pools.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </Select>
          </Field>
          <Field label="Notes">
            <textarea className="input" value={notes} onChange={(e) => set('notes', e.target.value)} placeholder="Where this key lives and who holds it" />
          </Field>
        </div>
      </>
    ),
    access: (
      <div className="cells">
        {existing && (
          <Field label="Enabled" beside hint="Off refuses the key and keeps it, so it can come back on without a new secret. A revoked key cannot.">
            {/* The route refuses to re-enable a revoked key, so its switch says so rather than offering it. */}
            <span title={existing.status === 'revoked' ? 'a revoked key cannot be re-enabled' : undefined}>
              <Toggle checked={enabled} onChange={(v) => set('enabled', v)} label="Enabled" disabled={existing.status === 'revoked'} />
            </span>
          </Field>
        )}
        <Field label="Expiry" hint="An expired key is refused, but its history stays in the ledger.">
          <Select value={expiry} onChange={(e) => set('expiry', e.target.value)} aria-label="Expiry">
            {existing && <option value="keep">Keep the current expiry</option>}
            {!existing && <option value="never">No expiration</option>}
            <option value="7">7 days</option>
            <option value="30">30 days</option>
            <option value="60">60 days</option>
            <option value="90">90 days</option>
            <option value="365">1 year</option>
            {existing && <option value="never">No expiration</option>}
          </Select>
        </Field>
        <Expert>
          <Field label="Server-side auto mode" hint="Inherit follows the pool. Off drops the client's server-side tool-use checks on this key, so the client reviews with classifier requests of its own. On keeps them whatever the pool says.">
            <Select value={autoModeServer} onChange={(e) => set('autoModeServer', e.target.value as AutoModeOverride)} aria-label="Server-side auto mode">
              <option value="inherit">Inherit ({pool?.autoModeServer === false ? 'off' : 'on'}, the pool&apos;s setting)</option>
              <option value="on">On</option>
              <option value="off">Off</option>
            </Select>
          </Field>
        </Expert>
      </div>
    ),
    models: (
      <div className="cells">
        {/* Field's frame without its label element: a label forwards a click on its text to the first control
            inside, which here would press Add rule or flip the first row. */}
        <Field
          label="Model rules"
          plain
          hint="Top to bottom, first match wins. The saved key's Effective list shows the list it ends up with."
        >
          <PatternEditor label="Model rules" value={modelRules} onChange={(v) => set('modelRules', v)} inherited={modelRulesMode === 'extend' ? (pool?.modelRules ?? []) : []} suggestions={suggestions} />
        </Field>
        {rulesRefusal && <Refusal>{rulesRefusal}</Refusal>}
        <Expert>
          {/* Field's frame without its label element: a label forwards a click on its text to the first control
              inside, which here would flip the mode. */}
          <Field label="Pool rules" plain hint="Extend applies the pool's rules, in gray, after this key's own. Replace applies this key's rules alone.">
            {/* Its own block, so the segmented control keeps its content width. */}
            <div>
              <Segmented<ModelRulesMode> ariaLabel="How this key's rules meet the pool's" value={modelRulesMode} options={MODES} onChange={(v) => set('modelRulesMode', v)} />
            </div>
          </Field>
        </Expert>
        <Expert>
          <Field label="List [1m] twins in the models listing" beside hint="Claude Code sizes its context window by the id, so the twin lets it use the long-context window the upstream reports.">
            <Toggle checked={longContextTwins} onChange={(v) => set('longContextTwins', v)} label="List [1m] twins in the models listing" />
          </Field>
        </Expert>
        {existing && <EffectiveList processed={processed} />}
      </div>
    ),
    guardrails: (
      <div className="cells">
        {/* Field's frame without its label element: a label forwards a click on its text to the first button
            inside. */}
        <Field
          label="Guardrails"
          plain
          hint={
            <>
              {KEY_GUARDRAILS_HINT} <PageLink to="/guardrails">Manage guardrails</PageLink>
            </>
          }
        >
          {/* The chosen pool's list, so a key moved in this dialog shows the pool it lands in, and the key's own read
              of its pool's list until the pool list arrives. */}
          <GuardrailList inherited={pool?.guardrails ?? (existing?.poolId === poolId ? existing.poolGuardrails : [])} onChange={(v) => set('guardrails', v)} pending={existing ? undefined : 'Guardrails can be attached once the key is saved.'} refs={existing?.guardrails ?? []} registry={registry} value={guardrails} />
        </Field>
      </div>
    ),
    features: (
      <div className="cells">
        {accountsFailed !== null && <ErrorState title="The account list did not load" body={accountsFailed} retry={accountsRead.reload} />}
        {FEATURE_WORD_VALUES.map((feature) => {
          const choice = featureAccounts[feature];
          const named = choice.accountId;
          const options = featureAccountOptions(accounts, feature, poolNameOf, now, named);
          // The selection of the pool the form names, so a key moved in this dialog reads the pool it lands in.
          const inherited = pool?.featureAccounts[feature] ?? null;
          const inheritedAccount = inherited === null ? undefined : accounts.find((a) => a.id === inherited);
          const lineAccount = choice.mode === 'account' ? accounts.find((a) => a.id === named) : choice.mode === 'inherit' ? inheritedAccount : undefined;
          return (
            <Field key={feature} label={FEATURE_LABEL[feature]} hint={lineAccount && featureStandingLine(lineAccount, now)}>
              <Select
                value={keyFeatureValue(choice)}
                // Without the pool the first option cannot name its account, so a choice made then would rest on a
                // value the dialog does not know.
                disabled={accountsRead.current === null || pool === null}
                onChange={(e) => {
                  const next = keyFeatureChoice(e.target.value);
                  setForm((f) => ({ ...f, featureAccounts: { ...f.featureAccounts, [feature]: next } }));
                }}
                aria-label={FEATURE_LABEL[feature]}
              >
                <option value="inherit">Inherit ({inherited === null ? FEATURE_NONE : (inheritedAccount?.label ?? `${inherited}, not listed`)})</option>
                <option value="none">{FEATURE_NONE}</option>
                {options.map((o) => (
                  <option key={o.accountId} value={featureAccountValue(o.accountId)} disabled={o.disabled}>
                    {o.label}
                  </option>
                ))}
              </Select>
            </Field>
          );
        })}
      </div>
    ),
    sessions: (
      <div className="cells">
        <Field label="Allowance tool" beside hint="The model may read the pool's allowance through the companion.">
          <Toggle checked={allowance} onChange={(v) => set('allowance', v)} label="Allowance tool" />
        </Field>
        <Field label="Cache warming" beside hint="The router warms this key's sessions' caches.">
          <Toggle checked={keepalive} onChange={(v) => set('keepalive', v)} label="Cache warming" />
        </Field>
      </div>
    ),
    capture: (
      <>
        <Warning when={effectiveHours > 0}>Bodies contain the prompts, file contents, and tool results of every session using this key, and a session in auto mode sends the machine&apos;s permission rules with their literal text, its working and home directories and its git remotes with every request. Only the admin secret can read them.</Warning>
        <div className="cells">
          {/* Field's frame without its label element, since each choice is a label of its own. */}
          <Field
            label="Capture request and response bodies"
            plain
            hint={
              <>
                Stores the full JSON of each exchange for the request drawer on the <PageLink to="/requests">Requests page</PageLink>. Inherit follows the pool, and any value is capped by the bootstrap&apos;s limit.
              </>
            }
          >
            <div className="radio-list" role="radiogroup" aria-label="Capture request and response bodies">
              <label className={`radio-list__item ${captureMode === 'inherit' ? 'is-active' : ''}`}>
                <input type="radio" name={`capture-${existing?.id ?? 'new'}`} checked={captureMode === 'inherit'} onChange={() => set('captureMode', 'inherit')} />
                <span>Inherit from pool ({poolHours > 0 ? `${poolHours} h` : 'off'})</span>
              </label>
              <label className={`radio-list__item ${captureMode === 'override' ? 'is-active' : ''}`}>
                <input type="radio" name={`capture-${existing?.id ?? 'new'}`} checked={captureMode === 'override'} onChange={() => set('captureMode', 'override')} />
                <span className="row">
                  Override:
                  <NumberInput
                    min={1}
                    max={captureMaxHours}
                    value={captureHours}
                    onNumber={(v) => set('captureHours', v)}
                    onFocus={() => set('captureMode', 'override')}
                    style={{ flex: 1 }}
                    aria-label="Capture hours for this key"
                    aria-invalid={invalid('Capture hours for this key')}
                  />
                  hours
                </span>
              </label>
              <label className={`radio-list__item ${captureMode === 'off' ? 'is-active' : ''}`}>
                <input type="radio" name={`capture-${existing?.id ?? 'new'}`} checked={captureMode === 'off'} onChange={() => set('captureMode', 'off')} />
                <span>Off for this key</span>
              </label>
            </div>
          </Field>
        </div>
      </>
    ),
  });
  const bodies = open ? bodiesOf() : null;
  const groups: DialogGroup[] = KEY_GROUPS.map((id) => ({ body: bodies?.[id] ?? null, id, label: KEY_GROUP_LABEL[id], refused: refusedGroups.has(id) }));
  const save = saveStatus({ error, first, stale, subject: existing ? `the key ${existing.name}` : null });

  return (
    <GroupedDialog
      open={open}
      onClose={onClose}
      title={existing ? `Edit ${existing.name}` : 'Create key'}
      unsaved={!sameValues(form, opened)}
      railLabel="Key settings"
      groups={groups}
      chosen={chosen}
      initialGroup={initialGroup}
      onChoose={(id) => setChosen(id as KeyGroup)}
      status={save.status}
      hold={save.hold}
      held={save.held}
      notice={stale && <StaleRefusal message={stale} onReload={onReload} />}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={busy || refusals.length > 0} onClick={submit}>
            {busy ? (existing ? 'Saving…' : 'Creating…') : existing ? 'Save' : 'Create'}
          </Button>
        </>
      }
    />
  );
}

/** The list the saved key is judged by, from the route rather than the form, so an unsaved edit does not show here. */
function EffectiveList({ processed }: { processed: Loaded<KeyModels | null> }) {
  const rules = processed.current?.rules;
  return (
    <div className="field">
      <span className="field__label">Effective list</span>
      <p className="field__hint">What the saved key is judged by, top to bottom, with the level each entry comes from.</p>
      {processed.error ? (
        <p className="muted">Could not load the effective list: {processed.error}</p>
      ) : !rules ? (
        <Skeleton height={28} />
      ) : rules.length === 0 ? (
        <p className="muted">No rules: every model the pool's accounts serve is allowed.</p>
      ) : (
        <ol className="plain-list effective-rules">
          {rules.map((r, i) => {
            const cooling = new Set(r.cooldowns.map((c) => c.accountId)).size;
            return (
              <li key={i}>
                <Badge tone={r.level === 'key' ? 'accent' : 'neutral'}>{r.level}</Badge>
                <span className="effective-rules__effect">{r.not ? 'Deny' : 'Allow'}</span>
                <span className="mono">{r.pattern}</span>
                {cooling > 0 && (
                  <Badge tone="warning" title={r.cooldowns.map((c) => `${c.accountId}: ${c.scope.replace(/^model:/, '')}`).join(', ')}>
                    {plural(cooling, 'account')} cooling
                  </Badge>
                )}
              </li>
            );
          })}
        </ol>
      )}
    </div>
  );
}
