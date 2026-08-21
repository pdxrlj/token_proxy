// UI 结构验证：控制台错误、布局溢出、关键元素与设计 token
import { chromium } from "playwright-core";

const browser = await chromium.launch({ channel: "msedge", headless: true });
const page = await browser.newPage({ viewport: { width: 1320, height: 860 } });

const consoleErrors = [];
page.on("console", (m) => {
  if (m.type() === "error") consoleErrors.push(m.text());
});
page.on("pageerror", (e) => consoleErrors.push(String(e)));

await page.goto("http://localhost:4173", { waitUntil: "networkidle" });
await page.waitForTimeout(1500);

const results = [];
const check = (name, ok, detail = "") =>
  results.push(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);

// 1. 无横向溢出（布局健康）
const overflow = await page.evaluate(() => {
  const el = document.querySelector(".main");
  return el ? el.scrollWidth - el.clientWidth : -1;
});
check("主区域无横向溢出", overflow <= 0, `overflow=${overflow}px`);

// 2. 设计 token 生效
const styles = await page.evaluate(() => {
  const cs = getComputedStyle(document.body);
  const stat = document.querySelector(".stat .value");
  return {
    bg: cs.backgroundColor,
    font: cs.fontFamily.slice(0, 40),
    statValue: stat ? getComputedStyle(stat).fontFamily.slice(0, 30) : "none",
  };
});
check("背景为深海军蓝", styles.bg === "rgb(11, 18, 32)", styles.bg);
check("正文使用 Space Grotesk", styles.font.includes("Space Grotesk"), styles.font);
check("统计数字使用等宽字体", styles.statValue.includes("JetBrains Mono"), styles.statValue);

// 3. 关键元素齐全
const el = {
  路由链路图: ".link-list .link-row",
  统计卡: ".stat",
  服务器地址: ".pill.ok",
  请求日志: ".log-row",
  侧栏导航: ".nav-item",
  品牌标识: ".brand-mark",
};
for (const [name, sel] of Object.entries(el)) {
  check(`总览存在${name}`, (await page.locator(sel).count()) > 0);
}

// 4. 各页面切换无异常
const navs = ["上游 Provider", "路由", "请求日志", "重试与设置"];
for (const label of navs) {
  await page.getByRole("button", { name: label, exact: true }).first().click();
  await page.waitForTimeout(400);
  const title = await page.locator(".topbar h1").textContent();
  check(`导航到「${label}」`, title.length > 0, `h1=${title}`);
}

// 5. Provider 卡片与编辑弹窗
await page.getByRole("button", { name: "上游 Provider", exact: true }).first().click();
await page.waitForTimeout(400);
check("Provider 卡片渲染", (await page.locator(".provider-card").count()) >= 3);
await page.getByRole("button", { name: "编辑" }).first().click();
await page.waitForTimeout(400);
check("编辑弹窗打开", (await page.locator(".modal").count()) === 1);
await page.keyboard.press("Escape");
await page.waitForTimeout(300);

// 6. 设置页交互：切换开关
await page.getByRole("button", { name: "重试与设置", exact: true }).first().click();
await page.waitForTimeout(400);
const switchCount = await page.locator(".switch").count();
check("设置页开关存在", switchCount >= 2, `count=${switchCount}`);

console.log(results.join("\n"));
console.log("--- console errors:", consoleErrors.length);
consoleErrors.slice(0, 10).forEach((e) => console.log("  ERR:", e.slice(0, 200)));
await browser.close();
