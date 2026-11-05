import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import { NavLink, Outlet, useLocation } from 'react-router-dom';
import { api } from '@/api';
import { setBuildHook } from '@/api/http';
import { useSession, useToastQueue } from '@/app/session';
import { useAppSettings, type ThemeChoice } from '@/app/settings';
import { useAsync, useMediaQuery } from '@/lib/hooks';
import { ChartInkProvider } from '@/components/charts/useChartInk';
import { Button, Descriptions, Segmented, Toast, Toggle } from '@/components/ui/primitives';
import { IconActivity, IconArrowRight, IconChart, IconFunnel, IconGauge, IconGrid, IconKey, IconLayers, IconList, IconMonitor, IconMoon, IconRoute, IconSessions, IconSliders, IconSun, IconUser } from '@/components/ui/icons';
import { Mark } from '@/components/ui/Mark';
import { UnitLegend, UnitSwitch, WeekSwitch } from '@/components/ui/UnitControls';

type NavItem = { icon: ReactNode; label: string; to: string };

const NAV: NavItem[] = [
  { to: '/', label: 'Dashboard', icon: <IconGrid size={16} /> },
  { to: '/quota', label: 'Quota', icon: <IconGauge size={16} /> },
  { to: '/accounts', label: 'Accounts', icon: <IconUser size={16} /> },
  { to: '/pools', label: 'Pools', icon: <IconLayers size={16} /> },
  { to: '/keys', label: 'Keys', icon: <IconKey size={16} /> },
  { to: '/guardrails', label: 'Guardrails', icon: <IconFunnel size={16} /> },
  { to: '/requests', label: 'Requests', icon: <IconList size={16} /> },
  { to: '/analytics', label: 'Analytics', icon: <IconChart size={16} /> },
  { to: '/sessions', label: 'Sessions', icon: <IconSessions size={16} /> },
  { to: '/settings', label: 'Settings', icon: <IconSliders size={16} /> },
];

/** The views an operator opens to see why the router did what it did, kept out of the way of the daily ones. */
const DIAGNOSTICS: NavItem[] = [
  { to: '/routing-log', label: 'Routing log', icon: <IconRoute size={16} /> },
  { to: '/meter', label: 'Meter', icon: <IconActivity size={16} /> },
];

export const PAGE_TITLES: Record<string, string> = Object.fromEntries([...NAV, ...DIAGNOSTICS].map((i) => [i.to, i.label]));

const navLink = (item: NavItem) => (
  <NavLink key={item.to} to={item.to} end={item.to === '/'} className={({ isActive }) => `nav__link ${isActive ? 'is-active' : ''}`}>
    {item.icon}
    <span>{item.label}</span>
  </NavLink>
);

export function Shell() {
  const { pathname } = useLocation();
  const { expertView, setExpertView, setModelTiers, setTheme, setUnit, setWeek, theme, unit, week } = useAppSettings();
  const narrow = useMediaQuery('(max-width: 900px)');
  const { done, logout } = useSession();
  const queue = useToastQueue();
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

  // The settings answer is the one the shell holds for every view, so the views' model orders read its tiers.
  useEffect(() => {
    if (settings.data) setModelTiers({ modelTier: settings.data.subscriptionCosts.modelTier, tierOrder: settings.data.subscriptionCosts.tierOrder });
  }, [settings.data, setModelTiers]);

  // A registration of its own, since setHooks would replace the session's handlers. The banner holds until the reload,
  // so nothing takes it down again.
  const [newerBuild, setNewerBuild] = useState(false);
  useEffect(() => {
    setBuildHook(() => setNewerBuild(true));
    return () => setBuildHook(null);
  }, []);

  // A page's sticky bars dock under the block, whose height the banner changes and the stylesheet cannot read, so the
  // shell hands it over as the token.
  const topRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const top = topRef.current;
    if (top === null || typeof ResizeObserver !== 'function') return;
    const observer = new ResizeObserver(() => top.parentElement?.style.setProperty('--main-top', `${top.getBoundingClientRect().height}px`));
    observer.observe(top);
    return () => observer.disconnect();
  }, []);

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
          {NAV.map(navLink)}
          {diagnostics && (
            <div className="nav__group">
              <span className="nav__heading">Diagnostics</span>
              {DIAGNOSTICS.map(navLink)}
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
        <div className="main__top" ref={topRef}>
          {newerBuild && (
            <div className="callout callout--info" role="status">
              <span>The router runs a newer build than this console. Reload to pick it up.</span>
              <Button size="sm" onClick={() => window.location.reload()}>
                Reload
              </Button>
            </div>
          )}
          <header className="topbar">
            <button type="button" className="topbar__menu" aria-label="Toggle navigation" onClick={() => setNavOpen((v) => !v)}>
              <IconList size={18} />
            </button>
            <h1 className="topbar__title">{title}</h1>
            <div className="topbar__right">
              <UnitSwitch unit={unit} onChange={setUnit} short={narrow} />
              <WeekSwitch week={week} onChange={setWeek} short={narrow} disabled={unit !== 'native'} />
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
        </div>
        <main className="content">
          {/* A titled badge a page draws outside its cards, such as the attention line's, describes through this host. */}
          <Descriptions>
            <ChartInkProvider>
              <Outlet />
            </ChartInkProvider>
          </Descriptions>
        </main>
      </div>
      {navOpen && <button type="button" className="scrim" aria-label="Close navigation" onClick={() => setNavOpen(false)} />}
      <Toast queue={queue} onDone={done} />
    </div>
  );
}
