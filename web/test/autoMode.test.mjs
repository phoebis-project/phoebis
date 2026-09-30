import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

test("the Request shape row names the request's auto-mode standing", () => {
  assert.deepEqual(
    [d.autoModeLabel('server'), d.autoModeLabel('stripped'), d.autoModeLabel('body_shape'), d.autoModeLabel(null)],
    [
      'asked for a server-side check under an on setting',
      'carried the safeguards member under an off setting, which the dispatch drops',
      'refused for its body shape',
      'not requested',
    ],
  );
});
