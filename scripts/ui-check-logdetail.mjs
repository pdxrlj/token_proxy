// 验证日志详情弹窗与重试列
import { chromium } from "playwright-core";

const browser = await chromium.launch({ channel: "msedge", headless: true });
const page = await browser.newPage({ viewport: { width: 1320, height: 860 } });
const errors = [];
page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
page.on("pageerror", (e) => errors.push(String(e)));

await page.goto("http://localhost:4173", { waitUntil: "networkidle" });
await page.waitForTimeout(1500);

const results = [];
const check = (n, ok, d = "") => results.push(`${ok ? "PASS" : "FAIL"}  ${n}${d ? "  (" + d + ")" : ""}`);

// 1. 日志页：重试列存在
await page.getByRole("button", { name: "请求日志", exact: true }).first().click();
await page.waitForTimeout(500);
const headers = await page.locator(".table th").allTextContents();
check("日志表头含重试列", headers.some((h) => h.trim() === "重试"), headers.join(","));

// 2. 点击第一行 → 详情弹窗
await page.locator(".table tbody tr.clickable").first().click();
await page.waitForTimeout(500);
const modalVisible = (await page.locator(".modal").count()) === 1;
check("点击行打开详情弹窗", modalVisible);

// 3. 弹窗内容
const modalText = await page.locator(".modal").textContent();
check("详情含请求ID", modalText.includes("请求 ID"));
check("详情含重试次数", /重试 \d+ 次/.test(modalText), modalText.match(/重试 \d+ 次/)?.[0] ?? "");
check("详情含错误信息区块或成功提示", modalText.includes("错误信息") || modalText.includes("未产生错误"));
const body = await page.locator(".modal-body").textContent();
check("详情字段完整", ["时间", "端点", "Provider", "延迟", "Tokens"].every((k) => body.includes(k)));

// 4. 关闭弹窗
await page.keyboard.press("Escape");
await page.waitForTimeout(300);
check("Esc 关闭弹窗", (await page.locator(".modal").count()) === 0);

// 5. 总览页最近请求也可点击
await page.getByRole("button", { name: "总览", exact: true }).first().click();
await page.waitForTimeout(500);
await page.locator(".log-row.clickable").first().click();
await page.waitForTimeout(500);
check("总览点击日志打开详情", (await page.locator(".modal").count()) === 1);
await page.keyboard.press("Escape");

console.log(results.join("\n"));
console.log("--- console errors:", errors.length);
errors.slice(0, 5).forEach((e) => console.log("ERR:", e.slice(0, 150)));
await browser.close();
