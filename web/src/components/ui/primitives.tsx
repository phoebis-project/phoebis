import { createContext, Fragment, useContext, useEffect, useId, useRef, useState, type ButtonHTMLAttributes, type CSSProperties, type HTMLAttributes, type InputHTMLAttributes, type ReactNode, type SelectHTMLAttributes } from 'react';
import { Link } from 'react-router-dom';
import type { BadgeTone } from '@/lib/quota';
import { IconCheck, IconChevron, IconCopy, IconSearch, IconWarning, IconX } from './icons';

export function Badge({ tone = 'neutral', children, dot = false, title }: { children: ReactNode; dot?: boolean; title?: string; tone?: BadgeTone }) {
  return (
    <span className={`badge badge--${tone}`} title={title}>
      {dot && <span className="badge__dot" aria-hidden />}
      {children}
    </span>
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

export function Segmented<T extends string>({ value, options, onChange, ariaLabel }: { ariaLabel: string; onChange: (v: T) => void; options: { label: ReactNode; value: T }[]; value: T }) {
  return (
    <div className="segmented" role="group" aria-label={ariaLabel}>
      {options.map((o) => (
        <button key={o.value} type="button" className={o.value === value ? 'is-active' : ''} aria-pressed={o.value === value} onClick={() => onChange(o.value)}>
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function Card({ children, className = '', title, subtitle, actions, padded = true }: { actions?: ReactNode; children: ReactNode; className?: string; padded?: boolean; subtitle?: ReactNode; title?: ReactNode }) {
  return (
    <section className={`card ${padded ? 'card--padded' : ''} ${className}`}>
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
export function Field({ label, hint, children, inline = false, beside = false, plain = false, className, ...rest }: Omit<HTMLAttributes<HTMLDivElement>, 'children'> & { beside?: boolean; children: ReactNode; hint?: ReactNode; inline?: boolean; label: ReactNode; plain?: boolean }) {
  const Frame = plain ? 'div' : 'label';
  const text = <span className="field__label">{label}</span>;
  return (
    <div {...rest} className={`field ${inline ? 'field--inline' : ''} ${beside ? 'field--beside' : ''} ${className ?? ''}`}>
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

export function Tabs<T extends string>({ value, onChange, tabs }: { onChange: (v: T) => void; tabs: { count?: number; label: string; value: T }[]; value: T }) {
  return (
    <div className="tabs" role="tablist">
      {tabs.map((t) => (
        <button key={t.value} type="button" role="tab" aria-selected={t.value === value} className={t.value === value ? 'is-active' : ''} onClick={() => onChange(t.value)}>
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
      <div className="dialog__inner">
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
      </div>
    </dialog>
  );
}

export interface DialogGroup {
  body: ReactNode;
  /** Unique within the dialog; the rail button, the section and its heading carry it. */
  id: string;
  label: string;
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
  footer,
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
  // A choice made through the rail, a hint link or Go to, waiting for the page to show its group.
  const pending = useRef<{ focus: 'heading' | 'refused'; id: string } | null>(null);
  const focusIn = (id: string, focus: 'heading' | 'refused') => {
    const section = sections.current.get(id);
    const control = focus === 'refused' ? section?.querySelector<HTMLElement>('[aria-invalid="true"]') : null;
    (control ?? section?.querySelector<HTMLElement>('.dialog-group__title'))?.focus();
  };
  const choose = (id: string, focus: 'heading' | 'refused' = 'heading') => {
    if (id === chosen) {
      focusIn(id, focus);
      return;
    }
    pending.current = { focus, id };
    onChoose(id);
  };
  // Every group shown starts the body at its top. Only a choice moves focus, to the group's heading or, for Go to,
  // its refused control; a group the page sets itself, such as its reset when the dialog opens, leaves focus where
  // the browser put it, so a field's autoFocus still wins.
  useEffect(() => {
    bodyRef.current?.scrollTo?.({ top: 0 });
    const choice = pending.current;
    pending.current = null;
    if (open && choice?.id === chosen) focusIn(chosen, choice.focus);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, chosen]);
  const runs: { groups: DialogGroup[]; parent?: string }[] = [];
  for (const g of groups) {
    const last = runs[runs.length - 1];
    if (last && last.parent !== undefined && last.parent === g.parent) last.groups.push(g);
    else runs.push({ groups: [g], parent: g.parent });
  }
  const flat = groups.every((g) => g.parent === undefined);
  const labelOf = (id: string) => groups.find((g) => g.id === id)?.label ?? id;
  // The dot is decoration and the announcement sits outside the button, so the button's name stays its label and a
  // lookup by name or by the label span never meets the refusal's words.
  const railId = useId();
  const noteId = (key: string) => `${railId}-${key}-refused`;
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
        <div className="dialog__inner">
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
                const isChosen = run.groups.some((g) => g.id === chosen);
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
                        <button type="button" className="dialog-rail__group" data-group={g.id} aria-current={g.id === chosen ? 'true' : undefined} aria-describedby={g.refused ? noteId(g.id) : undefined} onClick={() => choose(g.id)}>
                          <span className="dialog-rail__label">{g.label}</span>
                          {g.refused && dot}
                        </button>
                        {g.refused && note(g.id)}
                      </Fragment>
                    ))}
                  </div>
                );
              })}
              <span className="dialog-rail__break" aria-hidden />
            </nav>
            <div className="dialog__body dialog__body--grouped" ref={bodyRef}>
              {groups.map((g) => (
                <section
                  key={g.id}
                  className="dialog-group"
                  aria-label={g.label}
                  hidden={g.id !== chosen}
                  ref={(el) => {
                    if (el) sections.current.set(g.id, el);
                    else sections.current.delete(g.id);
                  }}
                >
                  <h3 className="dialog-group__title" tabIndex={-1}>
                    {g.label}
                  </h3>
                  {g.body}
                </section>
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
        </div>
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

export function Menu({ items, label = 'Actions' }: { items: { danger?: boolean; disabled?: boolean; label: string; onClick: () => void; title?: string }[]; label?: string }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    return () => document.removeEventListener('mousedown', onDoc);
  }, [open]);
  return (
    <div className="menu" ref={ref}>
      <button type="button" className="menu__trigger" aria-haspopup="menu" aria-expanded={open} aria-label={label} onClick={() => setOpen((v) => !v)}>
        <span aria-hidden>···</span>
      </button>
      {open && (
        <div className="menu__list" role="menu">
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
      )}
    </div>
  );
}

export function KeyValue({ rows, className = '' }: { className?: string; rows: { label: string; mono?: boolean; title?: string; value: ReactNode }[] }) {
  return (
    <dl className={`kv ${className}`}>
      {rows.map((r) => (
        <div key={r.label} className="kv__row">
          <dt>{r.label}</dt>
          <dd className={r.mono ? 'mono' : ''} title={r.title}>
            {r.value}
          </dd>
        </div>
      ))}
    </dl>
  );
}

export function Toast({ message, onDone }: { message: string | null; onDone: () => void }) {
  useEffect(() => {
    if (!message) return;
    const id = window.setTimeout(onDone, 2400);
    return () => window.clearTimeout(id);
  }, [message, onDone]);
  if (!message) return null;
  return (
    <div className="toast" role="status">
      {message}
    </div>
  );
}
