import { createContext, forwardRef, Fragment, useCallback, useContext, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type ButtonHTMLAttributes, type CSSProperties, type HTMLAttributes, type InputHTMLAttributes, type KeyboardEvent, type MouseEvent as ReactMouseEvent, type ReactNode, type SelectHTMLAttributes } from 'react';
import { createPortal } from 'react-dom';
import { Link, useRouteError } from 'react-router-dom';
import type { ToastItem } from '@/app/session';
import { useAppSettings } from '@/app/settings';
import { clock } from '@/lib/clock';
import { fmtAbsolute, fmtDate, plural } from '@/lib/format';
import { typedNumber, typedOf, type TypedField } from '@/lib/forms';
import { fnv1a } from '@/lib/hash';
import { useKeyedMap, useLatest } from '@/lib/hooks';
import { floatAt, menuPlacement, type MenuAlign, type MenuPlacement } from '@/lib/menu';
import type { BadgeTone } from '@/lib/quota';
import { IconCheck, IconChevron, IconCopy, IconSearch, IconWarning, IconX } from './icons';
import { walkFocus } from './OptionList';
import { useHold, useLeaveOnMove, useOutsidePress } from './overlay';
import { blurLeaves, TABBABLE } from './Popover';

const DescriptionHost = createContext<{ base: string; register: (text: string) => () => void } | null>(null);
// The id derives from the text, so a described element points at its entry in its first render, before the entry draws.
const entryId = (base: string, text: string) => `${base}-${fnv1a(text).toString(16).padStart(8, '0')}`;

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
      {/* Marked so a whole-page text read can leave out the copies no sighted reader sees. Hidden rather than clipped:
          a reader still hears a node an `aria-describedby` names, and no scroll can bring a hidden node into view. */}
      {counts.size > 0 && (
        <div hidden data-descriptions="">
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
function useDescription(text: string | undefined): { id: string; sibling: boolean } | null {
  const host = useContext(DescriptionHost);
  const own = useId();
  useEffect(() => (host && text ? host.register(text) : undefined), [host, text]);
  if (!text) return null;
  return host ? { id: entryId(host.base, text), sibling: false } : { id: own, sibling: true };
}

/** The id `text` is described by through the card's description host, undefined outside every host, where the
 *  caller's title alone describes its element. */
export function useHostedDescription(text: string | undefined): string | undefined {
  const described = useDescription(text);
  return described && !described.sibling ? described.id : undefined;
}

/** A table cell's hover, described through the host. Outside one the title alone describes it. */
export function DescribedCell({ children, className, head = false, title }: { children: ReactNode; className?: string; head?: boolean; title: string | undefined }) {
  // A node after the cell would stand in the row, where only cells may.
  const hosted = useHostedDescription(title);
  const Tag = head ? 'th' : 'td';
  return (
    <Tag className={className} title={title} aria-describedby={hosted}>
      {children}
    </Tag>
  );
}

/**
 * Describes the element `children` draws by `text` for a reader: through the card's description host where one
 * stands, else through a hidden node after the element, as the attempt trail's is. `hostedOnly` keeps a chip's hover
 * to its title outside a host, since a node beside a chip would join the text of the cell or the row that holds it.
 */
export function Described({ children, hostedOnly = false, text }: { children: (describedBy: string | undefined) => ReactNode; hostedOnly?: boolean; text: string | undefined }) {
  const described = useDescription(text);
  const hosted = described && !described.sibling ? described.id : null;
  const own = described?.sibling && !hostedOnly ? described.id : null;
  return (
    <>
      {children(hosted ?? own ?? undefined)}
      {own !== null && (
        <span id={own} hidden data-descriptions="">
          {text}
        </span>
      )}
    </>
  );
}

export function Badge({ tone = 'neutral', children, dot = false, title, describe = true }: { children: ReactNode; describe?: boolean; dot?: boolean; title?: string; tone?: BadgeTone }) {
  // A title that only repeats the badge's text would have a reader say it twice under a host, so such a caller keeps
  // the hover and turns the description off.
  // Outside every host the title alone describes the badge: a node beside it would join its parent's text.
  const hosted = useHostedDescription(describe ? title : undefined);
  return (
    <span className={`badge badge--${tone}`} title={title} aria-describedby={hosted}>
      {dot && <span className="badge__dot" aria-hidden />}
      {children}
    </span>
  );
}

/** A badge that opens a page: it describes itself through its host as a `Badge` does. */
export function BadgeLink({ tone = 'neutral', children, title, to }: { children: ReactNode; title?: string; to: string; tone?: BadgeTone }) {
  const hosted = useHostedDescription(title);
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

type InputProps = InputHTMLAttributes<HTMLInputElement>;

/** The one button primitive, forwarding its ref so a popover can anchor on it and return focus to it. */
export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button({ variant = 'secondary', size = 'md', icon, children, className = '', ...rest }, ref) {
  return (
    <button ref={ref} type="button" className={`btn btn--${variant} btn--${size} ${className}`} {...rest}>
      {icon && <span className="btn__icon">{icon}</span>}
      {children}
    </button>
  );
});

/**
 * A list row's Move up, Move down and Remove buttons, `noun` naming the row in their labels ("Move rule 2 up"), or
 * `name` where the row carries a name of its own ("Move strict up").
 */
export function RowActions({ count, index, name, noun, onMove, onRemove }: { count: number; index: number; name?: string; noun: string; onMove: (from: number, to: number) => void; onRemove: (index: number) => void }) {
  const label = name ?? `${noun} ${index + 1}`;
  return (
    <>
      <Button size="sm" variant="ghost" className="row-actions__up" icon={<IconChevron size={14} />} aria-label={`Move ${label} up`} disabled={index === 0} onClick={() => onMove(index, index - 1)} />
      <Button size="sm" variant="ghost" icon={<IconChevron size={14} />} aria-label={`Move ${label} down`} disabled={index === count - 1} onClick={() => onMove(index, index + 1)} />
      <Button size="sm" variant="ghost" icon={<IconX size={14} />} aria-label={`Remove ${label}`} onClick={() => onRemove(index)} />
    </>
  );
}

/** A caller that passes `active` is a toggle and reads as pressed or not, and a one-shot action passes none. */
export function Pill({ children, active, onClick, disabled, title, icon }: { active?: boolean; children: ReactNode; disabled?: boolean; icon?: ReactNode; onClick?: () => void; title?: string }) {
  return (
    <button type="button" className={`pill ${active ? 'is-active' : ''}`} onClick={onClick} disabled={disabled} title={title} aria-pressed={typeof active === 'boolean' ? active : undefined}>
      {icon}
      {children}
    </button>
  );
}

/** An option's `title` replaces the tooltip its `name` sets, so a disabled option says why without a screen reader
 *  hearing the reason as its name. */
export function Segmented<T extends string>({ value, options, onChange, ariaLabel, title }: { ariaLabel: string; onChange: (v: T) => void; options: { disabled?: boolean; label: ReactNode; name?: string; title?: string; value: T }[]; title?: string; value: T }) {
  return (
    <div className="segmented" role="group" aria-label={ariaLabel} title={title}>
      {options.map((o) => (
        <button key={o.value} type="button" className={o.value === value ? 'is-active' : ''} aria-label={o.name} title={o.title ?? o.name} aria-pressed={o.value === value} disabled={o.disabled} onClick={() => onChange(o.value)}>
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

export function Meter({ value, tone, label, mark, thin = false, title }: { label?: string; /** A share of the track, 0 to 1, where a thin vertical mark is drawn. None without it. */ mark?: number; thin?: boolean; title?: string; tone: 'accent' | 'good' | 'low' | 'warn'; value: number }) {
  const pct = Math.round(Math.max(0, Math.min(1, value)) * 1000) / 10;
  return (
    <div className={`meter meter--${tone} ${thin ? 'meter--thin' : ''} ${mark !== undefined ? 'meter--marked' : ''}`} role="meter" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct} aria-label={label} title={title}>
      <div className="meter__fill" style={{ width: `${pct}%` }} />
      {mark !== undefined && <span className="meter__mark" style={{ left: `${mark * 100}%` }} aria-hidden />}
    </div>
  );
}

/** A meter for a window that holds no share: the whole track in the absent ink, with nothing for a reader to announce. */
export function MeterAbsent() {
  return (
    <div className="meter meter--thin meter--absent" role="presentation">
      <div className="meter__fill" style={{ width: '100%' }} />
    </div>
  );
}

export function Skeleton({ width = '100%', height = 12, className = '' }: { className?: string; height?: number; width?: number | string }) {
  return <span className={`skeleton ${className}`} style={{ width, height }} aria-hidden />;
}

export function SkeletonRows({ colSpan, count }: { colSpan: number; count: number }) {
  return (
    <>
      {Array.from({ length: count }, (_, i) => (
        <tr key={i} className="table__detail">
          <td colSpan={colSpan}>
            <Skeleton />
          </td>
        </tr>
      ))}
    </>
  );
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
 * The body carries the reason the call gave, and the route it came from is in the shell's toast. `retry` draws the
 * small Try again button in the action's place.
 */
export function ErrorState({ title = 'Could not load', body, action, retry }: { action?: ReactNode; body?: ReactNode; retry?: () => void; title?: string }) {
  return (
    <div className="empty empty--error" role="alert">
      <p className="empty__title">{title}</p>
      {body && <p className="empty__body">{body}</p>}
      {retry ? (
        <Button size="sm" onClick={retry}>
          Try again
        </Button>
      ) : (
        action
      )}
    </div>
  );
}

/**
 * A failed read named above content the other reads still draw, where `ErrorState` would stand in for the content.
 * `className` places it on its page.
 */
export function ReadNotice({ className, error, name, retry }: { className: string; error: string | null; name: string; retry: () => void }) {
  return (
    <p className={className} role="status">
      Could not load the {name}: {error}{' '}
      <Button size="sm" onClick={retry}>
        Try again
      </Button>
    </p>
  );
}

/** A failed read named in a muted line, for a place that offers no retry, which `ReadNotice` adds. */
export function ReadFailed({ error, name }: { error: string | null; name: string }) {
  return <p className="muted">Could not load the {name}: {error}</p>;
}

/** A table's one row for a read that failed: the error state across the columns. */
export function ErrorRow({ colSpan, error, errorTitle, retry }: { colSpan: number; error: string; errorTitle: string; retry?: () => void }) {
  return (
    <tr>
      <td colSpan={colSpan}>
        <ErrorState title={errorTitle} body={error} retry={retry} />
      </td>
    </tr>
  );
}

export function StateRow({ colSpan, empty, error, errorTitle, loading, retry }: { colSpan: number; empty: string; error: string | null; errorTitle: string; loading: boolean; retry?: () => void }) {
  if (error !== null) return <ErrorRow colSpan={colSpan} error={error} errorTitle={errorTitle} retry={retry} />;
  return (
    <tr>
      <td colSpan={colSpan} className="muted">
        {loading ? <Skeleton height={22} /> : empty}
      </td>
    </tr>
  );
}

/** A refused write or a failed step inside a dialog or an editor, announced the moment it appears. */
export function Refusal({ children }: { children: ReactNode }) {
  return (
    <div className="callout callout--critical" role="alert">
      <IconWarning size={16} />
      <div>{children}</div>
    </div>
  );
}

/**
 * A warning callout, announced politely. The region stays mounted while `when` is false, empty and out of the flow, so
 * a screen reader that already knows it announces the text once `when` turns true. `Refusal` is its critical twin.
 */
export function Warning({ children, when = true }: { children: ReactNode; when?: boolean }): JSX.Element {
  return (
    <div className={when ? 'callout callout--warning' : 'sr-only'} role="status">
      {when && (
        <>
          <IconWarning size={16} />
          <div>{children}</div>
        </>
      )}
    </div>
  );
}

/** A save refused as stale, with the Reload that reads the record again. */
export function StaleRefusal({ message, onReload }: { message: string; onReload?: () => void }) {
  return (
    <Refusal>
      {message}{' '}
      <Button variant="ghost" onClick={onReload}>
        Reload
      </Button>
    </Refusal>
  );
}

/**
 * A chunk the browser could not fetch. Chromium and Firefox throw a `TypeError` that names the dynamically imported
 * module, while WebKit's reads "Importing a module script failed." without those words, so both are matched.
 */
const failedChunk = (error: unknown): boolean =>
  error instanceof TypeError &&
  (error.message.includes('dynamically imported module') || error.message.includes('Importing a module script failed'));

/**
 * The error element of the routes whose page loads as its own chunk. It sits with the primitives rather than beside the
 * routes, since `App.tsx` builds its router as it loads, which an import of the element from there would run. A failed
 * chunk reads apart from an error while drawing, since only the former means the bundle changed under the tab.
 */
export function PageLoadFailed(): JSX.Element {
  const chunk = failedChunk(useRouteError());
  return (
    <ErrorState
      title={chunk ? 'This page could not load' : 'This page hit an error'}
      body={
        chunk
          ? "The console's files changed under this tab, and a reload did not fetch the current ones. Reload to try again."
          : 'Something went wrong while drawing this page. Reload to try again, and if it comes back, note what you opened last.'
      }
      action={<Button onClick={() => window.location.reload()}>Reload</Button>}
    />
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

/** Forwards its ref, so a caller can focus the field directly. */
export const Input = forwardRef<HTMLInputElement, InputProps>(function Input(props, ref) {
  return <input ref={ref} {...props} className={`input ${props.className ?? ''}`} />;
});

const numberText = (value: number | null) => (value === null || Number.isNaN(value) ? '' : String(value));

/**
 * A number field that keeps its typed text and reports its value as `typedNumber` reads it: a number, null for an
 * emptied field and NaN for text the browser cannot parse. It reads `onInput`, since `onChange` stays silent while a
 * number field's value reads empty for text it cannot parse. `value` seeds the text, and replaces it for a value the
 * text does not read, null included, compared with `Object.is`, so the field's own NaN leaves the text the browser
 * still draws.
 */
type NumberInputProps = Omit<InputProps, 'onChange' | 'onInput' | 'type' | 'value'> & { onNumber: (value: number | null) => void; value: number | null };
export const NumberInput = forwardRef<HTMLInputElement, NumberInputProps>(function NumberInput({ onNumber, value, ...rest }, ref) {
  const [typed, setTyped] = useState<TypedField>(() => ({ bad: false, text: numberText(value) }));
  const own = useRef<HTMLInputElement | null>(null);
  const merged = useCallback(
    (node: HTMLInputElement | null) => {
      own.current = node;
      if (typeof ref === 'function') ref(node);
      else if (ref) ref.current = node;
    },
    [ref],
  );
  // A box whose text the browser cannot parse reads an empty value, so React writes nothing for a figure that went
  // to none and the browser would keep drawing the text.
  useLayoutEffect(() => {
    const node = own.current;
    if (value === null && node?.validity.badInput) node.value = '';
  }, [value]);
  const text = Object.is(typedNumber(typed), value) ? typed.text : numberText(value);
  return (
    <Input
      ref={merged}
      {...rest}
      type="number"
      value={text}
      onInput={(e) => {
        const next = typedOf(e.currentTarget);
        setTyped(next);
        onNumber(typedNumber(next));
      }}
    />
  );
});

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
      <IconChevron size={12} strokeWidth={1.5} />
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
 * disables the switch and the number together, and with it off the switch alone gates the number. The number is a
 * `NumberInput`, so `input` hands it the value and `onNumber`.
 */
export function SwitchedNumber({ checked, disabled, hint, input, label, numberLabel, onChange }: { checked: boolean; disabled?: boolean; hint?: ReactNode; input: NumberInputProps; label: string; numberLabel?: string; onChange: (v: boolean) => void }) {
  // After the spread, so a `disabled` the caller put in `input` can never reopen a gated number.
  const number = <NumberInput {...input} disabled={disabled || !checked} />;
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

/**
 * The copy command: `navigator.clipboard` exists in secure contexts alone, and a console served over plain HTTP on
 * a LAN address has none. The temporary control lives inside `button`'s open modal, where the document is not inert,
 * and the focus returns to `button` after. Answers whether the command reported a copy.
 */
function copyByCommand(text: string, button: HTMLElement): boolean {
  const area = document.createElement('textarea');
  area.value = text;
  area.setAttribute('readonly', '');
  area.setAttribute('aria-hidden', 'true');
  // Off screen and fixed, so the page neither scrolls nor reflows for the moment the control exists.
  area.style.position = 'fixed';
  area.style.top = '0';
  area.style.left = '-9999px';
  // Inside the open modal when the button is in one: a `showModal` dialog leaves the rest of the document inert, so
  // a control appended to the body could take neither the focus nor the selection the command copies.
  const host = button.closest('dialog[open]') ?? document.body;
  const selection = document.getSelection();
  // Copies of the page's ranges, so a selection that moves its own range into the control leaves them whole.
  const ranges = selection ? Array.from({ length: selection.rangeCount }, (_, i) => selection.getRangeAt(i).cloneRange()) : [];
  host.appendChild(area);
  area.focus();
  area.select();
  let copied: boolean;
  try {
    copied = document.execCommand('copy');
  } catch {
    copied = false;
  }
  area.remove();
  // The button first: a focus can move the selection to the element that takes it, so the page's ranges go back
  // after the button has the focus.
  button.focus();
  if (selection) {
    selection.removeAllRanges();
    for (const range of ranges) selection.addRange(range);
  }
  return copied;
}

/** `text` as a function is read at each click, so a text that is long to build waits until it is copied. */
export function CopyButton({ text, label = 'Copy', size = 'sm' }: { label?: string; size?: 'md' | 'sm'; text: string | (() => string) }) {
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
      onClick={(e) => {
        // React resets the synthetic event's currentTarget once the handler returns, so the button is read first.
        const button = e.currentTarget;
        const value = typeof text === 'function' ? text() : text;
        // Saying Copied without a write matters most in the secret dialog: the operator could close the only
        // display of a secret they never received. The command runs inside the click, the user gesture it honors.
        // After a rejected write it runs in the rejection callback, where it relies on the click's transient
        // activation still standing: a browser that counts the activation spent answers false, and the button lands
        // at the failed state rather than claiming the copy.
        if (!navigator.clipboard) {
          setState(copyByCommand(value, button) ? 'copied' : 'failed');
          return;
        }
        void navigator.clipboard.writeText(value).then(
          () => setState('copied'),
          () => setState(copyByCommand(value, button) ? 'copied' : 'failed'),
        );
      }}
    >
      {state === 'copied' ? 'Copied' : state === 'failed' ? 'Copy failed, select the text' : label}
    </Button>
  );
}

// What takes the focus back after Keep editing: the control that held it, or else the first one a Tab reaches.
const HELD_FOCUS = 'a[href], button, input, select, textarea, [tabindex]';

/**
 * The outside path of a modal dialog: a backdrop click or the Escape key's `cancel`. While `unsaved` holds, both
 * ask before closing. The X button and a footer's Cancel never pass through here. The element follows `open`, and an
 * element the browser closed by force while `open` still holds shows again.
 */
function useModalDialog(open: boolean, onClose: () => void, unsaved: boolean) {
  const ref = useRef<HTMLDialogElement>(null);
  const [asking, setAsking] = useState(false);
  // The latest `onClose`, `open` and `unsaved` in one stable ref, so the `cancel` and `close` listeners are attached
  // once per element and never read a stale prop.
  const latest = useLatest({ onClose, open, unsaved });
  // The last control inside the dialog that took the focus: a click on the backdrop moves the focus off the control
  // before the dialog hears the click, so the active element alone would miss it.
  const lastFocused = useRef<HTMLElement | null>(null);
  // The control that held the focus as the question opened, so Keep editing can hand it back.
  const held = useRef<HTMLElement | null>(null);
  useHold(open, 'dialog');
  const request = useCallback(() => {
    if (!latest.current.unsaved) {
      latest.current.onClose();
      return;
    }
    const last = lastFocused.current;
    const active = last?.isConnected ? last : document.activeElement;
    held.current = active instanceof HTMLElement && ref.current?.contains(active) ? active : null;
    setAsking(true);
  }, [latest]);
  // Keep editing hands the focus back once the question is gone, not before: the question's modal leaves the dialog
  // under it inert, and a question that unmounts rather than closing restores nothing on its own.
  const refocus = useRef(false);
  const keep = useCallback(() => {
    refocus.current = true;
    setAsking(false);
  }, []);
  // A closed dialog drops its question in the render that closes it.
  if (!open && asking) setAsking(false);
  useEffect(() => {
    if (!open) lastFocused.current = null;
  }, [open]);
  // Ahead of the showModal effect, so a dialog that mounts open records the control its showModal focuses.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const onFocusIn = (e: Event) => {
      if (e.target instanceof HTMLElement && e.target !== el) lastFocused.current = e.target;
    };
    el.addEventListener('focusin', onFocusIn);
    return () => el.removeEventListener('focusin', onFocusIn);
  }, []);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    if (open && !el.open) el.showModal();
    if (!open && el.open) el.close();
  }, [open]);
  useEffect(() => {
    if (asking || !refocus.current) return;
    refocus.current = false;
    const el = ref.current;
    if (!el?.open) return;
    const was = held.current;
    held.current = null;
    const target = was?.isConnected && el.contains(was) && was.matches(HELD_FOCUS) && !was.matches(':disabled') ? was : el.querySelector<HTMLElement>(TABBABLE);
    target?.focus();
  }, [asking]);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const onCancel = (e: Event) => {
      // An uncancelable cancel (Chromium's Escape without a fresh activation) closes the element whatever the
      // listener does, so the state follows the element rather than asking over a dialog that is already gone.
      if (!e.cancelable) {
        latest.current.onClose();
        return;
      }
      e.preventDefault();
      request();
    };
    el.addEventListener('cancel', onCancel);
    return () => el.removeEventListener('cancel', onCancel);
  }, [latest, request]);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    // The browser can close the element over an owner that declined, so the element follows the owner's state again.
    const onElementClose = () => {
      if (latest.current.open && el.isConnected && !el.open) el.showModal();
    };
    el.addEventListener('close', onElementClose);
    return () => el.removeEventListener('close', onElementClose);
  }, [latest]);
  const onBackdrop = (e: ReactMouseEvent<HTMLDialogElement>) => {
    if (e.target === ref.current) request();
  };
  const question = asking && <ConfirmDialog open onClose={keep} onConfirm={onClose} title="Discard changes?" body="This dialog has changes that are not saved." cancelLabel="Keep editing" confirmLabel="Discard" danger initialFocus="cancel" />;
  return { onBackdrop, question, ref };
}

/** A dialog's head: its title, a subtitle where one is given, and the close button. */
function DialogHead({ onClose, subtitle, title, titleId }: { onClose: () => void; subtitle?: ReactNode; title: ReactNode; titleId: string }) {
  return (
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
  );
}

/** A titled element in a dialog describes through the dialog's own node, which no list item or cell of it holds. */
function DialogInner({ children }: { children: ReactNode }) {
  return (
    <div className="dialog__inner">
      <Descriptions>{children}</Descriptions>
    </div>
  );
}

export function Dialog({ open, onClose, title, subtitle, children, footer, width = 560, className, unsaved = false }: { children: ReactNode; className?: string; footer?: ReactNode; onClose: () => void; open: boolean; subtitle?: ReactNode; title: ReactNode; unsaved?: boolean; width?: number }) {
  const { onBackdrop, question, ref } = useModalDialog(open, onClose, unsaved);
  const titleId = useId();
  // The question is a sibling, mounted only while asked: inside the element it would be read by every check that
  // reads the dialog's text, and its clicks would bubble into the dialog's own handler. The body renders only while
  // open, so a page render rebuilds no closed form, and the element stays mounted, since its close hands the focus
  // back to the opener.
  return (
    <>
      <dialog
        ref={ref}
        className={className ? `dialog ${className}` : 'dialog'}
        aria-labelledby={titleId}
        style={{ width: `min(${width}px, calc(100vw - 32px))` }}
        onClick={onBackdrop}
      >
        <DialogInner>
          <DialogHead onClose={onClose} subtitle={subtitle} title={title} titleId={titleId} />
          {open && <div className="dialog__body">{children}</div>}
          {footer && <footer className="dialog__foot">{footer}</footer>}
        </DialogInner>
      </dialog>
      {question}
    </>
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
  // A layout effect, so the frame that opens the dialog already counts the fields the mode hides.
  useLayoutEffect(() => {
    if (registry === null) return;
    registry.set(id, !shown);
    return () => registry.drop(id);
  }, [id, registry, shown]);
  return shown ? <div className="expert">{children}</div> : null;
}

function DialogGroupPanel({ chosen, group, on, register, turnOn }: { chosen: string; group: DialogGroup; on: boolean; register: (id: string, el: HTMLElement | null) => void; turnOn: () => void }) {
  const { map: hidden, set, drop } = useKeyedMap<boolean>();
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
          {`${plural(count, 'more setting')} in the `}
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
 * A long form split into groups behind a rail, one group shown at a time. While the dialog is open the rest stay
 * mounted and hidden, so their state and their refusal checks live on while another group is in view. The panels
 * render only while it is open, as a `Dialog`'s body does, so a field's own state starts fresh at each opening.
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
  unsaved = false,
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
  /** The form differs from the values it opened with: an outside click or Escape asks before discarding. */
  unsaved?: boolean;
  width?: number;
}) {
  const { onBackdrop, question, ref } = useModalDialog(open, onClose, unsaved);
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
  // The open effect reads the rule through a ref, since the page hands in a new groups array on every render.
  const hides = useLatest(expertOnlyOff);
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
  // to, its refused control, and for the group the address named. A page's reset chooses that group in the render that
  // opens the dialog, before this effect runs, or after this effect has run, so the choice waits here until `chosen`
  // matches it and moves focus at once when it already does. Only a reset that runs after this effect can name a group
  // the mode hid: the hand-off above runs first and replaces a hidden group named in the opening render before this
  // effect turns the mode on. A group the mode hid mounts its section only in the render after the mode turns on, so
  // the choice also waits for the group to be shown, even when the page already holds it. A group the page sets itself
  // on open leaves focus where the browser's dialog focusing put it.
  useEffect(() => {
    if (!open) {
      wasOpen.current = false;
      pending.current = null;
      return;
    }
    if (!wasOpen.current) {
      wasOpen.current = true;
      if (initialGroup !== null) {
        if (hides.current(initialGroup)) setExpertView(true);
        pending.current = { focus: 'heading', id: initialGroup };
      }
    }
    bodyRef.current?.scrollTo?.({ top: 0 });
    const choice = pending.current;
    if (choice === null || choice.id !== chosen || !chosenShown) return;
    pending.current = null;
    focusIn(chosen, choice.focus);
    // Only a choice moves focus; the helpers and the body ref change without one.
  }, [chosen, chosenShown, hides, initialGroup, open, setExpertView]);
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
        className="dialog dialog--grouped dialog--fixed"
        aria-labelledby={titleId}
        style={{ '--dialog-w': `${width}px` } as CSSProperties}
        onClick={onBackdrop}
      >
        <DialogInner>
          <DialogHead onClose={onClose} title={title} titleId={titleId} />
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
            {open && (
              <div className="dialog__body dialog__body--grouped" ref={bodyRef}>
                {notice && <div style={{ paddingTop: 18 }}>{notice}</div>}
                {shown.map((g) => (
                  <DialogGroupPanel key={g.id} chosen={current} group={g} on={expertView} register={register} turnOn={() => setExpertView(true)} />
                ))}
              </div>
            )}
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
      {question}
    </GroupChoice.Provider>
  );
}

export function ConfirmDialog({ open, onClose, onConfirm, title, body, children, cancelLabel = 'Cancel', confirmLabel = 'Confirm', confirmDisabled = false, danger = false, initialFocus }: {
  body: ReactNode;
  cancelLabel?: string;
  /** Blocks drawn above `body`, which stays the dialog's closing paragraph. */
  children?: ReactNode;
  /** Holds the confirm button, for a confirmation that waits on a read. */
  confirmDisabled?: boolean;
  confirmLabel?: string;
  danger?: boolean;
  initialFocus?: 'cancel';
  onClose: () => void;
  onConfirm: () => void;
  open: boolean;
  title: string;
}) {
  const cancelId = useId();
  // After the dialog's own showModal effect (a child's effect runs first), so the browser's dialog focusing does not
  // take the focus back to the first button.
  useEffect(() => {
    if (open && initialFocus === 'cancel') document.getElementById(cancelId)?.focus();
  }, [cancelId, initialFocus, open]);
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={title}
      width={440}
      footer={
        <>
          <Button id={cancelId} variant="ghost" onClick={onClose}>
            {cancelLabel}
          </Button>
          <Button
            variant={danger ? 'danger' : 'primary'}
            disabled={confirmDisabled}
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
      {children}
      <p className="secondary">{body}</p>
    </Dialog>
  );
}

/**
 * An expandable row's handlers: a click opens the row unless it lands on a link or a button inside it, which follows,
 * and Enter or Space opens it while the row itself has the focus, not a control inside it.
 */
export const toggleRow = (onToggle: () => void): { onClick: (e: ReactMouseEvent<HTMLElement>) => void; onKeyDown: (e: KeyboardEvent<HTMLElement>) => void } => ({
  onClick: (e) => {
    if (!(e.target as Element).closest('a, button')) onToggle();
  },
  onKeyDown: (e) => {
    if (e.target !== e.currentTarget || (e.key !== 'Enter' && e.key !== ' ')) return;
    e.preventDefault();
    onToggle();
  },
});

const IN_FLOW: MenuPlacement = { kind: 'down' };
const CLIPPING = new Set(['auto', 'clip', 'hidden', 'scroll']);

/**
 * A row's actions behind a trigger. `align` names the trigger edge the list lines up with: a menu in a row's
 * first column opens rightward, since a list aligned to its right edge would run past the table wrap's left edge,
 * which clips it.
 */
export function Menu({ align = 'right', items, label = 'Actions' }: { align?: MenuAlign; items: { danger?: boolean; disabled?: boolean; label: string; onClick: () => void; title?: string }[]; label?: string }) {
  const [open, setOpen] = useState(false);
  useHold(open, 'menu');
  const [placement, setPlacement] = useState<MenuPlacement>(IN_FLOW);
  // Each opening measures the list in flow, so a closed menu drops the last opening's placement as it closes.
  if (!open && placement !== IN_FLOW) setPlacement(IN_FLOW);
  // Where a floating list is portalled, read with the placement: a modal dialog sits in the top layer, so a layer
  // portalled to the body would draw under it.
  const [host, setHost] = useState<HTMLElement | null>(null);
  const ref = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  // Where the trigger rested once the list floated, so a fixed layer can tell whether the page has moved under it.
  const anchor = useRef<DOMRect | null>(null);
  useOutsidePress(open, () => [ref.current, listRef.current], () => setOpen(false));
  // Measured once per opening, after the list has drawn below its trigger and before the browser paints it. A table
  // wrap that scrolls sideways clips on both axes, so its edges bound the list as much as the viewport's do. A row
  // too short for the list on either side inside its wrap (a table filtered to one row) moves the list out of the
  // wrap, into a fixed layer at the trigger.
  useLayoutEffect(() => {
    if (!open || !ref.current) return;
    const trigger = ref.current.getBoundingClientRect();
    // A wrap bounds the list only while it clips: the pool dialog's key table sets its wrap `overflow: visible`. The
    // clipping values are named because a page without styles, as in the smoke run, reads the property empty.
    const wrap = ref.current.closest<HTMLElement>('.table-wrap');
    const clip = wrap && CLIPPING.has(getComputedStyle(wrap).overflowY) ? wrap : null;
    const next = menuPlacement(trigger, clip, { height: listRef.current?.offsetHeight ?? 0, width: listRef.current?.offsetWidth ?? 0 }, window.innerHeight, align);
    setPlacement(next.kind === 'down' ? IN_FLOW : next);
    setHost(ref.current.closest<HTMLElement>('dialog[open]') ?? document.body);
  }, [align, open]);
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
  }, [align, floating]);
  useLeaveOnMove(floating, ref, anchor, () => setOpen(false));
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
    walkFocus(e, items);
  };
  const list = open && (
    <div
      ref={listRef}
      className={`menu__list${placement.kind === 'down' ? '' : ` menu__list--${placement.kind}`}${align === 'left' ? ' menu__list--left' : ''}`}
      role="menu"
      style={placement.kind === 'float' ? { left: placement.left, top: placement.top } : undefined}
      // Safari, and Firefox on macOS, give a clicked button no focus, so a click on an item blurs the list naming no
      // element. A blur closes the list only when it names an element outside the list and the trigger, whose own
      // click toggles the list.
      onBlur={(e) => blurLeaves(triggerRef.current, () => setOpen(false))(e)}
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
      {floating && host !== null ? createPortal(list, host) : list}
    </div>
  );
}

export function KeyValue({ rows, className = '' }: { className?: string; rows: { label: string; mono?: boolean; title?: string; value: ReactNode }[] }) {
  return (
    <dl className={`kv ${className}`}>
      {rows.map((r) => (
        <div key={r.label} className="kv__row">
          <dt>{r.label}</dt>
          <Described text={r.title}>
            {(describedBy) => (
              <dd className={r.mono ? 'mono' : ''} title={r.title} aria-describedby={describedBy}>
                {r.value}
              </dd>
            )}
          </Described>
        </div>
      ))}
    </dl>
  );
}

/** The sub line under a row's name that dates it by the list's sort date, the full instant on hover. */
export function StampSub({ at, word }: { at: string; word: string }) {
  return (
    <span className="sub" title={fmtAbsolute(at)}>
      {word} {fmtDate(at)}
    </span>
  );
}

/**
 * A failure's dwell is long enough to read the route it names. A confirmation only acknowledges the action just taken,
 * for the 2.4 s the smoke run's confirmation reads are timed against.
 */
const DWELL_MS: Record<ToastItem['kind'], number> = { confirmation: 2_400, failure: 6_000 };

/** Draws the first toast of the queue for its dwell, then hands its id to `onDone` so the next one shows. */
export function Toast({ onDone, queue }: { onDone: (id: number) => void; queue: ToastItem[] }) {
  // Keyed on the item rather than the callback, so a fresh callback on a re-render does not restart the dwell.
  const done = useLatest(onDone);
  const showing = queue[0];
  const id = showing?.id;
  const dwellMs = showing && DWELL_MS[showing.kind];
  useEffect(() => {
    if (id === undefined || dwellMs === undefined) return;
    const timer = clock.setTimeout(() => done.current(id), dwellMs);
    return () => clock.clearTimeout(timer);
  }, [id, dwellMs, done]);
  if (!showing) return null;
  return (
    <div className="toast" role="status">
      {showing.message}
    </div>
  );
}
