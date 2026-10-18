import { lazy, Suspense, type ReactNode } from 'react';
import { createBrowserRouter, Navigate, RouterProvider } from 'react-router-dom';
import { Shell } from '@/components/shell/Shell';
import { PageLoadFailed, Skeleton } from '@/components/ui/primitives';
import { reloadOnChunkFailure } from '@/lib/chunks';
import { DashboardPage } from '@/pages/DashboardPage';
import { GuardrailsPage } from '@/pages/GuardrailsPage';
import { KeysPage } from '@/pages/KeysPage';
import { ModelsPage } from '@/pages/ModelsPage';
import { QuotaPage } from '@/pages/QuotaPage';
import { RequestsPage } from '@/pages/RequestsPage';
import { RoutingLogPage } from '@/pages/RoutingLogPage';
import { SessionsPage } from '@/pages/SessionsPage';
import { SettingsPage } from '@/pages/SettingsPage';
import { SessionProvider } from './session';
import { AppSettingsProvider } from './settings';

// Gated on the build variable here, not on `MOCK`: the bundler gathers dynamic imports before it folds a constant
// imported from another module, so only a local condition keeps the fixture's chunk out of the served bundle.
const GroupedDialogFixtureRoute = import.meta.env.VITE_PHOEBIS_MOCK === '1' ? lazy(() => import('@/mock/fixtures/GroupedDialogFixture')) : null;

// The three pages that draw Recharts charts load as their own chunks, keeping the chart library out of the entry.
const AccountsPage = lazy(reloadOnChunkFailure(() => import('@/pages/AccountsPage').then((m) => ({ default: m.AccountsPage }))));
const AnalyticsPage = lazy(reloadOnChunkFailure(() => import('@/pages/AnalyticsPage').then((m) => ({ default: m.AnalyticsPage }))));
const PoolsPage = lazy(reloadOnChunkFailure(() => import('@/pages/PoolsPage').then((m) => ({ default: m.PoolsPage }))));
const PageFallback = () => (<div className="stack"><Skeleton height={36} /><Skeleton height={120} /><Skeleton height={120} /></div>);
const lazily = (page: ReactNode) => <Suspense fallback={<PageFallback />}>{page}</Suspense>;

export const routes = [
  {
    path: '/',
    element: <Shell />,
    children: [
      { index: true, element: <DashboardPage /> },
      // The element catches every render error on these four routes, a chunk that fails again after the reload among
      // them, and draws it in the shell's outlet. The other routes keep the router's own screen.
      { path: 'accounts', element: lazily(<AccountsPage />), errorElement: <PageLoadFailed /> },
      { path: 'accounts/:accountId', element: lazily(<AccountsPage />), errorElement: <PageLoadFailed /> },
      { path: 'analytics', element: lazily(<AnalyticsPage />), errorElement: <PageLoadFailed /> },
      { path: 'guardrails', element: <GuardrailsPage /> },
      // Bookmarks and links out there still point at the old path.
      { path: 'keepalive', element: <Navigate to="/sessions" replace /> },
      { path: 'keys', element: <KeysPage /> },
      { path: 'models', element: <ModelsPage /> },
      { path: 'pools', element: lazily(<PoolsPage />), errorElement: <PageLoadFailed /> },
      { path: 'quota', element: <QuotaPage /> },
      { path: 'requests', element: <RequestsPage /> },
      { path: 'requests/:requestId', element: <RequestsPage /> },
      { path: 'routing-log', element: <RoutingLogPage /> },
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
