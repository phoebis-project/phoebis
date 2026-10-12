// A static server for the mock bundle and the reference page: files under the directory by path, a directory's
// own index for a path that ends in a slash, the shell for every other extension-less path, so the console's
// client-side routes load the way the router's fallback loads them. `routes` answers a path with a handler in
// place of a file (the reference smoke's stub document and session), and `headers` ride every answer (the
// security headers the router sends, so a policy violation is observable).
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';

const TYPES = {
  '.css': 'text/css',
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript',
  '.json': 'application/json',
  '.png': 'image/png',
  '.svg': 'image/svg+xml',
  '.woff2': 'font/woff2',
};

export function serve(dir, routes = {}, headers = {}) {
  const server = createServer(async (req, res) => {
    const path = normalize(new URL(req.url, 'http://x').pathname);
    const route = routes[path];
    if (route) {
      const answer = route(req);
      res.writeHead(answer.status, { ...headers, ...answer.headers });
      res.end(answer.body);
      return;
    }
    const file = extname(path) ? join(dir, path) : path.endsWith('/') ? join(dir, path, 'index.html') : join(dir, 'index.html');
    try {
      const body = await readFile(file);
      res.writeHead(200, { ...headers, 'content-type': TYPES[extname(file)] ?? 'application/octet-stream' });
      res.end(body);
    } catch {
      res.writeHead(404, headers);
      res.end();
    }
  });
  return new Promise((resolve) =>
    server.listen(0, '127.0.0.1', () => resolve({ close: () => server.close(), port: server.address().port })),
  );
}
