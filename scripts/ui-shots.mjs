// 界面截图验证：用系统 Edge 打开预览，逐页截图
import { chromium } from "playwright-core";
import fs from "node:fs";

fs.mkdirSync("shots", { recursive: true });

const browser = await chromium.launch({
  channel: "msedge",
  headless: true,
});
const page = await browser.newPage({
  viewport: { width: 1320, height: 860 },
  deviceScaleFactor: 1,
});
await page.goto("http://localhost:4173", { waitUntil: "networkidle" });
await page.waitForTimeout(1800);
await page.screenshot({ path: "shots/01-overview.png" });

const nav = [
  ["上游 Provider", "02-providers"],
  ["路由", "03-routes"],
  ["请求日志", "04-logs"],
  ["重试与设置", "05-settings"],
];
for (const [label, name] of nav) {
  await page.getByRole("button", { name: label, exact: true }).first().click();
  await page.waitForTimeout(700);
  await page.screenshot({ path: `shots/${name}.png` });
}

// 打开 Provider 编辑弹窗
await page.getByRole("button", { name: "上游 Provider", exact: true }).first().click();
await page.waitForTimeout(500);
await page.getByRole("button", { name: "编辑" }).first().click();
await page.waitForTimeout(500);
await page.screenshot({ path: "shots/06-provider-modal.png" });

await browser.close();
console.log("done");
