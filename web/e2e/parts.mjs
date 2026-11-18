// The browser pass's parts: each walk over the views, by the key its measures lead their lines with, and `rest`, the
// measures behind the walks that belong to no walk. `views.mjs` runs the parts `PHOEBIS_E2E_WALKS` names, and
// `mock-pass.mjs` starts one `views.mjs` per part, side by side.

const REST = 'rest';

/**
 * The key of a theme under a palette, which every measure leads its lines with and the shots' directory takes: the bare
 * theme under the default palette, whose shots the site reads, and `<theme>-github` under the other.
 */
export const keyOf = (theme, palette) => (palette === 'default' ? theme : `${theme}-${palette}`);

/**
 * The pass's walks over the views, each theme under each palette and the default palette's first. The router's pass
 * walks the default palette alone, since its test counts one shot per view and theme. `mock-pass.mjs` reads the mock's
 * walks here, since `views.mjs` runs its pass on import.
 */
export const walksOf = (mock) => (mock ? ['default', 'github'] : ['default']).flatMap((palette) => ['light', 'dark'].map((mode) => ({ key: keyOf(mode, palette), mode, palette })));

/**
 * The parts `value` names, in the pass's own order and each once: the walks of `walkKeys` it names, and `rest`. A value
 * that names nothing names every part, and a word that names no part throws, since a mistyped part would run no
 * measure and leave the pass green.
 */
export function selectParts(value, walkKeys) {
  const every = [...walkKeys, REST];
  const words = (value ?? '').split(',').map((word) => word.trim()).filter((word) => word !== '');
  if (words.length === 0) return every;
  const unknown = words.filter((word) => !every.includes(word));
  if (unknown.length > 0) throw new Error(`PHOEBIS_E2E_WALKS names no part ${unknown.join(', ')}: the parts are ${every.join(', ')}`);
  return every.filter((part) => words.includes(part));
}

/**
 * Starts every part's child at once through `start` and waits for all of them. `start` answers a child process, or
 * anything alike: a `stdout` and a `stderr` stream, and the `error` and `close` events. Each run keeps its child's
 * output chunks in the order they arrived, each with its stream, so the caller can print one child's output whole
 * before the next one's. The pass's status is 0 only where every child closed with 0, and otherwise the first failed
 * child's in the parts' order: a child that never started, or that a signal ended, counts as failed.
 */
export async function fanOut(parts, start) {
  const notStarted = (part, error) => `the browser pass of ${part} did not start: ${error.message}`;
  const runs = await Promise.all(
    parts.map((part) => {
      let child;
      try {
        child = start(part);
      } catch (error) {
        // A spawn the host refuses for memory throws in place of emitting `error`, and a fan-out that rejected on it
        // would leave the children already started running with no one waiting for them.
        return { output: [{ chunk: `${notStarted(part, error)}\n`, stream: 'stderr' }], part, status: 1 };
      }
      const output = [];
      child.stdout?.on('data', (chunk) => output.push({ chunk, stream: 'stdout' }));
      child.stderr?.on('data', (chunk) => output.push({ chunk, stream: 'stderr' }));
      return new Promise((resolve) => {
        // A child that never started emits `error`, and `close` may or may not follow it, so the first one settles.
        let ended = false;
        const end = (status, note) => {
          if (ended) return;
          ended = true;
          if (note) output.push({ chunk: `${note}\n`, stream: 'stderr' });
          resolve({ output, part, status });
        };
        child.on('error', (error) => end(1, notStarted(part, error)));
        child.on('close', (code, signal) => end(code ?? 1, signal ? `the browser pass of ${part} ended on ${signal}` : null));
      });
    }),
  );
  const failed = runs.find((run) => run.status !== 0);
  return { runs, status: failed ? failed.status : 0 };
}
