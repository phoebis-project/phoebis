import type { ReactNode } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { createMemoryRouter, MemoryRouter, RouterProvider } from 'react-router-dom';
import { api } from '@/api';
import { decodeSettings, effectiveLabel } from '@/api/decode';
import { ApiError, call, noteBuild, reportFailure, SessionNotKept } from '@/api/http';
import { routes } from '@/app/App';
import { SessionProvider } from '@/app/session';
import { AppSettingsProvider } from '@/app/settings';
import { installClock } from '@/lib/clock';
import { planLabel } from '@/lib/quota';
import { inFlight, installMockClock } from '@/mock';
import { SETTINGS_WIRE } from '@/mock/operations';
import { ALL_ROLLUPS, DELETED_HOLDER_DAY, DELETED_HOLDER_GENERATION, REQUEST_EVENTS, REQUESTS } from '@/mock/requests';
import { SETTINGS_SAMPLE } from '@/mock/samples';
import { fakeClock } from './clock';

export { api, decodeSettings, DELETED_HOLDER_DAY, DELETED_HOLDER_GENERATION, effectiveLabel, planLabel, SessionNotKept, SETTINGS_WIRE };
// The console's routes and the page load's error element, so a check reads which routes carry it.
export { routes };
export { PageLoadFailed } from '@/components/ui/primitives';
// The transport itself, for the check that needs its 401 path, which no mock call takes.
export { call };
// The fold the Cost section sums its groups by, so a check reads the totals the page draws from one derivation.
export { sumGroups } from '@/api/fold';
export { AttentionRows } from '@/components/AttentionRows';
export { MAX_BURN_LINES, NATIVE_SUBSET, shareHint } from '@/lib/analytics';
// The model item's sentence, so the attention line's check reads its title against the story's own detail.
export { ATTENTION_REFRESH_MS, attentionDetail } from '@/lib/attention';
export { default as React } from 'react';
export { AttemptCount, AttemptTrail } from '@/components/ui/AttemptTrail';
// The viewer's row height and margin, so a body check bounds the drawn rows by the figures the viewer draws by.
export { ROW_PX, WINDOW_ROWS } from '@/components/BodyViewer';
// The cost panels a check mounts alone, on a cost model the mock does not answer.
export { PeakBand } from '@/components/costs/PeakBand';
export { PlanRates } from '@/components/costs/PlanRates';
export { WindowsByTier } from '@/components/costs/WindowsByTier';
export { Expert, Field, GroupedDialog, GroupLink, Input, PageLink, Pill, SwitchedNumber, Toggle } from '@/components/ui/primitives';
// The guardrail dialog a check mounts alone, on a stamp the story does not hold.
export { GuardrailDialog } from '@/pages/GuardrailsPage';
// The figures and the model form a Dashboard check computes its expectation from, so the check and the page read one
// derivation; the cost reference's and the About card's checks read theirs the same way.
export { anchorTier, bandSentence, fullWindowsAWeek, inBand, nextLocalChange, ratesOf, regimeClients, regimeNames, regimeTitle, regimeWord, revisionAt, shareIn, smallestTier, tierCapacity, tierGroups, tierOrder, weekScale, weightOf, windowMultiple, withRegime } from '@/lib/costs';
// The Cache miss reasons, their labels and short words in the page's order, so the filter and Analytics table checks
// read the words the pages draw.
export { CACHE_MISS_LABEL, CACHE_MISS_REASONS, CACHE_MISS_SHORT, missLabel, missShort } from '@/lib/cache';
// The strip's model and its buttons' names, so a card check spells the stations a record draws and the names a reader
// hears from the derivation the strip reads.
export { leafName, stationModel, stationName, terminusName } from '@/lib/decisionStrip';
export { earlierHolderLabel, fmtAbsolute, fmtCompact, fmtInt, fmtPct, fmtPeriodEnd, fmtRelative, fmtShare, fmtUsd, fmtWeek, shortModel, spanText, undatedModel, weekShort } from '@/lib/format';
// The pace mark's offset, so the check and the meter place it by one arithmetic.
export { remainingTimeShare } from '@/lib/quota';
// The projection's phrase, its burn and the span it spells, so a check computes the words a window draws from the one
// derivation the pages draw them from, at the instant the page read.
export { burnWords, minuteSpan, projectionWords } from '@/lib/quota';
// A request's native unit, so a check picks the rows it reads by the rule the Cost cell draws them by.
export { costUnitOf } from '@/lib/requests';
// The drawer's widenings, so a check reads the drawer's links against the addresses the page builds them from.
export { requestsWiden } from '@/lib/requestsLink';
export { fmtWindows, ringFigures } from '@/lib/rings';
// Every kind the Kinds list offers, so the Kinds link check mounts an address that lists every kind.
export { ALL_KINDS } from '@/lib/routingLog';
// The walk's words, so a card check reads a popover's column and its title as the console words them.
export { keyWord, STEP_NAME, words } from '@/lib/routingLog';
// The legend's words, so the shell's check reads the popover against the one source the console draws it from.
export { UNIT_LABEL, UNIT_LEGEND, UNIT_LEGEND_NOTE, WEEK_LABEL, WEEK_POSITIONS, weekOf } from '@/lib/unit';
export const MOCK_VERSION = SETTINGS_SAMPLE.about.version;
export const requestEvents = REQUEST_EVENTS;
export const requests = REQUESTS;
// The mock's raw hourly rows, so a check can read the story itself rather than a page's derivation of it.
export const rollups = ALL_ROLLUPS;

// One clock for the whole run, live until a mount freezes it, so a check that moves time moves every timer a page
// armed through the clock module and none of the real ones the settles wait on.
const fake = fakeClock();
installClock(fake);
// A mock answer lands on the tick that reaches it or after its real latency, whichever comes first. A frozen clock
// arms no window timer, so the real wait keeps a mount's settle seeing its reads answer, and the tick lets a check that
// steps the clock read the answer with no real wait. The pages' own timers stay on the harness clock alone.
installMockClock({
  now: () => fake.now(),
  setTimeout: (fn, ms) => {
    let real = 0;
    const faked = fake.setTimeout(() => {
      window.clearTimeout(real);
      fn();
    }, ms);
    real = window.setTimeout(() => {
      fake.clearTimeout(faked);
      fn();
    }, ms);
    return faked;
  },
});

/**
 * Moves a frozen clock by `ms`, firing each timer due inside the span at its own deadline; on a live clock it
 * throws, so a check that forgot `mount(path, { now })` fails at once instead of racing real time.
 */
export const tick = (ms: number) => fake.tick(ms);

/**
 * Fails one mock call the way the transport fails a real one: the rejection reaches the page's `useAsync`,
 * and the route reaches the shell's toast, which the mock would otherwise never report because it never goes
 * through `call`. The override puts the original method back as it fires, so a retry reads the mock again.
 * `network` fails it as a router that never answered, with status 0; otherwise the refusal carries `status`, 503
 * unless one is given. Every status reaches the toast as a route failure, where the transport takes three apart: a
 * 401 returns to the login, a 403 shows the router's own sentence, and a 404 reports nothing. `quiet` fails one of the
 * dialogs' writes, which the transport sends quiet, so its refusal reaches no toast.
 */
export const failNextCall = (name: string, path: string, message: string, options: { network?: boolean; quiet?: boolean; status?: number } = {}) => {
  const mock = api as unknown as Record<string, () => unknown>;
  const original = mock[name].bind(api);
  const status = options.network ? 0 : (options.status ?? 503);
  mock[name] = () => {
    mock[name] = original;
    // The transport reads the session straight through and keeps the login's refusals to the login screen, so
    // neither one reaches the shell.
    if (name !== 'getSession' && name !== 'login' && !options.quiet) reportFailure({ message, path, status });
    return Promise.reject(new ApiError(status, message, null, path));
  };
};

/** Feeds the transport a router build other than the bundle's, so the shell's banner is on the smoke. */
export const mismatchBuild = (): void => noteBuild('0000000000000000000000000000000000000000');

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
      return Promise.reject(new ApiError(503, message, null, path));
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

/** A pass-through spy on one mock method: every call's arguments, in order, until `restore()` puts the method back. */
export const countCalls = (name: string) => {
  const mock = api as unknown as Record<string, (...args: unknown[]) => unknown>;
  const original = mock[name];
  const calls: unknown[][] = [];
  mock[name] = (...args: unknown[]) => {
    calls.push(args);
    return original.apply(api, args);
  };
  return {
    calls,
    restore: () => {
      mock[name] = original;
    },
  };
};

/**
 * A pass-through spy on one mock method: every call's arguments and, once the call lands, its answer, in order, until
 * `restore()` puts the method back. A check reads the answer the page itself was handed, which a second call with the
 * same arguments need not repeat.
 */
export const recordedCalls = (name: string) => {
  const mock = api as unknown as Record<string, (...args: unknown[]) => unknown>;
  const original = mock[name];
  const calls: { answer?: unknown; args: unknown[] }[] = [];
  mock[name] = (...args: unknown[]) => {
    const entry: { answer?: unknown; args: unknown[] } = { args };
    calls.push(entry);
    const answer = original.apply(api, args);
    void Promise.resolve(answer).then((value) => (entry.answer = value), () => undefined);
    return answer;
  };
  return {
    calls,
    restore: () => {
      mock[name] = original;
    },
  };
};

// Set once at load so every mount starts signed in; the login checks clear it and put it back themselves.
localStorage.setItem('phoebis.mockSession', 'admin');

export interface Mounted {
  container: HTMLElement;
  /** The memory router's address, so a check reads what a page wrote without a window location. */
  location: () => { hash: string; pathname: string; search: string };
  navigate: (path: string) => Promise<void>;
  settle: (ms?: number) => Promise<void>;
  text: () => string;
  unmount: () => void;
}

const wait = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/**
 * A mounted view draws its headings and skeletons at once while the mock answers through `latency` on a real timer, so
 * the signal is the mock's own: two reads of the container's text 50 ms apart that agree, are non-empty and each find
 * the in-flight counter at zero. One zero read is not enough, since a read can fall between an answer's timer and the
 * call the next render makes, while the text still shows the frame before it, such as the session check's splash. The
 * wait is bounded at 5 s, so a mount that draws nothing waits the whole bound and goes on.
 */
async function settled(container: HTMLElement): Promise<void> {
  const started = Date.now();
  let last: string | null = null;
  while (Date.now() - started < 5_000) {
    await wait(50);
    const text = container.textContent ?? '';
    const idle = inFlight() === 0;
    if (idle && text !== '' && text === last) return;
    last = idle ? text : null;
  }
}

function mounted(container: HTMLElement, root: Root, navigate: Mounted['navigate'], location: Mounted['location']): Mounted {
  return {
    container,
    location,
    navigate,
    settle: (ms = 400) => wait(ms),
    text: () => {
      // Closed <dialog> elements keep their text in the DOM but are not visible, and a description host's node repeats
      // titles no sighted reader sees, so a needle that only a title carries cannot pass on it.
      const clone = container.cloneNode(true) as HTMLElement;
      clone.querySelectorAll('dialog:not([open]), [data-descriptions]').forEach((d) => d.remove());
      return clone.textContent ?? '';
    },
    unmount: () => {
      root.unmount();
      container.remove();
      fake.freeze(null);
    },
  };
}

// The three lazy pages load once before the first mount, so the settle on the in-flight count never waits on a chunk
// fetch the count does not see.
const lazyPages = Promise.all([import('@/pages/AccountsPage'), import('@/pages/AnalyticsPage'), import('@/pages/PoolsPage')]);

/** `now` freezes the clock at that instant before the first render, so only `tick` moves it or fires a timer. */
export async function mount(path: string, options: { now?: number } = {}): Promise<Mounted> {
  await lazyPages;
  if (options.now !== undefined) fake.freeze(options.now);
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
  await settled(container);
  return mounted(
    container,
    root,
    async (p) => {
      // A navigation that moves the search alone calls no mock route, so the settle has no call to wait out, and two of
      // its reads could agree on the frame a transition still holds. Written at once, as the pages' own address writes
      // are, the page has drawn the new address by the time the router answers.
      await router.navigate(p, { flushSync: true });
      await settled(container);
    },
    () => {
      const { hash, pathname, search } = router.state.location;
      return { hash, pathname, search };
    },
  );
}

/**
 * Mounts one element under a memory router, for a check over a component the pages do not reach alone. The providers
 * a page has are there too, so a dialog reads the stored expert view as a page's does.
 */
export async function mountNode(node: ReactNode): Promise<Mounted> {
  const container = document.createElement('div');
  document.body.appendChild(container);
  const root = createRoot(container);
  root.render(
    <SessionProvider>
      <AppSettingsProvider>
        <MemoryRouter>{node}</MemoryRouter>
      </AppSettingsProvider>
    </SessionProvider>,
  );
  await settled(container);
  return mounted(
    container,
    root,
    () => {
      throw new Error('a mounted node has no routes to navigate');
    },
    () => {
      throw new Error('a mounted node has no address');
    },
  );
}
