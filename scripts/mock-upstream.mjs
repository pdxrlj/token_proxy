// Mock OpenAI 兼容上游，支持故障注入，用于冒烟测试。
// 用法: node scripts/mock-upstream.mjs [port]
// 故障注入通过请求 query 控制：
//   ?fail=500       —— 永远返回 500
//   ?failN=2        —— 前 2 次请求返回 500，之后成功（按全局计数）
import http from "node:http";

const PORT = Number(process.argv[2] || 9999);
let callCount = 0;

const server = http.createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    callCount++;
    const url = new URL(req.url, "http://x");
    const fail = url.searchParams.get("fail");
    const failN = Number(url.searchParams.get("failN") || 0);
    const shouldFail = fail ? true : callCount <= failN;

    if (shouldFail) {
      res.writeHead(Number(fail || 500), { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { message: "mock upstream failure", type: "upstream_error" } }));
      return;
    }

    if (req.url.includes("/v1/models")) {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          object: "list",
          data: [{ id: "mock-model", object: "model", created: 0, owned_by: "mock" }],
        }),
      );
      return;
    }

    let parsed = {};
    try {
      parsed = JSON.parse(body || "{}");
    } catch {}
    const model = parsed.model || "mock-model";

    if (parsed.stream) {
      res.writeHead(200, { "content-type": "text/event-stream" });
      const chunks = ["Hello", " from", " the", " mock", " upstream"];
      let i = 0;
      const t = setInterval(() => {
        if (i >= chunks.length) {
          res.write("data: [DONE]\n\n");
          clearInterval(t);
          res.end();
          return;
        }
        const c = chunks[i++];
        res.write(
          `data: ${JSON.stringify({
            id: "chatcmpl-mock",
            object: "chat.completion.chunk",
            created: 0,
            model,
            choices: [{ index: 0, delta: { content: c }, finish_reason: null }],
          })}\n\n`,
        );
      }, 30);
      return;
    }

    res.writeHead(200, { "content-type": "application/json" });
    res.end(
      JSON.stringify({
        id: "chatcmpl-mock",
        object: "chat.completion",
        created: 0,
        model,
        choices: [
          {
            index: 0,
            message: { role: "assistant", content: "Hello from the mock upstream" },
            finish_reason: "stop",
          },
        ],
        usage: { prompt_tokens: 10, completion_tokens: 6, total_tokens: 16 },
      }),
    );
  });
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`mock upstream listening on http://127.0.0.1:${PORT}`);
});
