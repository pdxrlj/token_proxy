// 本地 crates.io 代理：由于当前环境 schannel TLS 不可用，cargo 无法直接访问
// crates.io；本代理用 Node(OpenSSL) 完成 https 上行，对 cargo 暴露 http 源。
// 用法: node scripts/crates-proxy.mjs [port]
import http from "node:http";
import https from "node:https";

const PORT = Number(process.argv[2] || 8765);
const INDEX = "https://index.crates.io";
const STATIC = "https://static.crates.io";

function fetchUpstream(url) {
  return new Promise((resolve, reject) => {
    https
      .get(url, (res) => {
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () =>
          resolve({ status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks) }),
        );
      })
      .on("error", reject);
  });
}

const server = http.createServer(async (req, res) => {
  try {
    const u = new URL(req.url, `http://127.0.0.1:${PORT}`);
    let upstream;
    let outBody;
    let contentType = "application/json";

    if (u.pathname.startsWith("/index/")) {
      upstream = INDEX + u.pathname.slice("/index".length);
      const r = await fetchUpstream(upstream);
      if (u.pathname === "/index/config.json") {
        const cfg = JSON.parse(r.body.toString());
        // 关键：把 crate 下载地址重写到本地代理
        cfg.dl = `http://127.0.0.1:${PORT}/crates`;
        outBody = Buffer.from(JSON.stringify(cfg));
      } else {
        outBody = r.body;
        contentType = r.headers["content-type"] || "application/json";
      }
      res.writeHead(r.status, { "content-type": contentType });
    } else if (u.pathname.startsWith("/crates/")) {
      upstream = STATIC + u.pathname;
      const r = await fetchUpstream(upstream);
      outBody = r.body;
      contentType = "application/octet-stream";
      res.writeHead(r.status, { "content-type": contentType });
    } else {
      res.writeHead(404);
      res.end("not found");
      return;
    }
    res.end(outBody);
  } catch (e) {
    res.writeHead(502, { "content-type": "text/plain" });
    res.end(String(e));
  }
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`crates proxy listening on http://127.0.0.1:${PORT}`);
});
