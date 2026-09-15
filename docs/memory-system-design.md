# Memora（忆庐）· AI 记忆库系统技术设计文档

- **@author** yujinping
- **@date** 2026-09-15
- **@intent** 在自有 2GB Ubuntu 命令行服务器上，构建一款免费开源、单二进制、低资源、可公网部署、带访问保护、按项目隔离的 AI 长期记忆服务。复用既有 Rust（Axum + sea-orm + SQLite）技术栈，采用 MCP 协议接入 Cursor / Claude / Cline 等 AI 客户端，由 AI 辅助按 TDD 分期开发。

---

## 0. 项目命名

| 项 | 值 |
|---|---|
| 英文名 | **Memora** |
| 中文名 | **忆庐** |
| binary / 仓库名 | `memora` |
| 域名建议 | `memora.dev` / `memora.space` |
| MCP endpoint | `https://<host>/mcp` |

**命名理由**
- 英文 Memora = memory + aura 的自造词，柔和品牌感，读音清晰（/məˈmɔːrə/），不与 Mem0 等现成产品混淆（后缀不同、读音区分明显）。
- 中文「庐」= 小屋 / 居所，暗合「自托管、自有的一亩三分地」意象；「忆庐」即「记忆的小屋」，亲和、非技术腔，契合个人 / 小团队自部署定位。
- 中英文均为自创 / 柔化词，无既有强绑定含义，便于后续建立独立品牌认知。

**约定**
- 代码仓库、可执行 binary、Docker 镜像统一使用 `memora`。
- 中文名用于文档标题、UI 文案与对外介绍；英文短名用于一切技术标识（路径、端口、日志前缀等）。

---

## 1. 背景与目标

### 1.1 动机
调研现成方案（OpenMemory / Hindsight / Nocturne / 官方 MCP Memory）后，在「2GB 内存 + 命令行 Ubuntu + 公网 + 鉴权 + 项目隔离」约束下：

| 方案 | 问题 |
|---|---|
| Hindsight 全量 | API+Worker 各需约 2GB，直接超预算 |
| Hindsight slim | 临界可行，但逼近内存上限、容错低 |
| Nocturne | 开源、Docker 一键，但鉴权为单一共享 token（非项目级） |
| 官方 MCP Memory | 零数据库极轻，但无原生鉴权、无项目隔离 |

结论：现成方案要么内存超标，要么鉴权/隔离非原生。自建单二进制方案可同时把内存、隔离、鉴权、可控性拉到理想区间，且复用已有技术栈，维护成本可控。

### 1.2 验收条件（目标）
- [ ] 免费、开源（自有仓库，MIT / Apache-2.0）
- [ ] 单二进制部署，无外部服务依赖（除可选远程 embedding API）
- [ ] 常驻内存 < 100MB（2GB 服务器余量充足）
- [ ] 可公网 HTTPS 访问
- [ ] Bearer Token 访问保护
- [ ] 按项目硬隔离（数据独立、互不越权）
- [ ] 标准 MCP 协议，兼容主流 MCP 客户端

### 1.3 非目标（首版范围外）
- 不做内置大模型推理（事实抽取 / 语义嵌入走外部 API，可选）
- 不做复杂图谱推理 / 反思（首版对标官方 MCP Memory 的图谱模型）
- 不做多节点集群（单实例即可）

---

## 2. 技术选型

| 层次 | 选型 | 理由 |
|---|---|---|
| 语言 | Rust | 单二进制、无 GC、低内存；与 tb-console / dashboard 同栈 |
| Web / MCP 框架 | Axum | 异步、生态成熟、dashboard 已在用 |
| ORM | sea-orm | tb-console 已用；类型安全 + 迁移管理 |
| 存储 | SQLite（每项目一文件） | 零运维、单文件即备份、硬隔离 |
| 配置 | dotenvy | dashboard 已用，PORT 等变量一致 |
| 传输 | MCP Streamable HTTP | 当前 MCP 主流，取代旧 SSE |
| 反代 / TLS | Caddy | 自动证书，命令行环境最省心 |
| 测试 | tokio::test + reqwest + rstest | 契合 TDD 习惯 |

**备选语言 Go**：亦可实现单二进制 + 低内存，MCP 库 `mark3labs/mcp-go` 成熟；若更熟 Go 可切换，架构不变。本文以 Rust 为准。

---

## 3. 系统架构

```
                 HTTPS
   MCP Client ──▶ Caddy ──▶ Axum 单二进制
                           │
        ┌──────────────────┼────────────────────┐
        ▼                  ▼                    ▼
   鉴权中间件         路由 / 项目解析        存储抽象层
   Bearer→project    /mcp/{project_id}     按 project_id 产出 Repository
                           │
                  ┌────────┴────────┐
            MCP 工具层          REST 管理层
        add/search/list/      create/list/
        delete/reflect        delete project
```

**组件职责**
- **鉴权中间件**：解析 `Authorization: Bearer`，查 `projects` 表得 `project_id` 与 db 路径，注入请求扩展。无效 / 越权返回 401。
- **存储抽象层（StorageBackend + Repository）**：按 `project_id` 经 `StorageBackend` 产出 `MemoryRepository` / `ConversationRepository` 实例；后端实现（SQLite 文件 / 单库 / Postgres / 内存）可插拔，业务层依赖 trait 不感知具体存储。详见 §12。
- **MCP 工具层**：实现标准记忆工具，语义对齐官方 MCP Memory，保证客户端零改造。
- **REST 管理层**：项目管理，仅 `ADMIN_TOKEN` 可调用。

---

## 4. 数据模型

每个项目独立 SQLite 文件 `data/{project_id}/mem.db`。**全局元库** `data/_meta.db` 存项目注册信息。

### 4.1 项目元库 `_meta.db`
```sql
CREATE TABLE projects (
  project_id  TEXT PRIMARY KEY,
  token_hash  TEXT NOT NULL,   -- SHA-256(Bearer Token)
  db_path     TEXT NOT NULL,   -- 相对 DATA_DIR 的 SQLite 路径
  created_at  INTEGER NOT NULL
);
```

### 4.2 项目库 `mem.db`
```sql
-- 实体
CREATE TABLE entities (
  name        TEXT PRIMARY KEY,
  entity_type TEXT NOT NULL DEFAULT 'unknown',
  created_at  INTEGER NOT NULL
);

-- 观测（实体的属性 / 事实，可多条）
CREATE TABLE observations (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  entity_name TEXT NOT NULL,
  content     TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  FOREIGN KEY(entity_name) REFERENCES entities(name)
);

-- 关系
CREATE TABLE relations (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  from_name     TEXT NOT NULL,
  to_name       TEXT NOT NULL,
  relation_type TEXT NOT NULL,
  created_at    INTEGER NOT NULL
);

-- 全文检索（FTS5，写入时同步维护，删除时清理）
-- obs_id 为 UNINDEXED 列：把索引行映射回观测，删除时按 id 精确清理
-- 索引行分两类：实体占位行（obs_id = -1，content 存 entity_type，保证只有实体无观测也可检索）
--              观测行（obs_id = 观测 id，name 为实体名，content 为观测内容）
CREATE VIRTUAL TABLE memory_fts USING fts5(name, content, obs_id UNINDEXED);

-- 可选：语义向量（远程 embedding，float32 存 BLOB），首版可不建
CREATE TABLE embeddings (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  source_type TEXT NOT NULL,   -- 'entity' | 'observation'
  source_id   TEXT NOT NULL,
  vector      BLOB NOT NULL,
  created_at  INTEGER NOT NULL
);
```

-- 会话记录（P5 扩展：支持跨机器会话恢复，按项目隔离，存于同一 mem.db）
CREATE TABLE conversations (
  session_id  TEXT    NOT NULL,             -- 会话标识（客户端生成或服务器分配）
  turn_index  INTEGER NOT NULL,             -- 轮次序号，从 0 递增
  role        TEXT    NOT NULL,             -- 'user' | 'assistant' | 'system'
  content     TEXT    NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY(session_id, turn_index)
);
CREATE INDEX idx_conv_session ON conversations(session_id, created_at);

**设计要点**
- 实体-关系-观测三元组与官方 MCP Memory 一致，客户端零改造即可接入。
- FTS5 为 SQLite 后端专属实现；检索方言差异（如 Postgres 的 `tsvector` / `pg_trgm`）被隔离在对应后端 impl 的 `search_nodes` 内，不向外层泄漏，详见 §12。
- FTS5 在写入 / 删除时同步维护，检索走 `MATCH`，无需向量库。
- 语义向量为可选增强：仅启用远程 embedding 时写入，Rust 内做余弦相似度，不引入额外服务。

---

## 5. 接口设计

### 5.1 MCP 工具（Streamable HTTP，路径 `/mcp`）
对齐官方 MCP Memory 语义，保证客户端兼容。项目**不在 URL 里**：由 `Authorization: Bearer <项目token>` 解析出 `project_id`，避免「路径与 token 指向不同项目」这类不一致。

| 工具 | 关键参数 | 行为 | 返回 |
|---|---|---|---|
| `create_entities` | `entities[{name, entity_type}]` | 批量建实体，重复名忽略（不覆盖原类型） | 请求名在调用后的状态（含观测） |
| `create_relations` | `relations[{from_name, to_name, relation_type}]` | 批量建关系，三元组重复跳过 | 实际落库的关系（**含 id**） |
| `add_observations` | `observations[{entity_name, contents[]}]` | 追加观测；实体不存在报 `-32602` | 受影响实体（**含新观测 id**） |
| `delete_entities` | `names[]` | 级联删除关系 / 观测 / FTS | 确实存在并被删除的名字 |
| `delete_observations` | `ids[]` | 按 id 删观测（幂等） | 请求 id（去重保序回显） |
| `delete_relations` | `ids[]` | 按 id 删关系（幂等） | 请求 id（去重保序回显） |
| `read_graph` | — | 返回全图 | `{entities[], relations[]}` |
| `search_nodes` | `query` | FTS5 全词检索（bm25 排序）；无结果时退化为 `LIKE %q%` 子串扫描 | 命中实体（**带完整观测集**，按相关度排序） |
| `open_nodes` | `names[]` | 按名取节点明细 | 命中实体（按 name 升序） |

**出参约定**
- `Json<T>` 包装：响应同时给出 `content[0].text`（JSON 字符串）与 `structuredContent`，兼容只看 `content` 的旧客户端与支持结构化输出的新客户端。
- 实体视图为 `{name, entity_type, created_at, observations[]}`；观测视图为 `{id, content, created_at}`；关系视图为 `{id, from_name, to_name, relation_type, created_at}`。
- **必须回读 id**：`create_relations` / `add_observations` 回读 id 是刻意的——`delete_relations` / `delete_observations` 只接受 id，不回读则调用方拿不到句柄，记忆的「可修订」闭环无法成立。
- **入参别名**：接受官方 schema 的 camelCase 写法（`entityType` / `relationType` / `entityName` / `from` / `to`），容忍模型照抄旧 schema；别名不进入 JSON Schema，对外只宣传规范字段名。

MCP 服务端采用 **rmcp（Rust MCP SDK，3.x）** 实现 Streamable HTTP 传输。与官方 MCP Memory 的差异仅在**检索能力**（本服务额外支持中文片段与词内片段）与**返回结构的丰富度**（带 id 与时间戳），不改变三元组语义。

### 5.2 REST 管理层（`/api/v1`，需 `ADMIN_TOKEN`）
| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/projects` | 创建项目（生成 token 并返回明文，201） |
| GET | `/projects` | 项目列表 |
| DELETE | `/projects/{id}` | 注销项目（删登记行 + 删项目库文件） |
| GET | `/projects/{id}/stats` | 实体 / 关系 / 观测统计 |
| GET | `/health` | 健康检查（无鉴权） |

**请求 / 响应契约**
- `POST /projects`：体为 `{"project_id"?: string, "backend"?: string}`，两字段均可省略——省略 `project_id` 时自动生成 `p_<12 位十六进制>`，省略 `backend` 时取 `STORAGE_BACKEND`。
- 创建响应 `{"project_id", "token", "backend"}`：**明文 `token` 仅此一次出现**，库内只存 SHA-256，丢失只能重建项目。
- 注销响应 `{"project_id", "data_removed": bool}`：`data_removed=false` 表示「登记已撤销，但物理数据未能删除」（该项目的后端未编译进本构建）——如实报告，不假装成功。
- 统计响应 `{"project_id", "backend", "entities", "relations", "observations"}`，数据一律从 `read_graph` 派生，与 MCP 客户端所见同源。

**状态码语义**（与 MCP 侧 `-32602` / `-32603` 的分流原则一致：客户端可否自行修正）
| 码 | 含义 | 触发场景 |
|---|---|---|
| 400 | 入参可修正 | project_id 非法（目录穿越 / 分隔符 / 空白）；请求的后端未编译进本构建（响应附带 `available_backends` 列表供自我修正） |
| 401 | 凭据问题 | 缺 / 错 `ADMIN_TOKEN`（项目 token 不能当管理凭据用） |
| 404 | 资源不存在 | 项目未登记 |
| 409 | 资源冲突 | project_id 已被占用 |
| 500 | 服务端故障 | 元库不可用、后端未注册（登记的 backend 值本身合法但本构建未编译）、熵源不可用 |

**注销的顺序约定**：先删物理数据（`StorageBackend::drop_project`），再删元库登记行。反序时若删文件失败，会留下「登记行已消失、数据仍在盘上」的不可见残留——既违背删除承诺又无法重试自愈；本顺序最坏情况是「文件已删、登记行尚在」，重试一次 DELETE 即可收敛。

### 5.3 MCP 客户端接入配置（Zed / WorkBuddy）

本服务为标准 MCP（Streamable HTTP）+ Bearer 鉴权。Zed 与 WorkBuddy 均原生支持远程 HTTP MCP 且可透传 `Authorization` 头，接入仅需约 5 行配置。

**接入契约**：客户端连接 `https://host/mcp` 并在请求头携带 `Authorization: Bearer <项目 token>`；中间件以 token 解析 `project_id` 并打开对应 `data/{project}/mem.db`。同一项目 token 即共享同一份记忆——Zed 与 WorkBuddy 配置相同 token，天然跨客户端、跨机器共享。

**部署前置：`MCP_ALLOWED_HOSTS`**。MCP 传输层强制校验请求的 `Host`（防 DNS 重绑定），默认只接受 `localhost` / `127.0.0.1` / `::1`。经 Caddy 反代时反代会透传真实域名，因此公网部署**必须**把对外域名加入白名单，否则请求会被协议层以 `403` 拒绝：

```bash
MCP_ALLOWED_HOSTS=mem.example.com,localhost,127.0.0.1
```

**WorkBuddy**（`~/.workbuddy/mcp.json`）：
```json
{
  "mcpServers": {
    "my-memory": {
      "url": "https://mem.example.com/mcp",
      "headers": { "Authorization": "Bearer <项目token>" }
    }
  }
}
```

**Zed**（`~/.config/zed/settings.json`，新版本原生支持 `url` + `headers`）：
```json
{
  "context_servers": {
    "my-memory": {
      "url": "https://mem.example.com/mcp",
      "headers": { "Authorization": "Bearer <项目token>" }
    }
  }
}
```
> 若 Zed 版本不支持直连 Streamable HTTP，改用官方 `mcp-remote` stdio 桥接（一行命令转发到同一 URL，用 `--header` 传入 Bearer）。

**接入约定（代理侧记忆指令）**：MCP 工具由模型按需调用，不会自动落盘。须在 Zed / WorkBuddy 的系统提示或 Agent profile 中固化两条规则，否则记忆库长期为空：
1. **持久化规则**：当对话出现用户偏好、项目约定、关键决策、待办时，主动调用 `create_entities` / `add_observations` 写入记忆库。
2. **恢复规则**：新会话开始时先 `read_graph` / `search_nodes` 拉取相关记忆作为上下文前置。

### 5.4 会话工具（P5 扩展，MCP）

在标准 9 个记忆工具之外，新增会话类工具（独立命名，不污染标准语义），由代理驱动实现跨机器会话恢复：

| 工具 | 关键参数 | 行为 |
|---|---|---|
| `log_turn` | `session_id, role, content` | 追加一轮对话（自动 `turn_index`） |
| `get_session` | `session_id` | 拉回完整会话 transcript |
| `list_sessions` | `prefix?` | 列出历史会话标识 |

---

## 6. 鉴权与多租户

- 全局 `_meta.db` 存 `projects(token_hash, db_path)`。
- 客户端连接 `https://host/mcp/{project_id}` 或统一 `https://host/mcp`，请求头 `Authorization: Bearer <token>`；后者由中间件按 token 解析 `project_id`（详见 5.3 接入契约）。
- 中间件流程：`token → SHA-256 → 查 projects → 解析 project_id（URL 路径优先，缺省回退到 token 绑定项目）→ 打开对应 SQLite`。
- 无效 / 越权 token 返回 401。
- **项目隔离双保险**：独立 SQLite 文件（数据物理隔离）+ 路径 / token 绑定（逻辑隔离），互不越权。
- `ADMIN_TOKEN` 为独立环境变量，仅 REST 管理层使用，与项目 token 体系分离。

---

## 7. 检索策略

1. **首版**：FTS5 关键词检索（bm25 排序），覆盖绝大多数记忆召回场景，零额外依赖。
2. **中文与词内片段兜底**：SQLite 内置 `unicode61` 分词器不做中文分词（整段中文会成为一个 token），故 FTS5 无结果时自动退化为 `LIKE %q%` 子串扫描。该行为已落地并有测试覆盖（含注入式恶意查询串不报错的用例）。
3. **增强（可选）**：远程 embedding API 生成向量存 BLOB，Rust 内余弦相似度，与 FTS5 结果以 RRF 融合。
4. **原则**：不引入向量数据库，保持「单二进制 + SQLite」的极简形态。

---

## 8. 部署方案（2GB Ubuntu 命令行）

### 8.1 运行形态：一份二进制，两种托管方式

| 形态 | 启动方式 | 适用场景 | 崩溃自动拉起 |
|---|---|---|---|
| **前台**（`run`，默认） | `memora run` 或 systemd `ExecStart=... run` | 生产托管、容器 | 由 systemd `Restart=always` 提供 |
| **自守护**（`start` / `stop` / `restart` / `status`） | `memora start` | 无 systemd、无 root 的裸机 / 单机临时使用 | **无** |

> 保留前台形态的根本原因：**纯自守护无法在进程崩溃后自动拉起**，对长期记忆服务是硬伤。两种形态共用同一份二进制，systemd 因此退化为「可选组件」，只负责开机自启与崩溃重启。

| 子命令 | 行为 | 退出码 |
|---|---|---|
| `run` | 前台运行；收到 SIGTERM / SIGINT 后优雅退出（在途请求收尾、WAL 落盘、清理自身 PID 文件） | 0 |
| `start` | 后台启动：`setsid` 脱离控制终端、日志追加到 `DATA_DIR/memora.log`、写 PID 文件，**`/health` 通过才返回 0** | 0 / 1 |
| `stop` | SIGTERM 等待至多 10s，超时升级 SIGKILL；未运行时幂等成功 | 0 |
| `restart` | 等价于 `stop` + `start`，未运行时可直接拉起 | 0 / 1 |
| `status` | 打印 pid / 端口 / 健康状态 / 数据目录 / 日志路径 | 0 健康 · 1 存活但不健康 · **3 未运行** |
| `init [--force]` | 生成 `.env` 与 systemd / Caddy 模板（模板经 `include_str!` 内嵌二进制，`init` 不依赖部署目录） | 0 |
| `-h/--help`、`-V/--version` | 用法与版本 | 0 |

**无子命令时等价于 `run`**，因此既有部署方式（`PORT=... ./memora`）零改动。未知子命令 / 选项返回 2。

PID 文件与日志都锚定在 `DATA_DIR` 下（`memora.pid` / `memora.log`），使控制面与数据同生命周期——迁移或备份数据目录时不会留下指向旧机器的 PID 文件。

### 8.2 配置来源与优先级

四层叠加，高者胜：

1. **命令行选项** —— `--port` / `--data-dir` / `--admin-token` / `--storage-backend` / `--mcp-allowed-hosts` / `--env-file`
2. **进程环境变量** —— systemd `EnvironmentFile=` 注入
3. **`.env` 文件**
4. **内置默认值**

`.env` 探测顺序：`CWD/.env` → **可执行文件所在目录/.env** → `DATA_DIR/.env`，取首个存在者；启动日志会打印实际命中的路径，未命中时列出**全部**探测过的路径。`--env-file` 为显式指定：文件缺失或语法错误一律硬失败。同理 `PORT` 取值非法（`abc` / `0` / `70000`）直接报错退出，**不静默回退默认值**。

> 为什么仍推荐 systemd 注入？不是「环境变量优于 `.env`」，而是**位置确定性**：`.env` 的查找依赖 CWD，而 systemd 下 CWD 由 `WorkingDirectory=` 决定，不等于二进制所在目录；`EnvironmentFile=` 不依赖 CWD。两者并不冲突——`dotenvy` 不覆盖已存在的进程环境变量，故可同时使用。

### 8.3 部署步骤

1. `cargo build --release` 产单二进制（约 13MB，可选 musl 静态链接）。
2. 建 **2GB swap** 文件作为安全垫。
3. `memora init` 生成 `.env` 与部署模板；至少设置 `ADMIN_TOKEN`（`openssl rand -hex 32`），并把 `.env` 权限收紧为 `600`。
4. `memora start`；或安装 systemd unit（`init` 的输出即一份可直接使用的 unit，环境变量含 `PORT`、`DATA_DIR`、`ADMIN_TOKEN`、`STORAGE_BACKEND`、`MCP_ALLOWED_HOSTS`）。
5. **Caddy** 反代 `/mcp`、`/api/v1`、`/health`，其余路径一律 404（`init` 输出的 Caddyfile 已按此写好），自动 Let's Encrypt 证书。**`MCP_ALLOWED_HOSTS` 必须包含对外域名**，否则反代后的 MCP 请求会被协议层以 403 拒绝（见 §5.3）。
6. SQLite 调优由后端在建池时完成：`journal_mode=WAL`、`synchronous=NORMAL`；同一项目的建池按项目串行化（原因见 §9 P4.2 取舍）。

**资源预估**：二进制 + SQLite 常驻 < 100MB，2GB 服务器余量充足，可并行运行其他服务。

---

## 9. 开发分期（TDD）

| 期 | 内容 | 交付 | 测试 |
|---|---|---|---|
| **P1 骨架** | Axum + sea-orm 启动、项目路由、`Bearer` 中间件、`_meta.db` | 服务可启动，健康检查通过 | 中间件鉴权单测、路由单测 |
| **P2 模型** | 定义 `MemoryRepository` trait + `SqliteFileBackend` 实现（实体/关系/观测 CRUD + FTS5 同步）；`_meta.db` 增加 `backend` 字段 | 记忆增删改查，业务层仅依赖 trait | Repository 契约单测（含 `InMem` 后端）+ FTS5 检索单测 |
| **P3 MCP** | 接入 rmcp，9 个工具改为调用 `MemoryRepository` trait（不直连 sea-orm） | 客户端可读写记忆 | 工具语义单测 + 协议级端到端（`initialize` / `tools/list` / `tools/call`）;<br>MCP Inspector 人工核验待 P4.2 联调时补 |
| **P4.1 管理面** | REST 项目增删 + 用量统计；后端新增 `drop_project` 生命周期方法 | 项目生命周期脱离手工改库 | 管理面协议级用例 + 后端级契约（双后端） |
| **P4.2 运行形态** | caddy 式自守护 CLI（`run`/`start`/`stop`/`restart`/`status`/`init`）+ `.env` 查找路径显式化 + release 构建 + systemd/Caddy + 压测 | 公网可用，且无 systemd 也能跑 | 负载 / 隔离验证、进程生命周期端到端 |
| **P5 会话** | `conversations` 表 + `log_turn`/`get_session`/`list_sessions` + Zed/WorkBuddy 接入指令落地 | 会话跨机器恢复 | 会话写入 / 恢复单测 + 双客户端联调 |

**实施进度（截至 2026-09-15）**

- **P1 已完成**：工程目录 `personal-open-source/memora`，Axum + sea-orm(SQLite) 骨架、Bearer 鉴权中间件、`/health` 公开、`/mcp` 需 Bearer、`/api/v1` 需 ADMIN_TOKEN。
  关键实现取舍：采用 `middleware::from_fn_with_state` + 闭包注入，使整棵路由树统一为 `Router<()>`，规避 Axum 0.8 各子路由 `State<S>` 推断冲突（详见技能 `axum-seaorm-sqlite-scaffold`）。
- **P2 已完成**：存储抽象 + 双后端 + 全链路依赖注入。
  - 新增模块：`domain.rs`（领域模型）、`storage/{mod,repo,normalize,contract,sqlite_file,mem}.rs`、`reply.rs`；`entity.rs` 增加 memory 三元组实体；`meta.rs` 增加 `backend` 列迁移与 `ProjectRecord`。
  - **契约测试复用**：`storage::contract::run_all` 是一份后端无关的 12 阶段契约（实体 CRUD 幂等、整批非法不落库、观测追加与 `EntityNotFound`、关系去重、检索命中名/类型/内容、CJK 子串、级联删除、幂等删除、跨项目隔离）。`InMemBackend` 与 `SqliteFileBackend` 各自跑同一份契约，保证「换后端不改业务代码」不是口号。
  - **验证结果**：`cargo test` **40 项全部通过**（契约 2 项、后端与元库/路由 38 项）；`cargo build` 与 `cargo clippy --all-targets` **零告警**；运行时冒烟验证 `/health`=200、`/mcp` 无 token=401、sqlite_file 与 in_mem 两项目各自返回正确统计、管理接口列出项目与其后端、项目库文件按需懒创建（含 WAL 附属文件）。
  - **键实现取舍**：
    1. 写入一律走「先整批校验、再事务写入」，`begin` + 显式 `commit`/`rollback` 收口于一个私有方法，保证非法批次与中途失败都不产生半成品数据。
    2. FTS5 索引行携带 `obs_id`（UNINDEXED），并额外为「只有实体、暂无观测」的名字建占位索引行，使实体名与 `entity_type` 同样可被检索。
    3. `search_nodes` 的方言差异（FTS5 / LIKE）全部锁在 SQLite 后端 impl 内，对外只有「命中名 / 类型 / 内容」这一行为契约。
    4. `project_id` 在存储层二次校验（仅允许 `[A-Za-z0-9_-]`、长度 ≤ 64），纵深防御目录穿越。
    5. 项目库位置由后端派生（`data/{project_id}/mem.db`），元库 `db_path` 仅作记录，避免两个真相源。
    6. P2 阶段仓库写侧接口仅由测试驱动，非测试构建存在「暂未被生产代码调用」告警，故在 `domain` / `storage` / `meta` 模块级临时放行 `dead_code`。**P3 已按计划移除其中两处**：
       - `domain` / `storage` 的放行已删除——相关 API 现均由 MCP 工具层在生产路径上使用；仅测试使用的访问器（`ObservationInput::single`、`Graph::entity_count` 等、`BackendKind::ALL`）下沉为 `#[cfg(test)]`。
       - `meta` 的 `create_project` / `NewProject` / `DEFAULT_BACKEND` 属 **P4 的 REST 管理面**（`POST /api/v1/projects`），P3 无法使其进入生产路径；已从模块级放行收敛为**条目级** `cfg_attr(not(test), allow(dead_code))`，P4 接入后连同标注一并删除。
       - 顺带修正了原计划中「P3 移除全部三处」的错误预期。
- **P3 已完成**：MCP 工具层接入，9 个标准记忆工具可读写，协议层端到端验证。
  - 新增模块：`mcp/{mod,dto,tools,server}.rs`；`config.rs` 增加 `MCP_ALLOWED_HOSTS`；`routes.rs` 的 `/mcp` 由统计占位替换为 `nest_service(StreamableHttpService)`。
  - **三层拆分**（本期的核心设计）：`dto`（协议数据契约，serde + JsonSchema）→ `tools`（后端无关语义，只依赖 `Arc<dyn MemoryRepository>`）→ `server`（唯一依赖 rmcp 的地方）。好处是业务语义可脱离 MCP 传输层用内存后端做快速单测——13 个语义用例无需任何 HTTP 与协议栈。
  - **验证结果**：`cargo test` **71 项全部通过**（MCP 语义 21、路由协议级端到端 15、P2 存量 35）；`cargo build` 与 `cargo clippy --all-targets` **零告警**；运行时冒烟：`initialize` 返回 `serverInfo.name=memora`、`tools/list` 返回 9 个工具、`tools/call` 完成「建实体 → 建关系 → 追加中文观测 → 中文片段检索 → 删除」全链路，数据落盘至 `data/smoke/mem.db`（含 WAL），无 token=401、Host 不在白名单=403、实体不存在=`-32602`。
  - **关键实现取舍**：
    1. **无状态传输**（`legacy_session_mode = false` + `json_response = true`）：工具都是「一问一答」，服务端无需推送；无会话即无「会话 id 被另一项目 token 复用」的越权面，也省掉会话表的内存驻留（契合 2GB 目标）。代价是每请求重新解析 token→项目（元库单次索引查询 + 后端连接缓存命中），相对一次记忆读写可忽略。
    2. **仓库经请求扩展注入，而非工具持有**：rmcp 会把 HTTP 请求的 `Parts`（含 Bearer 中间件插入的 `ProjectRepos`）挂到 JSON-RPC 请求扩展上，工具从 `RequestContext` 取出即可。因此服务对象可以完全无字段状态，工厂闭包只需 `|| Ok(MemoraServer::new())`。
    3. **错误码分流**：入参非法 / 实体不存在 → `-32602`（调用方可修正）；后端不可用 / 项目 id 非法 → `-32603`（服务端故障）。混为一谈会误导代理去改参数。
    4. **回读 id 而非回显请求**：关系与观测的 id 是后续删除的唯一凭据，故 `create_relations` / `add_observations` 写后回读。代价是 `create_relations` 需要读全图过滤（仓库契约未提供按三元组反查），在个人 / 小团队记忆规模下可接受，已在此显式记录。
    5. **删除类返回语义统一为「诚实可算者才回读」**：`delete_entities` 用 `open_nodes`（名字是主键，O(k)）真实回读「哪些名字存在过」；`delete_observations` / `delete_relations` 无按 id 的存在性查询，为避免为一次删除引入全图扫描，只回显请求 id（去重保序），并在文档中写明删除是幂等的。
    6. **Host 白名单必须可配**：rmcp 默认只信回环 Host。公网经 Caddy 反代会透传真实域名，若不暴露该配置项，线上表现为费解的 `403`。故新增 `MCP_ALLOWED_HOSTS`（默认仍取最小权限的回环），并新增路由测试固定「非白名单 Host → 403」。
    7. **工具清单双保险**：`TOOL_NAMES` 是「对外承诺的工具面」的单一来源，既用于启动自检（`build_service` 内断言宏注册的工具与清单一致，不一致即启动失败），也用于路由测试校验 `tools/list` 的实际返回值。
    8. **camelCase 入参别名**：接受官方 schema 的 `entityType` / `relationType` / `entityName` / `from` / `to`，容忍模型照抄旧 schema；别名不进入 JSON Schema。
    9. **`ProjectRepos` 的诊断字段落到了实处**：`project_id` / `backend` 在生产代码中被读取——Bearer 中间件在解析成功后打一条 debug 归属日志，用于排查「跨项目误写」。
- **P4.1 管理面已完成**：REST 项目生命周期可用，项目不再依赖手工改元库。
  - 新增模块：`admin.rs`（管理面 handler + DTO）；`meta.rs` 增加 `find_project` / `delete_project`；`storage` 的 `StorageBackend` 增加 `drop_project`，`contract.rs` 增加**后端级契约** `run_backend_contract`（双后端各跑一遍）；`reply.rs` 增加 400 / 404 / 409 构造器；`auth.rs` 增加 `generate_token` / `random_hex`；`domain.rs` 的 `Graph::*_count` 从 `#[cfg(test)]` 提回生产路径（供统计使用）。
  - **验证结果**：`cargo test` **93 项全部通过**（管理面协议级 11、后端级契约 5、P3 存量 77）；`cargo build` 与 `cargo clippy --all-targets` **零告警**；运行时冒烟：`POST /projects` 201 并签发 token → 新 token 立刻可用（`initialize` 返回 `memora`、写入实体、追加中文观测、中文检索命中）→ `stats` 反映写入计数 → `DELETE` 200 `data_removed=true` 且目录消失、旁项目数据完好 → 注销后 `stats`/重复注销 404、旧 token 401；`../evil` 400、`postgres` 400 且附带 `available_backends`、重复 id 409。
  - **关键实现取舍**：
    1. **注销顺序：先物理数据、后登记行**。反序时若删文件失败，会留下「登记行已消失、数据仍在盘上」的不可见残留（违背删除承诺且无法重试自愈）；本顺序最坏是「文件已删、登记行尚在」，重试一次即可收敛（详见 §5.2）。
    2. **后端未注册仍允许注销登记**，但必须如实回传 `data_removed=false`。否则管理面会永久残留一个既用不了也删不掉的死项目（本仓库的测试环境就有 `backend=postgres` 的这类项目）。
    3. **`drop_project` 不给默认实现**：留默认空实现等于允许新后端「忘记」清理逻辑。同时把它纳入共享的**后端级契约**（取用幂等 / 项目隔离 / 丢弃项目 / 丢弃幂等 / 非法 id 拒绝），使「换后端不改业务代码」这一承诺延伸到项目生命周期。
    4. **文件后端必须先摘连接缓存再删目录**：留着已打开的连接会让进程继续持有旧 inode（Unix 下删除只是 unlink），表现为「项目已删除但仍有写入落在不可见的文件上」。删除整个项目目录而非只删 `mem.db`，因为 WAL 模式还有 `-wal` / `-shm`，漏删即等于「声称删除但数据仍可恢复」。
    5. **契约刻意不断言「旧句柄立刻读到空」**：旧句柄可能仍指向被 unlink 的旧文件。契约只承诺「此后重新取用得到全新空项目」——这才是管理面与后续请求真正依赖的语义。
    6. **明文 token 绝不落库**，并有测试直接扫描元库全部相关文件（主库 + WAL / journal）断言 token 字节序列不存在。只扫主库会让该断言变成永真的空断言，故 `meta_db_bytes` 覆盖全部附属文件。
    7. **400 附带 `available_backends`**：请求了未编译的后端属「调用方可自行修正」，回传可用列表让运维无需翻文档即可改对参数——这是「400 vs 500」分流约定的具体兑现。
    8. **先查后插**：管理面是低频人工操作，竞态窗口可接受；库层唯一约束仍兜底（并发插入时后到者得到 500 而非悄然覆盖）。刻意不为一次管理操作引入事务性「插入或返回冲突」的复杂度。
    9. **统计口径与客户端同源**：`stats` 走 `MemoryRepository::read_graph` 而非自己写 SQL 计数，避免出现两套计数逻辑；代价是一次全图读取，在个人 / 小团队记忆规模下可接受。
    10. **`DEFAULT_BACKEND` 真正成为单一来源**：P2 遗留的三处内联字面量（建表 SQL、ALTER 回填、常量）收敛为常量插值，并新增用例钉住它与 `BackendKind::SqliteFile` 的一致性——否则建表默认值会与运行期后端注册表悄悄脱节。
- **P4.2 运行形态已完成**：单二进制自带进程管理（无 systemd 也能运维），配置来源显式化，压测发现并修复一处并发缺陷。
  - 新增模块：`cli.rs`（子命令与全局选项解析）、`pidfile.rs`（PID 文件与进程存活探测）、`daemon.rs`（`start`/`stop`/`restart`/`status`/`init` + 健康自检 + 部署模板渲染）；新增 `deploy/`（`memora.service` / `Caddyfile` / `env.template`，经 `include_str!` 内嵌）；`config.rs` 改为四层来源 + 显式探测；`main.rs` 按子命令分发并接入优雅退出；`Cargo.toml` 新增 `libc`（纯 FFI 声明）。
  - **验证结果**：`cargo test` **144 项全部通过**（本期新增 54：CLI 14、PID 11、daemon 12、配置 14、并发回归 3）；`cargo build` 与 `cargo clippy --all-targets` **零告警**；`release` 二进制 13MB。运行时冒烟覆盖 `init` → `start`（健康自检通过才返回 0）→ `status` → `restart` → `stop` 全链路，退出码语义逐条核对（重复 start=1、未运行 status=3、配置类故障=2、未知子命令=2）；并发压测 160 次工具调用（约 302 调用/秒）零失败、统计一致、跨项目零泄漏。
  - **关键实现取舍**：
    1. **并发建池缺陷（压测发现，本期最有价值的修复）**：`repositories_for` 是「查缓存 → 建池 → 回填」，两步之间存在 await 点，并发首次访问会各自建出一个指向**同一文件**的连接池；而建池时要执行 `PRAGMA journal_mode = WAL`，该 PRAGMA 需要**独占锁且不受 `busy_timeout` 保护**（SQLite 在独占锁场景遇 BUSY 立即返回以避免死锁），于是必然有一方以 `database is locked`(SQLITE_BUSY) 失败。修复是按项目串行化建池（每项目一把闸门 + 双重检查），闸门条目随项目生命周期清理。**注意这不是「等待超时」问题**：sqlx 默认的 5 秒 busy_timeout 对此无效。此缺陷在所有串行验证中都不会出现，只有真实并发才暴露。
    2. **混合形态而非纯自守护**：纯自守护不能自动拉起崩溃进程，对长期记忆服务是硬伤；保留 `run` 后 systemd 退化为可选组件，两种场景都不妥协（见 §8.1）。
    3. **`start` 以 `/health` 通过为唯一成功判据**：自守护最常见、最恶劣的失败模式是「命令返回 0 但服务其实没起来」。失败时回滚（终止子进程 + 删除 PID 文件），并把日志路径写进错误信息。健康探测同时校验**响应体**含 `ok`，仅凭 200 会在端口被其他服务占用时误判。
    4. **子进程显式注入已解析配置**：`start` 把 `PORT`/`DATA_DIR`/`ADMIN_TOKEN`/`STORAGE_BACKEND`/`MCP_ALLOWED_HOSTS` 注入子进程，而非让它重新探测 `.env`。否则父子 CWD 不一致时配置会漂移，表现为「`start` 时是 6789、实际监听 7000」。空值不注入，避免用空串覆盖子进程的有效配置。
    5. **零新依赖实现自守护**：`setsid` 经 `libc`（`CommandExt::pre_exec` 本就是 unsafe）；优雅退出用 `tokio::signal::unix`（tokio 已带 `full`）；健康探测与退出等待是手写的极简 HTTP/1.1 GET 与 `kill(pid, 0)` 轮询，为此引入 HTTP 客户端栈不划算。CLI 解析同样手写——选项面仅 6 个，clap 会带来可观的编译时间与体积，与「2GB 服务器上的单二进制」定位不符。
    6. **`.env` 来源显式化**：原先 `dotenvy::dotenv().ok()` 依赖 CWD 且失败被静默吞掉，症状是「`.env` 明明填了却没生效」而日志无线索。现在探测路径写入日志（未命中时列出全部候选）、`--env-file` 缺失或语法错误硬失败、`PORT` 非法值硬失败。解析逻辑（`env_candidates` / `resolve_port` / `resolve_text`）做成纯函数，使配置语义可被单测穷尽覆盖，而不必在测试中改进程环境（并发测试下不安全）。
    7. **修正 `RUST_LOG` 的加载顺序**：原先日志初始化早于配置加载，导致写在 `.env` 里的 `RUST_LOG` **永远不生效**。现改为「解析命令行 → 加载配置（含 `.env`）→ 初始化日志」，并由冒烟实测确认（`.env` 中 `RUST_LOG=debug` 后能观察到 `DEBUG memora::auth: rejected request`）。
    8. **恢复 SIGPIPE 默认处置**：Rust 运行时忽略 SIGPIPE，导致 `memora restart | head -1` 这类用法在管道提前关闭时把 `println!` 的 EPIPE 变成 panic 并打印堆栈。启动最早期恢复默认处置后，进程被 SIGPIPE 正常终止，符合 Unix 工具惯例。
    9. **配置来源日志只对启动类命令打印**：`stop` / `status` 每次执行都打一遍只会变成噪声，而它们的输出已包含 PID 文件路径，足以定位目录。
    10. **部署模板内嵌二进制**：`deploy/` 三份模板经 `include_str!` 编译进二进制，`memora init` 因而**不依赖部署目录**——这与「单文件交付」的定位一致，也避免「模板文件没跟着二进制走」的部署事故。Caddyfile 用 `handle` 块而非顶层 `reverse_proxy` + 兜底 `respond`：Caddy 的 directive 排序会把 `respond` 提到 `reverse_proxy` 之前执行，从而拦掉代理规则。
    11. **PID 文件内容损坏时报错而非视作「未运行」**：后者会直接覆盖一个可能仍在运行的服务。存活探测用 `kill(pid, 0)`，且 **`EPERM` 判为存活**（进程存在但不属于当前用户），其余错误保守判为存活。
    12. **`stop` 幂等成功而 `status` 未运行返回 3**：脚本可无条件调用 `memora stop`，而「是否在运行」的判断交给 `status` 的退出码（遵循 LSB init 脚本惯例）。

每期产出完整可运行代码与测试，评审通过后再推进下一期。

---

## 10. 风险与权衡

- **自维护成本**：需自行跟进 MCP 协议变更与客户端兼容。缓解：对齐官方 MCP Memory 语义，降低适配风险。
- **SQLite 并发**：单写者模型，项目内串行写入即可；多项目互不影响。高并发非目标场景。
- **语义检索首版偏弱**：仅 FTS5；如需语义再启远程 embedding，不阻塞首版交付。

---

## 11. 后续扩展（非首版）

- 记忆版本 / 回滚（类 Nocturne 的 diff / 回滚）
- 远程 embedding 语义检索
- 可选 Web 看板（复用 Axum 静态托管）
- 导入 / 导出（SQLite 文件即天然备份）

---

## 12. 持久化抽象与多后端支持

### 12.1 设计目标
使持久化后端可互换——无论是当前的「SQLite 每项目一文件」，还是将来可能的「单 SQLite + 租户列」「Postgres（schema / 租户列）」「内存测试库」——且**新增或切换后端时改动点收敛到最小**：业务层（MCP 工具、REST 管理层、路由）完全不感知具体存储实现。不为任何具体后端提前耦合。

### 12.2 采用的模式
- **Repository 模式**：将领域操作（实体 / 关系 / 观测 / 会话的 CRUD 与检索）抽象为 trait，屏蔽 sea-orm 与具体 SQL 方言。
- **Strategy + Abstract Factory**：`StorageBackend` 按 `project_id` 产出对应的 Repository 集合；启动时按配置（`STORAGE_BACKEND`）选择具体后端实现。

二者组合后，业务代码只依赖 trait，后端的选择与构造全部集中在工厂一处。

### 12.3 核心抽象（P2 已落地）
```rust
// 记忆领域仓库（后端无关）；Node ≡ Entity（观测内嵌），统一为一个模型避免互相转换
#[async_trait]
pub trait MemoryRepository: Send + Sync {
    async fn create_entities(&self, e: Vec<EntityInput>) -> Result<(), StorageError>;
    async fn create_relations(&self, r: Vec<RelationInput>) -> Result<(), StorageError>;
    async fn add_observations(&self, o: Vec<ObservationInput>) -> Result<(), StorageError>;
    async fn delete_entities(&self, names: &[String]) -> Result<(), StorageError>;
    async fn delete_observations(&self, ids: &[i64]) -> Result<(), StorageError>;
    async fn delete_relations(&self, ids: &[i64]) -> Result<(), StorageError>;
    async fn read_graph(&self) -> Result<Graph, StorageError>;
    async fn search_nodes(&self, q: &str) -> Result<Vec<Entity>, StorageError>; // 方言差异锁死在此方法内
    async fn open_nodes(&self, names: &[String]) -> Result<Vec<Entity>, StorageError>;
}

// 项目生命周期（P4 落地）：丢弃某项目 = 释放资源 + 删除其持久化数据（幂等）
// 刻意不给默认实现——「数据在哪」是后端知识，留空实现等于允许后端「忘记」清理逻辑
#[async_trait]
pub trait StorageBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    async fn repositories_for(&self, project_id: &str) -> Result<ProjectRepos, StorageError>;
    async fn drop_project(&self, project_id: &str) -> Result<(), StorageError>;
}

// 会话领域仓库（P5 落地，同样后端无关）
#[async_trait]
pub trait ConversationRepository {
    async fn log_turn(&self, s: SessionInput) -> Result<()>;
    async fn get_session(&self, session_id: &str) -> Result<Vec<Turn>>;
    async fn list_sessions(&self, prefix: Option<&str>) -> Result<Vec<String>>;
}

// 中间件注入载体：一次解析出「项目 + 后端 + 仓库集合」，handler 只依赖 trait 对象
#[derive(Clone)]
pub struct ProjectRepos {
    pub project_id: String,
    pub backend: BackendKind,          // 该项目实际使用的后端
    pub memory: Arc<dyn MemoryRepository>,
    // P5 追加：pub conversation: Arc<dyn ConversationRepository>,
}

// 后端选择（Strategy + Factory）；注意 repositories_for 必须为 async（首次访问需建库 / 建表）
#[async_trait]
pub trait StorageBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    async fn repositories_for(&self, project_id: &str) -> Result<ProjectRepos, StorageError>;
}
```
具体实现（每个独立一个文件）：
- `SqliteFileBackend`：当前默认，每项目 `data/{project_id}/mem.db`，连接按项目缓存。
- `SqliteSingleBackend`：单库，所有表加 `project_id` 列过滤（P6）。
- `PostgresBackend`：单连接池，按 `BackendMode` 选 schema 隔离或租户列隔离（P6）。
- `InMemBackend`：纯内存（HashMap），仅供测试。

工厂为 `StorageRegistry`：`from_config` 注册本构建可用的实现（当前 `sqlite_file` + `in_mem`）并校验配置默认后端，`get(kind)` 按项目登记值取实现；未注册的后端在访问时返回 500 而非 401。

`ProjectRepos` 的 `project_id` / `backend` 在 P3 已投入实际用途：Bearer 中间件解析成功后以此打一条 debug 归属日志，用于排查「跨项目误写」。若不这样用，它们在 MCP 工具层就只是两个无人读取的字段（P2 的统计占位端点是唯一读者，该端点已随 P3 移除）——编译器会以 `never read` 告警指出这一点。

### 12.4 依赖注入
鉴权中间件解析出 `project_id` 后，调用 `state.backend.repositories_for(project_id)`，将 `Arc<dyn MemoryRepository>`（及 `Arc<dyn ConversationRepository>`）注入请求扩展；MCP / REST handler 仅从扩展取出 trait 对象使用，不引用任何具体后端或 sea-orm 实体。由此 handler 层对存储实现完全盲视。

### 12.5 改动点分析（新增一种后端时）
| 关注点 | 隔离位置 | 新增后端时的改动 |
|---|---|---|
| 项目 → 存储绑定 | `StorageBackend::repositories_for` | 新增 1 个 impl 文件 |
| SQL 方言（FTS5 / tsvector） | 后端内 `search_nodes` | 仅在 impl 内 |
| `db_path` / schema 语义 | 对应后端 impl 内 | 不扩散 |
| 租户过滤（列 vs schema） | Repository 方法内 | 仅在 impl 内 |
| Handler / MCP / REST 代码 | 仅依赖 trait | **零改动** |
| 项目数据清理（`drop_project`） | 后端 impl 内（文件 / schema / 内存 map） | 仅在 impl 内 |
| 启动选择 | 工厂 `match STORAGE_BACKEND` | +1 个分支 |

**结论**：新增一种持久化 = 1 个 impl 文件 + 工厂 1 个分支；切换生效后端 = 改 1 个环境变量。**业务层代码零改动**。`projects` 表新增 `backend` 字段记录该项目所用后端，使不同项目可并存不同存储策略。

### 12.6 测试收益
`InMemBackend` 使 P2 / P3 的单元测试与集成测试无需真实文件或外部 DB，直接验证 Repository 契约；未来 Postgres 后端可加独立集成测试（testcontainers 或本地 PG 实例）。

### 12.7 与现有代码的关系
- P1 仅打开 `_meta.db` 并解析 `project_id`，P2 在此窗口一次性引入本抽象，避免文件逻辑写散在 handler 里。
- sea-orm 作为各后端 impl 内部的 SQL 执行层保留（实体落在 `src/entity.rs::memory::{entities, observations, relations}`），其跨库可移植性收益不丢弃；`mem.db` 的原始 SQL 仅用于 FTS5 索引维护与 PRAGMA 调优。Repository 边界在其之上再包一层，使「换后端」从「跨模块重构」降为「局部实现」。
- 入参规范化（去空白、空值拒绝、类型缺省为 `unknown`）由 `src/storage/normalize.rs` 统一提供，两个后端共用，避免语义漂移。

### 12.8 文件布局与契约测试用法
```
src/
  main.rs                   入口：子命令分发（print 形态与自守护形态）、优雅退出、日志初始化
  cli.rs                     命令行解析（子命令 + 全局选项 + --help/--version 短路）
  daemon.rs                  自守护进程管理：start/stop/restart/status/init + 健康探测 + 模板渲染
  pidfile.rs                 PID 文件读写与清理、kill(pid,0) 存活探测、SIGTERM/SIGKILL 发送
  domain.rs                 领域模型（Entity / Relation / Observation / Graph + 各类 Input）
  entity.rs                 sea-orm 实体：project（元库）+ memory::{entities, observations, relations}
  meta.rs                   元库建表 / backend 列迁移 / 项目登记查询（含 find / delete）
  config.rs                 运行配置：四层来源叠加 + .env 显式探测（EnvSource）+ 纯函数式解析
  error.rs                  AuthError / StorageError / ProjectResolveError
  reply.rs                  统一 400 / 401 / 404 / 409 / 500 响应
  state.rs                  AppState：元库连接 + 配置 + StorageRegistry；token → 仓库集合的解析入口
  auth.rs                   Bearer 与 ADMIN_TOKEN 中间件（注入 ProjectRepos）+ token 生成与散列
  admin.rs                  REST 管理面 handler：项目创建 / 注销 / 用量统计（只依赖 trait 与注册表）
  routes.rs                 /health、/mcp（Streamable HTTP 服务）、/api/v1/{projects,...}
  storage/
    mod.rs                  BackendKind / ProjectRepos / StorageBackend / StorageRegistry / project_id 校验
    repo.rs                 MemoryRepository trait（后端无关契约）
    normalize.rs            入参规范化与校验（后端共用，含读/删路径的名字规范化）
    contract.rs             共享契约测试集（#[cfg(test)]）：run_all（仓库级）+ run_backend_contract（项目生命周期级）
    sqlite_file.rs          SQLite 文件后端 + FTS5 索引维护 + LIKE 子串回退 + 项目目录删除
                            （含按项目串行化的建池闸门，见 §9 P4.2 取舍 1）
    mem.rs                  内存后端（测试参照实现）
  mcp/
    mod.rs                  /mcp 服务装配（无状态 Streamable HTTP 配置 + 工具清单自检）
    dto.rs                  9 个工具的入参 / 出参契约（serde + JsonSchema）
    tools.rs                9 个工具的语义实现（只依赖 MemoryRepository，可脱离协议层单测）
    server.rs               MemoraServer：rmcp 宏声明的工具 + 从 RequestContext 取 ProjectRepos
deploy/
  memora.service            systemd unit 模板（`memora init` 渲染后输出）
  Caddyfile                 反向代理模板（同上）
  env.template              `.env` 模板（同上）
```

**分层依赖方向**（单向，不得回指）：
`main` → `cli` / `daemon` / `routes`；
`routes` → `mcp::server` → `mcp::tools` → `storage::repo`（trait）/ `domain`；
`routes` → `admin` → `storage`（trait / 注册表）/ `meta` / `domain`；
`daemon` → `config` / `pidfile`（**不触碰存储与路由**，进程管理与业务完全解耦）；
`mcp::dto` 只被 `tools` 与 `server` 使用。**`rmcp` 仅出现在 `mcp::server` 与 `mcp::mod`**，
故工具语义的测试不依赖任何协议栈；**`admin` 不引用任何具体后端**，只经 `StorageRegistry` 取 trait 对象。

**新增后端时**：实现 `MemoryRepository` + `StorageBackend` 各一份（或复用同一份 Repository 实现），在 `StorageRegistry::from_config` 追加一行注册，并在该后端的测试模块中调用 `contract::run_all(&*repo).await`**与** `contract::run_backend_contract(&*backend).await` 即可获得与现有后端等价的行为保证。业务层（`routes.rs` / `mcp` / `admin`）无需改动。

---

*本文档为 AI 协作开发蓝图，后续各期按第 9 节分批实现与验收。*
