import React from 'react';
import { createRoot } from 'react-dom/client';
import { createMemoryRouter, RouterProvider } from 'react-router-dom';
import { api } from '@/api';
import { ApiError, reportFailure } from '@/api/http';
import type { LiveEvent } from '@/api/types';
import { routes } from '@/app/App';
import { SessionProvider } from '@/app/session';
import { AppSettingsProvider } from '@/app/settings';
import { REQUEST_EVENTS, REQUESTS } from '@/mock/requests';

export const requestEvents = REQUEST_EVENTS;
export const requests = REQUESTS;

/**
 * Drops the feed's ticks so a check can watch the page notice the silence. Only the ticks: the deltas keep
 * flowing, so the store a later check reads is the one it would have been without the stall.
 */
let stalled = false;
export const stallLive = (on: boolean) => {
  stalled = on;
};
const subscribeLive = api.subscribeLive.bind(api);
api.subscribeLive = (handler: (event: LiveEvent) => void) =>
  subscribeLive((e) => {
    if (stalled && e.kind === 'delta' && e.delta.type === 'tick') return;
    handler(e);
  });

/**
 * Fails one mock call the way the transport fails a real one: the rejection reaches the page's `useAsync`,
 * and the route reaches the shell's toast, which the mock would otherwise never report because it never goes
 * through `call`. The override puts the original method back as it fires, so a retry reads the mock again.
 */
export const failNextCall = (name: string, path: string, message: string) => {
  const mock = api as unknown as Record<string, () => unknown>;
  const original = mock[name].bind(api);
  mock[name] = () => {
    mock[name] = original;
    reportFailure({ message, path, status: 503 });
    return Promise.reject(new ApiError(503, message, null));
  };
};

// Set once at load so every mount starts signed in; the login checks clear it and put it back themselves.
localStorage.setItem('phoebis.mockSession', 'admin');

export interface Mounted {
  container: HTMLElement;
  navigate: (path: string) => Promise<void>;
  settle: (ms?: number) => Promise<void>;
  text: () => string;
  unmount: () => void;
}

const wait = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

export async function mount(path: string): Promise<Mounted> {
  const container = document.createElement('div');
  document.body.appendChild(container);
  const root = createRoot(container);
  const router = createMemoryRouter(routes, { initialEntries: [path] });
  root.render(
    <SessionProvider>
      <AppSettingsProvider>
        <RouterProvider router={router} />
      </AppSettingsProvider>
    </SessionProvider>,
  );
  await wait(600);
  return {
    container,
    navigate: async (p) => {
      await router.navigate(p);
      await wait(600);
    },
    settle: (ms = 400) => wait(ms),
    text: () => {
      // Closed <dialog> elements keep their text in the DOM but are not visible.
      const clone = container.cloneNode(true) as HTMLElement;
      clone.querySelectorAll('dialog:not([open])').forEach((d) => d.remove());
      return clone.textContent ?? '';
    },
    unmount: () => {
      root.unmount();
      container.remove();
    },
  };
}
