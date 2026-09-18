# Civil Buddy 工作台 Demo（WorkBuddy 用法）

- **不召唤专家**：普通对话（你配置的模型），无知识库、无出稿工具。
- **召唤专家**：该专家独立走完「理解 → 检索私库+大类共享库 → 成稿 → 自检」，像易标的一个模块。
- **一类专家共享** `kb/<大类>/_shared/`，**每个专家另有私库** `kb/<大类>/<专家id>/`，公司规则在 `kb/company/`。

## 启动

```powershell
cd C:\Users\LW\civil-buddy\demo
copy .env.example .env
# 编辑 .env，填入 CIVIL_API_KEY / OPENAI_API_KEY / DEEPSEEK_API_KEY（自选模型，不必 DeepSeek）
python -m pip install -r requirements.txt
python serve.py            # 读 .env 里的 CIVIL_HOST / CIVIL_PORT；等价于 uvicorn app:app --host 127.0.0.1 --port 8765
```

浏览器打开 http://127.0.0.1:8765

### 手机 / 局域网

1. `.env` 里加 `CIVIL_HOST=0.0.0.0`（工作台无鉴权，只在可信 Wi-Fi 下开；`/api/local` 能读这台电脑的文件）。
2. 手机同一 Wi-Fi 打开 `http://<电脑IP>:8765`。≤720 px 时左栏「岗位/对话」和右栏「文件」变成顶栏按钮拉出的抽屉；本机路径一行在手机上隐藏。
3. 成稿期间锁屏/切后台回来：已生成的正文保留，落盘文件在「文件」抽屉；切回同一对话会从服务端重新拉转写和文件。

### 断流 / 任务切换（v0.7 起）

- `/api/chat` 逐 token 流式；模型沉默时每 `CIVIL_SSE_PING_SEC`（默认 15 s）发一条 `: ping` 心跳。
- 工具一落盘立刻推 `file` 事件；`error` 事件带 `partial_text` 与已落盘 `deliverables`，前端不再用错误覆盖正文。
- 「停止」按钮 / `/stop`：前端 abort，后端在下一个 chunk 关闭上游流；客户端断开同样会停。
- 每条对话一个 `thread_id`：`GET /api/threads/{id}/messages` 转写、`/files` 该会话所有落盘文件、`POST /cancel`、`DELETE`。刷新页面回到上次对话；「新建」清屏；「并行」任务完成后提示。
- `/api/health.capabilities` 声明本后端支持什么；共用前端按它隐藏按钮（Rust 工作台没有 threads/config，Python 没有一人公司成套）。

### 上传 / 下载

- `POST /api/upload`（multipart，字段 `session_id` + `file`，可多个）：pdf/docx/xlsx/txt/md/csv/json/log，20 MB/个，12 个/会话；抽出的文字随消息注入模型，模型也可 `read_attachment` 分段读。目录与 Rust 工作台共用 `demo/data/uploads/<session>/`。
- `GET /api/attachments?session_id=`；`POST /api/local {session_id, path}` 读工作台电脑上的文件夹（禁 `D:\layout`）。
- `GET /api/file?path=…` 下载（`filename*=utf-8''` 中文名正确）；`&inline=1` 在浏览器里直接看 .md/.txt。

高风险专家（施工方案、危大、交底、结构）写盘前勾选确认句。

经营投标三岗与装箱主线共用 `packing_assistant.tools.tender_parse`：

- 招标解析 → `extract_tender`（评分点 / ★ / 工期只抄原文）
- 废标检查 → `compliance_gaps`（P0 须人确认，不判定可投标）
- 技术标 → `tech_expand`（按抽出评分点出目录，不套上个项目模板）

## 自己设计专家和知识库

点右上角 **设计专家 / 知识库**：

- 新建大类、新建专家（id、职责、风险、别名）
- 打开任意一篇私库 / 大类共享 / 公司库，直接改 Markdown
- 每篇显示字节数和字数；专家私库合计超软上限会标红
- 可改软上限（默认 200 KB/专家）；单文件硬顶 512 KB
- 自定义专家可连私库一起删；内置专家只能从召唤墙隐藏

用户改动落在 `demo/kb/` 与 `demo/data/user_catalog.json`。

## 演示建议

1. 不点专家，问「栏杆一般多高」→ 普通闲聊，会声明不是专家稿。
2. 点「危大识别」，问临边要不要论证。
3. 勾选确认后点「施工方案」，出 11 章讨论提纲。
4. 再点「工友白话」，同一任务出口播稿。
5. 右侧应出现 **私库 / 大类 / 公司** 三层引用。
