# 前端流程验收（手机视口，jsdom）

真机浏览器（Playwright/Chromium）在很多内网环境下载不下来，这里用 jsdom + Node 的 fetch 把 **未经修改的 `demo/static/app.js`** 跑在 390 px 宽、非安全上下文（没有 `crypto.randomUUID`）的窗口里，对着真后端走完整流程：抽屉、新建/切换对话、流式、停止保留正文、文件事件与下载头、上传。

```bash
# 1) 假上游（逐 token 慢速流式，第一步返回 write_deliverable 工具调用）
cd scripts/e2e && npm ci && npm run fake-llm &
# 2) 工作台（Python 或 Rust，二选一或都起）
CIVIL_API_KEY=test CIVIL_API_BASE=http://127.0.0.1:9999/v1 CIVIL_SSE_PING_SEC=1 python ../../demo/serve.py &
CIVIL_API_KEY=test CIVIL_API_BASE=http://127.0.0.1:9999/v1 CIVIL_SSE_PING_SEC=1 CIVIL_PORT=8766 ../../workbench/target/release/civil-workbench &
# 3) 跑
npm test              # Python :8765，34 项
npm run test:rust     # Rust   :8766，同一套 34 项
npm run stream-checks # HTTP 级：心跳 / 实时 token / file 先于 done / 断连后上游 aborted
```

真机仍要人手过一遍的项见 `docs/civil-buddy/optimize-2026-09-18.md` 末尾清单（视觉、锁屏 40 s、iOS 分享面板）。
