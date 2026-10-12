// The API reference page's browser smoke: the built page in headless Chromium over a static server and a stub
// document, or over a running router when PHOEBIS_E2E_BASE names one. It asserts what the page must do (load
// without an error or a policy violation, list the document, take a bearer and send it, follow the color scheme)
// and what it must not (leave the origin, store the token).
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';
import { serve } from './serve.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const token = process.env.PHOEBIS_E2E_SECRET ?? 'smoke-token-never-a-real-secret';

// The ui crate's policy, pinned here as the crate's own test pins it, so a Scalar release that needs a directive
// the constant lacks fails this smoke rather than loosening the console's policy.
const SECURITY_HEADERS = {
  'content-security-policy':
    "base-uri 'none'; connect-src 'self'; default-src 'self'; form-action 'self'; frame-ancestors 'none'; img-src 'self' data:; object-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'",
  'referrer-policy': 'no-referrer',
  'x-content-type-options': 'nosniff',
};

// Three operations under two tags, the bearer scheme on each of them: enough for the sidebar, the Authorization input
// and one call. A test file, never the real document's source of truth. The summaries are the real document's, period
// included, so the stub and the router render the same text.
const STUB = {
  openapi: '3.1.0',
  info: { title: 'Stub admin API', version: '0.0.0' },
  servers: [{ url: '/' }],
  tags: [{ name: 'Accounts' }, { name: 'Session' }],
  security: [{ bearer: [] }],
  components: { securitySchemes: { bearer: { type: 'http', scheme: 'bearer' } } },
  paths: {
    '/admin/session': {
      get: {
        operationId: 'session_show',
        tags: ['Session'],
        summary: 'Whether the cookie the console holds still names a session.',
        responses: { 200: { description: 'The session', content: { 'application/json': { schema: { type: 'object' } } } } },
      },
    },
    '/admin/accounts': {
      get: { operationId: 'accounts_list', tags: ['Accounts'], summary: 'List the accounts.', responses: { 200: { description: 'The accounts' } } },
    },
    '/admin/accounts/{id}': {
      get: { operationId: 'account_show', tags: ['Accounts'], summary: 'Show an account.', parameters: [{ name: 'id', in: 'path', required: true, schema: { type: 'string' } }], responses: { 200: { description: 'The account' } } },
    },
  },
};

const json = (body) => ({ status: 200, headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
let base = process.env.PHOEBIS_E2E_BASE;
let server = null;
let authorizationSeen = null;
if (!base) {
  const dist = join(here, '..', 'dist');
  if (!existsSync(join(dist, 'api-reference', 'index.html'))) {
    console.error(`no reference page under ${dist}; run npm run build first`);
    process.exit(2);
  }
  server = await serve(
    dist,
    {
      '/api-reference/openapi.json': () => json(STUB),
      '/admin/session': (req) => {
        authorizationSeen = req.headers.authorization ?? null;
        return json({ session_id: 'bearer' });
      },
    },
    SECURITY_HEADERS,
  );
  base = `http://127.0.0.1:${server.port}`;
}
const origin = new URL(base).origin;

const failures = [];
const check = (ok, what) => {
  if (ok) console.log(`ok ${what}`);
  else {
    console.error(`FAIL ${what}`);
    failures.push(what);
  }
};

const browser = await chromium.launch();
for (const colorScheme of ['light', 'dark']) {
  const context = await browser.newContext({ colorScheme });
  const page = await context.newPage();
  const requests = [];
  const errors = [];
  page.on('request', (request) => requests.push(request.url()));
  page.on('console', (message) => message.type() === 'error' && errors.push(message.text()));
  page.on('pageerror', (error) => errors.push(String(error)));
  await page.goto(`${base}/api-reference/`, { waitUntil: 'networkidle' });

  const title = await page.title();
  check(title.startsWith('Phoebis Router API reference'), `${colorScheme}: the title starts with the shell's (${title})`);
  const sidebar = page.getByRole('complementary', { name: /^Sidebar for / });
  await sidebar.getByRole('link', { name: 'Session', exact: true }).waitFor();
  check(await sidebar.getByRole('link', { name: 'Accounts', exact: true }).isVisible(), `${colorScheme}: the sidebar lists the tags`);
  check(await sidebar.getByRole('link', { name: 'List the accounts' }).isVisible(), `${colorScheme}: the sidebar lists the operations`);
  // Each of these opens a Scalar-hosted service (the agent chat, the developer tools' sharing and deploying, the
  // hosted client, the MCP generator and its editor installs), so the page renders none of them.
  const offOrigin = {
    'Ask AI': page.getByRole('button', { name: 'Ask AI' }),
    'Ask AI Agent': page.getByPlaceholder('Ask AI Agent'),
    Configure: page.getByText('Configure', { exact: true }),
    Cursor: page.getByText('Cursor', { exact: true }),
    Deploy: page.getByText('Deploy', { exact: true }),
    'Developer Tools': page.getByText('Developer Tools', { exact: true }),
    'Generate MCP': page.getByText('Generate MCP', { exact: true }),
    'Open API Client': page.getByText('Open API Client', { exact: true }),
    Share: page.getByText('Share', { exact: true }),
    'VS Code': page.getByText('VS Code', { exact: true }),
  };
  for (const [name, locator] of Object.entries(offOrigin)) check((await locator.count()) === 0, `${colorScheme}: no ${name} control`);
  const authorization = page.getByPlaceholder('Token');
  await authorization.waitFor();
  check((await authorization.inputValue()) === '', `${colorScheme}: the Authorization input is empty`);
  await authorization.fill(token);

  // Scalar draws a Test Request button for every operation in the document's tag order, and a collapsed tag
  // draws none, so the session operation is opened from the sidebar and its button is taken by its name.
  await sidebar.getByRole('link', { name: 'Session', exact: true }).click();
  await sidebar.getByRole('link', { name: 'Whether the cookie the console holds still names a session' }).click();
  await page.getByRole('button', { name: 'Test Request (get /admin/session)' }).click();
  const client = page.getByRole('dialog', { name: 'API Client' });
  // The page's own text already holds "session_id" and "bearer" (the example, the description, the scheme's
  // name), so the call is proven from the response the browser received and the request it sent.
  const answered = page.waitForResponse((response) => /\/admin\/session$/.test(new URL(response.url()).pathname));
  await client.getByRole('button', { name: /^Send get request to / }).waitFor();
  const send = client.getByRole('button', { name: `Send get request to ${origin}/admin/session`, exact: true });
  check((await send.count()) === 1, `${colorScheme}: the client sends to ${origin}/admin/session`);
  await send.click();
  const response = await answered;
  check(response.status() === 200, `${colorScheme}: GET /admin/session answered 200 (${response.status()})`);
  const answer = await response.json().catch(() => null);
  check(answer?.session_id === 'bearer', `${colorScheme}: the session answered bearer (${JSON.stringify(answer)})`);
  check(response.request().headers().authorization === `Bearer ${token}`, `${colorScheme}: the request carried the typed bearer`);
  if (server) check(authorizationSeen === `Bearer ${token}`, `${colorScheme}: the stub saw the bearer`);
  const shown = client.getByRole('region', { name: 'Response' });
  await shown.getByText('"session_id"').waitFor();
  check(await shown.getByText('"bearer"').isVisible(), `${colorScheme}: the client shows the answer`);
  check((await client.getByText('Open API Client', { exact: true }).count()) === 0, `${colorScheme}: the client links to no hosted client`);

  check(errors.length === 0, `${colorScheme}: no console error and no policy violation (${errors.join(' | ')})`);
  const elsewhere = requests.filter((url) => !url.startsWith('data:') && !url.startsWith('blob:') && new URL(url).origin !== origin);
  check(elsewhere.length === 0, `${colorScheme}: every request targets ${origin} (${elsewhere.join(' ')})`);
  const stored = await page.evaluate(() => JSON.stringify([Object.entries(localStorage), Object.entries(sessionStorage)]));
  check(!stored.includes(token), `${colorScheme}: the token is in neither localStorage nor sessionStorage`);
  check((await context.cookies()).length === 0, `${colorScheme}: the page sets no cookie`);
  const mode = await page.evaluate(() => (document.body.classList.contains('dark-mode') ? 'dark' : document.body.classList.contains('light-mode') ? 'light' : 'none'));
  check(mode === colorScheme, `${colorScheme}: the page follows the browser's scheme (${mode})`);
  await context.close();
}
await browser.close();
server?.close();
console.log(failures.length ? `${failures.length} failed` : 'the reference smoke passed');
process.exit(failures.length ? 1 : 0);
