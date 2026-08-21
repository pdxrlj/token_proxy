// 沙箱验证用：把 vite 的 esbuild 依赖重定向到 esbuild-wasm（进程内 WASM，
// 不需要 spawn 子进程，规避沙箱的 Node spawn+pipe 限制）。
// 用法: node --import ./scripts/esbuild-wasm-loader.mjs node_modules/vite/bin/vite.js build
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";
import path from "node:path";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const wrapperUrl = new URL("./esbuild-wasm-wrapper.mjs", import.meta.url).href;

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier === "esbuild") {
      return {
        url: wrapperUrl,
        format: "module",
        shortCircuit: true,
      };
    }
    return nextResolve(specifier, context);
  },
});
