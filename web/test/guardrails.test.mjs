import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as g from './dist/api.js';

const valid = { operator: 'equals', subject: 'request_class', value: 'auxiliary' };
const form = (patch = {}) => ({ conditions: [valid], description: '', id: 'fresh', name: 'Fresh', ...patch });
const texts = (refusals) => refusals.map((r) => r.text);

test('the_subjects_name_their_operators_and_cells', () => {
  const rows = Object.fromEntries(g.SUBJECTS.map((s) => [s.value, s]));
  assert.deepEqual(
    g.SUBJECTS.map((s) => s.label),
    ['Agent type', 'Body', 'Client', 'Header', 'Kind', 'Message', 'Model', 'Request class', 'User agent'],
    'the nine subjects, alphabetical by their label',
  );
  assert.deepEqual([rows.header.cell, rows.header.operators], ['header', ['absent', 'equals', 'matches', 'present']]);
  assert.deepEqual([rows.body.cell, rows.body.operators], ['path', ['absent', 'equals', 'gt', 'lt', 'matches', 'present']]);
  assert.deepEqual([rows.message.cell, rows.message.operators], ['selector', ['matches']], 'a message condition offers matches and no other operator');
  assert.deepEqual([rows.kind.cell, rows.kind.operators], ['none', ['equals']]);
  for (const subject of ['agent_type', 'request_class', 'user_agent']) assert.deepEqual([rows[subject].cell, rows[subject].operators], ['none', ['absent', 'equals', 'matches']], subject);
  for (const subject of ['client', 'model']) assert.deepEqual([rows[subject].cell, rows[subject].operators], ['none', ['equals', 'matches']], subject);
  assert.deepEqual(g.OPERATOR_LABEL, { absent: 'is absent', equals: 'equals', gt: 'is greater than', lt: 'is less than', matches: 'matches', present: 'is present' });
  assert.equal(g.SELECTOR_LABEL.last_user, 'last user message');
  assert.equal(Object.keys(g.SELECTOR_LABEL).length, 7);
  assert.deepEqual([g.MAX_CONDITIONS, g.MAX_PATTERN_CHARS], [16, 2048]);
});

test('a_condition_refusal_names_its_row_and_its_missing_cell', () => {
  assert.equal(g.conditionRefusal({ operator: 'present', subject: 'header' }, 1), 'Condition 2: needs a header name');
  assert.equal(g.conditionRefusal({ name: '', operator: 'present', subject: 'header' }, 1), 'Condition 2: needs a header name');
  assert.equal(g.conditionRefusal({ operator: 'present', subject: 'body' }, 1), 'Condition 2: needs a path');
  assert.equal(g.conditionRefusal({ operator: 'matches', subject: 'message', value: '^x' }, 1), 'Condition 2: needs a selector');
  assert.equal(g.conditionRefusal({ operator: 'equals', subject: 'model' }, 1), 'Condition 2: needs a value');
  assert.equal(g.conditionRefusal({ operator: 'gt', path: 'max_tokens', subject: 'body', value: 'many' }, 1), 'Condition 2: many is not a number');
  assert.equal(g.conditionRefusal({ operator: 'matches', subject: 'model', value: 'x'.repeat(2049) }, 1), 'Condition 2: the pattern is longer than 2048 characters');
  assert.equal(g.conditionRefusal({ operator: 'present', path: 'a..b', subject: 'body' }, 0), 'Condition 1: the path has an empty member at segment 2');
  assert.equal(g.conditionRefusal({ operator: 'matches', subject: 'model', value: 'x'.repeat(2048) }, 0), null, 'a pattern at the bound is the route\'s to compile');
  assert.equal(g.conditionRefusal({ operator: 'lt', path: 'max_tokens', subject: 'body', value: '1e3' }, 0), null);
  assert.equal(g.conditionRefusal({ operator: 'equals', subject: 'model', value: '' }, 0), null, 'an empty value on equals compares against the empty string');
});

test('the_path_grammar_refuses_what_the_router_refuses', () => {
  assert.equal(g.pathRefusal(''), 'the path is empty');
  assert.equal(g.pathRefusal(`a${'b'.repeat(256)}`), 'the path is longer than 256 characters');
  assert.equal(g.pathRefusal(Array.from({ length: 33 }, () => 'a').join('.')), 'the path holds more than 32 segments');
  assert.equal(g.pathRefusal('[0].a'), 'the path starts with an index');
  assert.equal(g.pathRefusal('a..b'), 'the path has an empty member at segment 2');
  assert.equal(g.pathRefusal('a[x]'), 'the path has a malformed index at segment 2');
  assert.equal(g.pathRefusal('a[0'), 'the path has an unclosed index at segment 2');
  for (const path of ['messages[*].content[*].type', 'tools[*].name', 'messages[-1].role', 'metadata.user_id', Array.from({ length: 32 }, () => 'a').join('.')]) assert.equal(g.pathRefusal(path), null, path);
});

test('the_form_refusals_read_their_six_sentences', () => {
  const taken = new Set(['large-auxiliary']);
  assert.deepEqual(texts(g.guardrailRefusals(form(), taken, false)), []);
  assert.deepEqual(texts(g.guardrailRefusals(form({ id: 'item', name: '' }), taken, false)), ['The guardrail needs a name.']);
  assert.deepEqual(texts(g.guardrailRefusals(form({ id: 'large-auxiliary', name: 'Large auxiliary' }), taken, false)), ['Another guardrail already has the id large-auxiliary.']);
  assert.deepEqual(texts(g.guardrailRefusals(form({ id: 'large-auxiliary', name: 'Large auxiliary' }), taken, true)), [], 'an edit keeps its own id');
  assert.deepEqual(texts(g.guardrailRefusals(form({ id: 'presets', name: 'Presets' }), taken, false)), ['The id presets is reserved.']);
  assert.deepEqual(texts(g.guardrailRefusals(form({ id: 'test', name: 'Test' }), taken, false)), ['The id test is reserved.']);
  // Two bytes a character, so the bound counts bytes rather than characters.
  assert.deepEqual(texts(g.guardrailRefusals(form({ name: 'é'.repeat(129) }), taken, false)), ['The name is at most 256 bytes.']);
  assert.deepEqual(texts(g.guardrailRefusals(form({ description: 'é'.repeat(129) }), taken, false)), ['The description is at most 256 bytes.']);
  assert.deepEqual(texts(g.guardrailRefusals(form({ conditions: [] }), taken, false)), ['A guardrail holds 1 to 16 conditions.']);
  assert.deepEqual(texts(g.guardrailRefusals(form({ conditions: Array.from({ length: 17 }, () => valid) }), taken, false)), ['A guardrail holds 1 to 16 conditions.']);
  const refused = g.guardrailRefusals(form({ conditions: [valid, { operator: 'present', subject: 'body' }] }), taken, false);
  assert.deepEqual(refused, [{ field: 'Condition 2', group: 'conditions', row: 1, text: 'Condition 2: needs a path' }], 'a row refusal holds the save on its own row');
  assert.deepEqual(g.guardrailRefusals(form({ name: '' }), taken, false)[0], { field: 'Name', group: 'identity', text: 'The guardrail needs a name.' });
});

test('the_derived_id_lowers_the_name_joins_its_words_with_hyphens_and_stops_at_64_characters', () => {
  // The id shape the route admits, so a derived id never meets the route's id refusal.
  const shape = /^[a-z0-9][a-z0-9-]{0,63}$/;
  const plain = g.derivedId('Subagent Status: Summary!');
  assert.deepEqual(plain, { id: 'subagent-status-summary', unnamed: false }, 'uppercase lowered, a run of spaces and punctuation one hyphen, none at either end');
  const long = g.derivedId(`Guardrail ${'x'.repeat(60)}`);
  assert.deepEqual(long, { id: `guardrail-${'x'.repeat(54)}`, unnamed: false }, 'a slug past 64 characters is cut at 64');
  for (const { id } of [plain, long]) assert.match(id, shape);
});

test('the_deleted_label', () => {
  const list = [{ id: 'large-auxiliary', name: 'Large auxiliary requests' }];
  assert.equal(g.guardrailLabel('ghost', list), 'ghost (deleted)');
  assert.equal(g.guardrailLabel('large-auxiliary', list), 'Large auxiliary requests');
  assert.equal(g.guardrailLabel('ghost', null), 'ghost', 'a list not yet read marks nothing deleted');
});

test('the_guardrail_options_list_the_registry_then_the_rows_deleted_ids_then_none', () => {
  const list = [
    { id: 'large-auxiliary', name: 'Large auxiliary requests' },
    { id: 'subagent-status-summary', name: 'Subagent status summary' },
  ];
  const rows = [{ guardrails: [{ blocked: false, id: 'zeta', position: 0 }] }, { guardrails: [] }, { guardrails: [{ blocked: true, id: 'large-auxiliary', position: 0 }, { blocked: false, id: 'alpha', position: 1 }] }];
  assert.deepEqual(g.guardrailOptions(list, rows, ['ghost']), [
    { label: 'Large auxiliary requests', value: 'large-auxiliary' },
    { label: 'Subagent status summary', value: 'subagent-status-summary' },
    { label: 'alpha (deleted)', value: 'alpha' },
    { label: 'ghost (deleted)', value: 'ghost' },
    { label: 'zeta (deleted)', value: 'zeta' },
    { label: 'none', value: '-' },
  ]);
});

test('the_verdict_words', () => {
  const row = { id: 'r', receivedAt: '2026-10-17T00:00:00.000Z' };
  assert.equal(g.verdictWords({ ...row, condition: null, verdict: 'matched' }), 'matched');
  assert.equal(g.verdictWords({ ...row, condition: 0, verdict: 'unmatched' }), 'failed at condition 1');
  assert.equal(g.verdictWords({ ...row, condition: 2, verdict: 'indeterminate' }), 'condition 3 needs the live header');
});

test('the_preset_badges_read_customized_and_update_available', () => {
  assert.deepEqual(g.presetBadges({ currentVersion: 2, customized: true, id: 'p', version: 1 }), ['customized', 'update available']);
  assert.deepEqual(g.presetBadges({ currentVersion: 1, customized: false, id: 'p', version: 1 }), []);
  assert.deepEqual(g.presetBadges({ currentVersion: 1, customized: true, id: 'p', version: 1 }), ['customized']);
  assert.deepEqual(g.presetBadges(null), []);
});
