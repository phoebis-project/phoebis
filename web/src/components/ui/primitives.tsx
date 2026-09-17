import { useEffect, useId, useRef, useState, type ButtonHTMLAttributes, type InputHTMLAttributes, type ReactNode, type SelectHTMLAttributes } from 'react';
import type { BadgeTone } from '@/lib/quota';
import { scrollToSection, useScrollSpy } from '@/lib/scroll';
import { IconCheck, IconChevron, IconCopy, IconSearch, IconX } from './icons';

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

/** The hint is a block outside the label, so label and hint never share a line. */
export function Field({ label, hint, children, inline = false }: { children: ReactNode; hint?: ReactNode; inline?: boolean; label: ReactNode }) {
  return (
    <div className={`field ${inline ? 'field--inline' : ''}`}>
      <label className="field__control">
        <span className="field__label">{label}</span>
        {children}
      </label>
      {hint && <p className="field__hint">{hint}</p>}
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
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const id = window.setTimeout(() => setCopied(false), 1600);
    return () => window.clearTimeout(id);
  }, [copied]);
  return (
    <Button
      size={size}
      variant="ghost"
      icon={copied ? <IconCheck size={14} /> : <IconCopy size={14} />}
      onClick={() => {
        navigator.clipboard?.writeText(text).catch(() => undefined);
        setCopied(true);
      }}
    >
      {copied ? 'Copied' : label}
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

export interface DialogSection {
  body: ReactNode;
  /** Unique within the dialog; combined with a generated prefix to form the DOM id. */
  id: string;
  label: string;
}

/**
 * A long form split into a section list and one scrolling body. Clicking a
 * section scrolls the body to it; scrolling the body moves the highlight.
 */
export function SectionedDialog({ open, onClose, title, subtitle, sections, footer, width = 860 }: { footer?: ReactNode; onClose: () => void; open: boolean; sections: DialogSection[]; subtitle?: ReactNode; title: ReactNode; width?: number }) {
  const ref = useModalDialog(open, onClose);
  const titleId = useId();
  const prefix = useId().replace(/:/g, '');
  const bodyRef = useRef<HTMLDivElement>(null);
  const domId = (id: string) => `${prefix}-${id}`;
  const ids = sections.map((s) => domId(s.id));
  const [active, select] = useScrollSpy(ids, { enabled: open, offset: 12, root: bodyRef });
  const goTo = (id: string) => {
    select(id);
    scrollToSection(id, { offset: 12, root: bodyRef });
  };

  return (
    <dialog
      ref={ref}
      className="dialog dialog--sectioned"
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
        <div className="dialog__split">
          <nav className="dialog-nav" aria-label="Sections">
            {sections.map((s) => (
              <button
                key={s.id}
                type="button"
                className={`dialog-nav__link ${domId(s.id) === active ? 'is-active' : ''}`}
                aria-current={domId(s.id) === active ? 'true' : undefined}
                onClick={() => goTo(domId(s.id))}
              >
                {s.label}
              </button>
            ))}
          </nav>
          <div className="dialog__body" ref={bodyRef}>
            {sections.map((s) => (
              <section key={s.id} id={domId(s.id)} className="dialog-section" aria-label={s.label}>
                <h3 className="dialog-section__title">{s.label}</h3>
                {s.body}
              </section>
            ))}
          </div>
        </div>
        {footer && <footer className="dialog__foot">{footer}</footer>}
      </div>
    </dialog>
  );
}

/** A titled group that starts collapsed and keeps its state while mounted. */
export function Collapsible({ title, hint, children, defaultOpen = false }: { children: ReactNode; defaultOpen?: boolean; hint?: ReactNode; title: ReactNode }) {
  const [open, setOpen] = useState(defaultOpen);
  const bodyId = useId();
  return (
    <div className={`collapsible ${open ? 'is-open' : ''}`}>
      <button type="button" className="collapsible__summary" aria-expanded={open} aria-controls={bodyId} onClick={() => setOpen((v) => !v)}>
        <IconChevron size={14} className="collapsible__chevron" />
        <span className="collapsible__title">{title}</span>
        {hint && <span className="collapsible__hint">{hint}</span>}
      </button>
      {open && (
        <div className="collapsible__body" id={bodyId}>
          {children}
        </div>
      )}
    </div>
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
