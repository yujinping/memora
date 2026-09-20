# Memora（忆庐）· 测试指南

- **@author** yujinping
- **@date** 2026-09-20
- **@intent** 把「怎么证明 memora 是对的」固化成一份分层清单。核心判断是：`cargo test` 证明的是**进程内语义**，
  而部署后最常出问题的地方——反代有没有透传 Host、白名单配没配、token 是否真的按项目隔离——**只有打真实端口才能验**。
  故本指南按「验证成本递增、发现问题的层级也递增」组织，五层各管一段，缺一层就有盲区。

---

## 1. 五层策略：每层证明什么

| 层 | 手段 | 证明什么 | 发现不了什么 |
|---|---|---|---|
| L1 单元 / 契约 | `cargo test` | 存储语义、入参校验、配置优先级、并发建池 | 部署配置是否配对 |
| L2 静态质量 | `cargo clippy --all-targets` | 交付标准（零告警） | 运行时行为 |
| L3 协议端到端 | `scripts/smoke-e2e.py` | MCP 握手、9 工具闭环、状态码语义、项目隔离、传输层防线 | 真实客户端能否接入 |
| L4 运行形态 | `init` / `start` / `status` / `stop` 退出码 | 进程管理、实例记录、退出码契约 | 业务语义正确性 |
| L5 真实客户端 | WorkBuddy / Cursor / Zed 接入 | 客户端与协议的实际兼容性 | 无法自动化，须人工核验 |

**为什么 L1 不可替代 L3**：`cargo test` 里凡涉及 HTTP 的用例都跑在进程内（`Router` 直接调用，无 TCP、无反代、无 Host 校验），
因此「Host 白名单配错导致线上 403」这类缺陷在 L1 永远是绿的。

---

## 2. L1 / L2：构建与静态质量

```bash
cargo test                     # 期望：170 passed; 0 failed
cargo build                    # 期望：Finished，无 warning
cargo clippy --all-targets     # 期望：Finished，零告警
```

> ⚠ **先重新构建再测**。`memora --help` 的输出就是构建时间戳的指纹：若它仍显示 `--env-file` / `.env`，
> 说明二进制停留在 P4.2 时代，而 HEAD 的 `--config` / `config.toml` 是 P4.3 引入的。
> 直接跑旧二进制会得到一份**关于旧版本的正确结论**——比报错更难发现。

用 `--test-threads=1` 复跑一遍可暴露对全局状态的隐式依赖（配置与环境变量用例刻意做了注入隔离，应该照样全绿）：

```bash
cargo test -- --test-threads=1
```

---

## 3. L3：协议端到端冒烟

`scripts/smoke-e2e.py` 是黑盒脚本，只依赖标准库，对**已运行**的实例发真实 HTTP 请求，覆盖 9 组 28 项断言：

```bash
python3 scripts/smoke-e2e.py --base-url http://localhost:6789 --admin-token "$ADMIN_TOKEN"

# 想看留下的测试数据（默认自动注销测试项目）
python3 scripts/smoke-e2e.py --base-url http://localhost:6789 --admin-token "$ADMIN_TOKEN" --keep
```

| 分组 | 断言要点 |
|---|---|
| 健康检查 | `/health` 公开、200、body 含 `ok` |
| 管理面鉴权 | 错误 `ADMIN_TOKEN` → 401；**项目 token 不能当管理凭据** |
| 项目生命周期 | 201 / 409 / 400（目录穿越）/ 404（注销未登记）/ 注销后 `data_removed=true` |
| MCP 握手 | `initialize` 返回 `serverInfo`；`tools/list` **恰好** 9 个（多一个少一个都算契约变更） |
| 工具闭环 | 9 个工具：建实体 → 建关系（取 id）→ 追加观测（取 id）→ 中文检索 → 按名取明细 → 全图 → 按 id 幂等删除 |
| 错误码分流 | 实体不存在 → `-32602`，**不是** `-32603` |
| 项目隔离 | A 写入的实体对 B 完全不可见 |
| 传输层防线 | 非白名单 `Host` → 403；缺 `Accept` 协商头 → 406；伪造 token → 401 |

**为什么不写成 shell**：断言里要判断 JSON 结构（`tools/list` 的集合相等、`search_nodes` 的命中、`read_graph` 的计数），
在 shell 里只能靠 `grep` 近似匹配，会把「字段在但值错」误判为通过。

### 手工验证 MCP 时的两个必带请求头

```bash
curl -s -X POST localhost:6789/mcp \
  -H "Authorization: Bearer $PROJECT_TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

| 缺失项 | 现象 | 原因 |
|---|---|---|
| `Accept` 头 | **406 Not Acceptable** | MCP 传输要求客户端同时声明接受 JSON 与 SSE |
| `Host` 不在白名单 | **403** | 防 DNS 重绑定；反代透传真实域名，公网部署必须把域名写进 `mcp_allowed_hosts` |

> 无状态传输（`legacy_session_mode=false` + `json_response=true`）意味着**不需要先 `initialize`**，
> 每个请求自带 token 即可独立成立。这也正是手工 `curl` 能直接打工具的原因。

---

## 4. L4：运行形态与退出码

**先搭一个不污染 `$HOME` 的沙箱**——把两个 XDG 变量指到临时目录即可，这是 P4.3 改造带来的直接好处：

```bash
SANDBOX=$(mktemp -d)
export XDG_CONFIG_HOME=$SANDBOX/config XDG_DATA_HOME=$SANDBOX/share

./target/debug/memora init            # 生成 $SANDBOX/config/memora/config.toml（权限 600）
$EDITOR $XDG_CONFIG_HOME/memora/config.toml   # 至少改掉 admin_token 占位符

./target/debug/memora start --port 16789
./target/debug/memora status
curl -s localhost:16789/health
./target/debug/memora stop
```

退出码契约（脚本化运维依赖它，逐条都值得手工确认一次）：

| 场景 | 退出码 | 说明 |
|---|---|---|
| `status` 实例存活 | 0 | 输出 `memora is running (pid …)` |
| `status` 未运行 | **3** | 与「故障」区分开，便于脚本判断 |
| `stop` 未运行 | 0 | **幂等**：重复 `stop` 不报错 |
| 配置类故障 / 未知子命令 | **2** | 硬失败，不静默回退默认值 |

实测的三个可观察行为，可作为「实例记录」机制是否生效的判据：

| 观察点 | 期望表现 |
|---|---|
| `start` 后立即 `status` | 显示 `port: 16789 (healthy)`，并提示 `the config asks for port 6789; memora restart to apply it`——**以实例记录为准**，而非按当前配置重新推导 |
| `stop` 后 | 进程确实退出，且 `DATA_DIR/memora.pid` **被删除** |
| 任意目录下执行 `status` | 指向同一实例（默认路径是绝对路径，与 CWD 无关） |

数据目录布局（`stop` 后逐项核对）：

```text
<DATA_DIR>/_meta.db                  元库（项目登记 + token 的 SHA-256）
<DATA_DIR>/<project_id>/mem.db       项目库（可能伴随 -wal / -shm）
<DATA_DIR>/memora.pid                实例记录（运行中存在）
<DATA_DIR>/memora.log                后台日志
```

资源占用（2GB 目标机上的关键指标，目标常驻 < 100MB）：

```bash
ps -o rss=,vsz= -p "$(cat "$DATA_DIR/memora.pid")"   # RSS / VSZ，单位 KB
```

> 该指标需在 2GB 实机上取值才有意义——开发机的 RSS 不能代表生产峰值。

---

## 5. L5：真实客户端接入（人工）

前四层全绿仍可能接不上，因为客户端对协议的实现有差异。接入步骤见 README 的「接入 AI 客户端」一节，核验要点：

| 核验项 | 判据 |
|---|---|
| 客户端能列出 9 个工具 | 说明 `tools/list` 与 schema 被正确解析 |
| 让模型主动写入记忆 | 若无写入，检查是否把「持久化规则 / 恢复规则」两条指令放进了系统提示——**MCP 工具是按需调用的，不写指令记忆库会长期为空** |
| 新会话能召回 | 检验项目 token 是否真的绑定了同一份记忆 |
| 公网部署 | 域名是否已加入 `mcp_allowed_hosts`（否则表现为费解的 403） |

**遗留的人工核验项**：MCP Inspector（自 P3 挂账）——用其 Streamable HTTP 传输指向 `<base>/mcp`，在请求头里带 `Authorization: Bearer <项目 token>`，逐个工具目视核对入参 schema 与返回结构。该项无法被 `smoke-e2e.py` 覆盖，因为它验的是**schema 对第三方的可读性**。

---

## 6. 部署后自检清单

| # | 动作 | 通过判据 |
|---|---|---|
| 1 | `cargo test` / `cargo clippy --all-targets` | 170 passed / 零告警 |
| 2 | `memora init` 后改掉 `admin_token` 占位符 | 配置权限 600，token 为 `openssl rand -hex 32` 产物 |
| 3 | `memora start` | 返回 0，`status` 显示 `(healthy)` |
| 4 | `scripts/smoke-e2e.py` | 28/28 通过 |
| 5 | Caddy 反代后重跑 `smoke-e2e.py --base-url https://<域名>` | 28/28 通过（**这是唯一能证伪 Host 白名单配错的检查**） |
| 6 | `ps -o rss=` | 常驻内存 < 100MB |
| 7 | 真实客户端记得住 | 新会话能召回上一会话写入的记忆 |
| 8 | 占用同一端口再 `start` 一次（或用另一 `XDG_DATA_HOME` 配同端口启动第二个实例） | **必须报错退出**（`port … is already in use`），而不是打印 `memora started`——后者意味着它把「该端口上有健康 memora」误当成了「我拉起的实例起来了」 |

> 第 5 步是关键：本机 `localhost` 永远在白名单里，所以**只有走真实域名那一次才能暴露白名单缺失**。

---

## 7. 已知的坑

| 坑 | 现象 | 处置 |
|---|---|---|
| 测试的是旧二进制 | `--help` 显示 `--env-file` / `.env` | 先 `cargo build`；把 `--help` 输出当作构建指纹 |
| `init` 生成的 `admin_token` 是占位符 `change-me` | 服务可正常启动、鉴权可通过 | 启动前必须替换；**公网部署若漏改，管理 API 等于无鉴权** |
| MCP 请求缺 `Accept` 头 | 406，且错误信息不指向该头 | 补 `Accept: application/json, text/event-stream` |
| 公网 MCP 403 | 错误信息不指向配置项 | `mcp_allowed_hosts` 加上对外域名 |
| 默认路径写入 `$HOME` | 测试污染本机配置 | 用 `XDG_CONFIG_HOME` / `XDG_DATA_HOME` 指向临时目录 |
| 并发用例假绿 | `#[tokio::test]` 默认 `current_thread`，`tokio::spawn` 不产生真并发 | 并发用例必须写 `flavor = "multi_thread", worker_threads = 4` |
| 同端口启动「假成功」（2026-09-20 已修） | 目标端口被**另一个 memora 实例**占用时，新子进程因 `EADDRINUSE` 立即退出，父进程的健康探测却命中那个旧实例（响应同样是 `200 ok`）→ `start` 返回 0、打印 `memora started`，实例记录写向一个已死的 pid；调用方以为「刚起的实例」在服务，实际所有写入都打到旧实例 | 已在 spawn 前加端口试绑预检（`ensure_port_available`，地址取 `0.0.0.0` 与服务一致）——端口不可独占即硬失败。手工核验见自检清单第 8 项 |

---

## 8. 本轮实测结论（2026-09-20）

| 项 | 结果 |
|---|---|
| `cargo test` | **170 passed; 0 failed**（含新增 2 项端口预检用例） |
| `cargo clippy --all-targets` | **零告警** |
| `scripts/smoke-e2e.py` | **28/28 通过** |
| 跨项目隔离 | 项目 A 写入的实体在项目 B 的 `read_graph` 中不可见 |
| 退出码契约 | `stop` = 0、重复 `stop` = 0、未运行时 `status` = **3**、`stop` 后 PID 文件已删除 |
| 状态码语义 | 201 / 400 / 401 / 403 / 404 / 406 / 409 全部与文档一致 |
