import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

test("clientVersion reads Claude Code's product token as its version, cut at the token's end, and none without a user agent", () => {
  assert.equal(d.clientVersion('claude-cli/2.1.310 (external, cli)'), 'Claude Code 2.1.310');
  assert.equal(d.clientVersion(null), null);
});
