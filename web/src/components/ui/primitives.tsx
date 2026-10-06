import { createContext, Fragment, useCallback, useContext, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type ButtonHTMLAttributes, type CSSProperties, type FocusEvent, type HTMLAttributes, type InputHTMLAttributes, type KeyboardEvent, type ReactNode, type SelectHTMLAttributes } from 'react';
import { createPortal } from 'react-dom';
import { Link } from 'react-router-dom';
import type { ToastItem } from '@/app/session';
import { useAppSettings } from '@/app/settings';
import { clock } from '@/lib/clock';
import { floatAt, menuPlacement, type MenuAlign, type MenuPlacement } from '@/lib/menu';
import type { BadgeTone } from '@/lib/quota';
import { IconCheck, IconChevron, IconCopy, IconSearch, IconWarning, IconX } from './icons';

const DescriptionHost = createContext<{ base: string; register: (text: string) => () => void } | null>(null);
// The id derives from the text, so a described element points at its entry in its first render, before the entry draws.
const entryId = (base: string, text: string) => `${base}-${encodeURIComponent(text)}`;

/**
 * Holds the descriptions of the titled elements inside it in one hidden node after its content. A node beside each
 * element would join the text of the cell or list item that holds it, which readers and checks take whole.
 */
export function Descriptions({ children }: { children: ReactNode }) {
  const base = useId();
  const [counts, setCounts] = useState<ReadonlyMap<string, number>>(() => new Map());
  const register = useCallback((text: string) => {
    setCounts((m) => new Map(m).set(text, (m.get(text) ?? 0) + 1));
    return () =>
      setCounts((m) => {
        const next = new Map(m);
        const n = (next.get(text) ?? 1) - 1;
        if (n > 0) next.set(text, n);
        else next.delete(text);
        return next;
      });
  }, []);
  const host = useMemo(() => ({ base, register }), [base, register]);
  return (
    <DescriptionHost.Provider value={host}>
      {children}
      {/* Marked so a whole-page text read can leave out the copies no sighted reader sees. */}
      {counts.size > 0 && (
        <div className="sr-only" data-descriptions="">
          {[...counts.keys()].map((t) => (
            <span key={t} id={entryId(base, t)}>
              {t}
            </span>
          ))}
        </div>
      )}
    </DescriptionHost.Provider>
  );
}

/** Null without text; under a host the id of the host's entry, else an own id the caller renders a sibling node for. */
export function useDescription(text: string | undefined): { id: string; sibling: boolean } | null {
  const host = useContext(DescriptionHost);
  const own = useId();
  useEffect(() => (host && text ? host.register(text) : undefined), [host, text]);
  if (!text) return null;
  return host ? { id: entryId(host.base, text), sibling: false } : { id: own, sibling: true };
}

export function Badge({ tone = 'neutral', children, dot = false, title }: { children: ReactNode; dot?: boolean; title?: string; tone?: BadgeTone }) {
  const described = useDescription(title);
  // Outside every host the title alone describes the badge: a node beside it would join its parent's text.
  const hosted = described && !described.sibling ? described.id : undefined;
  return (
    <span className={`badge badge--${tone}`} title={title} aria-describedby={hosted}>
      {dot && <span className="badge__dot" aria-hidden />}
      {children}
    </span>
  );
}

/** A badge that opens a page: it describes itself through its host as a `Badge` does. */
export function BadgeLink({ tone = 'neutral', children, title, to }: { children: ReactNode; title?: string; to: string; tone?: BadgeTone }) {
  const described = useDescription(title);
  const hosted = described && !described.sibling ? described.id : undefined;
  return (
    <Link to={to} className={`badge badge--${tone}`} title={title} aria-describedby={hosted}>
      {children}
    </Link>
  );
}

type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  icon?: ReactNode;
  size?: 'md' | 'sm';
  variant?: 'danger' | 'ghost' | 'primary' | 'secondary';
};

export function Button({ variant = 'secondary', size = 'md', icon, children, className = '', ...rest }: ButtonProps) {
  return (
    <button type="button" className={`btn btn--${variant} btn--${size} ${className}`} {...rest}>
      {icon && <span className="btn__icon">{icon}</span>}
      {children}
    </button>
  );
}

export function Pill({ children, active = false, onClick, disabled, title, icon }: { active?: boolean; children: ReactNode; disabled?: boolean; icon?: ReactNode; onClick?: () => void; title?: string }) {
  return (
    <button type="button" className={`pill ${active ? 'is-active' : ''}`} onClick={onClick} disabled={disabled} title={title} aria-pressed={onClick ? active : undefined}>
      {icon}
      {children}
    </button>
  );
}

export function Segmented<T extends string>({ value, options, onChange, ariaLabel, title }: { ariaLabel: string; onChange: (v: T) => void; options: { label: ReactNode; name?: string; value: T }[]; title?: string; value: T }) {
  return (
    <div className="segmented" role="group" aria-label={ariaLabel} title={title}>
      {options.map((o) => (
        <button key={o.value} type="button" className={o.value === value ? 'is-active' : ''} aria-label={o.name} title={o.name} aria-pressed={o.value === value} onClick={() => onChange(o.value)}>
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** `id` and `tabIndex` make the card an address's target: a link can name it, and the page can move focus to it. */
export function Card({ children, className = '', title, subtitle, actions, padded = true, id, tabIndex }: { actions?: ReactNode; children: ReactNode; className?: string; id?: string; padded?: boolean; subtitle?: ReactNode; tabIndex?: number; title?: ReactNode }) {
  return (
    <section id={id} tabIndex={tabIndex} className={`card ${padded ? 'card--padded' : ''} ${className}`}>
      <Descriptions>
        {(title || actions) && (
          <header className="card__head">
            <div>
              {title && <h2 className="card__title">{title}</h2>}
              {subtitle && <p className="card__subtitle">{subtitle}</p>}
            </div>
            {actions && <div className="card__actions">{actions}</div>}
          </header>
        )}
        {children}
      </Descriptions>
    </section>
  );
}

export function Meter({ value, tone, label, thin = false }: { label?: string; thin?: boolean; tone: 'accent' | 'good' | 'low' | 'warn'; value: number }) {
  const pct = Math.round(Math.max(0, Math.min(1, value)) * 1000) / 10;
  return (
    <div className={`meter meter--${tone} ${thin ? 'meter--thin' : ''}`} role="meter" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct} aria-label={label}>
      <div className="meter__fill" style={{ width: `${pct}%` }} />
    </div>
  );
}

export function Skeleton({ width = '100%', height = 12, className = '' }: { className?: string; height?: number; width?: number | string }) {
  return <span className={`skeleton ${className}`} style={{ width, height }} aria-hidden />;
}

export function EmptyState({ title, body, action }: { action?: ReactNode; body?: ReactNode; title: string }) {
  return (
    <div className="empty">
      <p className="empty__title">{title}</p>
      {body && <p className="empty__body">{body}</p>}
      {action}
    </div>
  );
}

/**
 * The failed twin of `EmptyState`, in the same box so a list swaps one for the other without the page moving.
 * The body carries the reason the call gave; the route it came from is in the shell's toast.
 */
export function ErrorState({ title = 'Could not load', body, action }: { action?: ReactNode; body?: ReactNode; title?: string }) {
  return (
    <div className="empty empty--error" role="alert">
      <p className="empty__title">{title}</p>
      {body && <p className="empty__body">{body}</p>}
      {action}
    </div>
  );
}

/**
 * The hint is a block outside the label, so label and hint never share a line. `beside` puts the control before its
 * label on one line (a switch); `plain` frames without a label element, for a control a label click would press.
 * Every other prop lands on the root `div.field` (`data-rule-set`, `id`, `title`), so a caller can mark the root the
 * selectors and the CSS read; the hint is a `div` because a hint may hold a dialog, which a `p` cannot.
 */
export function Field({ label, hint, children, beside = false, plain = false, className, ...rest }: Omit<HTMLAttributes<HTMLDivElement>, 'children'> & { beside?: boolean; children: ReactNode; hint?: ReactNode; label: ReactNode; plain?: boolean }) {
  const Frame = plain ? 'div' : 'label';
  const text = <span className="field__label">{label}</span>;
  return (
    <div {...rest} className={`field ${beside ? 'field--beside' : ''} ${className ?? ''}`}>
      <Frame className="field__control">
        {beside ? children : text}
        {beside ? text : children}
      </Frame>
      {hint && <div className="field__hint">{hint}</div>}
    </div>
  );
}

export function Input(props: InputHTMLAttributes<HTMLInputElement>) {
  return <input {...props} className={`input ${props.className ?? ''}`} />;
}

export function SearchInput(props: InputHTMLAttributes<HTMLInputElement>) {
  return (
    <span className="search">
      <IconSearch size={14} />
      <input type="search" {...props} className={`input ${props.className ?? ''}`} />
    </span>
  );
}

export function Select(props: SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <span className="select">
      <select {...props} className={`input ${props.className ?? ''}`} />
      <IconChevron size={14} />
    </span>
  );
}

export function Toggle({ checked, onChange, label, disabled }: { checked: boolean; disabled?: boolean; label?: string; onChange: (v: boolean) => void }) {
  return (
    <button type="button" role="switch" aria-checked={checked} aria-label={label} className={`toggle ${checked ? 'is-on' : ''}`} onClick={() => onChange(!checked)} disabled={disabled}>
      <span className="toggle__knob" />
    </button>
  );
}

/**
 * A switch over the number it enables: the number stays drawn under the switch and turns on with it. `disabled`
 * disables the switch and the number together; with it off, the switch alone gates the number.
 */
export function SwitchedNumber({ checked, disabled, hint, input, label, numberLabel, onChange }: { checked: boolean; disabled?: boolean; hint?: ReactNode; input: InputHTMLAttributes<HTMLInputElement>; label: string; numberLabel?: string; onChange: (v: boolean) => void }) {
  // After the spread, so a `disabled` the caller put in `input` can never reopen a gated number.
  const number = <Input type="number" {...input} disabled={disabled || !checked} />;
  return (
    <div className="switched">
      <Field label={label} beside>
        <Toggle label={label} checked={checked} onChange={onChange} disabled={disabled} />
      </Field>
      {numberLabel ? (
        <Field label={numberLabel} hint={hint}>
          {number}
        </Field>
      ) : (
        <div className="field">
          {number}
          {hint && <div className="field__hint">{hint}</div>}
        </div>
      )}
    </div>
  );
}

export function Tabs<T extends string>({ label, onChange, tabs, value }: { label?: string; onChange: (v: T) => void; tabs: { controls?: string; count?: number; id?: string; label: string; value: T }[]; value: T }) {
  return (
    <div className="tabs" role="tablist" aria-label={label}>
      {tabs.map((t) => (
        <button key={t.value} type="button" role="tab" id={t.id} aria-controls={t.controls} aria-selected={t.value === value} className={t.value === value ? 'is-active' : ''} onClick={() => onChange(t.value)}>
          {t.label}
          {t.count !== undefined && <span className="tabs__count">{t.count}</span>}
        </button>
      ))}
    </div>
  );
}

export function CopyButton({ text, label = 'Copy', size = 'sm' }: { label?: string; size?: 'md' | 'sm'; text: string }) {
  const [state, setState] = useState<'copied' | 'failed' | 'idle'>('idle');
  useEffect(() => {
    if (state === 'idle') return;
    const id = window.setTimeout(() => setState('idle'), 1600);
    return () => window.clearTimeout(id);
  }, [state]);
  return (
    <Button
      size={size}
      variant={state === 'failed' ? 'danger' : 'ghost'}
      icon={state === 'copied' ? <IconCheck size={14} /> : state === 'failed' ? <IconWarning size={14} /> : <IconCopy size={14} />}
      title={state === 'failed' ? 'select the text and copy it by hand' : undefined}
      onClick={() => {
        void (async () => {
          try {
            if (!navigator.clipboard) throw new Error('no clipboard');
            await navigator.clipboard.writeText(text);
            setState('copied');
          } catch {
            // The secret dialog is the one place this matters: saying Copied without a write lets the operator
            // close the only display of a secret they never received.
            setState('failed');
          }
        })();
      }}
    >
      {state === 'copied' ? 'Copied' : state === 'failed' ? 'Copy failed, select the text' : label}
    </Button>
  );
}

function useModalDialog(open: boolean, onClose: () => void) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    if (open && !el.open) el.showModal();
    if (!open && el.open) el.close();
  }, [open]);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const onCancel = (e: Event) => {
      e.preventDefault();
      onClose();
    };
    el.addEventListener('cancel', onCancel);
    return () => el.removeEventListener('cancel', onCancel);
  }, [onClose]);
  return ref;
}

/** A titled element in a dialog describes through the dialog's own node, which no list item or cell of it holds. */
function DialogInner({ children }: { children: ReactNode }) {
  return (
    <div className="dialog__inner">
      <Descriptions>{children}</Descriptions>
    </div>
  );
}

export function Dialog({ open, onClose, title, subtitle, children, footer, width = 560 }: { children: ReactNode; footer?: ReactNode; onClose: () => void; open: boolean; subtitle?: ReactNode; title: ReactNode; width?: number }) {
  const ref = useModalDialog(open, onClose);
  const titleId = useId();
  return (
    <dialog
      ref={ref}
      className="dialog"
      aria-labelledby={titleId}
      style={{ width: `min(${width}px, calc(100vw - 32px))` }}
      onClick={(e) => {
        if (e.target === ref.current) onClose();
      }}
    >
      <DialogInner>
        <header className="dialog__head">
          <div>
            <h2 id={titleId} className="dialog__title">
              {title}
            </h2>
            {subtitle && <p className="dialog__subtitle">{subtitle}</p>}
          </div>
          <button type="button" className="dialog__close" onClick={onClose} aria-label="Close">
            <IconX size={16} />
          </button>
        </header>
        <div className="dialog__body">{children}</div>
        {footer && <footer className="dialog__foot">{footer}</footer>}
      </DialogInner>
    </dialog>
  );
}

export interface DialogGroup {
  body: ReactNode;
  /** Every field of the group is expert: it leaves the rail while the mode is off, unless it holds a refusal. */
  expertOnly?: boolean;
  /** Unique within the dialog; the rail button, the section and its heading carry it. */
  id: string;
  label: string;
  /** A word after the label on the rail, drawn aria-hidden and repeated in the entry's description: "needed". */
  mark?: string;
  /** The rail heading this group sits under; consecutive groups that share one form a run under it. */
  parent?: string;
  /** Marks the group and its parent on the rail while a refusal sits in the group. */
  refused?: boolean;
}

const GroupChoice = createContext<(id: string) => void>(() => {});

/** A hint link that switches the rail to another group and focuses its heading; reads the dialog's context. */
export function GroupLink({ id, children }: { children: ReactNode; id: string }) {
  const choose = useContext(GroupChoice);
  return (
    <button type="button" className="link-btn" onClick={() => choose(id)}>
      {children}
    </button>
  );
}

/** A hint link to a console page in a new tab, so the unsaved form stays open behind it; says so to a reader. */
export function PageLink({ to, children }: { children: ReactNode; to: string }) {
  return (
    <Link to={to} target="_blank" rel="noopener">
      {children}
      <span className="sr-only"> (opens in a new tab)</span>
    </Link>
  );
}

interface ExpertRegistry {
  drop: (id: string) => void;
  /** Whether the mode is on: with it, every wrapper renders its field. */
  on: boolean;
  set: (id: string, hidden: boolean) => void;
}

const ExpertGroup = createContext<ExpertRegistry | null>(null);

/**
 * Wraps one expert field of a dialog group. With the mode on, or while the field holds a refusal, it renders the
 * field; otherwise nothing, and the group's line counts it. Each wrapper registers itself, so the count can never
 * drift from the fields a group wraps. The box is `display: contents`, so a wrapped cell keeps its place in the grid.
 */
export function Expert({ children, refused = false }: { children: ReactNode; refused?: boolean }) {
  const registry = useContext(ExpertGroup);
  const id = useId();
  const shown = registry === null || registry.on || refused;
  useEffect(() => {
    if (registry === null) return;
    registry.set(id, !shown);
    return () => registry.drop(id);
  }, [id, registry, shown]);
  return shown ? <div className="expert">{children}</div> : null;
}

function DialogGroupPanel({ chosen, group, on, register, turnOn }: { chosen: string; group: DialogGroup; on: boolean; register: (id: string, el: HTMLElement | null) => void; turnOn: () => void }) {
  const [hidden, setHidden] = useState<Map<string, boolean>>(new Map());
  const set = useCallback((id: string, isHidden: boolean) => setHidden((m) => (m.get(id) === isHidden ? m : new Map(m).set(id, isHidden))), []);
  const drop = useCallback(
    (id: string) =>
      setHidden((m) => {
        if (!m.has(id)) return m;
        const next = new Map(m);
        next.delete(id);
        return next;
      }),
    [],
  );
  const registry = useMemo<ExpertRegistry>(() => ({ drop, on, set }), [drop, on, set]);
  const count = [...hidden.values()].filter(Boolean).length;
  const ref = useRef<HTMLElement | null>(null);
  // Set by the line's button: once the mode is on and the fields render, the group's first expert field takes focus.
  const revealing = useRef(false);
  useEffect(() => {
    if (!on || !revealing.current) return;
    revealing.current = false;
    ref.current?.querySelector<HTMLElement>('.expert input, .expert select, .expert textarea, .expert button')?.focus();
  }, [on]);
  return (
    <section
      className="dialog-group"
      aria-label={group.label}
      hidden={group.id !== chosen}
      ref={(el) => {
        ref.current = el;
        register(group.id, el);
      }}
    >
      <h3 className="dialog-group__title" tabIndex={-1}>
        {group.label}
      </h3>
      <ExpertGroup.Provider value={registry}>{group.body}</ExpertGroup.Provider>
      {count > 0 && (
        <p className="field__hint dialog-group__more">
          {/* The trailing space keeps the button's focus ring clear of the last word. */}
          {count === 1 ? '1 more setting in the ' : `${count} more settings in the `}
          <button
            type="button"
            className="link-btn dialog-group__more-link"
            aria-label="Turn on the expert view"
            onClick={() => {
              revealing.current = true;
              turnOn();
            }}
          >
            expert view
          </button>
        </p>
      )}
    </section>
  );
}

/**
 * A long form split into groups behind a rail, one group shown at a time. The rest stay mounted and hidden, so
 * their state and their refusal checks live on while another group is in view.
 */
export function GroupedDialog({
  open,
  onClose,
  title,
  groups,
  chosen,
  onChoose,
  railLabel,
  status,
  hold = null,
  held = hold !== null,
  initialGroup = null,
  footer,
  notice,
  width = 1040,
}: {
  /** The id of the group shown; the page owns it and resets it on open. */
  chosen: string;
  /** The footer's buttons, after the status line. */
  footer?: ReactNode;
  groups: DialogGroup[];
  /** The refusal's tone on the status line; defaults to whether `hold` is set, so a server refusal sets it alone. */
  held?: boolean;
  /** The group of the first refusal: draws "Go to <label>", which opens it and focuses its `[aria-invalid]` control. */
  hold?: string | null;
  /**
   * The group the page opens the dialog on from its address; focus moves to that group's heading once the page's
   * reset has chosen it. Null (Edit, Create) leaves focus where the browser's dialog focusing put it.
   */
  initialGroup?: string | null;
  /** Shown above whichever group is open: a refusal that is about the whole save, not one group's field. */
  notice?: ReactNode;
  /** The rail, a hint link and Go to call it; the page stores the id. */
  onChoose: (id: string) => void;
  onClose: () => void;
  open: boolean;
  /** The rail's accessible name: "Pool settings", "Key settings". */
  railLabel: string;
  /** The footer's status line, read in a `role="status"` element before the buttons. */
  status: ReactNode;
  title: ReactNode;
  width?: number;
}) {
  const ref = useModalDialog(open, onClose);
  const titleId = useId();
  const bodyRef = useRef<HTMLDivElement>(null);
  const sections = useRef(new Map<string, HTMLElement>());
  // A choice made through the rail, a hint link, Go to or the address, waiting for the page to show its group.
  const pending = useRef<{ focus: 'heading' | 'refused'; id: string } | null>(null);
  const wasOpen = useRef(false);
  const { expertView, setExpertView } = useAppSettings();
  // While the mode is off a group whose every field is expert has nothing plain to show, so it leaves the rail and
  // its section with it; a refused one stays, since a refusal never hides. Every path that chooses such a group turns
  // the mode on in the same update, so no render shows a chosen group the rail lacks.
  const shown = expertView ? groups : groups.filter((g) => !g.expertOnly || g.refused);
  const expertOnlyOff = (id: string) => !expertView && groups.some((g) => g.id === id && g.expertOnly && !g.refused);
  // A reset or a first group can name a group the filter took off, which would leave the dialog on a hidden panel
  // with no rail entry current, so the first shown group takes its place. The rail and the panels draw it in this
  // render: a page's reset that runs after the hand-off and names the hidden group again leaves the page's state as
  // it was, and React renders nothing more.
  const chosenShown = shown.some((g) => g.id === chosen);
  const lost = open && shown.length > 0 && !chosenShown ? shown[0].id : null;
  const current = lost ?? chosen;
  useEffect(() => {
    if (lost !== null) onChoose(lost);
  }, [lost, onChoose]);
  const register = useCallback((id: string, el: HTMLElement | null) => {
    if (el) sections.current.set(id, el);
    else sections.current.delete(id);
  }, []);
  const focusIn = (id: string, focus: 'heading' | 'refused') => {
    const section = sections.current.get(id);
    const control = focus === 'refused' ? section?.querySelector<HTMLElement>('[aria-invalid="true"]') : null;
    (control ?? section?.querySelector<HTMLElement>('.dialog-group__title'))?.focus();
  };
  const choose = (id: string, focus: 'heading' | 'refused' = 'heading') => {
    if (expertOnlyOff(id)) setExpertView(true);
    if (id === chosen && chosenShown) {
      focusIn(id, focus);
      return;
    }
    pending.current = { focus, id };
    if (id !== chosen) onChoose(id);
  };
  // Every group shown starts the body at its top. Focus moves only for a choice, to the group's heading or, for Go
  // to, its refused control, and for the group the address named: the page's reset chooses that group after this
  // effect has run, so the choice waits here until `chosen` matches it. A group the mode hid mounts its section only in
  // the render after the mode turns on, so the choice also waits for the group to be shown, even when the page already
  // holds it. A group the page sets itself on open leaves focus where the browser's dialog focusing put it.
  useEffect(() => {
    if (!open) {
      wasOpen.current = false;
      pending.current = null;
      return;
    }
    if (!wasOpen.current) {
      wasOpen.current = true;
      if (initialGroup !== null) {
        if (expertOnlyOff(initialGroup)) setExpertView(true);
        pending.current = { focus: 'heading', id: initialGroup };
      }
    }
    bodyRef.current?.scrollTo?.({ top: 0 });
    const choice = pending.current;
    if (choice === null || choice.id !== chosen || !chosenShown) return;
    pending.current = null;
    focusIn(chosen, choice.focus);
    // Only a choice moves focus; the helpers and the body ref change without one.
  }, [open, chosen, chosenShown, initialGroup]);
  // The notice sits above the groups and is about the whole save, so one raised by a save pressed low in a long group
  // would land out of view; the body returns to its top when one appears.
  const noticed = Boolean(notice);
  useEffect(() => {
    if (noticed) bodyRef.current?.scrollTo?.({ top: 0 });
  }, [noticed]);
  const runs: { groups: DialogGroup[]; parent?: string }[] = [];
  for (const g of shown) {
    const last = runs[runs.length - 1];
    if (last && last.parent !== undefined && last.parent === g.parent) last.groups.push(g);
    else runs.push({ groups: [g], parent: g.parent });
  }
  const flat = shown.every((g) => g.parent === undefined);
  const labelOf = (id: string) => groups.find((g) => g.id === id)?.label ?? id;
  // The dot and the mark are decoration and their words sit outside the button, so the button's name stays its label
  // and a lookup by name or by the label span never meets them.
  const railId = useId();
  const noteId = (key: string) => `${railId}-${key}-refused`;
  const markId = (key: string) => `${railId}-${key}-mark`;
  const describedBy = (g: DialogGroup) => [g.mark ? markId(g.id) : null, g.refused ? noteId(g.id) : null].filter(Boolean).join(' ') || undefined;
  const dot = <span className="dialog-rail__dot" aria-hidden />;
  const note = (key: string) => (
    <span id={noteId(key)} className="sr-only">
      holds a refusal
    </span>
  );

  return (
    <GroupChoice.Provider value={(id) => choose(id)}>
      <dialog
        ref={ref}
        className="dialog dialog--grouped"
        aria-labelledby={titleId}
        style={{ '--dialog-w': `${width}px` } as CSSProperties}
        onClick={(e) => {
          if (e.target === ref.current) onClose();
        }}
      >
        <DialogInner>
          <header className="dialog__head">
            <div>
              <h2 id={titleId} className="dialog__title">
                {title}
              </h2>
            </div>
            <button type="button" className="dialog__close" onClick={onClose} aria-label="Close">
              <IconX size={16} />
            </button>
          </header>
          <div className="dialog__rail-split">
            <nav className={`dialog-rail ${flat ? 'dialog-rail--flat' : ''}`} aria-label={railLabel}>
              {runs.map((run, i) => {
                const isChosen = run.groups.some((g) => g.id === current);
                const refused = run.groups.some((g) => g.refused);
                return (
                  <div key={run.parent ?? run.groups[0].id} className={`dialog-rail__run ${run.parent ? 'has-parent' : ''} ${isChosen ? 'is-chosen' : ''}`}>
                    {run.parent && (
                      <>
                        <button type="button" className="dialog-rail__parent" data-parent={run.parent} aria-describedby={refused ? noteId(`parent-${i}`) : undefined} onClick={() => choose(run.groups[0].id)}>
                          <span className="dialog-rail__label">{run.parent}</span>
                          {refused && dot}
                        </button>
                        {refused && note(`parent-${i}`)}
                      </>
                    )}
                    {run.groups.map((g) => (
                      <Fragment key={g.id}>
                        <button type="button" className="dialog-rail__group" data-group={g.id} aria-current={g.id === current ? 'true' : undefined} aria-describedby={describedBy(g)} onClick={() => choose(g.id)}>
                          <span className="dialog-rail__label">{g.label}</span>
                          {g.mark && (
                            <span className="dialog-rail__mark" aria-hidden>
                              {g.mark}
                            </span>
                          )}
                          {g.refused && dot}
                        </button>
                        {g.mark && (
                          <span id={markId(g.id)} className="sr-only">
                            {g.mark}
                          </span>
                        )}
                        {g.refused && note(g.id)}
                      </Fragment>
                    ))}
                  </div>
                );
              })}
              <span className="dialog-rail__break" aria-hidden />
            </nav>
            <div className="dialog__body dialog__body--grouped" ref={bodyRef}>
              {notice && <div style={{ paddingTop: 18 }}>{notice}</div>}
              {shown.map((g) => (
                <DialogGroupPanel key={g.id} chosen={current} group={g} on={expertView} register={register} turnOn={() => setExpertView(true)} />
              ))}
            </div>
          </div>
          <footer className="dialog__foot dialog__foot--status">
            <p className={`dialog__status ${held ? 'is-held' : ''}`} role="status">
              {status}
              {hold && (
                <button type="button" className="link-btn" onClick={() => choose(hold, 'refused')}>
                  Go to {labelOf(hold)}
                </button>
              )}
            </p>
            <div className="dialog__actions">{footer}</div>
          </footer>
        </DialogInner>
      </dialog>
    </GroupChoice.Provider>
  );
}

export function ConfirmDialog({ open, onClose, onConfirm, title, body, confirmLabel = 'Confirm', danger = false }: { body: ReactNode; confirmLabel?: string; danger?: boolean; onClose: () => void; onConfirm: () => void; open: boolean; title: string }) {
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={title}
      width={440}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant={danger ? 'danger' : 'primary'}
            onClick={() => {
              onConfirm();
              onClose();
            }}
          >
            {confirmLabel}
          </Button>
        </>
      }
    >
      <p className="secondary">{body}</p>
    </Dialog>
  );
}

const IN_FLOW: MenuPlacement = { kind: 'down' };
const CLIPPING = new Set(['auto', 'clip', 'hidden', 'scroll']);

/**
 * A row's actions behind a trigger. `align` names the trigger edge the list lines up with: a menu in a row's
 * first column opens rightward, since a list aligned to its right edge would run past the table wrap's left edge,
 * which clips it.
 */
export function Menu({ align = 'right', items, label = 'Actions', onOpenChange }: { align?: MenuAlign; items: { danger?: boolean; disabled?: boolean; label: string; onClick: () => void; title?: string }[]; label?: string; onOpenChange?: (open: boolean) => void }) {
  const [open, setOpen] = useState(false);
  // A page that holds its refresh while a menu is open counts the open ones, so a closed menu says nothing as it
  // mounts and one that unmounts open still reports itself closed.
  const openChange = useRef(onOpenChange);
  openChange.current = onOpenChange;
  useEffect(() => {
    if (!open) return;
    openChange.current?.(true);
    return () => openChange.current?.(false);
  }, [open]);
  const [placement, setPlacement] = useState<MenuPlacement>(IN_FLOW);
  const ref = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  // Where the trigger rested once the list floated, so a fixed layer can tell whether the page has moved under it.
  const anchor = useRef<DOMRect | null>(null);
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      const target = e.target as Node;
      if (!ref.current?.contains(target) && !listRef.current?.contains(target)) setOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    return () => document.removeEventListener('mousedown', onDoc);
  }, [open]);
  // Measured once per opening, after the list has drawn below its trigger and before the browser paints it. A table
  // wrap that scrolls sideways clips on both axes, so its edges bound the list as much as the viewport's do. A row
  // too short for the list on either side inside its wrap (a table filtered to one row) moves the list out of the
  // wrap, into a fixed layer at the trigger.
  useLayoutEffect(() => {
    if (!open) {
      setPlacement(IN_FLOW);
      return;
    }
    if (!ref.current) return;
    const trigger = ref.current.getBoundingClientRect();
    // A wrap bounds the list only while it clips: the pool dialog's key table sets its wrap `overflow: visible`. The
    // clipping values are named because a page without styles, as in the smoke run, reads the property empty.
    const wrap = ref.current.closest<HTMLElement>('.table-wrap');
    const clip = wrap && CLIPPING.has(getComputedStyle(wrap).overflowY) ? wrap : null;
    const next = menuPlacement(trigger, clip, { height: listRef.current?.offsetHeight ?? 0, width: listRef.current?.offsetWidth ?? 0 }, window.innerHeight, align);
    setPlacement(next.kind === 'down' ? IN_FLOW : next);
  }, [open]);
  const floating = open && placement.kind === 'float';
  // The list in flow can overflow a clipping wrap and give it a scrollbar that pushes the trigger aside; leaving for
  // the portal takes the scrollbar away. So the layer is placed again from the trigger at rest, measured once it
  // floats, and that measurement is where the close-on-move checks below start.
  useLayoutEffect(() => {
    if (!floating || !ref.current) return;
    const trigger = ref.current.getBoundingClientRect();
    anchor.current = trigger;
    const at = floatAt(trigger, { height: listRef.current?.offsetHeight ?? 0, width: listRef.current?.offsetWidth ?? 0 }, window.innerHeight, align);
    setPlacement((p) => (p.kind === 'float' && p.left === at.left && p.top === at.top ? p : at));
  }, [floating]);
  // A fixed layer stays where it was drawn while the page moves under it, so it closes rather than float beside
  // another row. A scroll is read by whether it moved the trigger: the scroll that brought the row into view can
  // report after the click that opened the list, and a scroll inside the list or in another pane moves nothing.
  const moved = () => {
    const at = ref.current?.getBoundingClientRect();
    return !at || !anchor.current || at.top !== anchor.current.top || at.left !== anchor.current.left;
  };
  // A refresh that re-sorts the rows moves the trigger with no scroll or resize to report it, so every render of the
  // open layer reads the trigger by the same rule.
  useLayoutEffect(() => {
    if (floating && moved()) setOpen(false);
  });
  // Focus enters the list as it opens, so the list's keys reach it, and returns to the trigger when the list closes;
  // focus already elsewhere by then stays. A list that floats draws in flow first and moves into the portal, taking the
  // focused item with it, so the portalled list takes focus again: the portal draws it after every other control on
  // the page, where Tab from the trigger never reaches it.
  useEffect(() => {
    if (!open) return;
    const list = listRef.current;
    const trigger = triggerRef.current;
    list?.querySelector<HTMLButtonElement>('button:not(:disabled)')?.focus({ preventScroll: true });
    return () => {
      const at = document.activeElement;
      if (!at || at === document.body || list?.contains(at)) trigger?.focus({ preventScroll: true });
    };
  }, [open, floating]);
  // The keys act while the list is open, from the list or from the trigger a Shift+Tab leaves focus on. Escape stops
  // at the menu, since a dialog around it closes on the same key.
  const onKeys = (e: KeyboardEvent<HTMLDivElement>) => {
    if (!open) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      triggerRef.current?.focus({ preventScroll: true });
      setOpen(false);
      return;
    }
    const items = [...(listRef.current?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]:not(:disabled)') ?? [])];
    const at = items.indexOf(e.target as HTMLButtonElement);
    const to =
      e.key === 'ArrowDown'
        ? items[(at + 1) % items.length]
        : e.key === 'ArrowUp'
          ? items[(at <= 0 ? items.length : at) - 1]
          : e.key === 'Home'
            ? items[0]
            : e.key === 'End'
              ? items[items.length - 1]
              : undefined;
    if (!to) return;
    e.preventDefault();
    to.focus();
  };
  // Safari, and Firefox on macOS, give a clicked button no focus, so a click on an item blurs the list naming no
  // element; a blur closes the list only when it names an element outside the list and the trigger, whose own click
  // toggles the list.
  const closeOnLeave = (e: FocusEvent<HTMLDivElement>) => {
    const to = e.relatedTarget;
    if (to && !listRef.current?.contains(to) && !triggerRef.current?.contains(to)) setOpen(false);
  };
  useEffect(() => {
    if (!floating) return;
    const onScroll = () => {
      if (moved()) setOpen(false);
    };
    const onResize = () => setOpen(false);
    window.addEventListener('scroll', onScroll, true);
    window.addEventListener('resize', onResize);
    return () => {
      window.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('resize', onResize);
    };
  }, [floating]);
  const list = open && (
    <div
      ref={listRef}
      className={`menu__list${placement.kind === 'down' ? '' : ` menu__list--${placement.kind}`}${align === 'left' ? ' menu__list--left' : ''}`}
      role="menu"
      style={placement.kind === 'float' ? { left: placement.left, top: placement.top } : undefined}
      onBlur={closeOnLeave}
    >
      {items.map((item) => (
        <button
          key={item.label}
          type="button"
          role="menuitem"
          className={item.danger ? 'is-danger' : ''}
          disabled={item.disabled}
          title={item.title}
          onClick={() => {
            setOpen(false);
            item.onClick();
          }}
        >
          {item.label}
        </button>
      ))}
    </div>
  );
  return (
    <div className="menu" ref={ref} onKeyDown={onKeys}>
      <button type="button" ref={triggerRef} className="menu__trigger" aria-haspopup="menu" aria-expanded={open} aria-label={label} onClick={() => setOpen((v) => !v)}>
        <span aria-hidden>···</span>
      </button>
      {/* A modal dialog sits in the top layer, so a layer portalled to the body would draw under it. */}
      {floating && ref.current ? createPortal(list, ref.current.closest('dialog[open]') ?? document.body) : list}
    </div>
  );
}

export function KeyValue({ rows, className = '' }: { className?: string; rows: { label: string; mono?: boolean; title?: string; value: ReactNode }[] }) {
  const base = useId();
  // The node follows the dd inside the row, so the dd's own text stays the value alone.
  return (
    <dl className={`kv ${className}`}>
      {rows.map((r, i) => (
        <div key={r.label} className="kv__row">
          <dt>{r.label}</dt>
          <dd className={r.mono ? 'mono' : ''} title={r.title} aria-describedby={r.title ? `${base}-${i}` : undefined}>
            {r.value}
          </dd>
          {r.title && (
            <span id={`${base}-${i}`} className="sr-only" data-descriptions="">
              {r.title}
            </span>
          )}
        </div>
      ))}
    </dl>
  );
}

/** Draws the first toast of the queue for its dwell, then hands its id to `onDone` so the next one shows. */
export function Toast({ onDone, queue }: { onDone: (id: number) => void; queue: ToastItem[] }) {
  // Keyed on the item rather than the callback, so a fresh callback on a re-render does not restart the dwell.
  const done = useRef(onDone);
  done.current = onDone;
  const showing = queue[0];
  const id = showing?.id;
  const dwellMs = showing?.dwellMs;
  useEffect(() => {
    if (id === undefined || dwellMs === undefined) return;
    const timer = clock.setTimeout(() => done.current(id), dwellMs);
    return () => clock.clearTimeout(timer);
  }, [id, dwellMs]);
  if (!showing) return null;
  return (
    <div className="toast" role="status">
      {showing.message}
    </div>
  );
}
