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
const api = new d.MockApi();

async function recordOf(id) {
  const [requestId, seq] = id.split('/');
  const record = await api.getDecision(requestId, Number(seq));
  assert.ok(record, `the story holds ${id}`);
  return record;
}

const modelOf = (record) => d.stationModel(record, undefined, NOW);

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
const PARTITIONS = ['Spent', 'At cap', 'Above ceiling', 'Winding down', 'Floor'];
const FRESH_LINE = (pin, eligible) => [`Pin: ${pin} (taken)`, `Eligible: ${eligible} (taken)`, ...PARTITIONS.map((w) => `${w} (taken)`)];
const FIVE_RULES = (verdict, state) => ['spent', 'overage', 'return to plan', 'drain', 'shed'].map((w) => `${w}: ${verdict} (${state})`);
const THREE_TERMINI = (taken) => ['chosen', 'held', 'refused'].map((k) => (k === taken[0] ? `${d.TERMINUS_WORD[k]}: ${taken[1]} (taken)` : `${d.TERMINUS_WORD[k]}: - (idle)`));
const PINNED_TERMINI = (badge) => [`Chosen: ${badge} (taken)`, 'Held for a slot: - (idle, not drawn)', 'Refused: - (idle, not drawn)'];

/** A record built in the test over a story record's candidates and settings, with its own walk and outcome kind. */
const built = (base, walk, kind, more = {}) => ({ ...base, ...more, outcome: { ...base.outcome, kind }, walk });

test('every step the router records has a station, a renderer and a stub word, and every station a step', () => {
  const renderers = ['candidate', 'chips', 'hold', 'list', 'outcome', 'rung', 'sentence', 'table'];
  const stubs = ['fires', 'match', 'released'];
  for (const { step: id, verdicts } of fixture.steps) {
    assert.ok(Object.hasOwn(d.STATION_KINDS, id), `${id} has a station kind`);
    assert.ok(Object.hasOwn(d.STEP_NAME, id), `${id} has a step name`);
    const kind = d.STATION_KINDS[id];
    const name = d.STEP_NAME[id];
    const rest = name.replace(/^[^:]*: /, '');
    assert.ok([name, rest, rest.toLowerCase()].includes(d.stationWord(id)), `${id} reads ${d.stationWord(id)} against ${name}`);
    assert.ok(renderers.includes(kind.renderer), `${id} renders as ${kind.renderer}`);
    assert.ok(kind.stub === null || stubs.includes(kind.stub), `${id} has the stub ${kind.stub}`);
    assert.ok(kind.group === null || Object.hasOwn(d.GROUP_WORD, kind.group), `${id} sits in the group ${kind.group}`);
    for (const verdict of verdicts) assert.ok(d.TONED_VERDICTS.has(verdict), `${id} ${verdict} has a tone`);
  }
  const recorded = new Set(fixture.steps.map((s) => s.step));
  for (const id of Object.keys(d.STATION_KINDS)) assert.ok(recorded.has(id), `${id} is a step the router records`);
  for (const value of fixture.partition_values) {
    assert.ok(Object.hasOwn(d.PARTITION_WORD, value) && typeof d.PARTITION_WORD[value] === 'string' && d.PARTITION_WORD[value] !== '', `${value} has a word`);
    assert.ok(d.TONED_VERDICTS.has(value), `${value} has a tone`);
  }
  // Under a bracket the group's prefix leaves and the rest reads lowercased, while the one Partitions station stands alone.
  assert.deepEqual(['rung:content', 'rung:with_main', 'move:spent', 'move:return_to_plan', 'partitions', 'selection'].map(d.stationWord), ['content', 'with main', 'spent', 'return to plan', 'Partitions', 'Rest of the walk']);
});

test("the story's records give their stations, states, host and termini", async () => {
  const fresh = modelOf(await recordOf('req_ef099b34-453/1'));
  assert.deepEqual(read(fresh), [...FRESH_LINE('none', 'match'), 'content: none (taken)', 'with main: none (taken)', 'static: none (taken)', 'Pick: chosen (taken)']);
  assert.deepEqual([fresh.shape, fresh.decisive, fresh.host, fresh.branch], ['fresh', 'pick', 'pick', null]);
  assert.deepEqual(terminiOf(fresh), THREE_TERMINI(['chosen', 'placed']));
  assert.deepEqual(stationOf(fresh, 'pick').second, ['Pace P0']);
  assert.deepEqual(fresh.stations.filter((s) => s.id.startsWith('partitions:')).map((s) => [s.head, s.badge, s.tone]), [['Spent', 'off', 'neutral'], ['At cap', '0 out', 'good'], ['Above ceiling', 'off', 'neutral'], ['Winding down', '0 out', 'good'], ['Floor', '0 out', 'good']]);
  assert.deepEqual(fresh.brackets.map((b) => [b.word, b.level, b.stations.length]), [['Partitions', 'line', 5], ['Rungs', 'line', 3]]);
  assert.deepEqual(fresh.stations.filter((s) => s.stub !== null).map((s) => `${s.id} ${s.stub}`), ['rung:content match', 'rung:with_main match', 'rung:static match']);
  assert.deepEqual(stationOf(fresh, 'rung:content').head, 'Rung: content');
  assert.equal(fresh.stations.filter((s) => s.decisive).length, 1);

  for (const id of ['req_f8085653-422/1', 'req_1eaa9bc1-a18/1']) {
    const renewal = modelOf(await recordOf(id));
    assert.deepEqual(read(renewal), ['Pin: match (taken)', 'Eligible: match (taken)', ...FIVE_RULES('no', 'taken'), 'Renew: renewed (taken)'], id);
    assert.deepEqual([renewal.shape, renewal.decisive, renewal.host, renewal.branch], ['pinned', 'renew', 'renew', null], id);
    assert.deepEqual(renewal.brackets.map((b) => [b.word, b.level, b.stations]), [['Moves', 'line', RULES]], id);
    assert.deepEqual(renewal.stations.filter((s) => s.stub !== null).map((s) => s.stub), ['fires', 'fires', 'fires', 'fires', 'fires', 'released'], id);
    assert.deepEqual(terminiOf(renewal), PINNED_TERMINI('renewed'), id);
  }

  const spentMove = modelOf(await recordOf('req_da52d141-a5a/1'));
  assert.deepEqual(read(spentMove), ['Pin: match (taken)', 'Eligible: match (taken)', 'spent: fired (taken)', ...FIVE_RULES('not run', 'idle').slice(1), '^Move: moved (taken)', '^Cool reading: preferred (taken)', '^Cap filter: passed (taken)', '^Partitions: match (taken)', '^content: none (taken)', '^with main: none (taken)', '^static: match (taken)', 'Renew: not run (idle)']);
  assert.deepEqual([spentMove.shape, spentMove.decisive, spentMove.host, spentMove.branch], ['pinned', 'rung:static', 'rung:static', { from: 'move:spent', kind: 'move' }]);
  assert.deepEqual([stationOf(spentMove, 'move').head, stationOf(spentMove, 'move').second, stationOf(spentMove, 'move').tone, stationOf(spentMove, 'move').step], ['Move', ['spent rule'], 'warning', 'move:spent']);
  assert.deepEqual(spentMove.brackets.map((b) => [b.word, b.level, b.stations.length]), [['Moves', 'line', 5], ['Rungs', 'track', 3]]);
  assert.deepEqual(terminiOf(spentMove), PINNED_TERMINI('moved'));

  const shedMove = modelOf(await recordOf('req_868264bc-611/1'));
  assert.deepEqual(read(shedMove), ['Pin: match (taken)', 'Eligible: match (taken)', ...FIVE_RULES('no', 'taken').slice(0, 4), 'shed: fired (taken)', '^Move: moved (taken)', '^Cool reading: preferred (taken)', '^Cap filter: passed (taken)', '^Partitions: match (taken)', '^content: none (taken)', '^with main: none (taken)', '^static: none (taken)', '^Pick: chosen (taken)', 'Renew: not run (idle)']);
  assert.deepEqual([shedMove.decisive, shedMove.host, shedMove.branch], ['pick', 'pick', { from: 'move:shed', kind: 'move' }]);
  assert.deepEqual(stationOf(shedMove, 'move').second, ['shed rule']);
  // The fired rule left the line by the track, so its stub became the track, and the idle Renew keeps its own.
  assert.deepEqual(shedMove.stations.filter((s) => s.stub !== null).map((s) => s.id), ['move:spent', 'move:overage', 'move:return_to_plan', 'move:drain', 'renew']);
  assert.deepEqual(terminiOf(shedMove), PINNED_TERMINI('moved'));

  const staticPlaced = modelOf(await recordOf('req_b2bed817-a58/1'));
  assert.deepEqual(read(staticPlaced), [...FRESH_LINE('none', 'match'), 'content: none (taken)', 'with main: none (taken)', 'static: match (taken)', 'Pick: not run (idle)']);
  assert.deepEqual([staticPlaced.decisive, staticPlaced.host, staticPlaced.branch], ['rung:static', 'pick', { from: 'rung:static', kind: 'rung' }]);
  assert.deepEqual([stationOf(staticPlaced, 'pick').index, stationOf(staticPlaced, 'pick').verdict, stationOf(staticPlaced, 'rung:static').stub], [null, null, null]);
  assert.deepEqual(terminiOf(staticPlaced), THREE_TERMINI(['chosen', 'placed']));

  for (const [id, floor] of [['req_e8b7eb13-060/2', '0 out'], ['req_52f62e67-839/1', 'off']]) {
    const withMain = modelOf(await recordOf(id));
    assert.deepEqual(read(withMain), [...FRESH_LINE('none', 'match'), 'content: none (taken)', 'with main: match (taken)', 'static: not run (idle)', 'Pick: not run (idle)'], id);
    assert.deepEqual([withMain.decisive, withMain.host, withMain.branch], ['rung:with_main', 'pick', { from: 'rung:with_main', kind: 'rung' }], id);
    assert.deepEqual([stationOf(withMain, 'rung:static').verdict, stationOf(withMain, 'rung:static').tone, stationOf(withMain, 'partitions:floor').badge], ['not_run', 'neutral', floor], id);
    assert.deepEqual(terminiOf(withMain), THREE_TERMINI(['chosen', 'placed']), id);
  }

  for (const [id, place] of [['req_69e30548-32e/1', 'cohort'], ['req_e8b7eb13-060/1', 'strategy pick']]) {
    const hold = modelOf(await recordOf(id));
    assert.deepEqual(read(hold), [...FRESH_LINE('none', 'match'), 'content: none (taken)', 'with main: none (taken)', 'static: none (taken)', 'Pick: chosen (taken)', 'Hold: held (taken)'], id);
    assert.deepEqual([hold.decisive, hold.host], ['hold', 'pick'], id);
    assert.deepEqual([stationOf(hold, 'hold').second, stationOf(hold, 'hold').tone, stationOf(hold, 'pick').second], [[place, 'bound 1 min'], 'warning', ['Pace P0']], id);
    assert.deepEqual([stationOf(hold, 'partitions:at_cap').badge, stationOf(hold, 'partitions:at_cap').tone, stationOf(hold, 'partitions:at_cap').verdict], ['held', 'warning', 'held'], id);
    assert.deepEqual(terminiOf(hold), THREE_TERMINI(['held', 'held']), id);
  }

  for (const [id, strategy, kind] of [['req_51d0e2b8-7a0/1', 'round robin', 'timed'], ['req_77634945-72b/2', 'Pace', 'untimed']]) {
    const exhausted = modelOf(await recordOf(id));
    assert.deepEqual(read(exhausted), [...FRESH_LINE('none', 'none'), 'content: none (taken)', 'with main: none (taken)', 'static: none (taken)', 'Pick: none (taken)', 'Exhausted: exhausted (taken)'], id);
    assert.deepEqual([exhausted.decisive, exhausted.host, stationOf(exhausted, 'eligible').tone, stationOf(exhausted, 'exhausted').tone], ['exhausted', 'pick', 'critical', 'critical'], id);
    assert.deepEqual([stationOf(exhausted, 'pick').second, stationOf(exhausted, 'exhausted').second], [[strategy], [kind]], id);
    assert.deepEqual(terminiOf(exhausted), THREE_TERMINI(['refused', 'exhausted']), id);
  }

  const refusal = modelOf(await recordOf('req_e0a4c6f2-118/1'));
  assert.deepEqual(read(refusal), ['Admission: refused (taken)', 'Rest of the walk: not run (idle)']);
  assert.deepEqual([refusal.shape, refusal.decisive, refusal.host, refusal.branch, refusal.brackets, refusal.leaves], ['short', 'admission', null, null, [], []]);
  assert.deepEqual(terminiOf(refusal), THREE_TERMINI(['refused', 'refused']));

  const probe = modelOf(await recordOf('req_c41d0311-200/1'));
  assert.deepEqual(read(probe), ['Pin: match (taken)', 'Eligible: match (taken)', 'Probe: passed (taken)', 'spent: fired (taken)', ...FIVE_RULES('not run', 'idle').slice(1), '^Move: moved (taken)', '^Cool reading: preferred (taken)', '^Cap filter: passed (taken)', '^Partitions: match (taken)', '^content: none (taken)', '^with main: none (taken)', '^static: none (taken)', '^Pick: chosen (taken)', 'Renew: not run (idle)']);
  assert.deepEqual([probe.decisive, probe.host, probe.branch], ['pick', 'pick', { from: 'move:spent', kind: 'move' }]);

  const failover = modelOf(await recordOf('req_1d9c3e55-0b2/2'));
  assert.deepEqual(read(failover), [...FRESH_LINE('match', 'match'), 'content: none (taken)', 'with main: none (taken)', 'static: match (taken)', 'Pick: not run (idle)']);
  assert.deepEqual([failover.shape, failover.decisive, failover.host, failover.branch], ['fresh', 'rung:static', 'pick', { from: 'rung:static', kind: 'rung' }]);

  const overage = modelOf(await recordOf('req_4b0e9d17-c31/2'));
  assert.deepEqual(read(overage), [...FRESH_LINE('match', 'match'), 'content: none (taken)', 'with main: none (taken)', 'static: none (taken)', 'Pick: chosen (taken)']);
  assert.deepEqual([overage.decisive, overage.host, stationOf(overage, 'pick').second], ['pick', 'pick', ['Pace P0']]);
  assert.deepEqual(overage.leaves.map((l) => [l.accountId, l.chosen, l.tone]), [['primary', false, 'warning'], ['secondary', false, 'warning'], ['nightly', true, 'accent'], ['overflow', false, 'warning']]);

  assert.deepEqual(fresh.leaves.map((l) => [l.accountId, l.badge, l.chosen, l.tone]), [['primary', 'chosen', true, 'accent'], ['secondary', 'out', false, 'warning'], ['nightly', 'eligible', false, 'neutral'], ['overflow', 'out', false, 'warning']]);
  assert.deepEqual([d.stationName(stationOf(fresh, 'pick')), d.stationName(stationOf(shedMove, 'move:shed')), d.stationName(stationOf(staticPlaced, 'pick'))], ['Pick, chosen, Pace P0', 'Shed, fired', 'Pick, not run']);
  assert.deepEqual(fresh.termini.map(d.terminusName), ['Chosen, placed', 'Held for a slot, not reached', 'Refused, not reached']);
  assert.deepEqual(fresh.leaves.slice(0, 2).map(d.leafName), ['primary, chosen', `secondary, out, ${fresh.leaves[1].chip.value}`]);
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
  const m = modelOf(built(base, [step('pin', 'none'), step('eligible', 'match'), step('bound', 'bound', [fact('account_id', 'primary', null, 'primary')]), step('renew', 'renewed')], 'renewed'));
  assert.deepEqual(read(m), ['Pin: none (taken)', 'Eligible: match (taken)', 'Bound: bound (taken)', 'Renew: renewed (taken)']);
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
});

test('the session-pin cap exemption is decided by its hold after the pick', async () => {
  const base = await recordOf('req_ef099b34-453/1');
  const m = modelOf(built(base, [...base.walk, step('hold', 'placed', [fact('place', 'session_pin'), fact('account_id', 'primary', null, 'primary')])], 'placed'));
  assert.deepEqual(read(m).slice(-2), ['Pick: chosen (taken)', 'Hold: placed (taken)']);
  assert.deepEqual([m.decisive, m.host, stationOf(m, 'hold').second, stationOf(m, 'hold').tone], ['hold', 'pick', ['session pin'], 'good']);
  assert.deepEqual(terminiOf(m), THREE_TERMINI(['chosen', 'placed']));
});

test("the layout spaces by weight and keeps the surface's height", async () => {
  const freshModel = modelOf(await recordOf('req_ef099b34-453/1'));
  const fresh = d.layoutStrip(freshModel, 958);
  assert.ok(fresh.unit > 50 && fresh.unit < 60, `the unit is ${fresh.unit}`);
  assert.deepEqual([fresh.width, fresh.y0], [958, 104]);
  const xs = freshModel.stations.map((s) => fresh.stations[s.id].x);
  assert.ok(xs.every((x, i) => i === 0 || x > xs[i - 1]), 'every x increases');
  assert.ok(freshModel.stations.every((s) => fresh.stations[s.id].y === 104));
  const parts = freshModel.stations.filter((s) => s.id.startsWith('partitions:')).map((s) => fresh.stations[s.id].x);
  assert.ok(parts.every((x, i) => i === 0 || Math.abs(x - parts[i - 1] - fresh.unit) < 1e-9), 'the partitions sit one unit apart');
  assert.ok(Math.abs(fresh.stations.eligible.x - fresh.stations.pin.x - 1.45 * fresh.unit) < 1e-9, 'two stations of no group sit 1.45 units apart');
  const last = xs[xs.length - 1];
  assert.ok(Math.abs(fresh.termini.chosen.x - last - 1.9 * fresh.unit) < 1e-9, 'the termini sit 1.9 units past the last station');
  assert.deepEqual([fresh.termini.chosen.y, fresh.termini.held.y, fresh.termini.refused.y], [104, 140, 176]);
  assert.equal(fresh.height, Math.max(104 + 76 + freshModel.leaves.length * 30 + 4, 208));
  assert.ok(Object.values(fresh.labels).every((l) => l.width >= 58 && l.width <= 110));
  assert.deepEqual(Object.entries(fresh.labels).filter(([, l]) => l.above).map(([id]) => id), ['pick']);
  assert.deepEqual(fresh.leaves.map((p) => [p.x, p.y]), [0, 1, 2, 3].map((i) => [fresh.stations.pick.x - 16, 180 + i * 30]));
  const kinds = (layout) => layout.paths.reduce((n, p) => ({ ...n, [p.kind]: (n[p.kind] ?? 0) + 1 }), {});
  assert.deepEqual(kinds(fresh), { curve: 3, edge: 10, fan: 1, stub: 3, tick: 4 });
  assert.deepEqual(fresh.paths.filter((p) => p.kind === 'curve').map((p) => p.state), ['taken', 'idle', 'idle']);
  assert.deepEqual(fresh.paths.filter((p) => p.kind === 'tick').map((p) => p.state), ['taken', 'idle', 'idle', 'idle']);
  assert.deepEqual(fresh.brackets.map((b) => [b.word, b.y]), [['Partitions', 68], ['Rungs', 68]]);

  const narrow = d.layoutStrip(freshModel, 640);
  assert.ok(narrow.unit < fresh.unit);
  assert.ok([...Object.values(narrow.stations), ...Object.values(narrow.termini)].every((p) => p.x <= 640));

  const shedModel = modelOf(await recordOf('req_868264bc-611/1'));
  const shed = d.layoutStrip(shedModel, 958);
  assert.deepEqual([shed.y0, shed.yu], [176, 96]);
  assert.ok(shedModel.stations.filter((s) => s.level === 'track').every((s) => shed.stations[s.id].y === 96));
  assert.ok(shedModel.stations.filter((s) => s.level === 'line').every((s) => shed.stations[s.id].y === 176));
  assert.ok(Math.abs(shed.stations.move.x - shed.stations['move:shed'].x - 0.55 * shed.unit) < 1e-9, 'the path rises 0.55 units to the Move station');
  assert.ok(Math.abs(shed.stations.pick.x - shed.stations['rung:static'].x - 1.35 * shed.unit) < 1e-9, 'the pick sits 1.35 units past the rungs');
  assert.ok(Math.abs(shed.termini.chosen.x - shed.stations.pick.x - shed.unit) < 1e-9, 'the continuation ends one unit past its last station');
  assert.ok(shed.stations.renew.x > shed.stations['move:shed'].x && shed.stations.renew.x < shed.stations.pick.x, 'Renew stands between the fired rule and the host');
  assert.deepEqual(Object.keys(shed.termini), ['chosen']);
  assert.deepEqual(shed.paths.filter((p) => p.kind === 'track').map((p) => p.state), ['taken']);
  assert.ok(shedModel.stations.filter((s) => s.level === 'track').every((s) => shed.labels[s.id].above));
  assert.deepEqual(shed.brackets.map((b) => [b.word, b.y]), [['Moves', 140], ['Rungs', 34]]);
  assert.equal(shed.height, Math.max(176 + 76 + shedModel.leaves.length * 30 + 4, 280));

  const staticLayout = d.layoutStrip(modelOf(await recordOf('req_b2bed817-a58/1')), 958);
  assert.deepEqual([staticLayout.y0, staticLayout.yu], [128, 56]);
  assert.deepEqual(staticLayout.paths.filter((p) => p.kind === 'track').map((p) => p.state), ['taken']);
  assert.deepEqual(staticLayout.paths.filter((p) => p.kind === 'curve').map((p) => p.state), ['idle', 'idle', 'idle']);

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
});
