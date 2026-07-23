// Reproduce the user's bug: send a message, verify it appears in the chat.
import { chromium } from 'playwright';

const URL = 'http://localhost:4567';
const browser = await chromium.launch({
  headless: true,
  args: ['--no-sandbox', '--disable-dev-shm-usage'],
});
const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
const page = await ctx.newPage();

const consoleErrors = [];
page.on('console', (msg) => {
  if (msg.type() === 'error') consoleErrors.push(`[err] ${msg.text()}`);
  if (msg.type() === 'log') console.log(`[log] ${msg.text()}`);
});
page.on('pageerror', (err) => consoleErrors.push(`[pageerror] ${err.message}\n${err.stack || ''}`));
page.on('requestfailed', (req) => consoleErrors.push(`[reqfail] ${req.url()} ${req.failure()?.errorText || ''}`));

await page.goto(URL, { waitUntil: 'domcontentloaded', timeout: 15_000 });
await page.waitForSelector('#messages', { timeout: 5_000 });
await page.waitForTimeout(2_000);  // let SSE + initial session settle

console.log('\n=== test 1: send a plain message ===');
const probe = `hello world ${Date.now()}`;
await page.locator('#chat-input').fill(probe);
await page.locator('#chat-send').click();
await page.waitForTimeout(3_000);  // wait for backend roundtrip

const userMessages = await page.locator('.message-row.self').allTextContents();
console.log('user bubbles after send:', userMessages.map((t) => t.trim().slice(0, 80)));

if (userMessages.some((t) => t.includes(probe))) {
  console.log('✓ user message appeared');
} else {
  console.log('✗ user message NOT found — bug reproduces');
}

console.log('\n=== test 2: send a slash command ===');
await page.locator('#chat-input').fill('/help');
await page.locator('#chat-send').click();
await page.waitForTimeout(2_000);

const systemMessages = await page.locator('.message-row.system, .message-row.error').allTextContents();
console.log('system bubbles after /help:', systemMessages.map((t) => t.trim().slice(0, 80)));

console.log('\n=== console errors collected ===');
console.log(consoleErrors.length === 0 ? '(none)' : consoleErrors.join('\n'));

await browser.close();
process.exit(consoleErrors.length === 0 && userMessages.some((t) => t.includes(probe)) ? 0 : 1);
