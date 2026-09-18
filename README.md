# Civil Buddy

土木企业工作台 = **土木版 Codex**：**16 大类 / 66 岗 skill** · 任务选用 SOP · 工具算数 · 沙箱写盘。  
装箱 / 拼柜是其中一岗（**pack-ship**）：硬数字只走本仓的 packing 引擎，模型不写 xyz、不拍柜数。  
投标主线 C：招标文本 → 条款级响应矩阵 → 装柜 tools 作交付证据 → 经营岗交接（bid-tech / bid-compliance）。P0 资格/★/废标须人确认，**不**自动判定可投标。

> 内部讨论草稿，不是法定专项方案、不是签认件。  
> 高风险写盘前确认句：`我明白，将由持证人员签认`。

### Agent Middleware（赛道 1 · 完全合格）

对照表：[docs/civil-buddy/track1-qualified.md](docs/civil-buddy/track1-qualified.md)。  
Runtime 只深做两层：**策略引擎**（拒绝弹原因）和 **失败恢复**（retry → `UNSPECIFIED` 审计链）。  
剧本写死：正常下单 → 越权被拒 → 工具挂掉自动恢复 → 成本超限熔断。  
行业现网总判（人改口）：[industry-agent-eval-2026-08-25.md](docs/civil-buddy/industry-agent-eval-2026-08-25.md) — 内部起草搭子 **合格**；签认/投标 **不合格**。

```powershell
python scripts/demo_agent_middleware.py
npm run check
```

`npm run check` 必须过。不得把 API Key 提交进仓。

原独立仓 packing-agent 与 civil-buddy 已并入本树：https://github.com/LUOaini1213/civil-buddy

试用（别人可下载 exe）：[给试用的人.md](给试用的人.md) · LICENSE：MIT · 工作台包在 [GitHub Releases](https://github.com/LUOaini1213/civil-buddy/releases)。API Key **自带**，不必 DeepSeek。

---

## 两套入口

| 入口 | 地址 | 用途 |
|------|------|------|
| **Civil Buddy 工作台** | http://127.0.0.1:8765 | 召唤专家、投标/施工草稿、装箱作业单 |
| **主线 C · 投标应答 + 交付** | http://127.0.0.1:8000 | 招标要点 → 响应矩阵 → 装柜证据（草稿） |
| **工程装柜台** | http://127.0.0.1:8000/workbench | 成箱 → HITL → 拼柜 3D / CoG |
| **用户路径 / PRD（面试一页）** | [docs/civil-buddy/user-flow.html](docs/civil-buddy/user-flow.html) · [prd-pack-ship.md](docs/civil-buddy/prd-pack-ship.md) | 流程图 + 验收；不是座舱 / RoboOS |

### 1) Civil Buddy 工作台

```powershell
cd workbench
# API Key 写在 gitignored 的 demo/.env（CIVIL_API_KEY / OPENAI_API_KEY / DEEPSEEK_API_KEY）
cargo run --release --bin civil-workbench
```

Python 参考实现：`demo/`（`python demo/serve.py`，或 `uvicorn app:app --host 127.0.0.1 --port 8765`）。

产品 CLI（土木版 Codex）：`python -m packing_assistant.civil`（TUI）· `civil app` · `civil mcp --pack construction`。技能一岗一份：`.agents/skills/<id>/SKILL.md`。IDE：`ide/README.md`。Grok 总控：`skills/civil-buddy`。  
**全量产品规划书**：[docs/civil-buddy/product-plan.md](docs/civil-buddy/product-plan.md)。切片执行：[product-completion-plan.md](docs/civil-buddy/product-completion-plan.md)。

### 2) 装箱引擎（pack-ship 的计算器）

```powershell
pip install -r requirements.txt
python scripts/demo_one_shot.py              # 冒烟，无需 API Key
uvicorn gateway.app:app --host 127.0.0.1 --port 8000
```

工作台默认在同一仓库里找引擎：`PACKING_AGENT_ROOT` = 本仓根。也可另开网关：

```env
PACKING_AGENT_URL=http://127.0.0.1:8000
```

详见 [docs/civil-buddy/packing-agent.md](docs/civil-buddy/packing-agent.md)。

---

## 仓库结构

```
workbench/           # Civil Buddy Rust 工作台 + MCP
demo/                # 专家知识库 kb/ + Python 参考实现
.agents/skills/      # Codex：66 岗各一份 SKILL.md
skills/civil-buddy/  # Grok 总控 SOP
packing_assistant/   # 装箱 harness（Team A 成箱 + Team B 拼柜）
gateway/ + frontend/ # 装箱 HTTP / UI
docs/civil-buddy/    # 工作台设计、专家名册
docs/                # 装箱架构与产品主线
```

**原则（两套入口共用）：** tools compute numbers; the model only routes.

---

## 装箱引擎（原 packing-agent）

架构：**大 Team ⊃ Team A（成箱）+ Team B（拼柜）** · Harness 0.6.4  
NL → IntentSpec → 白名单 tools → HITL → 影子评测。

```powershell
python scripts/demo_one_shot.py --all
python scripts/run_hard_fail_cases.py --smoke
```

文档：[docs/harness-design.md](docs/harness-design.md) · [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) · [docs/product-mainline-tender-delivery.md](docs/product-mainline-tender-delivery.md)

---

## 环境变量

复制 `.env.example` / `demo/.env.example`，不要提交密钥。

| 变量 | 说明 |
|------|------|
| `CIVIL_API_KEY` / `OPENAI_API_KEY` / `DEEPSEEK_API_KEY` | 成稿用的 Chat Completions Key（自选；DeepSeek 可选） |
| `CIVIL_API_BASE` / `OPENAI_BASE_URL` | 兼容网关，例 `https://api.openai.com/v1` |
| `CIVIL_MODEL` / `LLM_MODEL` | 模型名，须与网关一致 |
| `CIVIL_JOB_ROOT` | 授权作业文件夹（禁止 `D:\layout`） |
| `PACKING_AGENT_URL` | 装箱网关（可选） |
| `PACKING_AGENT_ROOT` | 默认本仓根，一般不用设 |
| `CIVIL_PORT` | 工作台端口，默认 8765 |
| `CIVIL_HOST` | 工作台绑定地址，默认 `127.0.0.1`；`0.0.0.0` 开给同网段手机（无鉴权，仅可信内网） |
| `CIVIL_SSE_PING_SEC` / `CIVIL_LLM_READ_TIMEOUT` / `CIVIL_MAX_CHATS` | SSE 心跳间隔 / 上游读超时 / 并发对话数（Python 工作台） |

---

## 明确不做

桌面键鼠、微信/飞书、Rhino/Civil3D 改模、注册工程师签认、编造条款/单价/xyz。规范全文不进仓库。
