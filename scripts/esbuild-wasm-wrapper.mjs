// esbuild → esbuild-wasm 的 ESM 包装：import 时自动完成 WASM 初始化
import { createRequire } from "node:module";
import wasm from "esbuild-wasm";

const require = createRequire(import.meta.url);
const wasmURL = require.resolve("esbuild-wasm/esbuild.wasm");

await wasm.initialize({ wasmURL });

export default wasm;
export const version = wasm.version;
export const initialize = wasm.initialize;
export const transform = (...args) => wasm.transform(...args);
export const transformSync = (...args) => wasm.transformSync(...args);
export const build = (...args) => wasm.build(...args);
export const buildSync = (...args) => wasm.buildSync(...args);
export const formatMessages = (...args) => wasm.formatMessages(...args);
export const formatMessagesSync = (...args) => wasm.formatMessagesSync(...args);
export const analyzeMetafile = (...args) => wasm.analyzeMetafile(...args);
export const analyzeMetafileSync = (...args) => wasm.analyzeMetafileSync(...args);
