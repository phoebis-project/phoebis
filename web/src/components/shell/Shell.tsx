import { useEffect, useId, useRef, useState, type ReactNode } from 'react';
import { NavLink, Outlet, useLocation } from 'react-router-dom';
import { api } from '@/api';
import { useSession } from '@/app/session';
import { useAppSettings, type ThemeChoice } from '@/app/settings';
import { useAsync, useMediaQuery } from '@/lib/hooks';
import { positionOf, UNIT_LABEL, UNIT_LEGEND, UNIT_LEGEND_NOTE, UNIT_POSITIONS, UNIT_SHORT, type UnitPosition } from '@/lib/unit';
import { Button, Descriptions, Segmented, Select, Toast, Toggle } from '@/components/ui/primitives';
import { IconArrowRight, IconChart, IconGauge, IconGrid, IconInfo, IconKey, IconLayers, IconList, IconMonitor, IconMoon, IconRoute, IconSessions, IconShield, IconSliders, IconSun, IconUser } from '@/components/ui/icons';
import { Mark } from '@/components/ui/Mark';

type NavItem = { icon: ReactNode; label: string; to: string };

const NAV: NavItem[] = [
  { to: '/', label: 'Dashboard', icon: <IconGrid size={16} /> },
  { to: '/quota', label: 'Quota', icon: <IconGauge size={16} /> },
  { to: '/accounts', label: 'Accounts', icon: <IconUser size={16} /> },
  { to: '/pools', label: 'Pools', icon: <IconLayers size={16} /> },
  { to: '/keys', label: 'Keys', icon: <IconKey size={16} /> },
  { to: '/models', label: 'Models', icon: <IconShield size={16} /> },
  { to: '/requests', label: 'Requests', icon: <IconList size={16} /> },
  { to: '/analytics', label: 'Analytics', icon: <IconChart size={16} /> },
  { to: '/sessions', label: 'Sessions', icon: <IconSessions size={16} /> },
  { to: '/settings', label: 'Settings', icon: <IconSliders size={16} /> },
];

/** The views an operator opens to see why the router did what it did, kept out of the way of the daily ones. */
const DIAGNOSTICS: NavItem[] = [{ to: '/routing-log', label: 'Routing log', icon: <IconRoute size={16} /> }];

export const PAGE_TITLES: Record<string, string> = {
  '/': 'Dashboard',
  '/accounts': 'Accounts',
  '/analytics': 'Analytics',
  '/keys': 'Keys',
  '/models': 'Models',
  '/pools': 'Pools',
  '/quota': 'Quota',
  '/requests': 'Requests',
  '/routing-log': 'Routing log',
  '/sessions': 'Sessions',
  '/settings': 'Settings',
};

/** The unit every view prices in. At 900 px and below the bar spans the viewport, so each position reads its short label. */
function UnitSwitch({ onChange, short, unit }: { onChange: (u: UnitPosition) => void; short: boolean; unit: UnitPosition }) {
  return (
    // Keyed on the label set: Chromium keeps a select's width when its options' text changes, so a resize across the
    // breakpoint would draw the short labels in the long labels' box, or a long label cut short in the other direction.
    <Select key={short ? 'short' : 'long'} aria-label="Unit" className="unit-switch" value={unit} onChange={(e) => onChange(positionOf(e.target.value))}>
      {UNIT_POSITIONS.map((p) => (
        <option key={p} value={p}>
          {short ? UNIT_SHORT[p] : UNIT_LABEL[p]}
        </option>
      ))}
    </Select>
  );
}

const LEGEND_LABEL = 'What the units mean';

/**
 * The unit positions explained, a popover from the info button beside the switch. Escape and a press outside close it,
 * as the console's popovers close.
 */
export function UnitLegend() {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLSpanElement>(null);
  const id = useId();
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    // The button sits inside the anchor too, so its own press toggles rather than closing and opening again.
    const onPointer = (e: Event) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('keydown', onKey);
    document.addEventListener('pointerdown', onPointer);
    return () => {
      document.removeEventListener('keydown', onKey);
      document.removeEventListener('pointerdown', onPointer);
    };
  }, [open]);
  return (
    <span className="unit-legend__anchor" ref={ref}>
      <button type="button" className="unit-legend__button" aria-label={LEGEND_LABEL} title={LEGEND_LABEL} aria-expanded={open} aria-controls={open ? id : undefined} onClick={() => setOpen((v) => !v)}>
        <IconInfo size={15} />
      </button>
      {open && (
        <div id={id} role="dialog" aria-label={LEGEND_LABEL} className="unit-legend">
          {UNIT_POSITIONS.map((p) => (
            <p key={p} className="unit-legend__line">
              <b>{UNIT_LABEL[p]}</b> {UNIT_LEGEND[p]}
            </p>
          ))}
          <p className="unit-legend__line">{UNIT_LEGEND_NOTE}</p>
        </div>
      )}
    </span>
  );
}

export function Shell() {
  const { pathname } = useLocation();
  const { expertView, setExpertView, setTheme, setUnit, theme, unit } = useAppSettings();
  const narrow = useMediaQuery('(max-width: 900px)');
  const { done, logout, queue } = useSession();
  const settings = useAsync(() => api.getSettings(), []);
  const [navOpen, setNavOpen] = useState(false);
  // A navigation closes the menu in the render that brings the new view.
  const [navFor, setNavFor] = useState(pathname);
  if (navFor !== pathname) {
    setNavFor(pathname);
    setNavOpen(false);
  }
  const base = `/${pathname.split('/')[1] ?? ''}`;
  const title = PAGE_TITLES[base] ?? 'Phoebis Router';
  // A link into a diagnostics view leaves the mode as the reader set it, so the group shows for the open view too.
  const diagnostics = expertView || DIAGNOSTICS.some((d) => pathname.startsWith(d.to));

  useEffect(() => {
    document.title = `${title} - Phoebis Router`;
  }, [title]);

  return (
    <div className={`shell ${navOpen ? 'nav-open' : ''}`}>
      <aside className="sidebar">
        <div className="brand">
          <span className="brand__mark" aria-hidden>
            <Mark size={26} />
          </span>
          <span className="brand__name">Phoebis Router</span>
        </div>
        <nav className="nav" aria-label="Primary">
          {NAV.map((item) => (
            <NavLink key={item.to} to={item.to} end={item.to === '/'} className={({ isActive }) => `nav__link ${isActive ? 'is-active' : ''}`}>
              {item.icon}
              <span>{item.label}</span>
            </NavLink>
          ))}
          {diagnostics && (
            <div className="nav__group">
              <span className="nav__heading">Diagnostics</span>
              {DIAGNOSTICS.map((item) => (
                <NavLink key={item.to} to={item.to} className={({ isActive }) => `nav__link ${isActive ? 'is-active' : ''}`}>
                  {item.icon}
                  <span>{item.label}</span>
                </NavLink>
              ))}
            </div>
          )}
        </nav>
        <div className="sidebar__status">
          <label className="sidebar__row sidebar__expert" title="Shows every field of the pool and key dialogs, the advanced ones included.">
            <Toggle label="Expert view" checked={expertView} onChange={setExpertView} />
            <span>Expert view</span>
          </label>
          <div className="sidebar__row muted mono">v{settings.data?.about.version ?? '…'}</div>
        </div>
      </aside>
      <div className="main">
        <header className="topbar">
          <button type="button" className="topbar__menu" aria-label="Toggle navigation" onClick={() => setNavOpen((v) => !v)}>
            <IconList size={18} />
          </button>
          <h1 className="topbar__title">{title}</h1>
          <div className="topbar__right">
            <UnitSwitch unit={unit} onChange={setUnit} short={narrow} />
            <UnitLegend />
            <Button variant="ghost" size="sm" className="topbar__logout" aria-label="Log out" title="Log out" icon={<IconArrowRight size={15} />} onClick={() => void logout()}>
              <span className="topbar__logout-label">Log out</span>
            </Button>
            <Segmented<ThemeChoice>
              ariaLabel="Theme"
              value={theme}
              onChange={setTheme}
              options={[
                { value: 'light', label: <IconSun size={15} />, name: 'Light' },
                { value: 'dark', label: <IconMoon size={15} />, name: 'Dark' },
                { value: 'system', label: <IconMonitor size={15} />, name: 'System' },
              ]}
            />
          </div>
        </header>
        <main className="content">
          {/* A titled badge a page draws outside its cards, such as the attention line's, describes through this host. */}
          <Descriptions>
            <Outlet />
          </Descriptions>
        </main>
      </div>
      {navOpen && <button type="button" className="scrim" aria-label="Close navigation" onClick={() => setNavOpen(false)} />}
      <Toast queue={queue} onDone={done} />
    </div>
  );
}
