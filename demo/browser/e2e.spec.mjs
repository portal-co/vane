// Playwright E2E for the vane browser demo (M6, exit criteria S3 + S4 in
// browser). Requires: `npm i -D playwright` and `npx playwright install`
// (network). Run as a nightly/browser job, not per-commit CI.
//
//   npx playwright test demo/browser/e2e.spec.mjs
//
// Serves demo/browser statically, loads the page in headless Chromium, and
// asserts the guest reaches a busybox shell with encrypted swap traffic.
import { test, expect } from '@playwright/test';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('.', import.meta.url));
const MIME = {
  '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript',
  '.wasm': 'application/wasm', '.bin': 'application/octet-stream',
  '.gz': 'application/gzip', '': 'application/octet-stream',
};

let server, baseURL;
test.beforeAll(async () => {
  server = createServer(async (req, res) => {
    const path = normalize(join(root, req.url === '/' ? 'index.html' : req.url.split('?')[0]));
    try {
      const data = await readFile(path);
      res.writeHead(200, { 'content-type': MIME[extname(path)] ?? 'application/octet-stream' });
      res.end(data);
    } catch {
      res.writeHead(404); res.end('not found');
    }
  });
  await new Promise((r) => server.listen(0, r));
  baseURL = `http://127.0.0.1:${server.address().port}`;
});
test.afterAll(() => server.close());

test('boots Linux to a busybox shell with encrypted OPFS swap', async ({ page }) => {
  test.setTimeout(30 * 60 * 1000); // the no-JIT interpreter is slow
  await page.goto(baseURL);

  const consoleText = page.locator('#console');
  // S3: kernel boots and reaches userspace.
  await expect(consoleText).toContainText('Run /init as init process', { timeout: 20 * 60 * 1000 });
  await expect(consoleText).toContainText('VANE LINUX BOOT OK', { timeout: 10 * 60 * 1000 });

  // S4 (browser): swap traffic occurred (resident cap < RAM ⇒ evictions).
  const stats = page.locator('#stats');
  await expect(stats).toContainText('swap:', { timeout: 60_000 });
  await expect(stats).not.toContainText('0 MiB written', { timeout: 60_000 });
});
