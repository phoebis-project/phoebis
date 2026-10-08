import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

// An account's edit token carries its incarnation above a 32-bit revision.
const INCARNATION = 2 ** 32;

test("an operator's own save advances the other editor's token only from the token both opened on", () => {
  const sent = 3 * INCARNATION + 7;
  assert.equal(d.advancesOwnToken(sent, sent + 1, sent), true, 'both editors opened on the same row state');
  assert.equal(d.advancesOwnToken(sent, sent + 1, sent - 1), false, 'the other editor opened on an older row state and keeps its token');
  assert.equal(d.advancesOwnToken(sent, sent + 2, sent), false, 'another write landed beside the save');
  const top = 4 * INCARNATION - 1;
  assert.equal(d.advancesOwnToken(top, top + 1, top), false, 'one past the top of an incarnation is the next incarnation, which no save produces');
});
