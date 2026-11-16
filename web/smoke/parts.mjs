// The smoke's fan-out: the views a run holds, the cap on its runners, the runners themselves and the join of their
// reports. `run.mjs` runs its blocks at its import, so what a unit test reads of the fan-out stands here, and
// `launch.mjs` holds the one call that starts processes.

/**
 * Each view a block of `run.mjs` names, in the order a fan-out prints them. A run that holds several views starts one
 * runner per view, so a view the blocks name and this list lacks would run in no runner of a whole run: `run.mjs` fails
 * a run whose blocks name one.
 */
export const VIEWS = ['/', '/accounts', '/analytics', '/guardrails', '/keepalive', '/keys', '/meter', '/models', '/pools', '/quota', '/requests', '/routing-log', '/sessions', '/settings'];

/** The paths a filter's value names, as `run.mjs` reads `SMOKE_ONLY`: split at the commas, trimmed, no empty word. */
export function wordsOf(value) {
  return (value ?? '').split(',').map((word) => word.trim()).filter(Boolean);
}

/**
 * The views a run holds, each once: each view of the list where `words` is empty, else the listed views `words` names
 * in the list's order, and behind them each word outside the list, whose own runner answers for it as `run.mjs` answers
 * for a path no block names.
 */
export function selectViews(words) {
  if (words.length === 0) return [...VIEWS];
  const outside = words.filter((word) => !VIEWS.includes(word));
  return [...VIEWS.filter((view) => words.includes(view)), ...new Set(outside)];
}

/**
 * How many runners a run starts side by side: `value` where it names a whole number from 1 on, else one per core, and
 * never more than the run has views. Any other value throws with the value in its message, since a mistyped cap would
 * else run as the default, and a cap of 0 would run no view and end green.
 */
export function jobsOf(value, views, cores) {
  if (value === undefined) return Math.min(cores, views.length);
  if (!/^[0-9]+$/.test(value) || Number(value) < 1) throw new Error(`SMOKE_JOBS=${value} names no whole number from 1 on`);
  return Math.min(Number(value), views.length);
}

/**
 * One runner from its start to its end. The run ends once the child has closed and its channel has disconnected, since
 * a report can arrive behind the close, or at once where the child never started, which may emit no other event.
 */
function runOne(view, start) {
  const began = performance.now();
  const child = start(view);
  const output = [];
  let report = null;
  child.stdout?.on('data', (chunk) => output.push({ chunk, stream: 'stdout' }));
  child.stderr?.on('data', (chunk) => output.push({ chunk, stream: 'stderr' }));
  child.on('message', (message) => {
    report = message;
  });
  return new Promise((resolve) => {
    let closed = null;
    let disconnected = false;
    let ended = false;
    const end = (status, note) => {
      if (ended) return;
      ended = true;
      if (note) output.push({ chunk: `${note}\n`, stream: 'stderr' });
      resolve({ output, report, seconds: (performance.now() - began) / 1000, status, view });
    };
    const settle = () => {
      if (closed === null || !disconnected) return;
      if (closed.signal) end(1, `the runner of ${view} ended on ${closed.signal}`);
      else if (report === null) end(1, `the runner of ${view} sent no report`);
      else end(closed.code ?? 1, null);
    };
    child.on('error', (error) => end(1, `the runner of ${view} did not start: ${error.message}`));
    child.on('disconnect', () => {
      disconnected = true;
      settle();
    });
    child.on('close', (code, signal) => {
      closed = { code, signal };
      settle();
    });
  });
}

/**
 * Starts one runner per view through `start`, at most `limit` side by side and in the views' order, and answers the
 * runs in that order once each has ended. `start` answers a child process with a channel, or anything alike: a `stdout`
 * and a `stderr` stream and the events `error`, `message`, `disconnect` and `close`. A run keeps its child's output
 * chunks in the order they arrived, each with its stream, the report the child sent, its status and its seconds. It
 * ends once the child has closed and its channel has disconnected, since a report can arrive behind the close. A child
 * that never started, that a signal ended or that sent no report counts as failed, with a line behind its output that
 * says which.
 */
export async function fanOut(views, limit, start) {
  const runs = [];
  let next = 0;
  // Each slot takes the next view as its runner ends, so at most `limit` children live at one time.
  const slot = async () => {
    while (next < views.length) {
      const index = next;
      next += 1;
      runs[index] = await runOne(views[index], start);
    }
  };
  await Promise.all(Array.from({ length: Math.min(limit, views.length) }, slot));
  return runs;
}

/**
 * The counts of a fan-out as one runner over the same views prints them. A block that several runners ran counts once,
 * and a check of it passed where it passed in each of them. A result outside a block counts once per runner. `skipped`
 * counts the blocks no runner ran, and `rows` is the first report's.
 */
export function joinReports(reports) {
  const blocks = reports[0]?.blocks ?? 0;
  const ran = new Set(reports.flatMap((report) => report.ran));
  // A check of a block is its place among the block's results, the same in each runner that ran the block.
  const inBlocks = new Map();
  let outside = 0;
  let outsidePassed = 0;
  for (const report of reports) {
    const seen = new Map();
    for (const [block, ok] of report.checks) {
      if (block === 0) {
        outside += 1;
        if (ok) outsidePassed += 1;
        continue;
      }
      const place = seen.get(block) ?? 0;
      seen.set(block, place + 1);
      const key = `${block}:${place}`;
      inBlocks.set(key, (inBlocks.get(key) ?? true) && ok);
    }
  }
  const passed = [...inBlocks.values()].filter(Boolean).length + outsidePassed;
  return { blocks, passed, rows: reports[0]?.rows ?? 0, skipped: blocks - ran.size, total: inBlocks.size + outside };
}

/** A run's closing lines: its checks, under a filter the blocks it skipped, and the mock's request rows. */
export function closingLines({ blocks, filter, passed, rows, skipped, total }) {
  return [
    `${passed}/${total} checks passed`,
    ...(filter.length > 0 ? [`SMOKE_ONLY=${filter.join(',')}: ${skipped} of ${blocks} blocks skipped`] : []),
    `fixture rows: ${rows} requests`,
  ];
}
