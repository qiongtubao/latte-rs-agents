// Playwright screenshot utility for latte-agent testing
// Usage: node screenshot.mjs <url> <output-path> [width] [height]

import { chromium } from 'playwright';

const url = process.argv[2] || 'http://localhost:4567/';
const outputPath = process.argv[3] || '/tmp/latte-shot.png';
const width = parseInt(process.argv[4] || '1920', 10);
const height = parseInt(process.argv[5] || '1080', 10);

const browser = await chromium.launch({
  headless: true,
  args: ['--no-sandbox', '--disable-dev-shm-usage'],
});

const page = await browser.newPage({ viewport: { width, height } });

try {
  await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 15000 });
  // Wait for initial render
  await page.waitForTimeout(3000);
  await page.screenshot({ path: outputPath, fullPage: false });
  console.log(`Screenshot saved: ${outputPath} (${width}x${height})`);
} catch (err) {
  console.error(`Screenshot failed: ${err.message}`);
  process.exit(1);
} finally {
  await browser.close();
}
