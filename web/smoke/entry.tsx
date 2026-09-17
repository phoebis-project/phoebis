import React from 'react';
import { createRoot } from 'react-dom/client';
import { createMemoryRouter, RouterProvider } from 'react-router-dom';
import { routes } from '@/app/App';
import { AppSettingsProvider } from '@/app/settings';
import { REQUESTS } from '@/mock/requests';

export const requests = REQUESTS;

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
    <AppSettingsProvider>
      <RouterProvider router={router} future={{ v7_startTransition: true }} />
    </AppSettingsProvider>,
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
