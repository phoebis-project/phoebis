import { useEffect, useState, type ReactNode } from 'react';
import { NavLink, Outlet, useLocation } from 'react-router-dom';
import { api } from '@/api';
import { useSession } from '@/app/session';
import { useAppSettings, type ThemeChoice } from '@/app/settings';
import { useAsync } from '@/lib/hooks';
import { Badge, Button, Segmented, Toast, Toggle } from '@/components/ui/primitives';
import { IconActivity, IconBell, IconChart, IconGauge, IconGrid, IconKey, IconLayers, IconList, IconMonitor, IconMoon, IconSessions, IconShield, IconSliders, IconSun, IconUser } from '@/components/ui/icons';
import { Mark } from '@/components/ui/Mark';

const NAV: { icon: ReactNode; label: string; to: string }[] = [
  { to: '/', label: 'Dashboard', icon: <IconGrid size={16} /> },
  { to: '/observe', label: 'Observe', icon: <IconActivity size={16} /> },
  { to: '/quota', label: 'Quota', icon: <IconGauge size={16} /> },
  { to: '/accounts', label: 'Accounts', icon: <IconUser size={16} /> },
  { to: '/pools', label: 'Pools', icon: <IconLayers size={16} /> },
  { to: '/keys', label: 'Keys', icon: <IconKey size={16} /> },
  { to: '/models', label: 'Models', icon: <IconShield size={16} /> },
  { to: '/requests', label: 'Requests', icon: <IconList size={16} /> },
  { to: '/analytics', label: 'Analytics', icon: <IconChart size={16} /> },
  { to: '/sessions', label: 'Sessions', icon: <IconSessions size={16} /> },
  { to: '/alerts', label: 'Alerts', icon: <IconBell size={16} /> },
  { to: '/settings', label: 'Settings', icon: <IconSliders size={16} /> },
];

export const PAGE_TITLES: Record<string, string> = {
  '/': 'Dashboard',
  '/accounts': 'Accounts',
  '/alerts': 'Alerts',
  '/analytics': 'Analytics',
  '/keys': 'Keys',
  '/models': 'Models',
  '/observe': 'Observe',
  '/pools': 'Pools',
  '/quota': 'Quota',
  '/requests': 'Requests',
  '/sessions': 'Sessions',
  '/settings': 'Settings',
};

export function Shell() {
  const { pathname } = useLocation();
  const { expertView, setExpertView, setTheme, theme } = useAppSettings();
  const { clearToast, logout, role, toast } = useSession();
  const settings = useAsync(() => api.getSettings(), []);
  const [navOpen, setNavOpen] = useState(false);
  const base = `/${pathname.split('/')[1] ?? ''}`;
  const title = PAGE_TITLES[base] ?? 'Phoebis Router';

  useEffect(() => {
    document.title = `${title} - Phoebis Router`;
    setNavOpen(false);
  }, [title, pathname]);

  return (
    <div className={`shell ${navOpen ? 'nav-open' : ''}`}>
      <aside className="sidebar">
        <div className="brand">
          <span className="brand__mark" aria-hidden>
            <Mark size={20} />
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
            <Badge tone={role === 'admin' ? 'accent' : 'neutral'} title={role === 'admin' ? 'Admin secret: full read and write' : 'Viewer secret: read everything except captured bodies, live previews, question text, and webhook URLs beyond their host'}>
              {role}
            </Badge>
            <Button variant="ghost" size="sm" aria-label="Log out" onClick={() => void logout()}>
              Log out
            </Button>
            <Segmented<ThemeChoice>
              ariaLabel="Theme"
              value={theme}
              onChange={setTheme}
              options={[
                { value: 'light', label: <IconSun size={15} /> },
                { value: 'dark', label: <IconMoon size={15} /> },
                { value: 'system', label: <IconMonitor size={15} /> },
              ]}
            />
          </div>
        </header>
        <main className="content">
          <Outlet />
        </main>
      </div>
      {navOpen && <button type="button" className="scrim" aria-label="Close navigation" onClick={() => setNavOpen(false)} />}
      <Toast message={toast} onDone={clearToast} />
    </div>
  );
}
