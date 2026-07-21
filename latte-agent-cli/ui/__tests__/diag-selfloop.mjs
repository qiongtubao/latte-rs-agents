// 快速诊断：直接抓 pageerror + DOM 状态
import { chromium } from "@playwright/test";

const URL = "http://localhost:4567/";

const browser = await chromium.launch({ headless: true });
const page = await browser.newPage();

const errors = [];
page.on("pageerror", (e) => errors.push(e.message));
page.on("console", (m) => {
  if (m.type() === "error") errors.push(`[console.error] ${m.text()}`);
});

await page.goto(URL, { waitUntil: "load", timeout: 8_000 });
await page.waitForTimeout(800);

console.log("=== 错误列表 ===");
if (errors.length === 0) console.log("  (none)");
else errors.forEach((e) => console.log(`  ${e.slice(0, 200)}`));

console.log("\n=== DOM 元素 ===");
for (const id of ["self-loop-form", "self-loop-task", "self-loop-max", "self-loop-progress", "self-loop-screenshots"]) {
  const ok = await page.locator(`#${id}`).count();
  console.log(`  ${ok ? "✓" : "✗"} #${id}`);
}

console.log("\n=== 页面首屏文本（找 Error）===");
const text = await page.evaluate(() => document.body.innerText.slice(0, 300));
console.log(`  ${text.replace(/\n/g, " | ")}`);

await browser.close();