import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

// The mock skips its draws by the local hour and builds its story from the clock it loads at, so the file runs in UTC
// and loads the bundle at the smoke run's instant. `node --test` gives each file a process of its own.
process.env.TZ = 'UTC';
const NOW = Date.parse('2026-09-16T14:30:00Z');
const wall = Date.now;
Date.now = () => NOW;
let d;
try {
  d = await import('./dist/api.js');
} finally {
  Date.now = wall;
}

const fixture = JSON.parse(readFileSync(new URL('./fixtures/routing/walk_steps.json', import.meta.url), 'utf8'));
const legacyFixture = JSON.parse(readFileSync(new URL('./fixtures/routing/walk_steps_v1.json', import.meta.url), 'utf8'));
const api = new d.MockApi();

async function recordOf(id) {
  const [requestId, seq] = id.split('/');
  const record = await api.getDecision(requestId, Number(seq));
  assert.ok(record, `the story holds ${id}`);
  return record;
}

const modelOf = (record) => d.stationModel(record, NOW);

/**
 * The strip as one entry per station: its word, its badge and its state, a track station marked with a caret. A
 * partition station reads its word and state alone, since its count is a figure the story derives.
 */
const read = (m) => m.stations.map((s) => `${s.level === 'track' ? '^' : ''}${s.word}${s.id.startsWith('partitions:') ? '' : `: ${s.badge || '-'}`} (${s.state})`);
const terminiOf = (m) => m.termini.map((t) => `${t.word}: ${t.badge ?? '-'} (${t.state}${t.drawn ? '' : ', not drawn'})`);
const stationOf = (m, id) => {
  const s = m.stations.find((x) => x.id === id);
  assert.ok(s, `the strip holds ${id}`);
  return s;
};

const step = (name, verdict, facts = []) => ({ facts, step: name, verdict });
const fact = (key, value, unit = null, accountId = null) => ({ accountId, key, unit, value });
const RULES = ['move:spent', 'move:overage', 'move:return_to_plan', 'move:drain', 'move:shed'];
const RUNGS_NONE = () => ['rung:content', 'rung:with_main', 'rung:static'].map((r) => step(r, 'none'));
const PARTITIONS = ['Spent', 'At cap', 'No fit', 'Winding down'];
const LEGACY_PARTITIONS = ['Spent', 'At cap', 'Above ceiling', 'Winding down', 'Floor'];
const FRESH_LINE = (pin, eligible) => [`Pin: ${pin} (taken)`, `Eligible: ${eligible} (taken)`, ...PARTITIONS.map((w) => `${w} (taken)`)];
const FIVE_RULES = (verdict, state) => ['spent', 'overage', 'return to plan', 'drain', 'shed'].map((w) => `${w}: ${verdict} (${state})`);
const THREE_TERMINI = (taken) => ['chosen', 'held', 'refused'].map((k) => (k === taken[0] ? `${d.TERMINUS_WORD[k]}: ${taken[1]} (taken)` : `${d.TERMINUS_WORD[k]}: - (idle)`));
const PINNED_TERMINI = (badge) => [`Chosen: ${badge} (taken)`, 'Held for a slot: - (idle, not drawn)', 'Refused: - (idle, not drawn)'];

/** A record built in the test over a story record's candidates and settings, with its own walk and outcome kind. */
const built = (base, walk, kind, more = {}) => ({ ...base, ...more, outcome: { ...base.outcome, kind }, walk });

/** The points along a path drawn with M, H, V and C, each cubic sampled finely. */
function pointsOf(path) {
  const tokens = path.match(/[MHVC]|\d*\.?\d+(?:e-?\d+)?/g);
  const points = [];
  let [x, y, i] = [0, 0, 0];
  const n = () => Number(tokens[i++]);
  while (i < tokens.length) {
    const command = tokens[i++];
    if (command === 'M') [x, y] = [n(), n()];
    else if (command === 'H') x = n();
    else if (command === 'V') y = n();
    else if (command === 'C') {
      const [x1, y1, x2, y2, x3, y3] = [n(), n(), n(), n(), n(), n()];
      for (let k = 1; k < 64; k += 1) {
        const [t, u] = [k / 64, 1 - k / 64];
        points.push([u * u * u * x + 3 * u * u * t * x1 + 3 * u * t * t * x2 + t * t * t * x3, u * u * u * y + 3 * u * u * t * y1 + 3 * u * t * t * y2 + t * t * t * y3]);
      }
      [x, y] = [x3, y3];
    } else throw new Error(`${command} in ${path}`);
    points.push([x, y]);
  }
  return points;
}

/** The highest point a path reaches where it passes `x`, the one nearest the line above it. */
function highestAt(path, x) {
  const points = pointsOf(path);
  let best = Infinity;
  for (let k = 1; k < points.length; k += 1) {
    const [[ax, ay], [bx, by]] = [points[k - 1], points[k]];
    if (ax === bx) {
      if (ax === x) best = Math.min(best, ay, by);
    } else if ((ax - x) * (bx - x) <= 0) best = Math.min(best, ay + ((by - ay) * (x - ax)) / (bx - ax));
  }
  return best;
}

test('every step the router records has a station, a renderer and a stub word, and every station a step, in every record version', () => {
  const renderers = ['candidate', 'chips', 'hold', 'list', 'outcome', 'rung', 'sentence', 'table'];
  const stubs = ['fires', 'match', 'released'];
  // A record from version 2 on draws through the table, a version 1 record through the frozen fixture's legacy table.
  for (const [table, steps, version] of [[d.STATION_KINDS, fixture, 'from version 2'], [d.LEGACY_STATION_KINDS, legacyFixture, 'version 1']]) {
    for (const { step: id, verdicts } of steps.steps) {
      assert.ok(Object.hasOwn(table, id), `${version}: ${id} has a station kind`);
      assert.ok(Object.hasOwn(d.STEP_NAME, id), `${version}: ${id} has a step name`);
      const kind = table[id];
      assert.ok(renderers.includes(kind.renderer), `${version}: ${id} renders as ${kind.renderer}`);
      assert.ok(kind.stub === null || stubs.includes(kind.stub), `${version}: ${id} has the stub ${kind.stub}`);
      assert.ok(kind.group === null || Object.hasOwn(d.GROUP_WORD, kind.group), `${version}: ${id} sits in the group ${kind.group}`);
      for (const verdict of verdicts) assert.ok(d.TONED_VERDICTS.has(verdict), `${version}: ${id} ${verdict} has a tone`);
    }
    const recorded = new Set(steps.steps.map((s) => s.step));
    for (const id of Object.keys(table)) assert.ok(recorded.has(id), `${version}: ${id} is a step the router records`);
    for (const value of steps.partition_values) {
      assert.ok(Object.hasOwn(d.PARTITION_WORD, value) && typeof d.PARTITION_WORD[value] === 'string' && d.PARTITION_WORD[value] !== '', `${version}: ${value} has a word`);
      assert.ok(d.TONED_VERDICTS.has(value), `${version}: ${value} has a tone`);
    }
  }
  for (const { step: id } of fixture.steps) {
    const name = d.STEP_NAME[id];
    const rest = name.replace(/^[^:]*: /, '');
    assert.ok([name, rest, rest.toLowerCase()].includes(d.stationWord(id)), `${id} reads ${d.stationWord(id)} against ${name}`);
  }
  assert.deepEqual([d.PARTITION_FACTS.map((f) => d.PARTITION_STATION_WORD[f]), d.LEGACY_PARTITION_FACTS.map((f) => d.PARTITION_STATION_WORD[f])], [PARTITIONS, LEGACY_PARTITIONS]);
  // Under a bracket the group's prefix leaves and the rest reads lowercased, while the one Partitions station stands alone.
  assert.deepEqual(['rung:content', 'rung:with_main', 'move:spent', 'move:return_to_plan', 'partitions', 'selection', 'fit', 'reserve', 'cool'].map(d.stationWord), ['content', 'with main', 'spent', 'return to plan', 'Partitions', 'Rest of the walk', 'Fit', 'Reserve', 'Cool reading']);
});

test("the story's records give their stations, states, host and termini", async () => {
  const fresh = modelOf(await recordOf('req_ef099b34-453/1'));
  assert.deepEqual(read(fresh), [...FRESH_LINE('none', 'match'), 'content: none (taken)', 'with main: none (taken)', 'static: none (taken)', 'Pick: chosen (taken)', 'Reserve: booked (taken)']);
  assert.deepEqual([fresh.shape, fresh.decisive, fresh.host, fresh.branch], ['fresh', 'pick', 'pick', null]);
  assert.deepEqual(terminiOf(fresh), THREE_TERMINI(['chosen', 'placed']));
  assert.deepEqual(stationOf(fresh, 'pick').second, ['Pace P0']);
  assert.deepEqual(fresh.stations.filter((s) => s.id.startsWith('partitions:')).map((s) => [s.head, s.badge, s.tone]), [['Spent', 'off', 'neutral'], ['At cap', '0 out', 'good'], ['No fit', '1 out', 'warning'], ['Winding down', '0 out', 'good']]);
  assert.deepEqual(fresh.brackets.map((b) => [b.word, b.level, b.stations.length]), [['Partitions', 'line', 4], ['Rungs', 'line', 3]]);
  assert.deepEqual(fresh.stations.filter((s) => s.stub !== null).map((s) => `${s.id} ${s.stub}`), ['rung:content match', 'rung:with_main match', 'rung:static match']);
  assert.deepEqual(stationOf(fresh, 'rung:content').head, 'Rung: content');
  assert.equal(fresh.stations.filter((s) => s.decisive).length, 1);

  for (const id of ['req_f8085653-422/1', 'req_1eaa9bc1-a18/1']) {
    const renewal = modelOf(await recordOf(id));
    assert.deepEqual(read(renewal), ['Pin: match (taken)', 'Eligible: match (taken)', ...FIVE_RULES('no', 'taken'), 'Renew: renewed (taken)', 'Reserve: none (taken)'], id);
    assert.deepEqual([renewal.shape, renewal.decisive, renewal.host, renewal.branch], ['pinned', 'renew', 'renew', null], id);
    assert.deepEqual(renewal.brackets.map((b) => [b.word, b.level, b.stations]), [['Moves', 'line', RULES]], id);
    assert.deepEqual(renewal.stations.filter((s) => s.stub !== null).map((s) => s.stub), ['fires', 'fires', 'fires', 'fires', 'fires', 'released'], id);
    assert.deepEqual(terminiOf(renewal), PINNED_TERMINI('renewed'), id);
  }

  const spentMove = modelOf(await recordOf('req_da52d141-a5a/1'));
  assert.deepEqual(read(spentMove), ['Pin: match (taken)', 'Eligible: match (taken)', 'spent: fired (taken)', ...FIVE_RULES('not run', 'idle').slice(1), '^Move: moved (taken)', '^Fit: rewrite only (taken)', '^Cap filter: passed (taken)', '^Partitions: match (taken)', '^content: none (taken)', '^with main: none (taken)', '^static: match (taken)', 'Renew: not run (idle)', '^Reserve: booked (taken)']);
  assert.deepEqual([spentMove.shape, spentMove.decisive, spentMove.host, spentMove.branch], ['pinned', 'rung:static', 'rung:static', { from: 'move:spent', kind: 'move' }]);
  assert.deepEqual([stationOf(spentMove, 'move').head, stationOf(spentMove, 'move').second, stationOf(spentMove, 'move').tone, stationOf(spentMove, 'move').step], ['Move', ['spent rule'], 'warning', 'move:spent']);
  assert.deepEqual(spentMove.brackets.map((b) => [b.word, b.level, b.stations.length]), [['Moves', 'line', 5], ['Rungs', 'track', 3]]);
  assert.deepEqual(terminiOf(spentMove), PINNED_TERMINI('moved'));

  const shedMove = modelOf(await recordOf('req_868264bc-611/1'));
  assert.deepEqual(read(shedMove), ['Pin: match (taken)', 'Eligible: match (taken)', ...FIVE_RULES('no', 'taken').slice(0, 4), 'shed: fired (taken)', '^Move: moved (taken)', '^Fit: fits (taken)', '^Cap filter: passed (taken)', '^Partitions: match (taken)', '^content: none (taken)', '^with main: none (taken)', '^static: none (taken)', '^Pick: chosen (taken)', 'Renew: not run (idle)', '^Reserve: booked (taken)']);
  assert.deepEqual([shedMove.decisive, shedMove.host, shedMove.branch], ['pick', 'pick', { from: 'move:shed', kind: 'move' }]);
  assert.deepEqual(stationOf(shedMove, 'move').second, ['shed rule']);
  // The fired rule left the line by the track, so its stub became the track, and the idle Renew keeps its own.
  assert.deepEqual(shedMove.stations.filter((s) => s.stub !== null).map((s) => s.id), ['move:spent', 'move:overage', 'move:return_to_plan', 'move:drain', 'renew']);
  assert.deepEqual(terminiOf(shedMove), PINNED_TERMINI('moved'));

  const staticPlaced = modelOf(await recordOf('req_b2bed817-a58/1'));
  assert.deepEqual(read(staticPlaced), [...FRESH_LINE('none', 'match'), 'content: none (taken)', 'with main: none (taken)', 'static: match (taken)', 'Pick: not run (idle)', 'Reserve: booked (taken)']);
  assert.deepEqual([staticPlaced.decisive, staticPlaced.host, staticPlaced.branch], ['rung:static', 'pick', { from: 'rung:static', kind: 'rung' }]);
  assert.deepEqual([stationOf(staticPlaced, 'pick').index, stationOf(staticPlaced, 'pick').verdict, stationOf(staticPlaced, 'rung:static').stub], [null, null, null]);
  assert.deepEqual(terminiOf(staticPlaced), THREE_TERMINI(['chosen', 'placed']));

  // A placement beside main reaches Reserve past the idle Pick, and the booking it made stands taken there. The second
  // lands on secondary past the pool's ceiling and its rivals that fit read full, so the cap leaves secondary alone and
  // No fit keeps it on the least-bad rung.
  for (const [id, noFit, reserve] of [['req_d4a518a7-cd0/1', '1 out', 'Reserve: booked (taken)'], ['req_a0f5c94d-8f0/1', '0 out', 'Reserve: booked (taken)']]) {
    const withMain = modelOf(await recordOf(id));
    assert.deepEqual(read(withMain), [...FRESH_LINE('none', 'match'), 'content: none (taken)', 'with main: match (taken)', 'static: not run (idle)', 'Pick: not run (idle)', reserve], id);
    assert.deepEqual([withMain.decisive, withMain.host, withMain.branch], ['rung:with_main', 'pick', { from: 'rung:with_main', kind: 'rung' }], id);
    assert.deepEqual([stationOf(withMain, 'rung:static').verdict, stationOf(withMain, 'rung:static').tone, stationOf(withMain, 'partitions:no_fit').badge], ['not_run', 'neutral', noFit], id);
    assert.deepEqual(terminiOf(withMain), THREE_TERMINI(['chosen', 'placed']), id);
  }
  // On the least-bad rung no member fit, so its count of 0 warns, where the same count under fits is good.
  const leastBad = await recordOf('req_a0f5c94d-8f0/1');
  const onFits = { ...leastBad, walk: leastBad.walk.map((s) => (s.step === 'partitions' ? { ...s, facts: s.facts.map((f) => (f.key === 'ladder' ? { ...f, value: 'fits' } : f)) } : s)) };
  assert.deepEqual(
    [leastBad, onFits].map((r) => [stationOf(modelOf(r), 'partitions:no_fit').badge, stationOf(modelOf(r), 'partitions:no_fit').tone]),
    [['0 out', 'warning'], ['0 out', 'good']],
  );

  for (const [id, place] of [['req_69e30548-32e/1', 'cohort'], ['req_03ddc3af-4d5/1', 'strategy pick']]) {
    const hold = modelOf(await recordOf(id));
    assert.deepEqual(read(hold), [...FRESH_LINE('none', 'match'), 'content: none (taken)', 'with main: none (taken)', 'static: none (taken)', 'Pick: chosen (taken)', 'Hold: held (taken)', 'Reserve: none (taken)'], id);
    assert.deepEqual([hold.decisive, hold.host], ['hold', 'pick'], id);
    assert.deepEqual([stationOf(hold, 'hold').second, stationOf(hold, 'hold').tone, stationOf(hold, 'pick').second], [[place, 'bound 1 min'], 'warning', ['Pace P0']], id);
    assert.deepEqual([stationOf(hold, 'partitions:at_cap').badge, stationOf(hold, 'partitions:at_cap').tone, stationOf(hold, 'partitions:at_cap').verdict], ['held', 'warning', 'held'], id);
    assert.deepEqual(terminiOf(hold), THREE_TERMINI(['held', 'held']), id);
  }

  for (const [id, strategy, kind] of [['req_51d0e2b8-7a0/1', 'round robin', 'timed'], ['req_77634945-72b/2', 'Pace', 'untimed']]) {
    const exhausted = modelOf(await recordOf(id));
    assert.deepEqual(read(exhausted), [...FRESH_LINE('none', 'none'), 'content: none (taken)', 'with main: none (taken)', 'static: none (taken)', 'Pick: none (taken)', 'Exhausted: exhausted (taken)', 'Reserve: none (taken)'], id);
    assert.deepEqual([exhausted.decisive, exhausted.host, stationOf(exhausted, 'eligible').tone, stationOf(exhausted, 'exhausted').tone], ['exhausted', 'pick', 'critical', 'critical'], id);
    assert.deepEqual([stationOf(exhausted, 'pick').second, stationOf(exhausted, 'exhausted').second], [[strategy], [kind]], id);
    assert.deepEqual(terminiOf(exhausted), THREE_TERMINI(['refused', 'exhausted']), id);
  }

  const refusal = modelOf(await recordOf('req_e0a4c6f2-118/1'));
  assert.deepEqual(read(refusal), ['Admission: refused (taken)', 'Rest of the walk: not run (idle)']);
  assert.deepEqual([refusal.shape, refusal.decisive, refusal.host, refusal.branch, refusal.brackets, refusal.leaves], ['short', 'admission', null, null, [], []]);
  assert.deepEqual(terminiOf(refusal), THREE_TERMINI(['refused', 'refused']));
  const refusalLayout = d.layoutStrip(refusal, 958);
  assert.deepEqual(refusalLayout.paths.filter((p) => p.kind === 'edge' || p.kind === 'curve').map((p) => `${p.kind} ${p.state}`), ['edge idle', 'curve idle', 'curve idle', 'curve taken']);
  const refused = refusalLayout.paths.find((p) => p.kind === 'curve' && p.state === 'taken');
  // The samples below answer only where the path runs, so the path has to reach its terminus first.
  assert.deepEqual(pointsOf(refused.d).at(-1), [refusalLayout.termini.refused.x, refusalLayout.termini.refused.y], 'the taken path ends at Refused');
  assert.ok(highestAt(refused.d, refusalLayout.stations.selection.x) > refusalLayout.y0 + d.RADIUS, 'the taken path to Refused passes under the idle Rest of the walk');
  assert.ok(highestAt(refused.d, (refusalLayout.stations.admission.x + refusalLayout.stations.selection.x) / 2) > refusalLayout.y0 + d.RADIUS, 'it leaves the idle edge into Rest of the walk alone');
  assert.ok(refused.d.startsWith(`M${refusalLayout.stations.admission.x} ${refusalLayout.y0 + d.RADIUS} `), 'the taken path drops from the decisive Admission toward Refused');
  assert.equal(refusalLayout.labels.admission.above, true, 'the curve drops from Admission, so its label sits above the line');

  // A probe renews its pin with every move rule unrun, and the router answers its next selection with the pinned
  // member's obstacle at the Probe line: that walk ends there, the Reserve the router writes after every walk aside.
  const probe = modelOf(await recordOf('req_c41d0311-200/1'));
  assert.deepEqual(read(probe), ['Pin: match (taken)', 'Eligible: match (taken)', 'Probe: passed (taken)', ...FIVE_RULES('not run', 'idle'), 'Renew: renewed (taken)', 'Reserve: none (taken)']);
  assert.deepEqual([probe.shape, probe.decisive, probe.host, probe.branch], ['pinned', 'renew', 'renew', null]);
  assert.deepEqual(terminiOf(probe), PINNED_TERMINI('renewed'));
  const answered = modelOf(await recordOf('req_c41d0311-200/2'));
  assert.deepEqual([answered.shape, read(answered)], ['short', ['Pin: match (taken)', 'Eligible: match (taken)', 'Probe: answered (taken)', 'Reserve: none (taken)']]);

  const failover = modelOf(await recordOf('req_1d9c3e55-0b2/2'));
  assert.deepEqual(read(failover), [...FRESH_LINE('match', 'match'), 'content: none (taken)', 'with main: none (taken)', 'static: match (taken)', 'Pick: not run (idle)', 'Reserve: booked (taken)']);
  assert.deepEqual([failover.shape, failover.decisive, failover.host, failover.branch], ['fresh', 'rung:static', 'pick', { from: 'rung:static', kind: 'rung' }]);
  assert.deepEqual(terminiOf(failover), THREE_TERMINI(['chosen', 'placed']));

  const overage = modelOf(await recordOf('req_4b0e9d17-c31/2'));
  assert.deepEqual(read(overage), [...FRESH_LINE('match', 'match'), 'content: none (taken)', 'with main: none (taken)', 'static: none (taken)', 'Pick: chosen (taken)', 'Reserve: none (taken)']);
  assert.deepEqual([overage.shape, overage.decisive, overage.host, overage.branch, stationOf(overage, 'pick').second], ['fresh', 'pick', 'pick', null, ['Pace P0']]);
  assert.deepEqual(terminiOf(overage), THREE_TERMINI(['chosen', 'placed']));
  assert.deepEqual(overage.leaves.map((l) => [l.accountId, l.chosen, l.tone]), [['primary', false, 'neutral'], ['secondary', false, 'warning'], ['nightly', true, 'accent'], ['overflow', false, 'warning']]);

  assert.deepEqual(fresh.leaves.map((l) => [l.accountId, l.badge, l.chosen, l.tone]), [['primary', 'chosen', true, 'accent'], ['secondary', 'out', false, 'warning'], ['nightly', 'out', false, 'warning'], ['overflow', 'out', false, 'warning']]);
  assert.deepEqual([d.stationName(stationOf(fresh, 'pick')), d.stationName(stationOf(shedMove, 'move:shed')), d.stationName(stationOf(staticPlaced, 'pick'))], ['Pick, chosen, Pace P0', 'Shed, fired', 'Pick, not run']);
  assert.deepEqual(fresh.termini.map(d.terminusName), ['Chosen, placed', 'Held for a slot, not reached', 'Refused, not reached']);
  // The chosen member read rewrite only, which its leaf names as the candidate table's verdict cell does.
  assert.deepEqual(fresh.leaves.slice(0, 2).map(d.leafName), ['primary, chosen, rewrite only', `secondary, out, ${fresh.leaves[1].chip.value}`]);
});

test('a move draws Fit after Move and Reserve after its pick or its static rung, and a fresh line draws the four partition stations', async () => {
  const trackOf = (m) => m.stations.filter((s) => s.level === 'track').map((s) => s.id);
  // The shed rule and the least-bad move pick among their destinations, so Reserve stands after Pick on the track.
  for (const [id, fit, tone] of [['req_868264bc-611/1', 'fits', 'good'], ['req_5b2e8d47-c19/1', 'least bad', 'warning']]) {
    const m = modelOf(await recordOf(id));
    assert.deepEqual(trackOf(m), ['move', 'fit', 'cap_filter', 'partitions', 'rung:content', 'rung:with_main', 'rung:static', 'pick', 'reserve'], id);
    assert.deepEqual([stationOf(m, 'fit').badge, stationOf(m, 'fit').tone, stationOf(m, 'fit').renderer, stationOf(m, 'fit').head], [fit, tone, 'table', 'Fit'], id);
    assert.deepEqual([stationOf(m, 'reserve').badge, stationOf(m, 'reserve').tone, stationOf(m, 'reserve').state, stationOf(m, 'reserve').renderer, stationOf(m, 'reserve').head], ['booked', 'good', 'taken', 'list', 'Reserve'], id);
    // Reserve books what the pick decided, so it never decides the walk.
    assert.deepEqual([m.decisive, m.host], ['pick', 'pick'], id);
    const layout = d.layoutStrip(m, 958);
    assert.deepEqual(layout.paths.filter((p) => p.kind === 'track').map((p) => p.state), ['taken'], `${id}: the taken track runs on through Reserve`);
    assert.equal(layout.stations.reserve.y, layout.yu, id);
  }
  // The spent move's static rung placed the request with no pick, so Reserve stands after the rung.
  const spent = modelOf(await recordOf('req_da52d141-a5a/1'));
  assert.deepEqual(trackOf(spent), ['move', 'fit', 'cap_filter', 'partitions', 'rung:content', 'rung:with_main', 'rung:static', 'reserve']);
  assert.deepEqual([stationOf(spent, 'fit').badge, stationOf(spent, 'fit').tone, spent.decisive, spent.host], ['rewrite only', 'warning', 'rung:static', 'rung:static']);
  assert.deepEqual(d.layoutStrip(spent, 958).paths.filter((p) => p.kind === 'track').map((p) => p.state), ['taken']);
  // A fresh line draws the engine's four partitions, then Reserve after Pick before the termini.
  const fresh = modelOf(await recordOf('req_ef099b34-453/1'));
  assert.deepEqual(fresh.stations.filter((s) => s.id.startsWith('partitions:')).map((s) => [s.word, s.badge]), [['Spent', 'off'], ['At cap', '0 out'], ['No fit', '1 out'], ['Winding down', '0 out']]);
  assert.deepEqual(fresh.stations.slice(-2).map((s) => [s.id, s.badge, s.state]), [['pick', 'chosen', 'taken'], ['reserve', 'booked', 'taken']]);
  const layout = d.layoutStrip(fresh, 958);
  const taken = layout.paths.find((p) => p.kind === 'curve' && p.state === 'taken');
  assert.ok(taken.d.startsWith(`M${layout.stations.reserve.x} ${layout.y0} `), 'the taken curve leaves for Chosen from Reserve');
});

test('a spent move held for the reset ends in the fourth terminus drawn at the Held position while a fresh line keeps three', async () => {
  const held = modelOf(await recordOf('req_9d4c1a73-e58/2'));
  assert.deepEqual(read(held), ['Pin: match (taken)', 'Eligible: match (taken)', 'spent: held (taken)', ...FIVE_RULES('not run', 'idle').slice(1), 'Renew: not run (idle)', 'Reserve: none (idle)']);
  assert.deepEqual([held.shape, held.decisive, held.host, held.branch], ['pinned', 'move:spent', 'move:spent', null]);
  assert.deepEqual(terminiOf(held), ['Chosen: - (idle)', 'Held for the reset: held (taken)', 'Refused: - (idle, not drawn)']);
  assert.deepEqual(held.termini.map(d.terminusName), ['Chosen, not reached', 'Held for the reset, held', 'Refused, not reached']);
  const layout = d.layoutStrip(held, 958);
  assert.deepEqual(Object.keys(layout.termini), ['chosen', 'held_reset']);
  assert.deepEqual(layout.termini.held_reset, { x: layout.termini.chosen.x, y: layout.termini.chosen.y + 36 });
  const taken = layout.paths.find((p) => p.kind === 'curve' && p.state === 'taken');
  assert.deepEqual(pointsOf(taken.d).at(-1), [layout.termini.held_reset.x, layout.termini.held_reset.y + d.TERMINUS_RADIUS], 'the taken path rises into Held for the reset from below');
  // A hold for a slot and a placement keep the three termini, the fourth drawn nowhere.
  for (const id of ['req_ef099b34-453/1', 'req_69e30548-32e/1']) {
    const m = modelOf(await recordOf(id));
    assert.deepEqual(m.termini.map((t) => t.kind), ['chosen', 'held', 'refused'], id);
  }
});

test('a rewrite hold ends in the sixth terminus drawn at the Held position', async () => {
  const record = await recordOf('req_5d6f1b3a-e27/1');
  const held = modelOf(record);
  assert.deepEqual(held.termini.map((t) => t.kind), ['chosen', 'held_rewrite', 'refused'], "neither the slot's, the reset's nor the slow-down's way out stands");
  assert.deepEqual(terminiOf(held), ['Chosen: - (idle)', 'Held for the rewrite in flight: held (taken)', 'Refused: - (idle, not drawn)']);
  assert.deepEqual(held.termini.map(d.terminusName), ['Chosen, not reached', 'Held for the rewrite in flight, held', 'Refused, not reached']);
  const layout = d.layoutStrip(held, 958);
  assert.deepEqual(Object.keys(layout.termini), ['chosen', 'held_rewrite']);
  assert.deepEqual(layout.termini.held_rewrite, { x: layout.termini.chosen.x, y: layout.termini.chosen.y + 36 });
  const plan = planOf(record, { id: 'held_rewrite', kind: 'terminus' });
  assert.deepEqual([plan.renderer, plan.head.word, plan.head.badge], ['outcome', 'Held for the rewrite in flight', 'held']);
});

test('a slowed selection ends in the fifth terminus drawn at the Held position while a fresh line keeps three', async () => {
  for (const id of ['req_9f2e7c5a-c49/1', 'req_8e1c4a6d-b52/1']) {
    const record = await recordOf(id);
    const held = modelOf(record);
    assert.deepEqual(read(held), ['Pin: match (taken)', 'Eligible: match (taken)', ...FIVE_RULES('no', 'taken'), 'Renew: not run (idle)', 'Reserve: none (idle)'], id);
    assert.deepEqual([held.shape, held.decisive, held.host, held.branch], ['pinned', 'move:shed', 'move:shed', null], id);
    assert.deepEqual(held.termini.map((t) => t.kind), ['chosen', 'held_slow_down', 'refused'], `${id}: neither the slot's nor the reset's way out stands`);
    assert.deepEqual(terminiOf(held), ['Chosen: - (idle)', 'Held for the slow-down: held (taken)', 'Refused: - (idle, not drawn)'], id);
    assert.deepEqual(held.termini.map(d.terminusName), ['Chosen, not reached', 'Held for the slow-down, held', 'Refused, not reached'], id);
    const layout = d.layoutStrip(held, 958);
    assert.deepEqual(Object.keys(layout.termini), ['chosen', 'held_slow_down'], id);
    assert.deepEqual(layout.termini.held_slow_down, { x: layout.termini.chosen.x, y: layout.termini.chosen.y + 36 }, id);
    // The way out's popover is the outcome's rows, which read the hold's speed, stretch, deadline, bound and cycle.
    const plan = planOf(record, { id: 'held_slow_down', kind: 'terminus' });
    const rows = d.outcomeRows(record, NOW).map((r) => ({ chips: r.chips, label: r.label, ...(r.title === undefined ? {} : { title: r.title }) }));
    assert.deepEqual([plan.renderer, plan.head.word, plan.head.badge, rowsOf(plan)], ['outcome', 'Held for the slow-down', 'held', rows], id);
  }
  const slowed = await recordOf('req_9f2e7c5a-c49/1');
  const until = d.formatFact(slowed.outcome.hold.until, 'time', NOW);
  assert.deepEqual(readRows(planOf(slowed, { id: 'held_slow_down', kind: 'terminus' })), [['Hold', ['on studio', `for the slow-down until ${until}`, 'speed 0.5x', 'stretch 42 min 18 s of 30 min', 'bound 2 min', 'cycle 42 s']]]);
  for (const id of ['req_ef099b34-453/1', 'req_69e30548-32e/1', 'req_9d4c1a73-e58/2']) {
    const m = modelOf(await recordOf(id));
    assert.ok(!m.termini.some((t) => t.kind === 'held_slow_down'), `${id} draws no slow-down way out`);
  }
});

test("a long record's way-out popover reads the cache-map row's compact chips with no control", async () => {
  const record = await recordOf('req_1d9c3e55-0b2/2');
  const card = d.outcomeRows(record, NOW).find((r) => r.label === 'Cache-map entries');
  assert.ok(card?.more, 'the card folds the row behind its control');
  const popover = rowsOf(planOf(record, { id: 'chosen', kind: 'terminus' })).find((r) => r.label === 'Cache-map entries');
  assert.deepEqual([popover?.chips, popover?.more], [card.chips, undefined]);
});

test('a version 1 record draws through the legacy maps with its Cool reading and its five partitions', async () => {
  const base = await recordOf('req_da52d141-a5a/1');
  const partitions = step('partitions', 'match', [fact('spent', 'off'), fact('at_cap', 0, 'count'), fact('above_ceiling', 0, 'count'), fact('winding_down', 0, 'count'), fact('floor', 0, 'count')]);
  const cool = step('cool', 'preferred', [fact('cool', true, null, 'primary')]);
  // The record as the engine wrote it before the projection: the cool reading where the fit station stands, five
  // partitions and no reservation.
  const walk = base.walk.filter((s) => s.step !== 'reserve').map((s) => (s.step === 'fit' ? cool : s.step === 'partitions' ? partitions : s));
  const moved = modelOf({ ...base, version: 1, walk });
  assert.deepEqual(read(moved).slice(7), ['^Move: moved (taken)', '^Cool reading: preferred (taken)', '^Cap filter: passed (taken)', '^Partitions: match (taken)', '^content: none (taken)', '^with main: none (taken)', '^static: match (taken)', 'Renew: not run (idle)']);
  assert.deepEqual([stationOf(moved, 'cool').known, stationOf(moved, 'cool').renderer, stationOf(moved, 'cool').head], [true, 'table', 'Cool reading']);
  const freshBase = await recordOf('req_ef099b34-453/1');
  const freshWalk = freshBase.walk.filter((s) => s.step !== 'reserve').map((s) => (s.step === 'partitions' ? partitions : s));
  const fresh = modelOf({ ...freshBase, version: 1, walk: freshWalk });
  assert.deepEqual(read(fresh), ['Pin: none (taken)', 'Eligible: match (taken)', ...LEGACY_PARTITIONS.map((w) => `${w} (taken)`), 'content: none (taken)', 'with main: none (taken)', 'static: none (taken)', 'Pick: chosen (taken)']);
  assert.deepEqual(fresh.brackets.map((b) => [b.word, b.stations.length]), [['Partitions', 5], ['Rungs', 3]]);
  // The partition station's popover lists the five facts in the legacy order.
  const plan = d.anchorPlan({ id: 'partitions:floor', kind: 'station' }, fresh, { ...freshBase, version: 1, walk: freshWalk }, undefined, NOW);
  assert.deepEqual(readRows(plan).map(([label]) => label), ['spent', 'at cap', 'above ceiling', 'winding down', 'floor']);
});

test('with load shedding off the shed station reads not run and its Round group round off, and a shed that read a trigger groups it with the round', async () => {
  const base = await recordOf('req_f8085653-422/1');
  const groupsOf = (record) => stationPlan(record, 'move:shed').block.groups.map((g) => [g.label, g.rows.map((r) => r.chips.map(chipText).join(' / '))]);
  const off = { ...base, walk: base.walk.map((s) => (s.step === 'move:shed' ? step('move:shed', 'not_run', [fact('round', 'off')]) : s)) };
  const m = modelOf(off);
  assert.deepEqual([stationOf(m, 'move:shed').badge, stationOf(m, 'move:shed').tone, stationOf(m, 'move:shed').state], ['not run', 'neutral', 'idle']);
  assert.deepEqual(groupsOf(off), [['Round', ['round off']]]);
  const at = Date.parse(base.request.at);
  const read = step('move:shed', 'no', [fact('projection_five_hour', 0.81, 'percent'), fact('horizon_five_hour', at + 2 * 3_600_000, 'time'), fact('ceiling', 0.75, 'percent'), fact('trigger', 'five_hour', 'text'), fact('excess', 0.03, 'percent_per_hour'), fact('round', 'within_dwell'), fact('marked', false)]);
  const within = { ...base, walk: base.walk.map((s) => (s.step === 'move:shed' ? read : s)) };
  assert.deepEqual(groupsOf(within), [
    ['Windows', ['projection 5h 81%', `horizon 5h ${d.formatFact(at + 2 * 3_600_000, 'time', at)}`, 'ceiling 75%']],
    ['Round', ['trigger 5h', 'excess 3.00 %/h', 'round within dwell', 'marked no']],
  ]);
});

test('a round fired by the stretch groups fired by, over for, over since, stretch, threshold and the wait keys with the round', async () => {
  const groupsOf = (record) => stationPlan(record, 'move:shed').block.groups.map((g) => [g.label, g.rows.map((r) => r.chips.map(chipText).join(' / '))]);
  const labelsOf = (record) => stationPlan(record, 'move:shed').block.groups.map((g) => [g.label, g.rows.map((r) => r.label)]);
  // The renewal that ran the stretch round: the account's read in Windows, the source and the stretch with the round.
  const round = await recordOf('req_9f2e7c5a-c49/2');
  const at = Date.parse(round.request.at);
  const time = (key) => d.formatFact(round.walk.find((s) => s.step === 'move:shed').facts.find((f) => f.key === key).value, 'time', at);
  assert.deepEqual(groupsOf(round).find(([label]) => label === 'Windows')[1].slice(4), ['demand 12.86 %/h', 'speed 0.5x', 'ramp 100%', 'slow-down on', 'realized 0.5x', 'ceiling 5h 95%', 'ceiling weekly 100%']);
  assert.deepEqual(groupsOf(round).find(([label]) => label === 'Round')[1].slice(0, 6), ['trigger 5h', 'excess 2.86 %/h', 'fired by slow-down', `over since ${time('over_since')}`, 'stretch 42 min', 'threshold 30 min']);
  assert.deepEqual(labelsOf(round).map(([label]) => label), ['Windows', 'Round'], 'no fact of the line falls to an unlabeled group');
  // The hold before it read the stretch inside the safety wait, which the round names as the time it has read over.
  const waiting = await recordOf('req_9f2e7c5a-c49/1');
  assert.deepEqual(labelsOf(waiting), [
    ['Windows', ['projection 5h', 'horizon 5h', 'projection 7d', 'horizon 7d', 'demand', 'speed', 'ramp', 'slow-down', 'realized', 'ceiling 5h', 'ceiling weekly']],
    ['Round', ['over for', 'over since', 'stretch', 'threshold', 'wall 5h', 'marked']],
  ]);
  assert.deepEqual(groupsOf(waiting)[1][1][0], 'over for 1 min 48 s');
  // A pool with the slow-down off reads the switch alone, and a projection inside its wait names its stamp and its wait
  // per window with the round.
  const base = await recordOf('req_f8085653-422/1');
  const from = Date.parse(base.request.at);
  const line = step('move:shed', 'no', [
    fact('projection_five_hour', 0.97, 'percent'), fact('horizon_five_hour', from + 2 * 3_600_000, 'time'), fact('over_since_five_hour', from - 60_000, 'time'), fact('wait_five_hour', 300_000, 'ms'),
    fact('projection_seven_day', 0.96, 'percent'), fact('horizon_seven_day', from + 5 * 3_600_000, 'time'), fact('over_since_seven_day', from - 120_000, 'time'), fact('wait_seven_day', 900_000, 'ms'),
    fact('projection_seven_day_fable', 0.97, 'percent'), fact('horizon_seven_day_fable', from + 5 * 3_600_000, 'time'), fact('over_since_seven_day_fable', from - 180_000, 'time'), fact('wait_seven_day_fable', 900_000, 'ms'),
    fact('ceiling', 0.95, 'percent'), fact('marked', false), fact('slow_down', 'off', 'text'),
  ]);
  const off = { ...base, walk: base.walk.map((s) => (s.step === 'move:shed' ? line : s)) };
  assert.deepEqual(labelsOf(off), [
    ['Windows', ['projection 5h', 'horizon 5h', 'projection 7d', 'horizon 7d', 'projection Fable', 'horizon Fable', 'slow-down', 'ceiling']],
    ['Round', ['over since 5h', 'wait 5h', 'over since 7d', 'wait 7d', 'over since Fable', 'wait Fable', 'marked']],
  ]);
  assert.deepEqual(groupsOf(off)[1][1].slice(0, 2), [`over since 5h ${d.formatFact(from - 60_000, 'time', from)}`, 'wait 5h 5 min']);
  assert.deepEqual(groupsOf(off)[0][1][6], 'slow-down off');
});

test('the Fit table reads unbounded on a row with a projection and no runway, and leaves blank a row with no projection', async () => {
  const shed = await recordOf('req_868264bc-611/1');
  const unbounded = (ids) => ({ ...shed, walk: shed.walk.map((s) => (s.step === 'fit' ? { ...s, facts: s.facts.filter((f) => !(f.key === 'runway_ms' && ids.includes(f.accountId))) } : s)) });
  const runways = (record) => stationPlan(record, 'fit').block.table.rows.map((r) => [r.account.value, r.cells.runway_ms?.text ?? '']);
  assert.deepEqual(runways(unbounded(['primary'])), [['primary', 'unbounded'], ['nightly', 'none'], ['overflow', '']]);
  const every = unbounded(['primary', 'nightly']);
  assert.ok(stationPlan(every, 'fit').block.table.columns.some((c) => c.label === 'runway'), 'the column stands where no row carries the fact');
  assert.deepEqual(runways(every), [['primary', 'unbounded'], ['nightly', 'unbounded'], ['overflow', '']]);
});

test('the Fit table alone lets its figures wrap, which its nine columns need to keep inside the popover', async () => {
  const shed = await recordOf('req_868264bc-611/1');
  assert.deepEqual(['fit', 'pick'].map((id) => stationPlan(shed, id).block.table.wraps), [true, false]);
});

test("the Fit station's table reads a row per candidate the gate read with its columns window by window, and the Reserve station lists the booking", async () => {
  const shed = await recordOf('req_868264bc-611/1');
  const { table } = stationPlan(shed, 'fit').block;
  assert.deepEqual(table.columns.map((c) => c.label), ['projection 5h', 'projection 7d', 'rewrite 5h', 'rewrite 7d', 'reserved 5h', 'reserved 7d', 'runway', 'binding', 'verdict']);
  assert.deepEqual(table.rows.map((r) => r.account.value), ['primary', 'nightly', 'overflow']);
  const cells = (id) => table.columns.map((c) => table.rows.find((r) => r.account.value === id).cells[c.key]?.text ?? '');
  assert.deepEqual(cells('primary'), ['54%', '73%', '0.264%', '0.031%', '0%', '0%', '10 h 29 min', '7d', 'fits']);
  assert.deepEqual(cells('nightly').slice(6), ['none', '5h', 'least bad']);
  // An API key has no plan window to project, so its row is its verdict alone.
  assert.deepEqual(cells('overflow'), ['', '', '', '', '', '', '', '', 'fits']);
  assert.deepEqual(table.rows.map((r) => r.cells.verdict.tone), ['good', 'warning', 'good']);
  assert.deepEqual(readRows(stationPlan(shed, 'reserve')), [
    ['on', ['on primary']],
    ['reserved 5h', ['reserved 5h 0.264%']],
    ['reserved 7d', ['reserved 7d 0.031%']],
    ['burn 5h', ['burn 5h 1.42 %/h']],
    ['burn 7d', ['burn 7d 0.16 %/h']],
    ['expires at', [`expires at ${d.formatFact(Date.parse(shed.outcome.reservation.expiresAt), 'time', Date.parse(shed.request.at))}`]],
    ['attempt', ['attempt 1']],
  ]);
  // A walk that booked nothing reads its sentence: a renewal's names the pin renewed in place.
  const renewal = await recordOf('req_f8085653-422/1');
  assert.deepEqual([stationPlan(renewal, 'reserve').renderer, stationPlan(renewal, 'reserve').block.text], ['sentence', 'Nothing was booked: the pin renewed in place.']);
  const hold = await recordOf('req_69e30548-32e/1');
  assert.deepEqual([stationPlan(hold, 'reserve').renderer, stationPlan(hold, 'reserve').block.text], ['sentence', 'Nothing was booked.']);
});

test("a move's Partitions station names the gate's order for its no-fit row, never a count that reads as every candidate fitting", async () => {
  // The gate kept a move's destinations whole, so the partition's no-fit count is 0 whatever order it took.
  for (const [id, order] of [['req_da52d141-a5a/1', 'rewrite only'], ['req_868264bc-611/1', 'fits'], ['req_5b2e8d47-c19/1', 'least bad']]) {
    const record = await recordOf(id);
    assert.equal(record.walk.find((s) => s.step === 'partitions').facts.find((f) => f.key === 'no_fit').value, 0, id);
    assert.deepEqual(readRows(stationPlan(record, 'partitions')).find(([label]) => label === 'no fit'), ['no fit', [`no fit by the gate's ${order} order`]], id);
  }
  // A fresh line's No fit station reads its count, since no gate ran before it.
  const fresh = await recordOf('req_2c7d9f15-b63/1');
  assert.deepEqual(readRows(stationPlan(fresh, 'partitions:no_fit')).find(([label]) => label === 'no fit'), ['no fit', ['no fit 1']]);
});

test('a record of twelve candidates gives eight leaves and "and 4 more", the chosen among them', async () => {
  const base = await recordOf('req_ef099b34-453/1');
  const chosen = base.candidates.find((c) => c.verdict === 'chosen');
  const others = base.candidates.filter((c) => c !== chosen);
  const copies = Array.from({ length: 11 }, (_, i) => ({ ...others[i % others.length], accountId: `member-${i + 1}` }));
  const m = modelOf({ ...base, candidates: [...copies, chosen] });
  assert.equal(m.leaves.length, 9);
  assert.deepEqual(m.leaves.slice(0, 7).map((l) => l.accountId), copies.slice(0, 7).map((c) => c.accountId));
  assert.deepEqual([m.leaves[7].accountId, m.leaves[7].chosen, m.leaves[7].tone], [chosen.accountId, true, 'accent']);
  assert.deepEqual([m.leaves[8].accountId, m.leaves[8].more, m.leaves[8].badge, m.leaves[8].chip, d.leafName(m.leaves[8])], [null, 4, 'and 4 more', null, 'and 4 more']);
});

test('a bound renewal is the short shape with no placeholder', async () => {
  const base = await recordOf('req_ef099b34-453/1');
  const m = modelOf(built(base, [step('pin', 'match'), step('eligible', 'match'), step('bound', 'renewed', [fact('account_id', 'primary', null, 'primary')]), step('renew', 'renewed')], 'renewed'));
  assert.deepEqual(read(m), ['Pin: match (taken)', 'Eligible: match (taken)', 'Bound: renewed (taken)', 'Renew: renewed (taken)']);
  assert.deepEqual([m.shape, m.decisive, m.host, m.branch, m.brackets], ['short', 'renew', 'renew', null, []]);
  assert.deepEqual(terminiOf(m), THREE_TERMINI(['chosen', 'renewed']));
});

test('a bound pin the walk cannot place on is the short shape ending at its exhaustion', async () => {
  const base = await recordOf('req_ef099b34-453/1');
  const walk = [step('pin', 'match'), step('eligible', 'match'), step('bound', 'exhausted', [fact('account_id', 'primary', null, 'primary')]), step('exhausted', 'exhausted', [fact('kind', 'untimed')])];
  const m = modelOf(built(base, walk, 'exhausted'));
  assert.deepEqual(read(m), ['Pin: match (taken)', 'Eligible: match (taken)', 'Bound: exhausted (taken)', 'Exhausted: exhausted (taken)']);
  assert.deepEqual([m.shape, m.decisive, m.host, stationOf(m, 'exhausted').second], ['short', 'exhausted', 'exhausted', ['untimed']]);
  assert.deepEqual(terminiOf(m), THREE_TERMINI(['refused', 'exhausted']));
});

test('a move the cap cancels renews in place on the line', async () => {
  const base = await recordOf('req_f8085653-422/1');
  const walk = [step('pin', 'match'), step('eligible', 'match'), step('move:spent', 'fired', [fact('destinations', ['b'])]), ...RULES.slice(1).map((r) => step(r, 'not_run')), step('cap_filter', 'dropped', [fact('in_flight', '1/1', 'text', 'b')]), step('renew', 'renewed')];
  const m = modelOf(built(base, walk, 'renewed'));
  assert.deepEqual(read(m), ['Pin: match (taken)', 'Eligible: match (taken)', 'spent: fired (taken)', ...FIVE_RULES('not run', 'idle').slice(1), '^Move: all at cap (taken)', '^Cap filter: all at cap (taken)', 'Renew: renewed (taken)']);
  assert.deepEqual([m.shape, m.decisive, m.host, m.branch], ['pinned', 'renew', 'renew', { from: 'move:spent', kind: 'move' }]);
  assert.deepEqual([stationOf(m, 'move').tone, stationOf(m, 'move').verdict, stationOf(m, 'renew').level, stationOf(m, 'renew').decisive], ['warning', 'dropped', 'line', true]);
  assert.equal(new Set(m.stations.map((s) => s.id)).size, m.stations.length);
  assert.equal(m.stations.filter((s) => s.step === 'renew').length, 1);
  assert.deepEqual(terminiOf(m), PINNED_TERMINI('renewed'));
});

/**
 * The spent move as the engine records it when the content rung matches on the continuation: the rung's entry facts,
 * and the two later rungs `not_run`. No story record carries this walk, so these cases are its guard.
 */
const contentMove = async () => {
  const base = await recordOf('req_da52d141-a5a/1');
  const entry = base.walk.find((s) => s.step === 'rung:static').facts;
  const walk = base.walk.map((s) => (s.step === 'rung:content' ? step(s.step, 'match', entry) : s.step === 'rung:with_main' || s.step === 'rung:static' ? step(s.step, 'not_run') : s));
  return modelOf({ ...base, walk });
};

test('a move whose continuation the content rung decides hangs the fan from that rung', async () => {
  const m = await contentMove();
  assert.deepEqual(read(m), ['Pin: match (taken)', 'Eligible: match (taken)', 'spent: fired (taken)', ...FIVE_RULES('not run', 'idle').slice(1), '^Move: moved (taken)', '^Fit: rewrite only (taken)', '^Cap filter: passed (taken)', '^Partitions: match (taken)', '^content: match (taken)', '^with main: not run (idle)', '^static: not run (idle)', 'Renew: not run (idle)', '^Reserve: booked (taken)']);
  assert.deepEqual([m.shape, m.decisive, m.host, m.branch], ['pinned', 'rung:content', 'rung:content', { from: 'move:spent', kind: 'move' }]);
  assert.deepEqual(terminiOf(m), PINNED_TERMINI('moved'));
});

test('a move whose continuation the content rung decides comes down past the idle rungs into its booked Reserve and on into Chosen', async () => {
  const m = await contentMove();
  const layout = d.layoutStrip(m, 958);
  const { y0, yu } = layout;
  const at = (id) => layout.stations[id];
  const track = layout.paths.filter((p) => p.kind === 'track');
  assert.deepEqual(track.map((p) => p.state), ['taken', 'idle']);
  const [taken, idle] = track.map((p) => p.d);
  assert.equal(highestAt(taken, at('rung:content').x), yu, 'the taken track runs along the track to the decisive rung');
  for (const id of ['rung:with_main', 'rung:static']) {
    const lane = highestAt(taken, at(id).x);
    assert.ok(lane > yu + d.RADIUS && lane < y0 - d.RADIUS, `the taken track passes under the idle ${id} and above the line (${lane})`);
  }
  assert.equal(highestAt(taken, at('reserve').x), yu, 'the taken track comes back up into the booked Reserve');
  assert.deepEqual(pointsOf(taken).at(-1), [layout.termini.chosen.x, y0 - 8], 'and runs on from it down into Chosen, as a track ends');
  assert.equal(idle, `M${at('rung:content').x} ${yu} H${at('reserve').x}`, 'the idle track runs from the decisive rung through the idle rungs to Reserve');
  const fan = layout.paths.find((p) => p.kind === 'fan');
  assert.ok(fan.d.startsWith(`M${at('rung:content').x} ${yu + d.RADIUS} `), 'the fan drops from the decisive rung');
  const tick = layout.paths.find((p) => p.kind === 'tick' && p.state === 'taken');
  assert.ok(tick.d.startsWith(`M${at('rung:content').x} `), "the chosen leaf's taken tick hangs off the decisive rung");
  const spread = ['move:overage', 'move:return_to_plan', 'move:drain', 'move:shed', 'renew'].map((id) => at(id).x);
  assert.ok(spread.every((x, i) => x > (i === 0 ? at('move:spent').x : spread[i - 1]) && x < at('rung:content').x), 'the idle rules and Renew spread between the fired rule and the fan');
  assert.deepEqual(layout.paths.filter((p) => p.kind === 'curve').map((p) => p.state), ['idle']);
});

test('a step the table does not know is a plain station with its id and the chips renderer', async () => {
  const base = await recordOf('req_ef099b34-453/1');
  const walk = [...base.walk.slice(0, 2), step('a_word_added_later', 'seen', [fact('reason', 'later')]), ...base.walk.slice(2)];
  const m = modelOf({ ...base, walk });
  const s = stationOf(m, 'a_word_added_later');
  assert.deepEqual([s.word, s.head, s.known, s.renderer, s.tone, s.badge, s.group, s.stub, s.index], ['a_word_added_later', 'a_word_added_later', false, 'chips', 'neutral', 'seen', null, null, 2]);
  assert.deepEqual(m.stations.slice(1, 4).map((x) => x.id), ['eligible', 'a_word_added_later', 'partitions:spent']);
  assert.equal(stationOf(m, 'pick').known, true);
});

test('a record with no slot step expects the slot its outcome names after its last recorded step', async () => {
  const base = await recordOf('req_51d0e2b8-7a0/1');
  const m = modelOf(built(base, [step('eligible', 'none')], 'exhausted', { candidates: [] }));
  assert.deepEqual(read(m), ['Pin: not recorded (inferred)', 'Eligible: none (taken)', ...PARTITIONS.map((w) => `${w} (idle)`), 'content: - (idle)', 'with main: - (idle)', 'static: - (idle)', 'Exhausted: not run (idle)']);
  assert.deepEqual([m.shape, m.decisive, m.host, stationOf(m, 'eligible').tone], ['fresh', 'eligible', null, 'critical']);
  assert.deepEqual(m.brackets.map((b) => [b.word, b.recorded]), [['Partitions, not recorded', false], ['Rungs, not recorded', false]]);
  assert.ok(m.stations.filter((s) => s.group !== null).every((s) => s.badge === '' && s.tone === 'neutral' && s.verdict === null));
  assert.deepEqual([d.stationName(stationOf(m, 'pin')), d.stationName(stationOf(m, 'partitions:spent')), d.stationName(stationOf(m, 'rung:content')), d.stationName(stationOf(m, 'exhausted'))], ['Pin, not recorded', 'Spent, not recorded', 'Rung: content, not recorded', 'Exhausted, not run']);
  assert.deepEqual(terminiOf(m), THREE_TERMINI(['refused', 'exhausted']));
  const layout = d.layoutStrip(m, 958);
  const curves = layout.paths.filter((p) => p.kind === 'curve');
  assert.deepEqual(curves.map((p) => p.state), ['idle', 'idle', 'taken']);
  assert.deepEqual(pointsOf(curves[2].d).at(-1), [layout.termini.refused.x, layout.termini.refused.y], 'the taken path ends at Refused');
  for (const s of m.stations.slice(2)) assert.ok(highestAt(curves[2].d, layout.stations[s.id].x) > layout.y0 + d.RADIUS, `the taken path to Refused passes under the idle ${s.id}`);
  assert.ok(curves[2].d.startsWith(`M${layout.stations.eligible.x} ${layout.y0 + d.RADIUS} `), 'the taken path drops from the decisive Eligible toward Refused');
  assert.equal(layout.labels.eligible.above, true);
  assert.deepEqual(layout.paths.filter((p) => p.kind === 'edge').map((p) => p.state), ['taken', ...Array(8).fill('idle')]);
});

test('a live pin that turned ineligible is decided by its recorded pick, never by a renewal', async () => {
  const base = await recordOf('req_ef099b34-453/1');
  const head = [step('pin', 'match'), step('eligible', 'match'), ...RULES.map((r) => step(r, 'no')), step('partitions', 'match', [fact('spent', 'off')]), step('rung:content', 'none'), step('rung:with_main', 'none'), step('rung:static', 'none')];
  const placed = modelOf(built(base, [...head, step('pick', 'chosen', [fact('strategy', 'pace'), fact('tier', ['0', '0', '0'])])], 'placed'));
  assert.deepEqual(read(placed), ['Pin: match (taken)', 'Eligible: match (taken)', ...FIVE_RULES('no', 'taken'), 'Partitions: match (taken)', 'content: none (taken)', 'with main: none (taken)', 'static: none (taken)', 'Pick: chosen (taken)']);
  assert.deepEqual([placed.shape, placed.decisive, placed.host, placed.branch, stationOf(placed, 'pick').second], ['pinned', 'pick', 'pick', null, ['Pace P0']]);
  assert.deepEqual(placed.brackets.map((b) => [b.word, b.level]), [['Moves', 'line'], ['Rungs', 'line']]);
  assert.equal(placed.stations.some((s) => s.step === 'renew'), false);
  assert.deepEqual(terminiOf(placed), PINNED_TERMINI('placed'));

  const exhausted = modelOf(built(base, [...head, step('pick', 'none', [fact('strategy', 'pace')]), step('exhausted', 'exhausted', [fact('kind', 'untimed')])], 'exhausted'));
  assert.deepEqual(read(exhausted).slice(-2), ['Pick: none (taken)', 'Exhausted: exhausted (taken)']);
  assert.deepEqual([exhausted.decisive, exhausted.host], ['exhausted', 'pick']);
  assert.equal(exhausted.stations.some((s) => s.step === 'renew'), false);
  assert.deepEqual(terminiOf(exhausted), ['Chosen: - (idle)', 'Held for a slot: - (idle, not drawn)', 'Refused: exhausted (taken)']);

  // A rung that placed the pinned request afresh leaves the line by its track, as on a fresh placement.
  const rungWalk = [...head.slice(0, -2), step('rung:with_main', 'match'), step('rung:static', 'not_run')];
  const rungPlaced = modelOf(built(base, rungWalk, 'placed'));
  assert.deepEqual([rungPlaced.shape, rungPlaced.decisive, rungPlaced.branch], ['pinned', 'rung:with_main', { from: 'rung:with_main', kind: 'rung' }]);
  assert.deepEqual(terminiOf(rungPlaced), PINNED_TERMINI('placed'));
  const rungLayout = d.layoutStrip(rungPlaced, 958);
  assert.deepEqual([rungLayout.y0, rungLayout.yu], [128, 56]);
  const rungTrack = rungLayout.paths.filter((p) => p.kind === 'track');
  assert.deepEqual(rungTrack.map((p) => p.state), ['taken']);
  assert.ok(rungTrack[0].d.startsWith(`M${rungLayout.stations['rung:with_main'].x} 128 `) && rungTrack[0].d.endsWith(`${rungLayout.termini.chosen.x} 120`), 'the matched rung runs up and on into Chosen');
  assert.deepEqual(rungLayout.paths.filter((p) => p.kind === 'curve').map((p) => p.state), ['idle']);
});

test('a move whose continuation ends in a hold runs its track into the held terminus', async () => {
  const base = await recordOf('req_ef099b34-453/1');
  const walk = [step('pin', 'match'), step('eligible', 'match'), step('move:spent', 'fired'), ...RULES.slice(1).map((r) => step(r, 'not_run')), step('cap_filter', 'passed'), step('partitions', 'match'), ...RUNGS_NONE(), step('pick', 'chosen', [fact('strategy', 'pace')]), step('hold', 'held', [fact('place', 'strategy_pick'), fact('bound_ms', 60_000, 'ms')])];
  const m = modelOf(built(base, walk, 'held'));
  assert.deepEqual(read(m).slice(7), ['^Move: passed (taken)', '^Cap filter: passed (taken)', '^Partitions: match (taken)', '^content: none (taken)', '^with main: none (taken)', '^static: none (taken)', '^Pick: chosen (taken)', '^Hold: held (taken)', 'Renew: not run (idle)']);
  assert.deepEqual([m.decisive, m.host], ['hold', 'pick']);
  assert.deepEqual(terminiOf(m), ['Chosen: - (idle)', 'Held for a slot: held (taken)', 'Refused: - (idle, not drawn)']);
  const layout = d.layoutStrip(m, 958);
  const track = layout.paths.filter((p) => p.kind === 'track');
  assert.deepEqual(track.map((p) => p.state), ['taken']);
  assert.ok(track[0].d.endsWith(` ${layout.termini.held.y} H${layout.termini.held.x}`), 'the track ends in Held for a slot');
  assert.deepEqual(layout.termini.held, { x: layout.termini.chosen.x, y: 176 + 36 });
  assert.deepEqual(layout.paths.filter((p) => p.kind === 'curve').map((p) => p.state), ['idle', 'idle']);
});

test('the session-pin cap exemption is decided by its hold after the pick', async () => {
  const base = await recordOf('req_ef099b34-453/1');
  const m = modelOf(built(base, [...base.walk.slice(0, -1), step('hold', 'placed', [fact('place', 'session_pin'), fact('account_id', 'primary', null, 'primary')]), base.walk.at(-1)], 'placed'));
  assert.deepEqual(read(m).slice(-3), ['Pick: chosen (taken)', 'Hold: placed (taken)', 'Reserve: booked (taken)']);
  assert.deepEqual([m.decisive, m.host, stationOf(m, 'hold').second, stationOf(m, 'hold').tone], ['hold', 'pick', ['session pin'], 'good']);
  assert.deepEqual(terminiOf(m), THREE_TERMINI(['chosen', 'placed']));
});

test("two partitions sit far enough apart that a badge pill wider than its label clears its neighbors' labels", async () => {
  // A No fit station reading `fell through` wraps at the label's width, and its pill around `through` (63 px in
  // Chromium) stands past the label's edges. Half of it and half of a neighbor's 58 px label take 60.5 px. The pill's
  // width is what the case measures, so the badge is set by hand: the failover landing reads `1 out` under the
  // router's ladder.
  const record = await recordOf('req_b8c3e9d4-e61/2');
  const fell = (s) => (s.step === 'partitions' ? { ...s, facts: s.facts.map((f) => (f.key === 'no_fit' ? { ...f, value: 'fell_through' } : f)) } : s);
  const m = modelOf({ ...record, walk: record.walk.map(fell) });
  assert.equal(stationOf(m, 'partitions:no_fit').badge, 'fell through');
  const layout = d.layoutStrip(m, 958);
  const [atCap, noFit, windingDown] = ['partitions:at_cap', 'partitions:no_fit', 'partitions:winding_down'].map((id) => layout.stations[id].x);
  assert.ok(noFit - atCap >= 60.5 && windingDown - noFit >= 60.5, `the partitions sit ${noFit - atCap} and ${windingDown - noFit} px apart`);
});

/** The chosen terminus's lead over the line's last station, and the unit it is read against. */
const terminiLead = (m, layout) => ({ lead: layout.termini.chosen.x - Math.max(...m.stations.map((s) => layout.stations[s.id].x)), unit: layout.unit });

test("at the strip's 640 px floor the failover landing keeps its termini past its last station", async () => {
  // The run to the termini gives the partitions' floor its room down to one unit, and the unit gives the rest.
  const m = modelOf(await recordOf('req_b8c3e9d4-e61/2'));
  const layout = d.layoutStrip(m, 640);
  const { lead, unit } = terminiLead(m, layout);
  assert.ok(unit > 0 && lead >= unit - 1e-9, `Chosen stands ${lead} px past the last station at a unit of ${unit}`);
  assert.ok(layout.termini.chosen.x <= 640 - 150 + 1e-9, `Chosen at ${layout.termini.chosen.x} px keeps the 150 px the termini take at the right`);
  assert.ok(Math.abs(layout.stations['partitions:no_fit'].x - layout.stations['partitions:at_cap'].x - 61) < 1e-9, 'the partitions keep their 61 px');
});

test("at the strip's 640 px floor a version 1 placement with five partitions keeps its termini past its last station", async () => {
  const base = await recordOf('req_ef099b34-453/1');
  const partitions = step('partitions', 'match', [fact('spent', 'off'), fact('at_cap', 0, 'count'), fact('above_ceiling', 0, 'count'), fact('winding_down', 0, 'count'), fact('floor', 0, 'count')]);
  const m = modelOf({ ...base, version: 1, walk: base.walk.filter((s) => s.step !== 'reserve').map((s) => (s.step === 'partitions' ? partitions : s)) });
  assert.equal(m.stations.filter((s) => s.group === 'partitions').length, 5);
  const layout = d.layoutStrip(m, 640);
  const { lead, unit } = terminiLead(m, layout);
  assert.ok(unit > 0 && lead >= unit - 1e-9, `Chosen stands ${lead} px past the last station at a unit of ${unit}`);
  assert.ok(layout.termini.chosen.x <= 640 - 150 + 1e-9, `Chosen at ${layout.termini.chosen.x} px keeps the 150 px the termini take at the right`);
});

test("the layout spaces by weight and keeps the surface's height", async () => {
  const freshModel = modelOf(await recordOf('req_ef099b34-453/1'));
  const fresh = d.layoutStrip(freshModel, 958);
  assert.ok(fresh.unit > 50 && fresh.unit < 60, `the unit is ${fresh.unit}`);
  assert.equal(fresh.width, 958);
  const xs = freshModel.stations.map((s) => fresh.stations[s.id].x);
  assert.ok(xs.every((x, i) => i === 0 || x > xs[i - 1]), 'every x increases');
  assert.ok(freshModel.stations.every((s) => fresh.stations[s.id].y === fresh.y0));
  const parts = freshModel.stations.filter((s) => s.id.startsWith('partitions:')).map((s) => fresh.stations[s.id].x);
  // The partitions stand 61 px apart where a unit is narrower, the room coming out of the run to the termini.
  const gap = Math.max(fresh.unit, 61);
  assert.ok(parts.every((x, i) => i === 0 || Math.abs(x - parts[i - 1] - gap) < 1e-9), `the partitions sit ${gap} px apart`);
  assert.equal(fresh.height, Math.max(104 + 76 + freshModel.leaves.length * 30 + 4, 208));
  assert.ok(Object.values(fresh.labels).every((l) => l.width > 0 && l.width <= 110));
  assert.deepEqual(Object.entries(fresh.labels).filter(([, l]) => l.above).map(([id]) => id), ['pick']);
  assert.deepEqual(fresh.leaves.map((p) => [p.x, p.y]), [0, 1, 2, 3].map((i) => [fresh.stations.pick.x - 16, 180 + i * 30]));
  const kinds = (layout) => layout.paths.reduce((n, p) => ({ ...n, [p.kind]: (n[p.kind] ?? 0) + 1 }), {});
  assert.deepEqual(kinds(fresh), { curve: 3, edge: 10, fan: 1, stub: 3, tick: 4 });
  assert.deepEqual(fresh.paths.filter((p) => p.kind === 'curve').map((p) => p.state), ['taken', 'idle', 'idle']);
  assert.deepEqual(fresh.paths.filter((p) => p.kind === 'tick').map((p) => p.state), ['taken', 'idle', 'idle', 'idle']);
  assert.deepEqual(fresh.brackets.map((b) => b.word), ['Partitions', 'Rungs']);

  const narrow = d.layoutStrip(freshModel, 640);
  assert.ok(narrow.unit < fresh.unit);
  assert.ok([...Object.values(narrow.stations), ...Object.values(narrow.termini)].every((p) => p.x <= 640));

  const shedModel = modelOf(await recordOf('req_868264bc-611/1'));
  const shed = d.layoutStrip(shedModel, 958);
  assert.deepEqual([shed.y0, shed.yu], [176, 96]);
  assert.ok(shedModel.stations.filter((s) => s.level === 'track').every((s) => shed.stations[s.id].y === 96));
  assert.ok(shedModel.stations.filter((s) => s.level === 'line').every((s) => shed.stations[s.id].y === 176));
  assert.ok(Math.abs(shed.termini.chosen.x - shed.stations.reserve.x - shed.unit) < 1e-9, 'the continuation ends one unit past its last station');
  assert.ok(shed.stations.renew.x > shed.stations['move:shed'].x && shed.stations.renew.x < shed.stations.pick.x, 'Renew stands between the fired rule and the host');
  assert.deepEqual(Object.keys(shed.termini), ['chosen']);
  assert.deepEqual(shed.paths.filter((p) => p.kind === 'track').map((p) => p.state), ['taken']);
  assert.ok(shedModel.stations.filter((s) => s.level === 'track').every((s) => shed.labels[s.id].above));
  assert.deepEqual(shed.brackets.map((b) => b.word), ['Moves', 'Rungs']);
  assert.equal(shed.height, Math.max(176 + 76 + shedModel.leaves.length * 30 + 4, 280));

  const staticLayout = d.layoutStrip(modelOf(await recordOf('req_b2bed817-a58/1')), 958);
  assert.deepEqual([staticLayout.y0, staticLayout.yu], [128, 56]);
  assert.deepEqual(staticLayout.paths.filter((p) => p.kind === 'track').map((p) => p.state), ['taken']);
  // The rung's track comes down into the Reserve it booked, which runs on into Chosen.
  assert.ok(staticLayout.paths.find((p) => p.kind === 'track').d.endsWith(`${staticLayout.stations.reserve.x} 128`), "the rung's track comes down into Reserve");
  assert.deepEqual(staticLayout.paths.filter((p) => p.kind === 'curve').map((p) => p.state), ['taken', 'idle', 'idle']);
  assert.deepEqual(staticLayout.paths.filter((p) => p.kind === 'edge').map((p) => p.state), [...Array(8).fill('taken'), 'idle', 'idle']);

  const freshRecord = await recordOf('req_ef099b34-453/1');
  const unpartitioned = modelOf({ ...freshRecord, walk: freshRecord.walk.filter((s) => s.step !== 'partitions') });
  assert.deepEqual(unpartitioned.stations.filter((s) => s.id.startsWith('partitions:')).map((s) => s.state), Array(4).fill('inferred'));
  assert.deepEqual(d.layoutStrip(unpartitioned, 958).paths.filter((p) => p.kind === 'edge').map((p) => p.state), ['taken', ...Array(4).fill('inferred'), ...Array(5).fill('taken')]);

  const base = await recordOf('req_f8085653-422/1');
  const walk = [step('pin', 'match'), step('eligible', 'match'), step('move:spent', 'fired'), ...RULES.slice(1).map((r) => step(r, 'not_run')), step('cap_filter', 'dropped'), step('renew', 'renewed')];
  const capped = d.layoutStrip(modelOf(built(base, walk, 'renewed')), 958);
  assert.ok(Math.abs(capped.stations.renew.x - capped.stations.cap_filter.x - capped.unit) < 1e-9, 'Renew sits one unit past the last track station');
  assert.equal(capped.stations.renew.y, 176);
  assert.ok(Math.abs(capped.termini.chosen.x - capped.stations.renew.x - capped.unit) < 1e-9, 'the termini follow Renew');
  const idleRules = RULES.slice(1).map((r) => capped.stations[r].x);
  assert.ok(idleRules.every((x, i) => x > (i === 0 ? capped.stations['move:spent'].x : idleRules[i - 1]) && x < capped.stations.renew.x), 'the idle rules spread between the fired rule and Renew');
  const track = capped.paths.filter((p) => p.kind === 'track');
  assert.equal(track.length, 1);
  assert.ok(track[0].d.endsWith(`${capped.stations.renew.x} 176`), 'the track descends to the line at Renew');
  assert.deepEqual(capped.paths.filter((p) => p.kind === 'curve').map((p) => p.state), ['taken']);
  // The walk reached Renew down the track, so the line's edge out of the idle Shed stays idle.
  assert.deepEqual(capped.paths.filter((p) => p.kind === 'edge').map((p) => p.state), ['taken', 'taken', 'idle', 'idle', 'idle', 'idle', 'idle']);
  assert.deepEqual([capped.labels.renew.above, capped.labels.cap_filter.above], [false, true], 'the track comes down into Renew, so its label stays below the line');
  // The fan drops from Renew past its label, and the first leaf row keeps its place below it.
  const renewLabel = capped.labels.renew;
  const fan = capped.paths.find((p) => p.kind === 'fan');
  assert.ok(fan.d.startsWith(`M${capped.stations.renew.x} ${renewLabel.top + renewLabel.height} `), 'the fan starts at the label\'s bottom edge');
  assert.ok(renewLabel.top + renewLabel.height > 176 + d.RADIUS);
  assert.equal(capped.leaves[0].y, 176 + 76);
});

test("a label's measured height moves the leaf rows below it and the track's bracket above it", async () => {
  const freshModel = modelOf(await recordOf('req_ef099b34-453/1'));
  const counted = d.layoutStrip(freshModel, 958);
  // A missing or a zero reading leaves the counted metrics, as jsdom's boxes do.
  assert.deepEqual(d.layoutStrip(freshModel, 958, { pick: 0 }), counted);
  const oneLine = counted.labels['rung:with_main'].height;
  // "with main" wrapped onto a second 16 px line below the line, left of the leaves' right edge.
  const fresh = d.layoutStrip(freshModel, 958, { 'rung:with_main': oneLine + 16 });
  const label = fresh.labels['rung:with_main'];
  assert.deepEqual([label.above, label.height, label.top], [false, oneLine + 16, counted.labels['rung:with_main'].top]);
  assert.ok(fresh.leaves[0].y - d.LEAF_HALF >= label.top + label.height, `the first leaf row (top ${fresh.leaves[0].y - d.LEAF_HALF}) starts below the label (bottom ${label.top + label.height})`);
  assert.ok(fresh.leaves.every((p, i) => i === 0 || p.y - fresh.leaves[i - 1].y === 30), 'the rows keep their 30 px');
  assert.equal(fresh.height, Math.max(fresh.leaves[0].y + freshModel.leaves.length * 30 + 4, fresh.y0 + 104), 'the surface grows with the rows');
  assert.equal(fresh.y0, counted.y0);

  const shedModel = modelOf(await recordOf('req_868264bc-611/1'));
  const shedCounted = d.layoutStrip(shedModel, 958);
  const rise = shedCounted.labels['rung:with_main'].height + 16;
  // The same word wrapped above the track, under the Rungs bracket.
  const shed = d.layoutStrip(shedModel, 958, { 'rung:with_main': rise });
  const above = shed.labels['rung:with_main'];
  const rungs = shed.brackets.find((b) => b.word === 'Rungs');
  assert.deepEqual([above.above, above.height], [true, rise]);
  assert.equal(above.top, shed.stations['rung:with_main'].y - 12 - rise, 'the label rises from the node by its measured height');
  assert.ok(rungs.y + d.BRACKET_TICK + 3 <= above.top, `the bracket's ticks (to ${rungs.y + d.BRACKET_TICK}) clear the label (top ${above.top}) by 3 px`);
  assert.ok(rungs.y - d.BRACKET_WORD_RISE >= 0, 'the bracket word stays inside the surface');
  assert.ok(Object.values(shed.labels).every((l) => l.top >= 0), 'every label stays inside the surface');
  // Room above the track is made by moving the whole strip down, so the rest of the drawing keeps its shape.
  const down = shed.y0 - shedCounted.y0;
  assert.equal(shed.yu - shedCounted.yu, down);
  assert.ok(shedModel.stations.every((s) => shed.stations[s.id].y === shedCounted.stations[s.id].y + down && shed.stations[s.id].x === shedCounted.stations[s.id].x));
  assert.equal(shed.height, shedCounted.height + down);
});

/** How far a station stands from the nearest other station on its level whose label stands on the same side. */
const neighborOf = (layout, id) => {
  const p = layout.stations[id];
  const above = layout.labels[id].above;
  return Math.min(...Object.keys(layout.stations).filter((o) => o !== id && layout.stations[o].y === p.y && layout.labels[o].above === above).map((o) => Math.abs(layout.stations[o].x - p.x)));
};

test("a station's label is no wider than the way to its nearest neighbor on its side of its level less 4 px", async () => {
  // The cap cancels a spent move with shedding off, so the four rules it cut short spread between it and Renew.
  const base = await recordOf('req_f8085653-422/1');
  const walk = [step('pin', 'match'), step('eligible', 'match'), step('move:spent', 'fired'), ...RULES.slice(1).map((r) => step(r, 'not_run')), step('cap_filter', 'dropped'), step('renew', 'renewed')];
  const capped = d.layoutStrip(modelOf(built(base, walk, 'renewed')), 958);
  for (const id of RULES.slice(1)) {
    const { width } = capped.labels[id];
    const room = neighborOf(capped, id) - 4;
    assert.ok(width > 0 && width <= room + 1e-9, `${id}'s label is ${width} px wide beside ${room} px`);
  }
  // Two partitions of one group stand 61 px apart at this width, both reading "fell through" when neither preference
  // keeps a member.
  const freshRecord = await recordOf('req_ef099b34-453/1');
  const fell = (s) => (s.step === 'partitions' ? { ...s, facts: s.facts.map((f) => (f.key === 'no_fit' || f.key === 'winding_down' ? { ...f, value: 'fell_through' } : f)) } : s);
  const m = modelOf({ ...freshRecord, walk: freshRecord.walk.map(fell) });
  const pair = ['partitions:no_fit', 'partitions:winding_down'];
  assert.deepEqual(pair.map((id) => stationOf(m, id).badge), ['fell through', 'fell through']);
  const fresh = d.layoutStrip(m, 958);
  for (const id of pair) {
    const label = fresh.labels[id];
    assert.equal(label.above, false, id);
    assert.ok(Math.abs(label.width - (Math.max(fresh.unit, 61) - 4)) < 1e-9, `${id}'s label is ${label.width} px wide, not the partitions' gap less 4`);
  }
});

test("a short walk's line moves right so its leaves keep inside the strip, and a long line stays where it starts", async () => {
  const base = await recordOf('req_ef099b34-453/1');
  // An answered probe: three stations at the widest unit, so the host stands near the strip's left edge.
  const record = built(base, [step('pin', 'match'), step('eligible', 'match'), step('probe', 'answered')], 'renewed');
  const m = modelOf(record);
  assert.deepEqual([m.shape, m.host, m.leaves.length > 0], ['short', 'probe', true]);
  const layout = d.layoutStrip(m, 958);
  // Without candidates the line draws no leaves, so it starts at its left margin.
  const bare = d.layoutStrip(modelOf({ ...record, candidates: [] }), 958);
  const host = layout.stations.probe;
  const shift = host.x - bare.stations.probe.x;
  assert.ok(shift > 0, `the line moves right by ${shift} px`);
  assert.ok(host.x >= d.LEAF_ROOM - 1e-9 || Math.abs(layout.termini.chosen.x - (958 - 150)) < 1e-9, `the host stands at ${host.x}, short of ${d.LEAF_ROOM} px with room left at the right`);
  for (const s of m.stations) assert.ok(layout.stations[s.id].y === bare.stations[s.id].y && Math.abs(layout.stations[s.id].x - bare.stations[s.id].x - shift) < 1e-9, `${s.id} keeps its place on the line`);
  for (const t of ['chosen', 'held', 'refused']) assert.ok(Math.abs(layout.termini[t].x - bare.termini[t].x - shift) < 1e-9, `${t} moves with the line`);
  assert.deepEqual(layout.leaves.map((p) => [p.x, p.maxWidth]), m.leaves.map(() => [host.x - 16, host.x - 16 - 8]));

  const freshModel = modelOf(await recordOf('req_ef099b34-453/1'));
  const fresh = d.layoutStrip(freshModel, 958);
  const freshBare = d.layoutStrip(modelOf({ ...(await recordOf('req_ef099b34-453/1')), candidates: [] }), 958);
  assert.equal(fresh.stations.pin.x, 36);
  assert.ok(freshModel.stations.every((s) => fresh.stations[s.id].x === freshBare.stations[s.id].x), 'a line whose host stands far enough right stays');
  assert.deepEqual(fresh.leaves.map((p) => p.maxWidth), freshModel.leaves.map(() => fresh.stations.pick.x - 16 - 8));
});

/** The story's decisions, and the holds and the exhaustion after a hold its cards are read beside. */
const STORY = [
  ...['e5a1c093-4d7', 'f3b6d218-85a', '0b7c2d94-e61', '1eaa9bc1-a18', '868264bc-611', 'a1f04c2e-7b3', 'b72e9d05-c14', 'c90d3f61-a28', 'd4e8a7b2-916', '3b8e51f4-0a9', 'da52d141-a5a', 'c41d0311-200', 'ef099b34-453', 'b2bed817-a58', '6c3a9e27-5b1', 'e0a4c6f2-118', '51d0e2b8-7a0', '31f0b7c2-6e1', '9a07c4e1-d55', 'f8085653-422'].map((id) => `req_${id}/1`),
  ...['1d9c3e55-0b2', '69e30548-32e', '2e7c90a1-6d4', '4b0e9d17-c31'].flatMap((id) => [`req_${id}/1`, `req_${id}/2`]),
  'req_03ddc3af-4d5/1',
  'req_03ddc3af-4d5/2',
  'req_77634945-72b/2',
  ...['9d4c1a73-e58/1', '9d4c1a73-e58/2', '5b2e8d47-c19/1', '6e1f3b98-a24/1', '2c7d9f15-b63/1', '7a3f0c26-d81/1', '4e8b1d36-f72/1', '9f2e7c5a-c49/1', '9f2e7c5a-c49/2', 'b8c3e9d4-e61/2', '5d6f1b3a-e27/1', '5d6f1b3a-e27/2'].map((id) => `req_${id}`),
];
const chipText = (c) => (c.label ? `${c.label} ${c.value}` : c.value);
const rowsOf = (plan) => (plan.block.kind === 'groups' ? plan.block.groups.flatMap((g) => g.rows) : plan.block.kind === 'table' ? plan.block.table.loose : []);
const readRows = (plan) => rowsOf(plan).map((r) => [r.label, r.chips.map(chipText)]);
const planOf = (record, anchor) => d.anchorPlan(anchor, modelOf(record), record, undefined, NOW);
const stationPlan = (record, id) => planOf(record, { id, kind: 'station' });

/**
 * Every popover the strip opens over `record`: each station, each leaf and each drawn terminus, with what it stands
 * for.
 */
function popoversOf(record) {
  const m = modelOf(record);
  const firedAt = record.walk.findIndex((s) => s.step.startsWith('move:') && s.verdict === 'fired');
  const stations = m.stations.map((s) => {
    const at = s.id === 'move' ? firedAt : s.index;
    const step = at === null || at < 0 ? null : record.walk[at];
    const cut = s.id !== 'move' && step !== null && s.step.startsWith('move:') && step.verdict === 'not_run' && firedAt >= 0 && s.index > firedAt;
    return { anchor: { id: s.id, kind: 'station' }, cut, name: s.id, station: s, step, word: s.head };
  });
  const leaves = m.leaves.map((leaf, index) => ({ anchor: { index, kind: 'leaf' }, leaf, name: `leaf ${index}`, step: null, word: leaf.accountId ?? leaf.badge }));
  const termini = m.termini.filter((t) => t.drawn).map((terminus) => ({ anchor: { id: terminus.kind, kind: 'terminus' }, name: terminus.kind, step: null, terminus, word: terminus.word }));
  return { m, popovers: [...stations, ...leaves, ...termini] };
}

/** The spec's inventory of popovers, the first row a popover matches naming its renderer. */
const INVENTORY = [
  ['a rule a fired rule cut short', (x) => x.cut === true, 'chips'],
  ['Rest of the walk', (x) => x.station?.step === 'selection', 'sentence'],
  ['a placeholder', (x) => x.station !== undefined && x.step === null, 'sentence'],
  ['Held for the reset', (x) => x.terminus?.kind === 'held_reset', 'outcome'],
  ['Held for the slow-down', (x) => x.terminus?.kind === 'held_slow_down', 'outcome'],
  ['Held for the rewrite in flight', (x) => x.terminus?.kind === 'held_rewrite', 'outcome'],
  ['an idle terminus', (x) => x.terminus?.state === 'idle', 'sentence'],
  ['a recorded step that did not run, with no facts', (x) => x.step?.verdict === 'not_run' && x.step.facts.length === 0, 'sentence'],
  ['a taken terminus', (x) => x.terminus?.state === 'taken', 'outcome'],
  ['the "and N more" leaf', (x) => x.leaf !== undefined && x.leaf.more > 0, 'table'],
  ['a leaf', (x) => x.leaf !== undefined, 'candidate'],
  ['a partition station', (x) => x.station?.step === 'partitions', 'list'],
  ['Admission, Bound, Exhausted, Probe, Renew', (x) => ['admission', 'bound', 'exhausted', 'probe', 'renew'].includes(x.station?.step), 'list'],
  ['Cap filter, Eligible, Fit', (x) => ['cap_filter', 'eligible', 'fit'].includes(x.station?.step), 'table'],
  ['Hold', (x) => x.station?.step === 'hold', 'hold'],
  ['Move', (x) => x.station?.id === 'move', 'list'],
  ['Pick', (x) => x.station?.step === 'pick', 'table'],
  ['Pin', (x) => x.station?.step === 'pin', 'list'],
  ['Reserve, nothing booked', (x) => x.station?.step === 'reserve' && x.step?.verdict === 'none', 'sentence'],
  ['Reserve', (x) => x.station?.step === 'reserve', 'list'],
  ['a rung', (x) => x.station?.step.startsWith('rung:') === true, 'rung'],
  ['Shed', (x) => x.station?.step === 'move:shed', 'list'],
  ['the other four rules', (x) => x.station?.step.startsWith('move:') === true, 'list'],
];

/** The account's slow-down facts the router adds to every shed line, a line a fired rule cut short among them. */
const SLOW_DOWN_KEYS = new Set(['demand_five_hour', 'factor', 'over_since', 'ramp', 'slow_down', 'speed', 'stretch_ms', 'threshold_ms']);

/** The labels a fact's rows read: its word, "on" for the account a step acted on, the lane's three parts. */
const labelsOf = (f) => (f.key === 'account_id' ? ['on'] : f.key === 'lane' ? ['speed', 'thinking', 'effort'] : [d.keyWord(f.key)]);

test("every popover of the story's records takes the inventory's renderer and places every fact", async () => {
  const seen = new Set();
  for (const id of STORY) {
    const record = await recordOf(id);
    const { m, popovers } = popoversOf(record);
    for (const x of popovers) {
      const where = `${id} ${x.name}`;
      const plan = d.anchorPlan(x.anchor, m, record, undefined, NOW);
      const row = INVENTORY.find(([, matches]) => matches(x));
      assert.ok(row, `${where} has an inventory row`);
      seen.add(row[0]);
      assert.equal(plan.renderer, row[2], `${where} is ${row[0]}`);
      assert.equal(plan.head.word, x.word, `${where} heads with its word`);
      // A rung draws its hash from the entry, a Reserve that booked nothing reads its sentence in place of the account,
      // and a shed rule a fired rule cut short reads the rules that did not run in place of the account's slow-down,
      // the only facts the router writes on such a line.
      const aside = (u) => (u.key === 'hash' && plan.renderer === 'rung') || (u.key === 'account_id' && plan.renderer === 'sentence' && x.station?.step === 'reserve') || (x.cut === true && plan.renderer === 'chips' && SLOW_DOWN_KEYS.has(u.key));
      assert.ok(plan.unplaced.every((u) => aside(u) && u.reason !== ''), `${where} sets aside only a rung's hash, an unbooked account or a cut-short rule's facts: ${JSON.stringify(plan.unplaced)}`);
      const set = new Set(plan.unplaced.map((u) => u.key));
      for (const f of x.step?.facts ?? []) {
        if (set.has(f.key)) continue;
        if (plan.renderer === 'table') {
          const placed = f.accountId === null ? plan.block.table.loose.some((r) => r.label === d.keyWord(f.key)) : plan.block.table.rows.find((r) => r.account.value === f.accountId)?.cells[f.key] != null;
          assert.ok(placed, `${where} places ${f.key} of ${f.accountId ?? 'the step'}`);
        } else {
          const labels = rowsOf(plan).map((r) => r.label);
          assert.ok(labelsOf(f).every((l) => labels.includes(l)), `${where} places ${f.key} as ${labelsOf(f).join(', ')} among ${labels.join(', ')}`);
        }
      }
    }
  }
  // The story's records reach every row of the inventory but the "and N more" leaf, which twelve candidates reach below.
  assert.deepEqual(INVENTORY.map(([what]) => what).filter((what) => !seen.has(what)), ['the "and N more" leaf']);
});

test("the pick's score column reads the Pace cell under the Pace header", async () => {
  let rows = 0;
  // A tier mate scoring above the chosen member sits at its cap and drops before the pick, so the story's own picks
  // rank one member each. A hold keeps the whole set, so its pick ranks the tier.
  for (const id of ['req_ef099b34-453/1', 'req_868264bc-611/1', 'req_03ddc3af-4d5/1']) {
    const record = await recordOf(id);
    const { table } = stationPlan(record, 'pick').block;
    const score = table.columns.find((c) => c.key === 'score');
    assert.deepEqual([score.label, score.numeric], [d.paceHeader(record), true], id);
    const scored = record.walk.find((s) => s.step === 'pick').facts.filter((f) => f.key === 'score');
    assert.deepEqual(table.rows.map((r) => r.account.value), scored.map((f) => f.accountId), id);
    for (const r of table.rows) {
      const cells = d.candidateCells(record.candidates.find((c) => c.accountId === r.account.value), record, NOW);
      assert.deepEqual(r.cells.score, { text: cells.pace, title: cells.paceTitle }, `${id} ${r.account.value}`);
      rows += 1;
    }
    assert.deepEqual(table.loose.map((r) => r.label), ['strategy', 'tier', 'tied', 'drawn'], id);
    assert.deepEqual(table.loose[0].chips.map(chipText), ['strategy Pace'], id);
  }
  assert.ok(rows > 2, `${rows} scores read`);
});

test("the pin's three groups hold their keys and the fired shed line reads its three groups", async () => {
  const groupsOf = (plan) => plan.block.groups.map((g) => [g.label, g.rows.map((r) => r.label)]);
  const renewal = await recordOf('req_f8085653-422/1');
  assert.deepEqual(groupsOf(stationPlan(renewal, 'pin')), [['Lookup', ['found', 'on', 'epoch']], ['Age', ['idle', 'refresh age']], ['Warmth', ['warmth', 'cause', 'marked']]]);

  const shedMove = await recordOf('req_868264bc-611/1');
  const fired = stationPlan(shedMove, 'move:shed');
  assert.deepEqual(groupsOf(fired).map(([label]) => label), ['Windows', 'Round', 'Destinations']);
  // The Move station draws the fired rule's list.
  assert.deepEqual(stationPlan(shedMove, 'move').block, fired.block);

  const round = await recordOf('req_1eaa9bc1-a18/1');
  const ran = stationPlan(round, 'move:shed');
  const marks = ran.block.groups.find((g) => g.label === 'Round').rows.find((r) => r.label === 'marks');
  assert.equal(marks.chips.length, round.outcome.shedMarks.length);
  // A mark's link opens the Sessions view on the mark's own session with Finished sessions on, and its title names that
  // session.
  assert.deepEqual(marks.chips.map((c) => [c.label, c.href, c.title.split('/')[1]]), round.outcome.shedMarks.map((mk) => ['mark', `/sessions?finished=1&session=${mk.sessionId}`, mk.sessionId]));
  // A pool with the slow-down off says so on every shed line, and a line that names a trigger names its source.
  for (const record of [shedMove, round]) {
    const keys = record.walk.find((s) => s.step === 'move:shed').facts.map((f) => [f.key, f.value]);
    assert.deepEqual([keys.find(([k]) => k === 'slow_down'), keys.find(([k]) => k === 'fired_by')], [['slow_down', 'off'], ['fired_by', 'projection']], `${record.requestId}/${record.seq}`);
  }
  // A pin the round passed over reads one chip, its session and the reason.
  const skipped = (await recordOf('req_6e1f3b98-a24/1'));
  const rows = stationPlan(skipped, 'move:shed').block.groups.find((g) => g.label === 'Round').rows.find((r) => r.label === 'skipped');
  assert.deepEqual(rows.chips.map((c) => [c.value, c.href]), [
    ['bcc805b6: unpriced', '/sessions?finished=1&session=bcc805b6-0588-4fe4-ac9e-62a13a0f6e99'],
    ['d7167642: no runway', '/sessions?finished=1&session=d7167642-751d-475e-b0bf-2f24fe70245e'],
    ['4ef130f1: no runway', '/sessions?finished=1&session=4ef130f1-90e0-46e3-9a3c-4a000fe854f1'],
    ['0d588de5: unpriced', '/sessions?finished=1&session=0d588de5-a77c-49ed-a40e-5dcb9d4f7fba'],
    ['d6f1a3c8: unpriced', '/sessions?finished=1&session=d6f1a3c8-2e94-4b07-8a5d-3c9e7f2b1a64'],
    ['d7167642: fast lane', '/sessions?finished=1&session=d7167642-751d-475e-b0bf-2f24fe70245e'],
    ['3c36d663: ended', '/sessions?finished=1&session=3c36d663-d074-42f6-8738-f54da7c9f026'],
  ]);
});

test("a held 5-hour window's age and its hold read as durations in the Round group after the wait", async () => {
  // The line the router writes while the 5-hour window is younger than its hold and its stamp still waits.
  const base = await recordOf('req_868264bc-611/1');
  const held = step('move:shed', 'no', [
    fact('projection_five_hour', 1.126, 'percent'),
    fact('horizon_five_hour', NOW + 7_200_000, 'time'),
    fact('over_since_five_hour', NOW - 300_000, 'time'),
    fact('wait_five_hour', 900_000, 'ms'),
    fact('age_five_hour', 600_000, 'ms'),
    fact('hold_five_hour', 3_600_000, 'ms'),
    fact('ceiling', 0.75, 'percent'),
    fact('marked', false),
    fact('slow_down', 'off'),
  ]);
  const keyed = { ...base, walk: base.walk.map((s) => (s.step === 'move:shed' ? held : s)) };
  const plan = stationPlan(keyed, 'move:shed');
  const round = plan.block.groups.find((g) => g.label === 'Round');
  assert.deepEqual(round.rows.map((r) => r.label), ['over since 5h', 'wait 5h', 'age 5h', 'age hold 5h', 'marked']);
  assert.deepEqual(round.rows.slice(1, 4).map((r) => r.chips.map(chipText)), [['wait 5h 15 min'], ['age 5h 10 min'], ['age hold 5h 1 h']]);
  assert.deepEqual(plan.block.groups.map((g) => g.label), ['Windows', 'Round'], 'no fact of the line falls to the unlabeled group');
});

test('a slowed shed line reads its ramp in the Windows group after the factor it applies to', async () => {
  // The slow-down facts in the order the router writes them: the demand's factor, the share of it the holds apply and
  // the speed they realize.
  const base = await recordOf('req_868264bc-611/1');
  const slowed = step('move:shed', 'no', [fact('slow_down', 'on'), fact('factor', 0.8), fact('ramp', 1 / 3, 'percent'), fact('speed', 0.93), fact('demand_five_hour', 0.1286, 'percent_per_hour')]);
  const keyed = { ...base, walk: base.walk.map((s) => (s.step === 'move:shed' ? slowed : s)) };
  const plan = stationPlan(keyed, 'move:shed');
  const windows = plan.block.groups.find((g) => g.label === 'Windows');
  assert.deepEqual(windows.rows.map((r) => r.label), ['demand_five_hour', 'factor', 'ramp', 'slow_down', 'speed'].map(d.keyWord));
  assert.deepEqual(windows.rows.find((r) => r.label === 'ramp').chips.map(chipText), ['ramp 33%']);
  assert.deepEqual(plan.block.groups.map((g) => g.label), ['Windows'], 'no fact of the line falls to the unlabeled group');
});

test("the return rule's popover reads its mode first, since the mode decides which facts the rule read", async () => {
  const labelsOf = (record) => readRows(stationPlan(record, 'move:return_to_plan')).map(([label]) => label);
  // A pin on a subscription reads the account's kind beside the mode, which the line writes after it.
  const renewal = await recordOf('req_f8085653-422/1');
  assert.deepEqual(renewal.walk.find((s) => s.step === 'move:return_to_plan').facts.map((f) => f.key), ['kind', 'mode']);
  assert.deepEqual(labelsOf(renewal), ['mode', 'kind']);
  // A pin on an API key under always reads the age against the return dwell.
  const base = await recordOf('req_f8085653-422/1');
  const always = step('move:return_to_plan', 'no', [fact('mode', 'always'), fact('cold', false), fact('dwell_age_ms', 300_000, 'ms'), fact('dwell_ms', 900_000, 'ms')]);
  const keyed = { ...base, walk: base.walk.map((s) => (s.step === 'move:return_to_plan' ? always : s)) };
  assert.deepEqual(readRows(stationPlan(keyed, 'move:return_to_plan')), [['mode', ['mode always']], ['cold', ['cold no']], ['dwell age', ['dwell age 5 min']], ['dwell', ['dwell 15 min']]]);
});

test("the rung's entry rows read the matched entry and never the deeper refused one", async () => {
  const record = await recordOf('req_b2bed817-a58/1');
  const chosen = record.candidates.find((c) => c.accountId === record.outcome.accountId);
  const match = chosen.prefix.staticMatch;
  assert.ok(chosen.prefix.static.depth > match.depth && chosen.prefix.static.refusedBy !== null, 'the member holds a deeper refused entry beside the match');
  const plan = stationPlan(record, 'rung:static');
  const entry = plan.block.groups.find((g) => g.label === 'Entry');
  assert.deepEqual(entry.rows.map((r) => r.label), ['hash', 'depth', 'prefix', 'written', 'TTL', 'live', 'on']);
  assert.equal(entry.rows[0].entry, match);
  // The TTL row carries the entry's warm-until time, the one place the popover draws it.
  assert.deepEqual(entry.rows.slice(1).map((r) => r.chips.map(chipText)), [[`depth ${match.depth}`], [`prefix ${d.fmtCompact(match.prefixTokens)}`], [`written ${d.formatFact(match.writtenAt, 'time', NOW)}`], [`TTL ${d.formatFact(match.ttlMs, 'ms', NOW)}`, d.warmUntil(match, NOW)], [`live ${match.live ? 'yes' : 'no'}`], [`on ${chosen.accountId}`]]);
  assert.ok(!JSON.stringify(plan).includes(chosen.prefix.static.hash), 'the deeper refused entry is nowhere in the plan');
  assert.deepEqual(plan.unplaced, [{ key: 'hash', reason: 'drawn from the entry' }]);
});

test("the hold's rows are place, on, slot and bound", async () => {
  for (const [id, place] of [['req_69e30548-32e/1', 'cohort'], ['req_03ddc3af-4d5/1', 'strategy pick']]) {
    const record = await recordOf(id);
    const on = record.outcome.hold.accountId;
    const member = record.candidates.find((c) => c.accountId === on);
    assert.deepEqual(readRows(stationPlan(record, 'hold')), [['place', [`place ${place}`]], ['on', [`on ${on}`]], ['slot', [`slot ${member.inFlight} of ${member.inFlightCap}`]], ['bound', ['bound 1 min']]], id);
  }
  // The session pin's exemption holds no slot back, so its step carries no bound.
  const base = await recordOf('req_ef099b34-453/1');
  const exempt = built(base, [...base.walk, step('hold', 'placed', [fact('place', 'session_pin'), fact('account_id', 'primary', null, 'primary')])], 'placed');
  const primary = exempt.candidates.find((c) => c.accountId === 'primary');
  assert.deepEqual(readRows(stationPlan(exempt, 'hold')), [['place', ['place session pin']], ['on', ['on primary']], ['slot', [`slot ${primary.inFlight} of ${primary.inFlightCap}`]]]);
});

test("a candidate's windows table has a row per reported window and one for a key account", async () => {
  const words = { five_hour: '5h', seven_day: '7d', seven_day_fable: 'Fable', worst_bucket: 'worst bucket' };
  let keys = 0;
  for (const id of ['req_f8085653-422/1', 'req_ef099b34-453/1']) {
    const record = await recordOf(id);
    const m = modelOf(record);
    m.leaves.forEach((leaf, index) => {
      const plan = planOf(record, { index, kind: 'leaf' });
      const member = record.candidates.find((c) => c.accountId === leaf.accountId);
      const cells = d.candidateCells(member, record, NOW);
      const reported = Object.keys(member.windows);
      assert.equal(plan.block.windows.length, reported.length, `${id} ${member.accountId}`);
      assert.deepEqual(new Set(plan.block.windows.map((w) => w.window)), new Set(reported.map((w) => words[w])));
      for (const w of plan.block.windows) {
        const reading = member.windows[Object.keys(words).find((k) => words[k] === w.window)];
        assert.deepEqual([w.used, w.left, w.source, w.age], [d.fmtPct(reading.utilization), d.fmtPct(1 - reading.utilization), reading.source, d.ageWord(reading.ageMs)]);
      }
      if (member.kind === 'api_key') {
        keys += 1;
        assert.deepEqual(plan.block.windows.map((w) => w.window), ['worst bucket']);
      }
      const out = member.verdict === 'out';
      assert.deepEqual([plan.head.account.value, plan.head.word, plan.head.tone, plan.head.chip.value], [member.accountId, member.accountId, leaf.tone, out ? cells.verdict.chip.value : cells.verdict.badge]);
      assert.deepEqual(plan.block.chips.slice(0, 3).map(chipText), [`plan ${cells.plan}`, `tier ${cells.tier}`, `in flight ${cells.inFlight}`]);
      assert.equal(plan.block.chips.find((c) => c.label === 'Pace')?.title ?? null, member.pace === null ? null : cells.paceTitle);
    });
  }
  assert.ok(keys > 0, 'a key account is among the leaves');
});

test("the taken terminus's rows equal the outcome rows", async () => {
  for (const id of ['req_f8085653-422/1', 'req_69e30548-32e/1', 'req_51d0e2b8-7a0/1', 'req_e0a4c6f2-118/1']) {
    const record = await recordOf(id);
    const m = modelOf(record);
    for (const t of m.termini.filter((x) => x.drawn)) {
      const plan = planOf(record, { id: t.kind, kind: 'terminus' });
      if (t.state !== 'taken') {
        assert.deepEqual([plan.renderer, plan.block.text, plan.head.badge], ['sentence', d.SENTENCE.notReached, null], `${id} ${t.kind}`);
        continue;
      }
      const rows = d.outcomeRows(record, NOW).map((r) => ({ chips: r.chips, label: r.label, ...(r.title === undefined ? {} : { title: r.title }) }));
      assert.ok(rows.length > 0);
      assert.deepEqual(rowsOf(plan), rows, `${id} ${t.kind}`);
      assert.deepEqual([plan.head.badge, plan.head.tone], [t.badge, t.tone]);
    }
  }
});

test('every list row carries its chips', async () => {
  let rows = 0;
  for (const id of STORY) {
    const record = await recordOf(id);
    const { m, popovers } = popoversOf(record);
    for (const x of popovers) {
      for (const r of rowsOf(d.anchorPlan(x.anchor, m, record, undefined, NOW))) {
        rows += 1;
        assert.ok(r.label !== '' && (r.chips.length > 0 || r.entry !== undefined), `${id} ${x.name}: ${r.label} carries chips`);
      }
    }
  }
  assert.ok(rows > 100, `${rows} rows read`);
});

test('a rule a fired rule cut short reads the rules that did not run with its own marked, and the steps with no facts their sentences', async () => {
  const record = await recordOf('req_da52d141-a5a/1');
  const later = ['move:overage', 'move:return_to_plan', 'move:drain', 'move:shed'];
  later.forEach((rule, i) => {
    const plan = stationPlan(record, rule);
    assert.deepEqual([plan.renderer, plan.block.chips.map(chipText), plan.block.marked], ['chips', ['overage', 'return to plan', 'drain', 'shed'], i], rule);
  });
  assert.equal(stationPlan(record, 'renew').block.text, d.SENTENCE.placeholder);
  const refusal = await recordOf('req_e0a4c6f2-118/1');
  assert.equal(stationPlan(refusal, 'selection').block.text, d.SENTENCE.rest);
  const withMain = await recordOf('req_a0f5c94d-8f0/1');
  assert.deepEqual([stationPlan(withMain, 'rung:static').block.text, stationPlan(withMain, 'pick').block.text], [d.SENTENCE.notRun, d.SENTENCE.placeholder]);
});

test('the and-N-more leaf is a table of the remaining candidates', async () => {
  const base = await recordOf('req_ef099b34-453/1');
  const chosen = base.candidates.find((c) => c.verdict === 'chosen');
  const others = base.candidates.filter((c) => c !== chosen);
  const copies = Array.from({ length: 11 }, (_, i) => ({ ...others[i % others.length], accountId: `member-${i + 1}` }));
  const record = { ...base, candidates: [...copies, chosen] };
  const m = modelOf(record);
  const plan = d.anchorPlan({ index: 8, kind: 'leaf' }, m, record, undefined, NOW);
  assert.deepEqual([plan.renderer, plan.head.word, plan.head.badge], ['table', 'and 4 more', null]);
  const { table } = plan.block;
  assert.deepEqual(table.columns.map((c) => c.label), ['Tier', '5h', '7d', 'Fable', 'Projection', 'Runway', 'In flight', d.paceHeader(record), 'Prefix', 'Verdict']);
  assert.deepEqual(table.rows.map((r) => r.account.value), ['member-8', 'member-9', 'member-10', 'member-11']);
  for (const r of table.rows) {
    const cells = d.candidateCells(record.candidates.find((c) => c.accountId === r.account.value), record, NOW);
    assert.deepEqual([r.cells.tier.text, r.cells.in_flight.text, r.cells.pace.text, r.cells.projection?.text, r.cells.runway?.text], [cells.tier, cells.inFlight, cells.pace, cells.projection[0], cells.runway]);
  }
});
