import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const list = (over = {}) => ({ field: 'accounts', group: 'Routing', kind: 'list', label: 'Account', word: 'account', ...over });
const ACCOUNTS = [
  { label: 'primary', value: 'acc_1' },
  { label: 'nightly', value: 'acc_2' },
  { label: 'backup', value: 'acc_3' },
  { label: 'spare', value: 'acc_4' },
];

test('a list chip prints up to three option labels then +N, every label in its title, and a value with no option as itself', () => {
  assert.deepEqual(d.chipText(list(), ['acc_1', 'acc_2'], ACCOUNTS), { text: 'account: primary, nightly', title: 'primary, nightly', values: 'primary, nightly' });
  assert.deepEqual(d.chipText(list(), ['acc_1', 'acc_2', 'acc_3', 'acc_4'], ACCOUNTS), { text: 'account: primary, nightly, backup +1', title: 'primary, nightly, backup, spare', values: 'primary, nightly, backup +1' });
  assert.deepEqual(d.chipText(list({ field: 'keys', label: 'Key', word: 'key' }), ['laptop@3'], []), { text: 'key: laptop@3', title: 'laptop@3', values: 'laptop@3' }, 'an earlier holder no option names prints itself');
});

test('a pattern chip prints the pattern, an id chip its first eight characters and an ellipsis with the whole id in the title', () => {
  const pattern = { field: 'agent', group: 'Request', kind: 'pattern', label: 'User agent', mono: true, word: 'user agent' };
  assert.deepEqual(d.chipText(pattern, 'claude-cli/2.1.*'), { text: 'user agent: claude-cli/2.1.*', title: 'claude-cli/2.1.*', values: 'claude-cli/2.1.*' });
  const id = { field: 'session', group: 'Subject', kind: 'id', label: 'Session', mono: true, word: 'session' };
  const whole = 'd7167642-3f1a-4c2e-9b8d-5e6f7a8b9c0d';
  assert.equal(whole.length, 36);
  assert.deepEqual(d.chipText(id, whole), { text: 'session: d7167642…', title: whole, values: 'd7167642…' });
});

test('a choice chip prints its option label, a flag its word alone with no values, and a choice at its default draws no chip', () => {
  const kind = { anyLabel: 'Any kind', field: 'kind', group: 'Request', kind: 'choice', label: 'Kind', word: 'kind' };
  assert.deepEqual(d.chipText(kind, 'inference', [{ label: 'Inference', value: 'inference' }]), { text: 'kind: Inference', title: 'Inference', values: 'Inference' });
  const held = { field: 'held', group: 'Outcome', kind: 'flag', label: 'Held only', word: 'held only' };
  assert.deepEqual(d.chipText(held, true), { text: 'held only', title: 'held only', values: null }, 'a flag has no values');
  const status = d.KEYS_DIMENSIONS.find((x) => x.field === 'status');
  const options = { status: [{ label: 'Disabled', value: 'disabled' }] };
  assert.deepEqual(d.chipsOf(d.KEYS_DIMENSIONS, { pools: [], search: '', status: 'all' }, options), [], 'the Keys status at all draws no chip');
  assert.deepEqual(d.chipsOf(d.KEYS_DIMENSIONS, { pools: [], search: '', status: 'disabled' }, options), [{ field: 'status', label: 'status', mono: false, text: 'status: Disabled', title: 'Disabled', values: 'Disabled' }]);
  assert.deepEqual([d.hasValue(status, 'all'), d.hasValue(status, 'disabled'), d.hasValue(kind, ''), d.hasValue(held, false), d.hasValue(list(), [])], [false, true, false, false, false]);
});

test("the Keys Pool filter draws a chip per id in the address's order, each carrying its id, while every other pool filter draws one chip", () => {
  const options = { pools: [{ label: 'Batch jobs', value: 'jobs' }], status: [{ label: 'Disabled', value: 'disabled' }] };
  assert.deepEqual(d.chipsOf(d.KEYS_DIMENSIONS, { pools: ['jobs', 'nope'], search: '', status: 'disabled' }, options), [
    { field: 'pools', label: 'pool', mono: true, text: 'pool: Batch jobs', title: 'Batch jobs', value: 'jobs', values: 'Batch jobs' },
    { field: 'pools', label: 'pool', mono: true, text: 'pool: nope', title: 'nope', value: 'nope', values: 'nope' },
    { field: 'status', label: 'status', mono: false, text: 'status: Disabled', title: 'Disabled', values: 'Disabled' },
  ]);
  assert.deepEqual(d.chipsOf(d.KEYS_DIMENSIONS, { pools: ['jobs', 'jobs'], search: '', status: 'all' }, options).map((c) => c.text), ['pool: Batch jobs'], 'an id the address repeats draws one chip');
  assert.deepEqual(d.chipsOf(d.REQUESTS_DIMENSIONS, { pools: ['jobs', 'nope'] }, options), [{ field: 'pools', label: 'pool', mono: true, text: 'pool: Batch jobs, nope', title: 'Batch jobs, nope', values: 'Batch jobs, nope' }]);
});

test("a per-value chip's x drops its own value, the last one emptying the list, and any other chip's x writes the dimension's empty value", () => {
  const pool = d.KEYS_DIMENSIONS.find((x) => x.field === 'pools');
  const status = d.KEYS_DIMENSIONS.find((x) => x.field === 'status');
  assert.deepEqual(d.withoutValue(pool, { pools: ['jobs', 'nope'] }, 'nope'), ['jobs']);
  assert.deepEqual(d.withoutValue(pool, { pools: ['jobs'] }, 'jobs'), []);
  assert.deepEqual(d.withoutValue(pool, { pools: ['jobs', 'nope'] }), [], 'a remove with no value clears the list');
  assert.equal(d.withoutValue(status, { status: 'disabled' }), 'all');
});

test('the status chip prints the codes alone, with the labels in its title', () => {
  const options = { statuses: [{ label: '429 Rate limited', value: '429' }, { label: '500 Server error', value: '500' }] };
  const [chip] = d.chipsOf(d.REQUESTS_DIMENSIONS, { statuses: ['429', '500'] }, options);
  assert.deepEqual(chip, { field: 'statuses', label: 'status', mono: false, text: 'status: 429, 500', title: '429 Rate limited, 500 Server error', values: '429, 500' });
});

test('the Stop reason and Refusal category chips print their option labels, none for the dash, in the Outcome group', () => {
  assert.deepEqual(
    d.REQUESTS_DIMENSIONS.filter((x) => x.group === 'Outcome').map((x) => x.label),
    ['Cache miss reason', 'Guardrail', 'Held only', 'Outcome', 'Refusal category', 'Status', 'Stop reason'],
    'the group in alphabetical order',
  );
  const options = { refusalCategories: [{ label: 'cyber', value: 'cyber' }, { label: 'none', value: '-' }], stopReasons: [{ label: 'refusal', value: 'refusal' }, { label: 'none', value: '-' }] };
  assert.deepEqual(d.chipsOf(d.REQUESTS_DIMENSIONS, { refusalCategories: ['cyber'], stopReasons: ['refusal', '-'] }, options), [
    { field: 'refusalCategories', label: 'refusal category', mono: true, text: 'refusal category: cyber', title: 'cyber', values: 'cyber' },
    { field: 'stopReasons', label: 'stop reason', mono: true, text: 'stop reason: refusal, none', title: 'refusal, none', values: 'refusal, none' },
  ]);
});

test("a dimension's own text supplies the values the chip carries, whatever its text reads", () => {
  const kinds = { field: 'kinds', group: 'Kinds', kind: 'list', label: 'Kinds', text: (value) => ({ text: value.join(', '), title: value.join(', '), values: value.join(', ') }), word: 'kinds' };
  assert.deepEqual(d.chipsOf([kinds], { kinds: ['moved', 'held'] }, {}), [{ field: 'kinds', label: 'kinds', mono: false, text: 'moved, held', title: 'moved, held', values: 'moved, held' }]);
});

test("the chips stand in the dimension list's order whatever the filters' key order", () => {
  const filters = { speeds: ['fast'], accounts: ['acc_1'], held: true, client: 'claude-code*', statuses: ['429'] };
  const chips = d.chipsOf(d.REQUESTS_DIMENSIONS, filters, { accounts: ACCOUNTS });
  assert.deepEqual(chips.map((c) => c.text), ['account: primary', 'held only', 'status: 429', 'client: claude-code*', 'speed: fast']);
  assert.deepEqual(chips.map((c) => [c.label, c.mono]), [['account', false], ['held only', false], ['status', false], ['client', true], ['speed', true]]);
});

test('the fold keeps five chips at most, fewer when they do not fit, one when none fits, and names the folded words', () => {
  const chips = ['account', 'key', 'model', 'pool', 'status', 'effort', 'speed'].map((word) => ({ field: word, label: word, mono: false, text: `${word}: x`, title: 'x', values: 'x' }));
  const five = d.foldChips(chips, () => true);
  assert.deepEqual([five.shown.length, five.folded.map((c) => c.label), five.title], [5, ['effort', 'speed'], 'Folded: effort, speed']);
  const two = d.foldChips(chips, (count) => count <= 2);
  assert.deepEqual([two.shown.map((c) => c.label), two.folded.length, two.title], [['account', 'key'], 5, 'Folded: model, pool, status, effort, speed']);
  const one = d.foldChips(chips, () => false);
  assert.deepEqual([one.shown.map((c) => c.label), one.folded.length], [['account'], 6]);
  const all = d.foldChips(chips.slice(0, 3), () => true);
  assert.deepEqual([all.shown.length, all.folded.length, all.title], [3, 0, '']);
});

test("the fold's title names each folded field once, in the chips' order", () => {
  const chip = (label, value) => ({ field: label, label, mono: false, text: `${label}: ${value}`, title: value, value, values: value });
  const chips = [chip('account', 'primary'), chip('pool', 'jobs'), chip('pool', 'nope'), chip('status', 'disabled')];
  const one = d.foldChips(chips, (count) => count <= 1);
  assert.deepEqual([one.folded.length, one.title], [3, 'Folded: pool, status'], 'the two per-value pool chips name their field once');
});

test('each dimension list names keys of its codec, and every codec key but the exempt ones has a dimension', () => {
  const views = [
    ['requests', d.REQUESTS_DIMENSIONS, d.REQUESTS_CODEC],
    ['routingLog', d.ROUTING_LOG_DIMENSIONS, d.ROUTING_LOG_CODEC],
    ['analytics', d.ANALYTICS_DIMENSIONS, d.ANALYTICS_CODEC],
    ['keys', d.KEYS_DIMENSIONS, d.KEYS_CODEC],
    ['accounts', d.ACCOUNTS_DIMENSIONS, d.ACCOUNTS_CODEC],
    ['sessions', d.SESSIONS_DIMENSIONS, d.SESSIONS_CODEC],
  ];
  for (const [view, dimensions, codec] of views) {
    const keys = Object.keys(codec.fromParams(new URLSearchParams()));
    const fields = dimensions.map((x) => x.field);
    assert.deepEqual(fields.filter((f) => !keys.includes(f)), [], `${view}: every dimension names a codec key`);
    assert.deepEqual(keys.filter((k) => !fields.includes(k) && !d.NOT_DIMENSIONS[view].includes(k)), [], `${view}: every codec key outside the exempt ones has a dimension`);
    assert.equal(new Set(fields).size, fields.length, `${view}: no key has two dimensions`);
  }
});

test("the Accounts view's sort key and its direction draw no chip, since the bar's sort control holds both", () => {
  assert.deepEqual(d.NOT_DIMENSIONS.accounts, ['dir', 'range', 'rows', 'search', 'sort']);
});

test("the lists are alphabetical inside each group, the groups in the view's order, and the Routing log leads with its kinds", () => {
  const groups = (dimensions) => [...new Set(dimensions.map((x) => x.group))];
  assert.deepEqual(groups(d.REQUESTS_DIMENSIONS), ['Routing', 'Outcome', 'Request', 'Subject']);
  assert.deepEqual(groups(d.ROUTING_LOG_DIMENSIONS), ['Kinds', 'Routing', 'Subject']);
  assert.deepEqual(groups(d.ANALYTICS_DIMENSIONS), ['Routing', 'Request', 'Subject']);
  assert.deepEqual(groups(d.KEYS_DIMENSIONS), ['Keys']);
  assert.deepEqual(groups(d.ACCOUNTS_DIMENSIONS), ['Accounts']);
  assert.deepEqual(groups(d.SESSIONS_DIMENSIONS), ['Sessions']);
  for (const dimensions of [d.REQUESTS_DIMENSIONS, d.ROUTING_LOG_DIMENSIONS, d.ANALYTICS_DIMENSIONS, d.KEYS_DIMENSIONS, d.ACCOUNTS_DIMENSIONS, d.SESSIONS_DIMENSIONS]) {
    for (const group of groups(dimensions)) {
      const labels = dimensions.filter((x) => x.group === group).map((x) => x.label);
      assert.deepEqual(labels, [...labels].sort((a, b) => a.localeCompare(b)), `${group} is alphabetical`);
    }
  }
  const words = Object.fromEntries(d.ANALYTICS_DIMENSIONS.map((x) => [x.field, x.word]));
  assert.deepEqual([words.clients, words.probes, words.userAgents], ['client', 'include probes', 'user agent'], 'the Analytics client patterns read client over their clients key');
  assert.equal(d.ROUTING_LOG_DIMENSIONS[0].word, 'kinds');
});

test('the_guardrail_dimension_sits_under_outcome_and_its_chip_reads_the_names', () => {
  const fields = d.REQUESTS_DIMENSIONS.map((x) => x.field);
  assert.deepEqual(d.REQUESTS_DIMENSIONS.find((x) => x.field === 'guardrails'), { field: 'guardrails', group: 'Outcome', kind: 'list', label: 'Guardrail', word: 'guardrail' });
  assert.deepEqual(fields.slice(fields.indexOf('cacheMissReasons'), fields.indexOf('held') + 1), ['cacheMissReasons', 'guardrails', 'held'], 'between Cache miss reason and Held only');
  // The registry's guardrail by its name, a loaded row's id the registry lacks as deleted, and none for a row no
  // guardrail matched.
  const options = d.guardrailOptions([{ id: 'large-auxiliary', name: 'Large auxiliary requests' }], [{ guardrails: [{ blocked: false, id: 'gone', position: 0 }] }], []);
  const [chip] = d.chipsOf(d.REQUESTS_DIMENSIONS, { guardrails: ['large-auxiliary', 'gone', '-'] }, { guardrails: options });
  const words = 'Large auxiliary requests, gone (deleted), none';
  assert.deepEqual(chip, { field: 'guardrails', label: 'guardrail', mono: false, text: `guardrail: ${words}`, title: words, values: words });
});

test('the_classifier_facet_chips_the_value_and_names_none_for_the_dash', () => {
  const fields = d.REQUESTS_DIMENSIONS.map((x) => x.field);
  assert.deepEqual(fields.slice(fields.indexOf('agentTypes'), fields.indexOf('client') + 1), ['agentTypes', 'classifiers', 'client'], 'between Agent type and Client');
  const options = { classifiers: [{ label: 'client classifier', value: 'client' }, { label: 'none', value: '-' }] };
  const chipOf = (classifiers) => d.chipsOf(d.REQUESTS_DIMENSIONS, { classifiers }, options)[0];
  assert.deepEqual(chipOf(['client']), { field: 'classifiers', label: 'classifier', mono: false, text: 'classifier: client', title: 'client classifier', values: 'client' });
  assert.deepEqual(chipOf(['-']), { field: 'classifiers', label: 'classifier', mono: false, text: 'classifier: none', title: 'none', values: 'none' });
  assert.deepEqual(chipOf(['client', '-']), { field: 'classifiers', label: 'classifier', mono: false, text: 'classifier: client, none', title: 'client classifier, none', values: 'client, none' });
});

test("a dimension whose list did not load carries the failure's words, and a list that loaded or is on its way carries none", () => {
  assert.deepEqual(d.notLoadedReason('keys', 'key', { data: null, error: 'the store is unavailable' }), { keys: 'The key list did not load: the store is unavailable' });
  assert.deepEqual(d.notLoadedReason('keys', 'key', { data: null, error: null }), {});
  assert.deepEqual(d.notLoadedReason('pools', 'pool', { data: [], error: null }), {});
  assert.deepEqual(d.notLoadedReason('models', 'model', { data: [{ id: 'm' }], error: 'a refresh failed' }), {}, 'a refresh that fails keeps the list it read');
});

test('every kind belongs to exactly one preset and a preset is exactly its members', () => {
  const members = d.KIND_PRESETS.flatMap((p) => p.members);
  assert.equal(d.ALL_KINDS.length, 44);
  assert.equal(new Set(members).size, members.length, 'no kind stands in two presets');
  assert.deepEqual([...members].sort(), [...d.ALL_KINDS].sort(), 'the presets together are every kind the Kinds list offers');
  assert.deepEqual(
    d.KIND_PRESETS.map((p) => [p.label, p.members.length]),
    [
      ['placements and pins', 7],
      ['moves and holds', 7],
      ['exhaustion and refusals', 5],
      ['account state and windows', 13],
      ['cooldowns and backoff', 4],
      ['keepalive', 2],
      ['registry changes', 4],
      ['router life cycle', 2],
    ],
  );
  assert.deepEqual(d.KIND_PRESETS.find((p) => p.label === 'keepalive').members, ['probe', 'warm_prefix_expired']);
  assert.deepEqual([d.kindWord('probe'), d.kindWord('moved'), d.kindWord('pin_migrated')], ['probe decisions', 'moved', 'pin moved']);
  // A booking the request's end let go belongs to the placement or the move that made it, and a mark to its round.
  assert.deepEqual(['reservation_expired', 'mark_cleared'].map((k) => d.KIND_PRESETS.find((p) => p.members.includes(k))?.label), ['placements and pins', 'moves and holds']);
});

test('the Kinds chip names a whole preset, a union of whole presets in the table\'s order, else the kinds\' words three then +N, and nothing for every kind', () => {
  assert.deepEqual(d.kindsChipText(['shedding_round', 'mark_cleared', 'slow_down_ended', 'moved', 'wind_down_started', 'slow_down_started', 'held']), {
    text: 'kinds: moves and holds',
    title: 'shedding round, mark cleared, slow-down ended, moved, wind-down started, slow-down started, held',
    values: 'moves and holds',
  });
  assert.deepEqual(d.KIND_PRESETS.find((p) => p.label === 'moves and holds').members.slice(-3), ['wind_down_started', 'slow_down_started', 'slow_down_ended'], 'the slow-down starts and ends after the wind-down');
  assert.deepEqual(d.kindsChipText(['cooldown_set', 'cooldown_cleared', 'poll_backoff_started', 'poll_backoff_ended', 'moved', 'held', 'shedding_round', 'mark_cleared', 'wind_down_started', 'slow_down_started', 'slow_down_ended']), {
    text: 'kinds: moves and holds, cooldowns and backoff',
    title: 'cooldown set, cooldown cleared, poll backoff started, poll backoff ended, moved, held, shedding round, mark cleared, wind-down started, slow-down started, slow-down ended',
    values: 'moves and holds, cooldowns and backoff',
  });
  // The preset's five kinds before the slow-down's two are a hand-picked set now.
  assert.equal(d.kindsChipText(['moved', 'held', 'shedding_round', 'mark_cleared', 'wind_down_started']).text, 'kinds: moved, held, shedding round +2');
  assert.deepEqual(d.kindsChipText(['moved', 'held', 'placed', 'probe', 'cooldown_set']), {
    text: 'kinds: moved, held, placed +2',
    title: 'moved, held, placed, probe decisions, cooldown set',
    values: 'moved, held, placed +2',
  });
  const nothing = { text: '', title: '', values: '' };
  assert.deepEqual([d.kindsChipText(d.ALL_KINDS), d.kindsChipText([...d.ALL_KINDS].reverse()), d.kindsChipText([])], [nothing, nothing, nothing]);
});

test('the Kinds dimension reads its chip through the presets, and the Model dimension opens on its search field', () => {
  const kinds = d.ROUTING_LOG_DIMENSIONS.find((x) => x.field === 'kinds');
  assert.deepEqual(d.chipsOf(d.ROUTING_LOG_DIMENSIONS, { kinds: ['probe', 'warm_prefix_expired'] }, {}), [{ field: 'kinds', label: 'kinds', mono: false, text: 'kinds: keepalive', title: 'probe decisions, warm prefix expired', values: 'keepalive' }]);
  assert.equal(kinds.emptyMeansAll, true, 'an empty Kinds value is every kind');
  assert.deepEqual([d.REQUESTS_DIMENSIONS, d.ROUTING_LOG_DIMENSIONS].map((list) => list.find((x) => x.field === 'models').searchable), [true, true]);
});

// Last in the file, since the DOM globals it sets for React stay set for the rest of the process.
test("a per-value chip opened from the bar reads expanded on its own body alone, not on its field's other chips", async (t) => {
  // React reads whether a DOM exists when its module loads, so the window stands before the bundle that draws is
  // imported.
  const { JSDOM } = await import('jsdom');
  const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
  for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
    Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
  }
  globalThis.window = window;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  t.after(() => window.close());
  const { act, createElement: h, createRoot, FilterBar } = await import('./dist/ui.js');
  const options = { pools: [{ label: 'Batch jobs', value: 'jobs' }, { label: 'Nightly', value: 'night' }], status: [{ label: 'Disabled', value: 'disabled' }] };
  const refresh = { intervalMs: 0, onIntervalChange() {}, onRefresh() {}, paused: null, refreshTitle: 'Refresh' };
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  act(() => root.render(h(FilterBar, { address: 'pools', dimensions: d.KEYS_DIMENSIONS, filters: { pools: ['jobs', 'night'], search: '', status: 'all' }, options, refresh, update() {} })));
  try {
    const expanded = () => [...container.querySelectorAll('.chip--filter .chip__body')].map((b) => b.getAttribute('aria-expanded'));
    assert.deepEqual(expanded(), ['false', 'false'], 'two pool chips, both closed');
    act(() => container.querySelectorAll('.chip--filter .chip__body')[1].click());
    assert.deepEqual(expanded(), ['false', 'true'], 'the chip the step opened from alone reads expanded');
  } finally {
    act(() => root.unmount());
  }
});
