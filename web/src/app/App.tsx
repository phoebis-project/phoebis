import { lazy, Suspense } from 'react';
import { createBrowserRouter, Navigate, RouterProvider } from 'react-router-dom';
import { Shell } from '@/components/shell/Shell';
import { AccountsPage } from '@/pages/AccountsPage';
import { AlertsPage } from '@/pages/AlertsPage';
import { AnalyticsPage } from '@/pages/AnalyticsPage';
import { DashboardPage } from '@/pages/DashboardPage';
import { KeysPage } from '@/pages/KeysPage';
import { ModelsPage } from '@/pages/ModelsPage';
import { ObservePage } from '@/pages/ObservePage';
import { PoolsPage } from '@/pages/PoolsPage';
import { QuotaPage } from '@/pages/QuotaPage';
import { RequestsPage } from '@/pages/RequestsPage';
import { SessionsPage } from '@/pages/SessionsPage';
import { SettingsPage } from '@/pages/SettingsPage';
import { SessionProvider } from './session';
import { AppSettingsProvider } from './settings';

// Gated on the build variable here, not on `MOCK`: the bundler gathers dynamic imports before it folds a constant
// imported from another module, so only a local condition keeps the fixture's chunk out of the served bundle.
const GroupedDialogFixtureRoute = import.meta.env.VITE_PHOEBIS_MOCK === '1' ? lazy(() => import('@/mock/fixtures/GroupedDialogFixture')) : null;

export const routes = [
  {
    path: '/',
    element: <Shell />,
    children: [
      { index: true, element: <DashboardPage /> },
      { path: 'accounts', element: <AccountsPage /> },
      { path: 'accounts/:accountId', element: <AccountsPage /> },
      { path: 'alerts', element: <AlertsPage /> },
      { path: 'analytics', element: <AnalyticsPage /> },
      // Bookmarks and links out there still point at the old path.
      { path: 'keepalive', element: <Navigate to="/sessions" replace /> },
      { path: 'keys', element: <KeysPage /> },
      { path: 'models', element: <ModelsPage /> },
      { path: 'observe', element: <ObservePage /> },
      { path: 'pools', element: <PoolsPage /> },
      { path: 'quota', element: <QuotaPage /> },
      { path: 'requests', element: <RequestsPage /> },
      { path: 'requests/:requestId', element: <RequestsPage /> },
      { path: 'sessions', element: <SessionsPage /> },
      { path: 'settings', element: <SettingsPage /> },
    ],
  },
  // The fixture the browser pass measures; the served bundle carries neither the path nor the module.
  ...(GroupedDialogFixtureRoute
    ? [
        {
          path: '/fixtures/grouped-dialog',
          element: (
            <Suspense fallback={null}>
              <GroupedDialogFixtureRoute />
            </Suspense>
          ),
        },
      ]
    : []),
];

const router = createBrowserRouter(routes);

export function App() {
  return (
    <SessionProvider>
      <AppSettingsProvider>
        <RouterProvider router={router} />
      </AppSettingsProvider>
    </SessionProvider>
  );
}
