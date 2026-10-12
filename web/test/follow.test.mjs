import assert from 'node:assert/strict';
import { test } from 'node:test';

const d = await import('./dist/api.js');

const at = (s) => new Date(Date.UTC(2026, 8, 16, 14, 0, s)).toISOString();
const byId = (r) => r.id;
/** The route's order, newest first and the higher id first at one instant, as the Requests page compares. */
const newestFirst = (a, b) => Date.parse(b.at) - Date.parse(a.at) || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0);
/** Whether a row sorts past the last row a list shows, as the hook hands the poll its list's bottom. */
const belowOf = (shown) => (r) => shown.length > 0 && newestFirst(r, shown.at(-1)) > 0;

/** `count` rows a second apart, newest first, and a route over them: pages of 25, the cursor a row index. */
function route(count) {
  const rows = Array.from({ length: count }, (_, i) => ({ at: at(count - i), id: `row_${String(i).padStart(3, '0')}` }));
  const reads = [];
  const read = async (cursor) => {
    reads.push(cursor);
    const start = cursor === undefined ? 0 : Number(cursor);
    return { nextCursor: start + 25 < rows.length ? String(start + 25) : null, rows: rows.slice(start, start + 25) };
  };
  return { read, reads, rows };
}

test('a Follow poll stops at the first page that brings no row the list lacks, by the identity given', async () => {
  // The list shows rows 100 to 124, 100 arrived above them, and older rows lie below, so a poll that missed the known
  // rows would read on past them.
  const r = route(150);
  const shown = r.rows.slice(100, 125);
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown));
  assert.deepEqual(r.reads, [undefined, '25', '50', '75', '100']);
  assert.equal(polled.overrun, null);
  assert.deepEqual(polled.rows.map(byId), r.rows.slice(0, 125).map(byId));
});

test('a Follow poll reads eight pages every 5 seconds and names the cursor past the last when none holds a known row', async () => {
  const r = route(325);
  const shown = r.rows.slice(300);
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown));
  assert.equal(d.FOLLOW_MS, 5_000);
  assert.equal(d.FOLLOW_PAGES, 8);
  assert.equal(r.reads.length, d.FOLLOW_PAGES);
  assert.deepEqual(polled.rows.map(byId), r.rows.slice(0, 200).map(byId));
  assert.equal(polled.overrun, '200');
});

test('a Follow poll reads one page when the route has no more', async () => {
  const r = route(10);
  const polled = await d.followPages(r.read, new Set(), byId, belowOf([]));
  assert.deepEqual(r.reads, [undefined]);
  assert.equal(polled.overrun, null);
  assert.deepEqual(polled.rows.map(byId), r.rows.map(byId));
});

test("a merge keeps one row per identity, the polled row winning, in the order of the compare it is given", () => {
  const shown = [
    { at: at(3), id: 'a' },
    { at: at(2), id: 'b', read: 'shown' },
  ];
  const polled = [
    { at: at(4), id: 'c' },
    { at: at(2), id: 'b', read: 'polled' },
  ];
  // Oldest first, an order the routing log's compare never gives.
  const oldestFirst = (x, y) => Date.parse(x.at) - Date.parse(y.at);
  const rows = d.merged(shown, polled, byId, oldestFirst);
  assert.deepEqual(rows.map(byId), ['b', 'a', 'c']);
  assert.equal(rows[0].read, 'polled');
});

test('a Follow poll reads on past a page bringing a row below known ones, which the merge sets in place', async () => {
  // The request ledger writes a row when its request finishes, so a long request lands between rows the list already
  // shows: row 10 of the 50 shown, on the first page.
  const r = route(60);
  const shown = r.rows.slice(0, 50).filter((row) => row.id !== 'row_010');
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown));
  assert.deepEqual(r.reads, [undefined, '25'], 'the second page, which brings nothing new, ends the poll');
  assert.equal(polled.overrun, null);
  const rows = d.merged(shown, polled.rows, byId, newestFirst);
  assert.deepEqual(rows.map(byId), r.rows.slice(0, 50).map(byId), 'the late row sits between rows 9 and 11');
});

test("a Follow poll that reaches past the list's last row stops there and leaves the rows below to Next", async () => {
  // Two rows arrived over the 25 the list shows, so the second page holds the list's last two rows and then the rows
  // Next reads, none of them new to the list.
  const r = route(300);
  const shown = r.rows.slice(2, 27);
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown));
  assert.deepEqual(r.reads, [undefined, '25']);
  assert.equal(polled.overrun, null);
  assert.deepEqual(polled.rows.map(byId), r.rows.slice(0, 27).map(byId), 'no row past row 26 lands');
});
