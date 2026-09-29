import { chromium } from 'playwright-core';

const BASE = 'http://127.0.0.1:4173/';
const SHOTS = [
  { label: '总览', file: 'docs/screenshots/overview.png' },
  { label: '上游 Provider', file: 'docs/screenshots/providers.png' },
  { label: '路由', file: 'docs/screenshots/routes.png' },
  { label: '用户管理', file: 'docs/screenshots/users.png' },
  { label: '请求日志', file: 'docs/screenshots/logs.png' },
  { label: '重试与设置', file: 'docs/screenshots/settings.png' },
];

const CHROME = 'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe';

const browser = await chromium.launch({
  executablePath: CHROME,
  headless: true,
  args: ['--disable-gpu', '--no-sandbox'],
});
const ctx = await browser.newContext({
  viewport: { width: 1440, height: 960 },
  deviceScaleFactor: 2,
});
const page = await ctx.newPage();

await page.goto(BASE, { waitUntil: 'networkidle' });
await page.waitForTimeout(1000);

for (const s of SHOTS) {
  await page.getByRole('button', { name: s.label }).click();
  await page.waitForTimeout(800);
  await page.screenshot({ path: s.file, fullPage: true });
  console.log('shot:', s.file);
}

await browser.close();
console.log('done');
