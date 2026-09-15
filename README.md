# Memora（忆庐）

> 给你的 AI 客户端一间自己的记忆小屋 —— 单二进制、按项目硬隔离的自托管长期记忆服务。

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](#许可证)
[![Rust](https://img.shields.io/badge/rust-edition%202021-orange.svg)](https://www.rust-lang.org/)
[![MCP](https://img.shields.io/badge/MCP-Streamable%20HTTP-8a2be2.svg)](https://modelcontextprotocol.io/)
[![Memory](https://img.shields.io/badge/常驻内存-目标%20%3C%20100MB-brightgreen.svg)](#资源占用)

**Memora（忆庐）** 是一个免费开源、单二进制的 AI 长期记忆服务。它通过标准 **MCP（Streamable HTTP）** 协议接入 Zed / WorkBuddy / Cursor / Claude 等 AI 客户端，把「用户偏好、项目约定、关键决策」沉淀到你自己机器上的 SQLite 文件里，并按项目做**物理隔离**——数据是你的，谁也拿不走，也不会上传到任何第三方。

一个二进制，一条 `curl` 建项目，两行 JSON 接入客户端。没有向量数据库，没有外部服务，没有托管账单。

---

## 目录

- [为什么需要它](#为什么需要它)
- [核心特性](#核心特性)
- [快速开始](#快速开始)
- [接入 AI 客户端](#接入-ai-客户端)
- [MCP 工具](#mcp-工具)
- [管理 API](#管理-api)
- [架构](#架构)
- [存储后端可插拔](#存储后端可插拔)
- [部署到 2GB 服务器](#部署到-2gb-服务器)
- [设计取舍速览](#设计取舍速览)
- [质量与测试](#质量与测试)
- [路线图](#路线图)
- [文档](#文档)
- [许可证](#许可证)

---

## 为什么需要它

调研现成方案后，在「**2GB 内存 + 命令行 Ubuntu + 公网 + 鉴权 + 项目隔离**」这组约束下，没有一款同时满足：

| 方案 | 本场景下的问题 |
|---|---|
| Hindsight（全量） | API + Worker 各需约 2GB，直接超预算 |
| Hindsight（slim） | 临界可行，但逼近内存上限，容错空间极小 |
| Nocturne | 开源、Docker 一键，但鉴权是**单一共享 token**，没有项目级隔离 |
| 官方 MCP Memory | 零数据库、极轻，但**无原生鉴权、无项目隔离** |

结论：要么内存超标，要么鉴权与隔离不是原生的。自建一个单二进制方案，可同时把**内存、隔离、鉴权、可控性**拉进理想区间，并且复用既有 Rust 技术栈，维护成本可控。这就是 Memora。

### 与官方 MCP Memory 的关系

**语义对齐，而非替代**。实体 / 关系 / 观测三元组与官方实现一致，客户端零改造即可接入。差异只在两处：

1. **检索更强**：额外支持中文与词内片段检索（FTS5 未命中时自动退化为子串扫描）。
2. **返回更全**：写操作的返回结构带 `id` 与时间戳，让「写完能改、改完能删」的闭环成立。

---

## 核心特性

| 特性 | 说明 |
|---|---|
| **单二进制** | `cargo build --release` 产出约 10–30MB 的可执行文件，无外部服务依赖（可选远程 embedding 除外） |
| **低资源** | 目标常驻内存 < 100MB；无 GC，SQLite 单文件即数据库 |
| **按项目硬隔离** | 每个项目独立 `data/{project_id}/mem.db`——物理隔离；再叠加路径/token 绑定做逻辑隔离，双保险互不越权 |
| **原生鉴权** | 项目 token（`Authorization: Bearer`）+ 独立的管理员 token（`ADMIN_TOKEN`），两套体系分离 |
| **标准 MCP** | Streamable HTTP 传输，9 个对齐官方语义的记忆工具，兼容主流 MCP 客户端 |
| **自带进程管理** | caddy 式 CLI：`run` / `start` / `stop` / `restart` / `status` / `init`——没有 systemd、没有 root 也能运维 |
| **可换成能的后端** | Repository + 共享契约测试：新增一种持久化后端 = 1 个 impl 文件 + 1 行注册，业务层**零改动** |
| **TDD 交付** | 141 项测试全通过，`clippy --all-targets` 零告警；后端行为由一份契约同时约束 |

---

## 快速开始

```bash
# 1. 构建（无外部依赖，开箱即用）
cargo build --release          # 产物：target/release/memora

# 2. 生成配置与部署模板（.env / systemd unit / Caddyfile 片段）
./target/release/memora init

# 3. 启动（二选一，共用同一个二进制）
./target/release/memora start  # 自守护：脱离终端、日志落盘、健康自检通过才返回 0
./target/release/memora run    # 前台：交给 systemd / 容器托管

# 4. 验证
./target/release/memora status # 运行中；未运行时退出码为 3，便于脚本判断
curl -s localhost:6789/health  # 公开端点，无需鉴权
```

创建第一个项目并拿到 token：

```bash
curl -s -X POST localhost:6789/api/v1/projects \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"project_id":"my-project"}'
```

```json
{"project_id":"my-project","token":"p_9f3c...","backend":"sqlite_file"}
```

> ⚠️ **明文 token 只在创建响应中出现一次**，服务端只保存它的 SHA-256。丢了只能重建项目——这是刻意的：数据库被拷走也无法反推出 token。

### 配置

配置优先级为 **命令行选项 > 进程环境变量 > `.env` > 内置默认值**。

| 变量 | 默认 | 说明 |
|---|---|---|
| `PORT` | `6789` | 监听端口 |
| `DATA_DIR` | `./data` | 数据目录（元库 + 各项目库） |
| `ADMIN_TOKEN` | — | 管理 API 凭据，**生产必须设置且足够随机** |
| `STORAGE_BACKEND` | `sqlite_file` | 新项目默认后端（既有项目以元库登记值为准） |
| `MCP_ALLOWED_HOSTS` | `localhost,127.0.0.1,::1` | MCP Host 白名单（防 DNS 重绑定），**公网部署必须加对外域名** |
| `RUST_LOG` | `info,sqlx=warn,sea_orm=warn` | 日志级别 |
| `EMBEDDING_API_KEY` | — | 可选，语义检索（规划中） |

---

## 接入 AI 客户端

服务是标准 MCP（Streamable HTTP）+ Bearer 鉴权，客户端只需连接 `https://<host>/mcp` 并在请求头带上项目 token。**同一项目 token 即同一份记忆**——多个客户端、多台机器配置同一个 token，天然共享。

> **部署前置（最容易踩的坑）**：MCP 传输层强制校验请求的 `Host`（防 DNS 重绑定），默认只接受回环地址。经 Caddy 反代时反代会透传真实域名，因此公网部署**必须**把域名写进白名单，否则 MCP 请求会被协议层以 `403` 拒绝，而错误信息并不指向这个配置项：
>
> ```bash
> MCP_ALLOWED_HOSTS=mem.example.com,localhost,127.0.0.1
> ```

**WorkBuddy**（`~/.workbuddy/mcp.json`）与 **Cursor**（`~/.cursor/mcp.json`）：

```json
{
  "mcpServers": {
    "memora": {
      "url": "https://mem.example.com/mcp",
      "headers": { "Authorization": "Bearer p_9f3c..." }
    }
  }
}
```

**Zed**（`~/.config/zed/settings.json`）：

```json
{
  "context_servers": {
    "memora": {
      "url": "https://mem.example.com/mcp",
      "headers": { "Authorization": "Bearer p_9f3c..." }
    }
  }
}
```

**Claude Code**：

```bash
claude mcp add --transport http memora https://mem.example.com/mcp \
  --header "Authorization: Bearer p_9f3c..."
```

> 若客户端尚不支持直连 Streamable HTTP，用官方 `mcp-remote` 做 stdio 桥接，一行命令转发到同一 URL。

### 别忘了给代理两条记忆指令

MCP 工具由模型**按需**调用，不会自动落盘。请把下面两条规则固化进客户端的系统提示或 Agent profile，否则记忆库会长期为空：

1. **持久化规则**：对话中出现用户偏好、项目约定、关键决策、待办时，主动调用 `create_entities` / `add_observations` 写入记忆。
2. **恢复规则**：新会话开始时先调用 `read_graph` / `search_nodes` 拉取相关记忆，作为上下文前置。

---

## MCP 工具

9 个工具，语义对齐官方 MCP Memory。**项目不在 URL 里**——由 `Authorization: Bearer <项目 token>` 解析出 `project_id`，从根上避免「路径与 token 指向不同项目」这类不一致。

| 工具 | 关键参数 | 行为 | 返回 |
|---|---|---|---|
| `create_entities` | `entities[{name, entity_type}]` | 批量建实体，重名忽略（不覆盖原类型） | 请求名在调用后的状态（含观测） |
| `create_relations` | `relations[{from_name, to_name, relation_type}]` | 批量建关系，三元组重复跳过 | 实际落库的关系（**含 id**） |
| `add_observations` | `observations[{entity_name, contents[]}]` | 追加观测；实体不存在报 `-32602` | 受影响实体（**含新观测 id**） |
| `delete_entities` | `names[]` | 级联删除关系 / 观测 / 检索索引 | 确实存在并被删除的名字 |
| `delete_observations` | `ids[]` | 按 id 删观测（幂等） | 请求 id（去重保序回显） |
| `delete_relations` | `ids[]` | 按 id 删关系（幂等） | 请求 id（去重保序回显） |
| `read_graph` | — | 返回全图 | `{entities[], relations[]}` |
| `search_nodes` | `query` | FTS5 全词检索（bm25 排序）；无结果时退化为子串扫描 | 命中实体（带完整观测集，按相关度排序） |
| `open_nodes` | `names[]` | 按名取节点明细 | 命中实体（按 name 升序） |

**三个值得说明的接口约定：**

- **写操作回读 id**：`create_relations` / `add_observations` 是刻意回读的——`delete_*` 只接受 id，不回读调用方就拿不到句柄，记忆的「可修订」闭环无法成立。
- **入参别名**：同时接受官方 schema 的 camelCase 写法（`entityType` / `relationType` / `entityName` / `from` / `to`），容忍模型照抄旧 schema；别名不进 JSON Schema，对外只宣传规范字段名。
- **错误码分流**：入参非法 / 实体不存在 → `-32602`（调用方改参数即可）；后端不可用 / 项目 id 非法 → `-32603`（服务端故障）。混为一谈会误导代理去改参数。

---

## 管理 API

前缀 `/api/v1`，需 `ADMIN_TOKEN`（项目 token 不能当管理凭据用）。

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/projects` | 创建项目（生成 token 并返回明文，`201`） |
| `GET` | `/projects` | 项目列表（含各自后端） |
| `DELETE` | `/projects/{id}` | 注销项目（删登记行 + 删项目数据） |
| `GET` | `/projects/{id}/stats` | 实体 / 关系 / 观测统计 |
| `GET` | `/health` | 健康检查（公开，无鉴权） |

**状态码语义与 MCP 侧同源**——判断依据是「调用方能否自行修正」：

| 码 | 含义 | 典型场景 |
|---|---|---|
| `400` | 入参可修正 | `project_id` 非法（目录穿越 / 分隔符 / 空白）；请求的后端未编译进本构建（响应附带 `available_backends` 列表） |
| `401` | 凭据问题 | 缺失或错误的 `ADMIN_TOKEN` |
| `404` | 资源不存在 | 项目未登记 |
| `409` | 资源冲突 | `project_id` 已被占用 |
| `500` | 服务端故障 | 元库不可用、后端未注册、熵源不可用 |

统计口径与客户端**同源**：`stats` 走 `read_graph` 派生，而不是另写一套 SQL 计数，避免出现两套不一致的计数逻辑。

---

## 架构

```
                 HTTPS
   MCP Client ──▶ Caddy ──▶ Axum 单二进制
                           │
        ┌──────────────────┼────────────────────┐
        ▼                  ▼                    ▼
   鉴权中间件         路由 / 项目解析        存储抽象层
   Bearer→project    /mcp                 按 project_id 产出 Repository
                           │
                  ┌────────┴────────┐
            MCP 工具层          REST 管理层
        9 个标准记忆工具      项目增 / 删 / 统计
```

**分层依赖单向，不得回指**：

```
main   → cli / daemon / routes
routes → mcp::server → mcp::tools → storage::repo (trait) / domain
routes → admin      → storage (trait / 注册表) / meta / domain
daemon → config / pidfile          （进程管理不触碰存储与路由）
mcp::dto 只被 tools 与 server 使用
```

| 层 | 职责 | 关键约束 |
|---|---|---|
| `cli.rs` | 子命令与全局选项解析 | 零依赖手写；无子命令等价于 `run`，既有部署零改动 |
| `daemon.rs` | `start` / `stop` / `restart` / `status` / `init` | 以 `/health` 通过为唯一成功判据；**不依赖存储层** |
| `pidfile.rs` | PID 文件与进程存活探测 | `kill(pid,0)` 判定，`EPERM` 视为存活；损坏内容报错而非当作「未运行」 |
| `mcp/dto.rs` | 9 个工具的入参 / 出参契约（serde + JsonSchema） | 只描述协议，不含逻辑 |
| `mcp/tools.rs` | 工具语义实现 | **只依赖 `Arc<dyn MemoryRepository>`**，可脱离 MCP 传输层用内存后端快速单测 |
| `mcp/server.rs` | rmcp 宏装配 + 从请求上下文取仓库 | **唯一允许 `use rmcp` 的地方** |
| `admin.rs` | 项目生命周期 handler | 只经 `StorageRegistry` 取 trait 对象，不引用任何具体后端 |
| `storage/` | Repository 契约 + 后端实现 | 方言差异锁死在各自 impl 内 |

这套拆分带来一个直接好处：**工具语义的用例不需要任何 HTTP 与协议栈**即可验证。

---

## 存储后端可插拔

「换后端不改业务代码」不是口号，而是被一份**共享契约测试**钉住的事实。

| 后端 | 状态 | 存储策略 |
|---|---|---|
| `sqlite_file` | ✅ 默认 | 每项目一个 SQLite 文件 `data/{project_id}/mem.db`，连接按项目缓存 |
| `in_mem` | ✅ 已实现 | 纯内存 HashMap，测试参照实现 |
| `sqlite_single` | 🔜 预留 | 单库 + 租户列过滤 |
| `postgres` | 🔜 预留 | 单连接池，按 schema 或租户列隔离 |

两份契约同时约束每个后端：

- `contract::run_all`——**仓库级** 12 阶段契约：实体 CRUD 幂等、整批非法不落库、观测追加与 `EntityNotFound`、关系去重、检索命中名/类型/内容、CJK 子串、级联删除、幂等删除、跨项目隔离。
- `contract::run_backend_contract`——**项目生命周期级**契约：取用幂等、项目隔离、丢弃项目、丢弃幂等、非法 id 拒绝。

因此新增一种持久化后端 = **1 个 impl 文件 + 工厂里 1 行注册 + 2 行契约调用**；切换生效后端 = **改 1 个环境变量**。`projects` 表记录每个项目所用的后端，不同项目可以并存不同存储策略。

---

## 部署到 2GB 服务器

```bash
# 1. 编译（可选 musl 静态链接）
cargo build --release

# 2. 建 2GB swap 作安全垫，防峰值 OOM
sudo fallocate -l 2G /swapfile && sudo chmod 600 /swapfile
sudo mkswap /swapfile && sudo swapon /swapfile

# 3. 生成配置与系统模板
./target/release/memora init
```

两种运行形态，按需二选一：

| 形态 | 命令 | 适用 | 取舍 |
|---|---|---|---|
| **自守护** | `memora start` / `stop` / `restart` / `status` | 裸机、无 systemd、无 root | 脱离终端、日志落盘、健康自检通过才返回成功；**无法在进程崩溃后自动拉起** |
| **前台托管** | `memora run` + systemd | 希望开机自启与崩溃自愈 | 获得 `Restart=always`，这是自守护形态唯一不可替代的能力 |

> 两种形态共用同一份二进制——这正是不做「纯自守护」的原因：`Restart=always` 只能在 systemd 侧提供。

反代用 **Caddy** 自动签发 Let's Encrypt 证书，`memora init` 会渲染好配置片段。生产只需暴露三个前缀，其余一律 404：

```caddyfile
mem.example.com {
	handle /mcp*      { reverse_proxy 127.0.0.1:6789 }
	handle /api/v1/*  { reverse_proxy 127.0.0.1:6789 }
	handle /health    { reverse_proxy 127.0.0.1:6789 }
	handle            { respond 404 }   # 记忆库里是私有内容，不该有未预期的暴露面
	encode zstd gzip
}
```

SQLite 侧启用 `journal_mode=WAL` 与 `synchronous=NORMAL`，且**同一项目的连接池按项目串行创建**（WAL 的独占锁不受 `busy_timeout` 保护，并发建池必然 `database is locked`）。WAL 亦使「停机时优雅退出」成为必要——SIGTERM 后应用需自行收尾落盘。

---

## 设计取舍速览

这些决定解释了「为什么代码长这样」，也是本项目最值得一读的部分：

| 决策 | 取舍 |
|---|---|
| **注销顺序：先删物理数据，后删登记行** | 反序时若删文件失败，会留下「登记行没了、数据还在盘上」的不可见残留——既违背删除承诺又无法重试自愈。本顺序最坏是「文件已删、登记行尚在」，重试一次即收敛 |
| **明文 token 绝不落库** | 库内只存 SHA-256，并有测试扫描元库**全部**文件（主库 + WAL / journal）断言 token 字节序列不存在。只扫主库会让该断言变成永真的空断言 |
| **无状态 MCP 传输** | 工具都是「一问一答」，无需服务端推送。无会话即无「会话 id 被另一项目 token 复用」的越权面，也省掉会话表的内存驻留 |
| **仓库经请求扩展注入，而非工具持有** | rmcp 把 HTTP 请求的 `Parts`（含 Bearer 中间件插入的 `ProjectRepos`）挂到 JSON-RPC 请求扩展上，工具从中取出即可——服务对象因此可完全无字段状态 |
| **`StorageBackend::drop_project` 不给默认实现** | 留空实现等于允许新后端「忘记」清理逻辑。同时纳入后端级契约，让「换后端」的承诺延伸到项目生命周期 |
| **契约不断言「旧句柄立刻读到空」** | 旧句柄可能仍指向被 unlink 的旧文件。契约只承诺「此后重新取用得到全新空项目」——这才是管理面真正依赖的语义 |
| **同一项目的连接池必须串行创建** | `PRAGMA journal_mode=WAL` 需要独占锁，且**不受 `busy_timeout` 保护**（SQLite 在独占锁场景遇 BUSY 立即返回以避免死锁）。并发首次访问本会各自建出一个指向同一文件的连接池，其中一方必然 `database is locked`。修复：每项目一把建池闸门 + 双重检查。**这类缺陷在串行验证中永不出现，只有真实并发才暴露** |
| **不引入 clap** | 选项只有 6 个，而 clap 带来可观编译时间与约 300KB 体积增长，与「2GB 服务器上的单二进制」定位不符 |
| **Host 白名单必须可配** | rmcp 默认只信回环 Host；不暴露该配置项时，公网经反代的线上表现是费解的 `403` |

更多取舍（含每期的验证结果与踩坑记录）见 [`docs/memory-system-design.md`](docs/memory-system-design.md)。

---

## 质量与测试

| 项目 | 现状 |
|---|---|
| `cargo test` | **144 项全部通过** |
| `cargo clippy --all-targets` | **零告警** |
| 测试构成 | 仓库契约（双后端各跑一遍）、后端生命周期契约、MCP 工具语义（脱离协议栈）、路由协议级端到端（`initialize` / `tools/list` / `tools/call`）、CLI 与进程管理（子命令解析、PID 存活探测、退出码语义）、配置来源与优先级、并发写入回归、鉴权与隔离 |
| 并发验证 | 4 项目 × 40 次工具调用（每项目 8 并发）：**零失败**、统计与写入一致、跨项目零泄漏 |
| 开发方式 | 全程 TDD：先写测试并观察其失败，再写实现 |

值得单独一提的用例：**注入式恶意检索串不报错**、**跨项目误写被隔离**、**非白名单 Host 返回 403**、**明文 token 不在元库任何文件中出现**、**并发首次取用同一项目只建立一个连接池**（最后一条是压测发现的真实缺陷，见下节）。

---

## 路线图

| 期 | 内容 | 状态 |
|---|---|---|
| P1 | Axum + sea-orm 骨架、项目路由、Bearer 中间件、元库 | ✅ |
| P2 | 存储抽象 + 双后端 + 全链路依赖注入 | ✅ |
| P3 | MCP 工具层接入（rmcp，9 个工具） | ✅ |
| P4.1 | REST 管理面：项目增删 + 用量统计 | ✅ |
| P4.2 | 运行形态：自守护 CLI + 配置显式化 + systemd / Caddy + 优雅退出 | ✅ |
| P5 | 会话表与跨机器会话恢复（`log_turn` / `get_session` / `list_sessions`） | 🔜 |
| 后续 | 远程 embedding 语义检索、记忆版本与回滚、Web 看板、导入导出 | 💡 |

**非目标（首版范围外）**：内置大模型推理、复杂图谱推理 / 反思、多节点集群。

---

## 文档

| 文档 | 内容 |
|---|---|
| [`docs/memory-system-design.md`](docs/memory-system-design.md) | 完整技术设计蓝图：数据模型、接口契约、鉴权与多租户、检索策略、部署方案、分期计划与全部实现取舍 |
| [`deploy/`](deploy) | systemd unit、Caddyfile 片段、环境变量模板（由 `memora init` 渲染） |
| [`.env.example`](.env.example) | 全量配置项与注释 |

---

## 许可证

MIT（见仓库根目录 [`LICENSE`](LICENSE)）。自托管、自修改、自商用均无限制。

---

*Memora（忆庐）= memory + aura 的自造词，「庐」取「记忆的小屋」之意：一间建在你自己机器上、按项目分间、钥匙只在你手里的记忆小屋。*
