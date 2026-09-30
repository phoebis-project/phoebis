import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

test("the Request shape row names the request's auto-mode standing", () => {
  assert.deepEqual(
    [d.autoModeLabel('server'), d.autoModeLabel('stripped'), d.autoModeLabel('body_shape'), d.autoModeLabel(null)],
    ['server-side check requested', "server-side check dropped by this key's setting", 'refused for its body shape', 'not requested'],
  );
});
