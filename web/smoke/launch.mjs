import { spawn } from 'node:child_process';
import { availableParallelism } from 'node:os';
import { fileURLToPath } from 'node:url';
import { closingLines, fanOut, jobsOf, joinReports, selectViews, wordsOf } from './parts.mjs';

/** Leads a view's lines, so `grep -e '^smoke of ' -e '^FAIL'` places each failing case under its view. */
const headerOf = ({ report, seconds, view }) => {
  if (report === null) return `smoke of ${view}: no report, ${Math.round(seconds)} s`;
  const failed = report.checks.filter(([, ok]) => !ok).length;
  return `smoke of ${view}: ${report.checks.length} checks, ${failed} failed, ${Math.round(seconds)} s, peak ${Math.round(report.peak / 1024)} MB`;
};

/**
 * Runs the smoke's views side by side. `run.mjs` calls it first, with its own address. In a runner this launcher
 * started, and where the run holds one view or `SMOKE_JOBS` is 1, it returns and the calling process runs the blocks.
 * Else it starts one `run.mjs` per view under `SMOKE_ONLY` with that view, prints each view's lines together in the
 * views' order and the closing lines one runner over the same views would print, and ends the process with 1 where a
 * runner failed.
 */
export async function launch(runner) {
  if (process.send !== undefined) return;
  const words = wordsOf(process.env.SMOKE_ONLY);
  const views = selectViews(words);
  let limit;
  try {
    limit = jobsOf(process.env.SMOKE_JOBS, views, availableParallelism());
  } catch (error) {
    console.error(error.message);
    process.exit(2);
  }
  if (limit === 1) return;
  const began = performance.now();
  const runs = await fanOut(views, limit, (view) =>
    spawn(process.execPath, [...process.execArgv, fileURLToPath(runner)], { env: { ...process.env, SMOKE_ONLY: view }, stdio: ['inherit', 'pipe', 'pipe', 'ipc'] }),
  );
  // Each runner's output whole, in the views' order, so the lines of two runners never mix.
  for (const run of runs) {
    process.stdout.write(`${headerOf(run)}\n`);
    for (const { chunk, stream } of run.output) process[stream].write(chunk);
  }
  const joined = joinReports(runs.map((run) => run.report).filter((report) => report !== null));
  console.log('');
  for (const line of closingLines({ ...joined, filter: words })) console.log(line);
  const failed = runs.filter((run) => run.status !== 0).map((run) => run.view);
  if (failed.length > 0) console.log(`failed: ${failed.join(' ')}`);
  console.log(`${views.length} views, ${limit} side by side, ${Math.round((performance.now() - began) / 1000)} s`);
  process.exit(failed.length > 0 ? 1 : 0);
}
