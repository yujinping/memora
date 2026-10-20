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
| **网页控制台** | `/admin` 用户名 + 口令登录即可建项目、复制 token、浏览记忆；口令只以 Argon2id 哈希落盘，会话 Cookie 与项目 token 严格隔离 |
| **标准 MCP** | Streamable HTTP 传输，9 个对齐官方语义的记忆工具 + 4 个交接（handoff）工具，兼容主流 MCP 客户端 |
| **自带进程管理** | caddy 式 CLI：`run` / `start` / `stop` / `restart` / `status` / `init`——没有 systemd、没有 root 也能运维 |
| **任务交接** | `memora handoff save/load/list/ack/rm` + 4 个 MCP 工具：跨客户端 / 跨机器接力零 ID 完成；设计见 `docs/handoff-integration-design.md` |
| **可换成能的后端** | Repository + 共享契约测试：新增一种持久化后端 = 1 个 impl 文件 + 1 行注册，业务层**零改动** |
| **TDD 交付** | 228 项测试全通过，`clippy --all-targets` 零告警；后端行为由一份契约同时约束 |

---

## 快速开始

```bash
# 1. 构建（无外部依赖，开箱即用）
cargo build --release          # 产物：target/release/memora

# 2. 生成配置与部署模板（config.toml / systemd unit / Caddyfile 片段）
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

### 网页控制台：不用 curl 也能建项目

脚本走 `Authorization: Bearer $ADMIN_TOKEN`，人走浏览器。服务启动后打开：

```
http://localhost:6789/admin
```

用 `memora init` 生成的控制台用户名 / 口令登录（口令在 `init` 的输出里**只显示一次**），
即可在页面上完成 **建项目 → 复制 token → 浏览记忆 / 交接 → 管理账号**。
在此之前，「怎么建一个项目」这件事只有脚本能回答——这是原始设计里一处真实的缺口，
补上的正是这条路径。

| 项 | 说明 |
|---|---|
| 前端技术栈 | **Vue 3 + Vite + Tailwind CSS v4**（`web/`），产物内嵌进二进制，**单文件即可发布** |
| 凭据形态 | 用户名 + 口令（默认种子用户名 `admin`）。口令以 **Argon2id** 哈希写入 `_meta.db` 的 `users` 表 |
| 账号从哪来 | `users` 表为空时，用 `config.toml` 的 `admin_user` / `admin_password_hash` **播种**第一个管理员；之后**以库为准**，改配置不再影响登录 |
| 多账号 | 控制台「账号」页可增删账号、改口令。删账号会立即踢掉该账号的全部会话；**最后一个账号禁止删除**（否则谁都被锁在门外） |
| 口令从哪来 | `memora init` 生成 20 位随机口令并只打印一次；忘记且进不去时重跑 `memora init --force` 重新播种 |
| 也可自己指定 | `--admin-password <明文>`（启动时即哈希，不落盘）、`--admin-password-hash <PHC>`，或环境变量 `ADMIN_PASSWORD` / `ADMIN_PASSWORD_HASH`。明文短于 8 字符**直接拒绝启动**——口令强度只在能拿到明文的那一刻可判 |
| 会话 | 内存会话表 + `HttpOnly; SameSite=Strict` Cookie，12 小时**绝对**过期（不滑动、重启即失效）；检测到 HTTPS 时自动加 `Secure` |
| 退出登录 | 页面上的「退出」，或 `DELETE /api/v1/session`（幂等，未登录也返回 204） |
| 关闭控制台 | 把 `config.toml` 的 `admin_password_hash` 留空**且**库里没有账号——只用 MCP 的部署不需要它。此时 `/admin` 明确提示「尚无控制台账号，请运行 `memora init`」，而不是给一个必然失败的登录框 |
| 公网部署 | 反代必须放行 `/admin*`、`/assets/*` 与 `/favicon.svg`（`deploy/Caddyfile` 已含）。页面本身不含任何数据 |
| 访问入口 | 控制台地址是 `https://<域名>/admin`。**根路径 `/` 返回 404 是设计行为**，不做跳转（见「部署到 2GB 服务器」一节的说明） |
| 页面安全 | 记忆内容一律**插值渲染**（不用 `v-html`，避免存储型 XSS）；CSP 已去掉 `'unsafe-inline'`（构建产物无内联脚本/样式） |
| token 轮换 | 项目页的「换 token」可只轮换 token 而不动数据（`POST /projects/{id}/token`），旧 token **立即**失效 |

**两套凭据严格隔离**，这条边界有专门用例守着，防止将来「顺手复用中间件」把它抹掉：

| 通道 | 凭据 | 可访问 |
|---|---|---|
| 脚本 / CI | `ADMIN_TOKEN`、项目 token | `ADMIN_TOKEN` → `/api/v1`；项目 token → `/mcp` |
| 浏览器 | 会话 Cookie | 仅 `/api/v1`，且**写请求要求同源**（CSRF 第二道防线，第一道是 `SameSite=Strict`）；**不能**用于 `/mcp` |

> 会话 Cookie 之所以不能用于 `/mcp`：`/mcp` 的鉴权只认 Bearer 项目 token。
> 浏览器里的一次登录不应等价于拿到某个项目的记忆读写权。

#### 改前端（可选，只改后端时不需要 Node）

前端产物 `src/console_dist/` **随仓库提交**，因此 `git clone && cargo build` 不需要 Node，
单二进制发布也不需要构建机装前端工具链。只有改动 `web/` 下的源码时才需要重新生成：

```bash
scripts/build-web.sh              # 安装依赖（有 lockfile 则严格按锁文件）+ 类型检查 + 构建
scripts/build-web.sh --no-install # 依赖已装好时只构建
cd web && pnpm dev                # 开发模式：热更新，/api 与 /mcp 代理到 127.0.0.1:6789
```

> `build.rs` 在产物缺失时会**直接编译失败**并给出这条命令，而不是内嵌一个占位页——
> 占位页会让服务正常启动、控制台打开却一片空白，属于「能跑但坏了」的最坏形态。

### 撤销本地部署

`init` / `start` 落盘的状态集中在配置目录与数据目录，外加一处**脚本不会写、只能由你手贴到客户端**的条目。用一条命令撤销：

```bash
scripts/deploy-local.sh --uninstall --dry-run   # 先看会移除哪些路径，不做任何改动
scripts/deploy-local.sh --uninstall --yes       # 执行；全部移入废纸篓，可恢复
```

顺序固定为 **停进程 → 摘客户端条目 → 移数据目录 → 移配置目录**，不可颠倒：实例记录
`<data_dir>/memora.pid` 就落在数据目录内，先删目录会让 `stop` 失去定位进程的唯一句柄，
残留进程继续持有 SQLite 连接与监听端口，只能靠 `lsof` + `kill` 手工了结。

| 行为 | 说明 |
|---|---|
| 删除方式 | 一律移入废纸篓（macOS `~/.Trash`；Linux 回退 XDG trash 路径），**不做不可逆删除**；废纸篓不可用时**在动手前**直接拒绝，避免留半成品状态 |
| 客户端条目 | 按 JSON 解析后**只摘 `mcpServers.memora`**，不动同文件里的其它 server，原文件先进废纸篓。若该文件只有 memora 一项，则整体移走 |
| 范围护栏 | `data_dir` 指向 `/`、`/tmp`、`$HOME`、`~/Documents`、仓库根等过宽目标时拒绝执行。路径两侧都经 realpath 规范化后比对——`/tmp` 与 `/private/tmp` 是同一目录的两种写法，只规范化一侧会让护栏静默失效 |
| 确认门槛 | 非交互环境必须显式 `--yes`，否则拒绝执行 |
| 配置缺失时 | 配置不可读则 `data_dir` 退回内置默认值并明确提示；若当初用 `--data-dir` 指定过自定义目录，需手工清理 |
| 不在范围 | `target/`（构建缓存，用 `cargo clean`）、备份归档 |

### 配置

运行配置集中在**一份 TOML 文件**里，默认路径是**绝对路径**，与当前工作目录无关：

```
$XDG_CONFIG_HOME/memora/config.toml   →   $HOME/.config/memora/config.toml   →   ./config.toml
```

因此**任何目录下**的 `memora status` / `memora stop` 都指向同一个实例，日常操作无需带任何参数；只有「为某个实例单独指定配置」时才需要 `--config /path/to/config.toml`。

配置优先级为 **命令行选项 > 进程环境变量 > 配置文件 > 内置默认值**。

| 键（TOML） | 环境变量 | 默认 | 说明 |
|---|---|---|---|
| `port` | `PORT` | `6789` | 监听端口 |
| `data_dir` | `DATA_DIR` | `$XDG_DATA_HOME/memora`（通常 `~/.local/share/memora`） | 数据目录（元库 + 各项目库） |
| `admin_token` | `ADMIN_TOKEN` | — | 管理 API 凭据，**生产必须设置且足够随机** |
| `storage_backend` | `STORAGE_BACKEND` | `sqlite_file` | 新项目默认后端（既有项目以元库登记值为准） |
| `mcp_allowed_hosts` | `MCP_ALLOWED_HOSTS` | `localhost,127.0.0.1,::1` | MCP Host 白名单（防 DNS 重绑定），**公网部署必须加对外域名** |
| `log` | `RUST_LOG` | `info,sqlx=warn,sea_orm=warn` | 日志级别（`RUST_LOG` 优先级更高，便于临时排查） |

> **键名写错会直接报错**，而不是被静默忽略——「改了配置却没生效」这类误诊由此根除。找不到默认配置文件不算错误（走内置默认值），但 `--config` 显式指向一个不存在的文件会硬失败。

---

## 接入 AI 客户端

服务是标准 MCP（Streamable HTTP）+ Bearer 鉴权，客户端只需连接 `https://<host>/mcp` 并在请求头带上项目 token。**同一项目 token 即同一份记忆**——多个客户端、多台机器配置同一个 token，天然共享。

> **部署前置（最容易踩的坑）**：MCP 传输层强制校验请求的 `Host`（防 DNS 重绑定），默认只接受回环地址。经 Caddy 反代时反代会透传真实域名，因此公网部署**必须**把域名写进白名单，否则 MCP 请求会被协议层以 `403` 拒绝，而错误信息并不指向这个配置项：
>
> ```toml
> mcp_allowed_hosts = ["mem.example.com", "localhost", "127.0.0.1"]
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

> ⚠️ **一个客户端条目只指向一个项目**，而这份配置是**用户级全局**的（不随工作区变化）；MCP 的 `initialize` 里也没有任何 cwd / 工作区字段（`roots` 已在 rmcp 3.x 标记废弃，且无状态传输下服务端无法反向请求）。
> 因此**无论你在哪个工作区发起写入，都落进同一个项目**，且不会收到任何提示。要按项目分库，就得开多个条目（`memora-iptv`、`memora-board`…），每个配不同 token；改完仍需在客户端**信任**并**新开会话**才生效。详见 [`docs/memory-system-design.md`](docs/memory-system-design.md) §5.3.1。

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

16 个工具 = 9 个记忆工具（语义对齐官方 MCP Memory）+ 4 个交接工具（`handoff_*` 家族前缀）+ 3 个会话工具（P5.1）。既有工具一个不改名。**项目不在 URL 里**——由 `Authorization: Bearer <项目 token>` 解析出 `project_id`，从根上避免「路径与 token 指向不同项目」这类不一致。

| 工具 | 关键参数 | 行为 | 返回 |
|---|---|---|---|
| `create_entities` | `entities[{name, entity_type, observations?}]` | 批量建实体，重名忽略（不覆盖原类型）。**可直接携带 `observations`**（字符串数组），与实体一并落库，来源继承实体的 `source` | 请求名在调用后的状态（含观测） |
| `create_relations` | `relations[{from_name, to_name, relation_type}]` | 批量建关系，三元组重复跳过 | 实际落库的关系（**含 id**） |
| `add_observations` | `observations[{entity_name, contents[]}]` | 追加观测；实体不存在报 `-32602` | 受影响实体（**含新观测 id**） |
| `delete_entities` | `names[]` | 级联删除关系 / 观测 / 检索索引 | 确实存在并被删除的名字 |
| `delete_observations` | `ids[]` | 按 id 删观测（幂等） | 请求 id（去重保序回显） |
| `delete_relations` | `ids[]` | 按 id 删关系（幂等） | 请求 id（去重保序回显） |
| `read_graph` | — | 返回全图 | `{entities[], relations[]}` |
| `search_nodes` | `query` | FTS5 全词检索（bm25 排序）；无结果时退化为子串扫描 | 命中实体（带完整观测集，按相关度排序） |
| `open_nodes` | `names[]` | 按名取节点明细 | 命中实体（按 name 升序） |

**交接（handoff）家族**——跨客户端 / 跨机器的「接力」与记忆严格分表（高频过程性写入不进 FTS5，否则 `search_nodes` 会被交接噪音淹没）：

| 工具 | 关键参数 | 行为 | 返回 |
|---|---|---|---|
| `handoff_save` | `goal, state, next_steps[], files[], decisions[], open_questions[], task_id?, client?, session_id?` | 追加一条不可变修订；缺 `task_id` 落 `current@<client>` 分片槽位；新修订重置接手态 | `{handoff, task_id, revision}` |
| `handoff_load` | `task_id?` | 缺省取当前待接手的那条（唯一时直接命中，多条返回候选）；读取无副作用 | `{status: found/none/ambiguous, handoff?, candidates[]}` |
| `handoff_list` | `unacked_only?=true, limit?=10, prefix?` | 列「有哪些活等着我」，摘要含 `goal / state_excerpt / client / session_id / 时间` | `{handoffs[]}` |
| `handoff_ack` | `task_id?, client?` | 标记接手（CAS）；缺省接当前待接手那条；重复 ack 幂等不覆盖 | `{status, won, handoff?, candidates[]}` |

**会话（session）家族**（P5.1）——细粒度逐轮 transcript，与交接（结论级摘要）互补；每轮 `turn_index` 由服务端分配并回读，**从不让客户端自己数轮次**：

| 工具 | 关键参数 | 行为 | 返回 |
|---|---|---|---|
| `log_turn` | `session_id, role, content` | 追加一轮对话（`role` 必须是 `user` / `assistant` / `system`；首写即建会话） | 落库后的完整轮次 `{session_id, turn_index, role, content, created_at}` |
| `get_session` | `session_id` | 拉回完整 transcript，按轮次升序；缺失会话返回空数组（正常返回，非错误） | `{turns[]}` |
| `list_sessions` | `prefix?` | 列会话标识，按最近活跃降序（并列按标识升序） | `{sessions[]}` |

**零 ID 主路径**：写入方不必给任务起名，接手方一个参数都不用传——`task_id` 只在工具结果里流转，**从不要求人输入**。多条待接手时服务端返回候选、绝不替调用方挑。交接规则随 `initialize` 的 `instructions` 连接即得（`HANDOFF RESUME` / `HANDOFF SAVE` 两段，措辞刻意零 ID）；可选的质量层见 `skills/memora-handoff/SKILL.md`（删掉它功能完全可用）。

**四个值得说明的接口约定：**

- **写操作回读 id**：`create_relations` / `add_observations` 是刻意回读的——`delete_*` 只接受 id，不回读调用方就拿不到句柄，记忆的「可修订」闭环无法成立。
- **入参别名**：同时接受官方 schema 的 camelCase 写法（`entityType` / `relationType` / `entityName` / `from` / `to`），容忍模型照抄旧 schema；别名不进 JSON Schema，对外只宣传规范字段名。
- **未知字段硬失败**：全部工具入参启用 `deny_unknown_fields`，字段名拼错、或把两个工具的入参混在同一层（顶层同时给 `entities` 与 `observations`）都会被拒并**点名出错字段**。原先 serde 默认忽略未知字段，后果是「返回 200 而数据静默消失」——调用方毫无线索，是最坏的失败形态。
  - 注意两种失败的**形状不同**：参数反序列化失败是 `result.isError = true` + 文本消息（rmcp 行为）；工具语义层错误（如实体不存在）是 JSON-RPC `error.code = -32602`。两者都能让调用方看到原因，但不要按同一种形状去断言。
- **错误码分流**：入参非法 / 实体不存在 → `-32602`（调用方改参数即可）；后端不可用 / 项目 id 非法 → `-32603`（服务端故障）。混为一谈会误导代理去改参数。

---

## 管理 API

前缀 `/api/v1`。两条凭据通道都可用：`ADMIN_TOKEN`（Bearer，给脚本 / CI）或控制台**会话 Cookie**（给浏览器）。
项目 token **不是**管理凭据，不可用于本前缀。

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/projects` | 创建项目（生成 token 并返回明文，`201`） |
| `GET` | `/projects` | 项目列表（含各自后端） |
| `DELETE` | `/projects/{id}` | 注销项目（删登记行 + 删项目数据） |
| `GET` | `/projects/{id}/stats` | 实体 / 关系 / 观测 / 交接任务数统计 |
| `POST` | `/projects/{id}/handoffs` | 保存交接修订（`201`；体同 `handoff_save`，`task_id` 可省） |
| `GET` | `/projects/{id}/handoffs` | 列摘要：`?unacked=1`（缺省）、`?limit=10`、`?prefix=` |
| `GET` | `/projects/{id}/handoffs/pending` | **零 ID 入口**：命中 `200`；无待接手 `204`；多条 `300` + 候选摘要 |
| `GET` | `/projects/{id}/handoffs/{task_id}` | 精确取最新修订；不存在 `404` |
| `POST` | `/projects/{id}/handoffs/pending/ack` | 零 ID 接手，体 `{"client":"..."}`；歧义 `300` |
| `POST` | `/projects/{id}/handoffs/{task_id}/ack` | 精确接手（CAS，重复 ack 幂等） |
| `DELETE` | `/projects/{id}/handoffs/{task_id}` | 幂等删除全部修订 |
| `GET` | `/projects/{id}/graph` | 该项目的全图快照（控制台「记忆浏览」用，只读） |
| `POST` | `/projects/{id}/token` | **轮换 token**：只换 `token_hash`，不动数据；旧 token 立即失效 |
| `GET` / `POST` | `/users` | 列账号 / 新建账号（`{username, password}`；口令过短 `400`，重名 `409`） |
| `DELETE` / `PUT` | `/users/{username}` | 删账号（**最后一个** `409 last_user`，并踢掉其全部会话）/ 改口令 |
| `POST` | `/session` | 控制台登录（公开；body `{username, password}`）→ 签发会话 Cookie |
| `GET` | `/session` | 会话自检（公开）→ `{console_login_enabled, authenticated, user, users}` |
| `DELETE` | `/session` | 登出（幂等）→ `204` + 立即失效的 Cookie |
| `GET` | `/health` | 健康检查（公开，无鉴权） |

> `/session` 三个方法与其余管理端点**分开注册**：登录接口本身不可能先鉴权，
> 若并进同一个 `route_layer`，`POST /session` 会在进入 handler 前就被 401 拒掉——
> 这是一个只在「第一次用浏览器」时才暴露的装配错误，故有专门用例钉住。

**状态码语义与 MCP 侧同源**——判断依据是「调用方能否自行修正」：

| 码 | 含义 | 典型场景 |
|---|---|---|
| `400` | 入参可修正 | `project_id` 非法（目录穿越 / 分隔符 / 空白）；请求的后端未编译进本构建（响应附带 `available_backends` 列表）；交接入参非法（空 `goal`、非法 `task_id`、超限）；口令过短（`weak_password`）；登录名含空白或控制字符 |
| `204` | 无待接手 | 交接零 ID 入口在「没有可接手的交接」时返回（正常状态；`404` 留给指定 `task_id` 不存在） |
| `300` | 多条待接手 | 交接歧义：响应体给候选摘要，服务端不替调用方挑 |
| `401` | 凭据问题 | 缺失或错误的 `ADMIN_TOKEN`；控制台用户名或口令错误；会话已过期 / 已登出；尚未创建任何控制台账号 |
| `403` | 来源不被接受 | 用会话 Cookie 鉴权的写请求来自跨源站（CSRF 二次防线） |
| `404` | 资源不存在 | 项目未登记；账号不存在 |
| `409` | 资源冲突 | `project_id` 已被占用；登录名已存在（`user_exists`）；删除最后一个账号（`last_user`） |
| `500` | 服务端故障 | 元库不可用、后端未注册、熵源不可用 |

统计口径与客户端**同源**：`stats` 与 `graph` 都走 `read_graph` 派生，而不是另写一套 SQL 查询，
避免出现两套不一致的读取逻辑。控制台**不提供**写入记忆的入口——写入仍只经 MCP 工具，
以免在管理面引入第二套写入语义（判重规则、来源字段等）。

> **`ADMIN_TOKEN` 因此从「管理面凭据」升格为「管理面 + 交接读写凭据」**（交接 CLI 只能拿
> 到它——明文项目 token 不可反查），信任等级不变（管理面本就能删项目）；项目 token 仍
> **不能**用于本前缀。

---

## 架构

```
                 HTTPS
   MCP Client ──▶ Caddy ──▶ Axum 单二进制
   浏览器     ──▶        │
        ┌──────────────────┼────────────────────┐
        ▼                  ▼                    ▼
   鉴权中间件         路由 / 项目解析        存储抽象层
   Bearer→project    /mcp                 按 project_id 产出 Repository
   会话Cookie→admin  /admin（页面）
                           │
                  ┌────────┴────────┐
            MCP 工具层          REST 管理层
      9 记忆 + 4 交接 + 3 会话工具  项目增删 / 统计 / 全图 / 交接 / 会话
```

**分层依赖单向，不得回指**：

```
main   → cli / daemon / routes
routes → mcp::server → mcp::tools → storage::repo (trait) / domain
routes → admin      → storage (trait / 注册表) / meta / domain
routes → console    → session / password / auth / reply
auth   → session（仅「会话 Cookie 是否可作管理凭据」这一判断）
daemon → config / instance / password（仅 init 生成凭据）
mcp::dto 只被 tools 与 server 使用
```

| 层 | 职责 | 关键约束 |
|---|---|---|
| `cli.rs` | 子命令与全局选项解析 | 零依赖手写；无子命令等价于 `run`，既有部署零改动 |
| `daemon.rs` | `start` / `stop` / `restart` / `status` / `init` | 以 `/health` 通过为唯一成功判据；`status` / `stop` 以**实例记录**（pid + port + data_dir + 启动时间 + 版本）为准，而非重新推导配置；**不依赖存储层** |
| `instance.rs` | 实例记录与进程存活探测 | `kill(pid,0)` 判定，`EPERM` 视为存活；损坏内容报错而非当作「未运行」；记录端口可消除「探错端口」误诊 |
| `console.rs` | `/admin` 页面、`/assets/*` 静态资源与 `/api/v1/session` | 页面**不含任何数据**，故可公开；资源清单由 `build.rs` 在编译期生成；未命中的资源一律 `404`（不回退 HTML） |
| `build.rs` | 扫描 `src/console_dist/` → 生成内嵌资源清单 | 产物缺失时**编译期失败**（附修复命令），不内嵌占位页 |
| `users.rs` | `_meta.db.users` 的读写与凭据校验 | 只存 PHC 哈希；账号不存在时也跑一次哈希校验（防枚举信道）；只在表为空时播种 |
| `password.rs` | 口令哈希 / 校验 / 随机生成 | 只含哈希与口令，**不含任何 HTTP 概念**；随机量仍走系统熵，不引 rand 生态 |
| `session.rs` | 内存会话表、Cookie 签发与解析、跨源判定 | 用内存表而非签名 Cookie：不必引入长期密钥；会话有效期**绝对**，不滑动 |
| `admin.rs` | 项目生命周期 handler | 只经 `StorageRegistry` 取 trait 对象，不引用任何具体后端 |
| `mcp/dto.rs` | 16 个工具的入参 / 出参契约（serde + JsonSchema） | 只描述协议，不含逻辑 |
| `mcp/tools.rs` | 工具语义实现 | **只依赖 `Arc<dyn MemoryRepository>`**，可脱离 MCP 传输层用内存后端快速单测 |
| `mcp/server.rs` | rmcp 宏装配 + 从请求上下文取仓库 | **唯一允许 `use rmcp` 的地方** |
| `storage/` | Repository 契约 + 后端实现 | 方言差异锁死在各自 impl 内 |

这套拆分带来一个直接好处：**工具语义的用例不需要任何 HTTP 与协议栈**即可验证。

---

## 存储后端可插拔

「换后端不改业务代码」不是口号，而是被一份**共享契约测试**钉住的事实。

| 后端 | 状态 | 存储策略 |
|---|---|---|
| `sqlite_file` | ✅ 默认 | 每项目一个 SQLite 文件 `data/{project_id}/mem.db`，连接按项目缓存 |
| `sqlite_single` | ✅ P6 | 单库 `data/mem-single.db` + `project_id` 租户列过滤（实体 / 观测 / 关系 / 交接 / FTS 全租户化） |
| `in_mem` | ✅ 已实现 | 纯内存 HashMap，测试参照实现 |
| `postgres` | 🔜 预留 | 单连接池，按 schema 或租户列隔离 |

两份契约同时约束每个后端：

- `contract::run_all`——**仓库级** 12 阶段契约：实体 CRUD 幂等、整批非法不落库、观测追加与 `EntityNotFound`、关系去重、检索命中名/类型/内容、CJK 子串、级联删除、幂等删除、跨项目隔离。
- `contract::run_backend_contract`——**项目生命周期级**契约：取用幂等、项目隔离、丢弃项目、丢弃幂等、非法 id 拒绝。

因此新增一种持久化后端 = **1 个 impl 文件 + 工厂里 1 行注册 + 2 行契约调用**；切换生效后端 = **改 1 处配置**（`storage_backend`，或等价的 `STORAGE_BACKEND`）。`projects` 表记录每个项目所用的后端，不同项目可以并存不同存储策略。

---

## 部署到 2GB 服务器

```bash
# 1. 编译（可选 musl 静态链接）
cargo build --release

# 2. 建 2GB swap 作安全垫，防峰值 OOM
sudo fallocate -l 2G /swapfile && sudo chmod 600 /swapfile
sudo mkswap /swapfile && sudo swapon /swapfile

# 3. 生成配置与系统模板（写入 ~/.config/memora/config.toml，权限 600）
#    同时打印管理 API token 与控制台口令——只出现这一次，抄进密码管理器
./target/release/memora init
```

两种运行形态，按需二选一：

| 形态 | 命令 | 适用 | 取舍 |
|---|---|---|---|
| **自守护** | `memora start` / `stop` / `restart` / `status` | 裸机、无 systemd、无 root | 脱离终端、日志落盘、健康自检通过才返回成功；**无法在进程崩溃后自动拉起** |
| **前台托管** | `memora run` + systemd | 希望开机自启与崩溃自愈 | 获得 `Restart=always`，这是自守护形态唯一不可替代的能力 |

> 两种形态共用同一份二进制——这正是不做「纯自守护」的原因：`Restart=always` 只能在 systemd 侧提供。

反代用 **Caddy** 自动签发 Let's Encrypt 证书，`memora init` 会渲染好配置片段。生产只需暴露六个前缀，其余一律 404：

```caddyfile
mem.example.com {
	handle /mcp*      { reverse_proxy 127.0.0.1:6789 }
	handle /api/v1/*  { reverse_proxy 127.0.0.1:6789 }
	handle /admin*    { reverse_proxy 127.0.0.1:6789 }   # 控制台 SPA；必须是前缀匹配，见下
	handle /assets/*  { reverse_proxy 127.0.0.1:6789 }   # 控制台 js / css，漏了页面一片空白
	handle /favicon.svg { reverse_proxy 127.0.0.1:6789 }
	handle /health    { reverse_proxy 127.0.0.1:6789 }
	handle            { respond 404 }   # 记忆库里是私有内容，不该有未预期的暴露面
	encode zstd gzip
}
```

> **`/admin` 必须写成 `handle /admin*`（前缀匹配），不能写成 `handle /admin`（精确匹配）。**
> 控制台是 Vue Router 的 history 模式，子路由（`/admin/projects`、`/admin/users` …）会被浏览器真的请求；
> 精确匹配只放行入口页本身，于是在站内点任何一个菜单都会 404。
>
> **根路径 `/` 刻意返回 404**，这不是故障：控制台挂在 `/admin` 下，根路径不属于它，也不做重定向。
> 访问控制台请直接用 `https://<你的域名>/admin`。刻意不做跳转的原因是最小暴露面——域名根路径被扫描器
> 高频探测，返回一个不含信息的 404 比返回 302 更少透露「这里有一个可登录入口」。`/health` 同样只回 `ok`。

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
| **不引入 clap** | 选项虽已增至 9 个，但都是「取值 → 覆盖配置」的直白形态，手写解析不到百行；而 clap 带来可观编译时间与约 300KB 体积增长，与「2GB 服务器上的单二进制」定位不符 |
| **Host 白名单必须可配** | rmcp 默认只信回环 Host；不暴露该配置项时，公网经反代的线上表现是费解的 `403` |
| **配置默认路径必须是绝对路径** | 相对路径 + CWD 探测会让「实例身份」随执行目录漂移：`status` / `stop` 竟要带参数才能找到同一实例。改成 XDG 绝对默认路径后，任何目录下无参数操作都指向同一实例；多实例由 `--config` 显式指定 |
| **配置文件用 TOML 而非 `.env`** | `.env` 无类型且**未知键静默忽略**（把 `ADMIN_TOKEN` 拼错后服务照常启动、鉴权全 401）。TOML 让键名写错在启动期硬失败，默认路径固定后也不必再记「探测顺序」 |
| **`status` / `stop` 以实例记录为准** | 记录含 pid + 端口 + 数据目录 + 启动时间 + 版本，故按**记录**探活与终止，而不是按当前配置重新推导——「探错端口」与「配置改过但进程没重启」两类误诊由此根除 |
| **`start` 前试绑端口，而非事后确认子进程存活** | `start` 的成功判据是「该端口上 `/health` 通」，而占用该端口的可能是**另一个** memora 实例——它的响应同样是 `200 ok`，响应体校验无从区分。此时新子进程因 `EADDRINUSE` 立即退出，父进程却判定成功：命令返回 0、实例记录指向一个已死的 pid。故在 spawn **之前**以 `0.0.0.0`（与服务实际监听地址一致）试绑一次：端口不可独占 ⇒ 本实例必然起不来，这是确定性前置条件，不存在「子进程还没死」的时序竞态 |
| **控制台用内存会话表，而非签名 Cookie** | 签名 Cookie 需要一把长期密钥，于是「怎么生成、存哪、怎么轮换」立刻变成三个新的运维问题，而落盘的密钥本身又是必须保护的秘密。内存表不需要任何密钥：会话 id 是 256 bit 随机量、服务端持有对应关系，客户端无法自证身份，也就不存在「伪造一个签名 Cookie」这条攻击路径。代价是重启即全员登出（对管理面反而是正向属性：等于强制重新认证），以及多实例不共享会话——而 memora 是单实例模型，这个代价当前为零 |
| **口令只以 Argon2id 哈希落盘** | `init` 生成随机口令、当场哈希、只打印一次明文然后丢弃。于是「配置被备份、截图或误提交」都不会泄漏口令。选 Argon2id 而非 PBKDF2 / bcrypt 是因其 memory-hard，显著抬高 GPU / ASIC 离线爆破成本；PHC 串把算法与参数内联在哈希里，将来提升强度时旧哈希仍可校验，不必改配置结构或做数据迁移 |
| **口令强度只在解析期校验** | 服务跑起来后只持有哈希，那时已无从判断强度——解析配置是唯一能拿到**明文**的时刻，也就是最后一道闸门。因此明文短于 8 字符**硬失败**（与非法 `PORT` 的处理同源：非法值不得静默通过），而不是记一条告警就放行。判据只看长度、不看「是否含大小写数字符号」：后者会诱导用户产出 `Passw0rd!` 这类可预测口令，而长度才是抵抗离线爆破的主要变量 |
| **控制台页面用 `textContent`，绝不用 `innerHTML`** | 页面展示的实体名与观测内容来自 AI 客户端写入的记忆库，属**不可信数据**；用 `innerHTML` 渲染等于在自己的管理界面里开一个存储型 XSS。这条用肉眼审查极难持续保证（任何一个新字段都可能顺手写错），故固化成用例：断言页面不含 `.innerHTML` / `insertAdjacentHTML` 等解析入口 |
| **反代白名单必须与路由表同步** | 新增 `/admin` 时忘了改 Caddyfile，症状是「localhost 直连正常、经 Caddy 反代 404」——两边各自看都没错，任何单元测试也发现不了（路由表与反代模板互不相干）。已加用例断言反代模板覆盖全部公开前缀 |
| **页面脚本的 id 引用必须自洽（无浏览器也要守）** | 脚本在文档末尾一次性绑定全部按钮，任何一个 id 拼错都会在 `addEventListener` 处抛 `TypeError` 并**中断整个脚本**——表现是「页面一片空白、连登录框都不出现」，而服务端日志里没有任何异常、HTTP 用例全绿。这类缺陷在没有浏览器的环境里几乎只能靠肉眼比对，故固化成用例：脚本引用的每个 id 必须存在于页面，且 id 不得重复 |

更多取舍（含每期的验证结果与踩坑记录）见 [`docs/memory-system-design.md`](docs/memory-system-design.md)。

---

## 质量与测试

| 项目 | 现状 |
|---|---|
| `cargo test` | **228 项全部通过** |
| `cargo clippy --all-targets` | **零告警** |
| `scripts/smoke-e2e.py` | **28 项全通过**——对**已运行**实例做黑盒验证：MCP 握手与 9 工具闭环、管理面状态码、错误码分流、跨项目隔离、Host 白名单与 `Accept` 协商 |
| 测试构成 | 仓库契约（双后端各跑一遍）、后端生命周期契约、MCP 工具语义（脱离协议栈）、路由协议级端到端（`initialize` / `tools/list` / `tools/call`）、控制台（登录 / 登出 / CSRF / 凭据隔离 / **页面 DOM 引用自洽**）、口令（Argon2id 往返、随机口令无偏采样）、CLI 与进程管理（子命令解析、实例记录与存活探测、退出码语义）、配置来源与优先级（TOML 解析、XDG 默认路径解析、`--config` 硬失败）、并发写入回归、鉴权与隔离 |
| 并发验证 | 4 项目 × 40 次工具调用（每项目 8 并发）：**零失败**、统计与写入一致、跨项目零泄漏 |
| 开发方式 | 全程 TDD：先写测试并观察其失败，再写实现 |

值得单独一提的用例：**注入式恶意检索串不报错**、**跨项目误写被隔离**、**非白名单 Host 返回 403**、**明文 token 不在元库任何文件中出现**、**并发首次取用同一项目只建立一个连接池**、**端口被另一实例占用时 `start` 必须失败**（后两条都是实机复现发现的真实缺陷，见下节）。

---

## 路线图

| 期 | 内容 | 状态 |
|---|---|---|
| P1 | Axum + sea-orm 骨架、项目路由、Bearer 中间件、元库 | ✅ |
| P2 | 存储抽象 + 双后端 + 全链路依赖注入 | ✅ |
| P3 | MCP 工具层接入（rmcp，9 个工具） | ✅ |
| P4.1 | REST 管理面：项目增删 + 用量统计 | ✅ |
| P4.2 | 运行形态：自守护 CLI + 配置显式化 + systemd / Caddy + 优雅退出 | ✅ |
| P4.3 | 配置单实例化：TOML + XDG 绝对路径 + 实例记录（`status` / `stop` 无需带参数） | ✅ |
| P4.4 | 网页控制台：`/admin` 登录（Argon2id）+ 建项目 / 取 token / 浏览记忆 | ✅ |
| P4.5 | 任务交接并入 memora：4 个 MCP 工具 + 7 个 REST 路由 + `handoff` CLI（设计见 `docs/handoff-integration-design.md`） | ✅ |
| P6 | 第三后端 `sqlite_single`：单库 + `project_id` 租户列过滤（记忆 / 交接 / FTS 全租户化，契约 H1–H18 + bc_01–bc_07 全覆盖） | ✅ |
| P5.1 | 会话表与跨机器会话恢复（`log_turn` / `get_session` / `list_sessions`，三后端契约 + MCP 协议端到端） | ✅ |
| 后续 | 远程 embedding 语义检索、记忆版本与回滚、导入导出、控制台里改记忆 | 💡 |

**非目标（首版范围外）**：内置大模型推理、复杂图谱推理 / 反思、多节点集群。

---

## 文档

| 文档 | 内容 |
|---|---|
| [`docs/memory-system-design.md`](docs/memory-system-design.md) | 完整技术设计蓝图：数据模型、接口契约、鉴权与多租户、检索策略、部署方案、分期计划与全部实现取舍 |
| [`docs/testing-guide.md`](docs/testing-guide.md) | 分层测试指南：七层策略、退出码契约、沙箱搭建、网页控制台的验证矩阵、部署后自检清单与已知的坑 |
| [`scripts/`](scripts) | `deploy-local.sh` 本地一键部署与 `--uninstall` 撤销（含范围护栏）；`smoke-e2e.py` 对已运行实例的黑盒验证 |
| [`deploy/`](deploy) | systemd unit、Caddyfile 片段、`config.toml` 模板（由 `memora init` 渲染） |
| [`deploy/config.toml.template`](deploy/config.toml.template) | 全量配置项与注释（键名、默认值、优先级、权限说明） |

---

## 许可证

MIT（见仓库根目录 [`LICENSE`](LICENSE)）。自托管、自修改、自商用均无限制。

---

*Memora（忆庐）= memory + aura 的自造词，「庐」取「记忆的小屋」之意：一间建在你自己机器上、按项目分间、钥匙只在你手里的记忆小屋。*
