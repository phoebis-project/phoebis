import { createBrowserRouter, Navigate, RouterProvider, useParams } from 'react-router-dom';
import { Shell } from '@/components/shell/Shell';
import { AccountsPage } from '@/pages/AccountsPage';
import { AlertsPage } from '@/pages/AlertsPage';
import { AnalyticsPage } from '@/pages/AnalyticsPage';
import { DashboardPage } from '@/pages/DashboardPage';
import { KeysPage } from '@/pages/KeysPage';
import { ObservePage } from '@/pages/ObservePage';
import { PoolsPage } from '@/pages/PoolsPage';
import { QuotaPage } from '@/pages/QuotaPage';
import { RequestsPage } from '@/pages/RequestsPage';
import { SessionsPage } from '@/pages/SessionsPage';
import { SettingsPage } from '@/pages/SettingsPage';
import { SessionProvider } from './session';
import { AppSettingsProvider } from './settings';

/** Keeps a bookmarked /accounts/:id landing on the same upstream. */
function UpstreamRedirect() {
  const { accountId } = useParams();
  return <Navigate to={accountId ? `/upstreams/${accountId}` : '/upstreams'} replace />;
}

export const routes = [
  {
    path: '/',
    element: <Shell />,
    children: [
      { index: true, element: <DashboardPage /> },
      // The upstreams page answered to /accounts until the copy was renamed.
      { path: 'accounts', element: <UpstreamRedirect /> },
      { path: 'accounts/:accountId', element: <UpstreamRedirect /> },
      { path: 'alerts', element: <AlertsPage /> },
      { path: 'analytics', element: <AnalyticsPage /> },
      // Bookmarks and links out there still point at the old path.
      { path: 'keepalive', element: <Navigate to="/sessions" replace /> },
      { path: 'keys', element: <KeysPage /> },
      { path: 'observe', element: <ObservePage /> },
      { path: 'pools', element: <PoolsPage /> },
      { path: 'quota', element: <QuotaPage /> },
      { path: 'requests', element: <RequestsPage /> },
      { path: 'requests/:requestId', element: <RequestsPage /> },
      { path: 'sessions', element: <SessionsPage /> },
      { path: 'settings', element: <SettingsPage /> },
      { path: 'upstreams', element: <AccountsPage /> },
      { path: 'upstreams/:accountId', element: <AccountsPage /> },
    ],
  },
];

const router = createBrowserRouter(routes);

export function App() {
  return (
    <SessionProvider>
      <AppSettingsProvider>
        <RouterProvider router={router} future={{ v7_startTransition: true }} />
      </AppSettingsProvider>
    </SessionProvider>
  );
}
