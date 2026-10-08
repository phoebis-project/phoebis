import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const START = { message: { id: 'msg_1' }, type: 'message_start' };
const STOP = { type: 'message_stop' };
/** A two-frame capture, its lines ending in `eol`. */
const capture = (eol) => ['event: message_start', `data: ${JSON.stringify(START)}`, '', 'event: message_stop', `data: ${JSON.stringify(STOP)}`, '', ''].join(eol);
const TWO_FRAMES = [
  { data: JSON.stringify(START), event: 'message_start', json: START },
  { data: JSON.stringify(STOP), event: 'message_stop', json: STOP },
];

test('a stream capture splits into its frames with the event and the data joined', () => {
  assert.deepEqual(d.sseFrames(capture('\n')), TWO_FRAMES);
});

test('a CRLF stream and a bare-CR stream split into the same frames as the LF one', () => {
  const lf = d.sseFrames(capture('\n'));
  assert.deepEqual(d.sseFrames(capture('\r\n')), lf);
  assert.deepEqual(d.sseFrames(capture('\r')), lf);
});

test('a frame with two data lines joins them with a newline', () => {
  const frames = d.sseFrames('event: ping\ndata: first\ndata: second\n\n');
  assert.deepEqual(frames, [{ data: 'first\nsecond', event: 'ping', json: undefined }]);
});

test('a frame whose data is not JSON keeps the text', () => {
  const frames = d.sseFrames('event: note\ndata: not json at all\n\n');
  assert.equal(frames?.length, 1);
  assert.equal(frames?.[0].data, 'not json at all');
  assert.equal(frames?.[0].json, undefined);
});

test('a capture cut mid-frame keeps its last frame as the text it has', () => {
  const frames = d.sseFrames('event: message_start\ndata: {"type":"message_start"}\n\nevent: content_block_delta\ndata: {"type":"content_block_de');
  assert.equal(frames?.length, 2);
  assert.deepEqual(frames?.[1], { data: '{"type":"content_block_de', event: 'content_block_delta', json: undefined });
});

test('a capture cut inside a data prefix keeps its last frame as the text it has', () => {
  const frames = d.sseFrames('event: message_start\ndata: {"type":"message_start"}\n\nevent: content_block_delta\nda');
  assert.equal(frames?.length, 2);
  assert.deepEqual(frames?.[1], { data: 'da', event: 'content_block_delta', json: undefined });
});

test('plain text is not a stream', () => {
  assert.equal(d.sseFrames('line one\nline two'), null);
  assert.equal(d.sseFrames('event: a\ndata: {}\n\nplain\n'), null);
  assert.equal(d.sseFrames('upstream connect error'), null, 'one unterminated line with no field before it');
  assert.equal(d.sseFrames(''), null);
});

test("bodyText indents a stream's JSON data under its event line and leaves a plain string as itself", () => {
  assert.equal(d.bodyText(capture('\n'), false), 'event: message_start\n{\n  "message": {\n    "id": "msg_1"\n  },\n  "type": "message_start"\n}\n\nevent: message_stop\n{\n  "type": "message_stop"\n}');
  assert.equal(d.bodyText('line one\nline two', false), 'line one\nline two');
});

test("bodyText draws no event line for a data-only frame and drops a frame of comment lines alone", () => {
  assert.equal(d.bodyText(': ping\n\ndata: {"a":1}\n\nid: 7\n\n', false), '{\n  "a": 1\n}');
});

test("bodyText under compact returns a stream's stored text untouched", () => {
  const stored = capture('\r\n');
  const text = d.bodyText(stored, true);
  assert.equal(text, stored);
  assert.ok(!/\n {2}/.test(text), 'no indented line');
});

test('bodyText indents a JSON object over several lines and keeps it on one line under compact', () => {
  const value = { a: { b: 1 } };
  assert.equal(d.bodyText(value, false), '{\n  "a": {\n    "b": 1\n  }\n}');
  assert.equal(d.bodyText(value, true), '{"a":{"b":1}}');
});

test("the highlighter classes a nested object's keys, strings, numbers and literals and concatenates back", () => {
  const text = '{\n  "name": "x",\n  "inner": {\n    "n": 12.5,\n    "on": true,\n    "off": false,\n    "none": null\n  }\n}';
  const tokens = d.highlightJson(text);
  assert.equal(tokens.map((t) => t.text).join(''), text);
  const of = (cls) => tokens.filter((t) => t.cls === cls).map((t) => t.text);
  assert.deepEqual(of('tok-key'), ['"name"', '"inner"', '"n"', '"on"', '"off"', '"none"']);
  assert.deepEqual(of('tok-string'), ['"x"']);
  assert.deepEqual(of('tok-number'), ['12.5']);
  assert.deepEqual(of('tok-literal'), ['true', 'false', 'null']);
  assert.deepEqual(of('tok-punct').join(''), '{:,:{:,:,:,:}}');
  assert.ok(tokens.every((t) => t.text.length > 0), 'no empty token');
});

test('an escaped quote stays inside its string', () => {
  const tokens = d.highlightJson('{"k": "a \\"quoted\\" word", "m": 1}');
  assert.deepEqual(tokens.filter((t) => t.cls === 'tok-string').map((t) => t.text), ['"a \\"quoted\\" word"']);
  assert.deepEqual(tokens.filter((t) => t.cls === 'tok-key').map((t) => t.text), ['"k"', '"m"']);
});

test('a string the cut left open runs to the text end as one string token', () => {
  const text = '{"text":"abc';
  const tokens = d.highlightJson(text);
  assert.equal(tokens.map((t) => t.text).join(''), text);
  assert.deepEqual(tokens.at(-1), { cls: 'tok-string', text: '"abc' });
});

test('a negative exponent number is one token', () => {
  const tokens = d.highlightJson('[-1.5e-10, 3]');
  assert.deepEqual(tokens.filter((t) => t.cls === 'tok-number').map((t) => t.text), ['-1.5e-10', '3']);
});

test('an event line is one token and its data is highlighted as JSON', () => {
  const text = 'event: message_start\n{\n  "type": "message_start"\n}';
  const tokens = d.highlightJson(text);
  assert.equal(tokens.map((t) => t.text).join(''), text);
  assert.deepEqual(tokens.filter((t) => t.cls === 'tok-event').map((t) => t.text), ['event: message_start']);
  assert.deepEqual(tokens.filter((t) => t.cls === 'tok-key').map((t) => t.text), ['"type"']);
  assert.deepEqual(tokens.filter((t) => t.cls === 'tok-string').map((t) => t.text), ['"message_start"']);
});
