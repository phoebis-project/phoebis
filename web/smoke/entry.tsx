import React from 'react';
import { createRoot } from 'react-dom/client';
import { createMemoryRouter, RouterProvider } from 'react-router-dom';
import { api } from '@/api';
import { ApiError, reportFailure } from '@/api/http';
import type { LiveEvent } from '@/api/types';
import { routes } from '@/app/App';
import { SessionProvider } from '@/app/session';
import { AppSettingsProvider } from '@/app/settings';
import { ALL_ROLLUPS, DELETED_HOLDER_DAY, DELETED_HOLDER_GENERATION, REQUEST_EVENTS, REQUESTS } from '@/mock/requests';
import { SETTINGS_SAMPLE } from '@/mock/samples';

export { api, DELETED_HOLDER_DAY, DELETED_HOLDER_GENERATION };
export const MOCK_VERSION = SETTINGS_SAMPLE.about.version;
export const requestEvents = REQUEST_EVENTS;
export const requests = REQUESTS;
// The mock's raw hourly rows, so a check can read the story itself rather than a page's derivation of it.
export const rollups = ALL_ROLLUPS;

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

/**
 * Fails every call of the named methods, keyed to the route each one reads, the way an outage of the store
 * behind them fails them all at once. `recover` lets calls through again and records each one's arguments, so a
 * check can tell which reads a retry made; `restore` puts the original methods back.
 */
export const failCalls = (routes: Record<string, string>, message: string) => {
  const mock = api as unknown as Record<string, (...args: unknown[]) => unknown>;
  const originals = Object.fromEntries(Object.keys(routes).map((name) => [name, mock[name]]));
  const calls: Record<string, unknown[][]> = Object.fromEntries(Object.keys(routes).map((name) => [name, []]));
  let down = true;
  for (const [name, path] of Object.entries(routes)) {
    mock[name] = (...args: unknown[]) => {
      if (!down) {
        calls[name].push(args);
        return originals[name].apply(api, args);
      }
      reportFailure({ message, path, status: 503 });
      return Promise.reject(new ApiError(503, message, null));
    };
  }
  return {
    calls,
    recover: () => {
      down = false;
    },
    restore: () => {
      for (const name of Object.keys(routes)) mock[name] = originals[name];
    },
  };
};

/**
 * Holds the next call to the named mock method behind a promise until `release()` resolves it, the way a slow
 * upstream answer lands after the caller has moved on. A recording pass-through replaces the method as soon as
 * the held call is made, so a later call is never blocked behind it; every call from then on, the held one
 * included, is recorded with its arguments in the returned `calls` list, as `failCalls`'s `recover` records them,
 * until `restore()` puts the original method back.
 */
export const deferNextCall = (name: string) => {
  const mock = api as unknown as Record<string, (...args: unknown[]) => unknown>;
  const original = mock[name].bind(api);
  const calls: unknown[][] = [];
  let fire: () => void = () => {};
  const gate = new Promise<void>((resolve) => {
    fire = resolve;
  });
  mock[name] = (...args: unknown[]) => {
    mock[name] = (...rest: unknown[]) => {
      calls.push(rest);
      return original(...rest);
    };
    calls.push(args);
    return gate.then(() => original(...args));
  };
  return {
    calls,
    release: () => fire(),
    restore: () => {
      mock[name] = original;
    },
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
