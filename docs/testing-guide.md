# Memora（忆庐）· 测试指南

- **@author** yujinping
- **@date** 2026-09-20
- **@intent** 把「怎么证明 memora 是对的」固化成一份分层清单。核心判断是：`cargo test` 证明的是**进程内语义**，
  而部署后最常出问题的地方——反代有没有透传 Host、白名单配没配、token 是否真的按项目隔离——**只有打真实端口才能验**。
  故本指南按「验证成本递增、发现问题的层级也递增」组织，五层各管一段，缺一层就有盲区。

---

## 1. 分层策略：每层证明什么

| 层 | 手段 | 证明什么 | 发现不了什么 |
|---|---|---|---|
| L1 单元 / 契约 | `cargo test` | 存储语义、入参校验、配置优先级、并发建池、口令与会话 | 部署配置是否配对 |
| L2 静态质量 | `cargo clippy --all-targets` | 交付标准（零告警） | 运行时行为 |
| L3 协议端到端 | `scripts/smoke-e2e.py` | MCP 握手、9 工具闭环、状态码语义、项目隔离、传输层防线 | 真实客户端能否接入 |
| L4 运行形态 | `init` / `start` / `status` / `stop` 退出码 | 进程管理、实例记录、退出码契约 | 业务语义正确性 |
| L5 真实客户端 | WorkBuddy / Cursor / Zed 接入 | 客户端与协议的实际兼容性 | 无法自动化，须人工核验 |
| L6 撤销部署 | `deploy-local.sh --uninstall --dry-run / --yes` | 部署可干净回退、范围护栏生效 | 业务语义 |
| L7 网页控制台 | 沙箱起实例，对 `/admin` 与 `/api/v1/session` 打真实 HTTP | 登录 / 会话 / 登出 / CSRF / 凭据隔离 / 页面可服务 | **浏览器端 JS 运行时错误与视觉**（见 §10 的静态补偿） |

**为什么 L1 不可替代 L3**：`cargo test` 里凡涉及 HTTP 的用例都跑在进程内（`Router` 直接调用，无 TCP、无反代、无 Host 校验），
因此「Host 白名单配错导致线上 403」这类缺陷在 L1 永远是绿的。同理，「反代漏放行 `/admin`」在 L1 与 L3 也都绿——
它只在 L7 走真实反代时暴露。

---

## 2. L1 / L2：构建与静态质量

```bash
cargo test                     # 期望：228 passed; 0 failed
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

./target/debug/memora start --port 16789
./target/debug/memora status
curl -s localhost:16789/health
./target/debug/memora stop
```

> `init` 已不需要人工编辑：`admin_token` 与控制台口令都是本次随机生成，口令只在输出里出现一次。
> **把它抄进密码管理器**——配置文件里只有 Argon2id 哈希，反推不出口令。
> 忘了就 `memora init --force` 重新生成（会覆盖配置）。

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
| 1 | `cargo test` / `cargo clippy --all-targets` | 228 passed / 零告警 |
| 2 | `memora init` | 配置权限 600；`admin_token` 与控制台口令都是本次随机生成（**口令只在输出里出现一次**，立即存进密码管理器）；启动日志出现 `web console enabled at /admin user=<名>` |
| 3 | `memora start` | 返回 0，`status` 显示 `(healthy)` |
| 4 | `scripts/smoke-e2e.py` | 28/28 通过 |
| 5 | Caddy 反代后重跑 `smoke-e2e.py --base-url https://<域名>` | 28/28 通过（**这是唯一能证伪 Host 白名单配错的检查**） |
| 6 | 浏览器打开 `https://<域名>/admin` 并登录 | 能登录、能建项目、能复制 token（**这是唯一能证伪反代漏放行 `/admin` 的检查**）；地址栏应为 https，Cookie 应带 `Secure` |
| 7 | `ps -o rss=` | 常驻内存 < 100MB |
| 8 | 真实客户端记得住 | 新会话能召回上一会话写入的记忆 |
| 9 | 占用同一端口再 `start` 一次（或用另一 `XDG_DATA_HOME` 配同端口启动第二个实例） | **必须报错退出**（`port … is already in use`），而不是打印 `memora started`——后者意味着它把「该端口上有健康 memora」误当成了「我拉起的实例起来了」 |

> 第 5 步与第 6 步是互补的：本机 `localhost` 永远在白名单里、也永远不需要反代，
> 所以**只有走真实域名那一次**才能暴露「白名单缺失」与「反代漏放行某个前缀」。
> 两者都属于「直连一切正常、上线才出问题」的类别。

---

## 7. 已知的坑

| 坑 | 现象 | 处置 |
|---|---|---|
| 测试的是旧二进制 | `--help` 显示 `--env-file` / `.env` | 先 `cargo build`；把 `--help` 输出当作构建指纹 |
| 改了代码但没重编 release，就 `restart` | `/health` 返回 200、`status` 显示 healthy、新加的路由却 404 | `restart` 起的是**本命令自己的 `current_exe`**——调 `target/release/memora restart` 就起 release，与最近一次 `cargo build`（debug）无关。诊断三步：`lsof -nP -iTCP:<port> -sTCP:LISTEN` 取 pid → `lsof -p <pid> \| awk '$4=="txt"{print $NF}'` 看**真正运行的映像路径** → `strings <映像> \| grep -c memora_session` 验特征串（release 无调试符号，但 `include_str!` 的常量与 CLI 标志名仍在）。处置：`cargo build --release` 后再 `restart` |
| `admin_password_hash` 留空 | `/admin` 给了登录框却永远登不进去 | 这是「关闭控制台登录」的正常形态（只用 MCP 的部署不需要它），页面会明确提示怎么修。要开启：`memora init --force`，或设 `ADMIN_PASSWORD` / `--admin-password` 后重启 |
| 明文口令短于 8 字符 | 启动即失败，报错点名是 `--admin-password` 还是 `ADMIN_PASSWORD` | 这是刻意的（口令强度只能在解析期判断，运行期只剩哈希）。换成更长的口令即可；走 `--admin-password-hash` 的哈希入口不受此限 |
| 反代漏放行 `/admin`（2026-09-20 已修） | localhost 直连 `/admin` 正常，公网经 Caddy 打开 404，且返回的正是模板里兜底的 `respond 404` | 反代白名单原先只列 mcp / api / health。已在 `deploy/Caddyfile` 补 `handle /admin`，并加用例断言模板覆盖全部公开前缀。**根因是「路由表与反代模板互不相干，各自看都对」** |
| 控制台页面「一片空白」 | 页面打开后连登录框都不出现，服务端日志干净、HTTP 用例全绿 | 脚本在文档末尾一次性绑定全部元素，某个 id 拼错就抛 `TypeError` 并**中断整个脚本**。已加静态用例比对 `$("id")` 引用与页面 id 集合（含 id 重复检查） |
| 控制台登录后立刻又变未登录 | 登录响应 200，但下一个请求 401 | Cookie 带了 `Secure` 而链路是明文 HTTP，浏览器直接丢弃。服务只依据 `X-Forwarded-Proto` 判定 HTTPS；不经反代直连 `http://localhost` 时不该有 `Secure`——若出现，检查是否在代理层之外人为注入了该头 |
| Playwright 在 macOS 12 上装不了 chromium | `npm install playwright` 成功，`npx playwright install chromium` 报 `Playwright does not support chromium on mac12` | 用 §10 的三条静态检查兜底。**别把「装不上」当成「不用测」**——那三条覆盖的正是最易发生的那类浏览器端缺陷 |
| MCP 请求缺 `Accept` 头 | 406，且错误信息不指向该头 | 补 `Accept: application/json, text/event-stream` |
| 公网 MCP 403 | 错误信息不指向配置项 | `mcp_allowed_hosts` 加上对外域名 |
| 默认路径写入 `$HOME` | 测试污染本机配置 | 用 `XDG_CONFIG_HOME` / `XDG_DATA_HOME` 指向临时目录 |
| 并发用例假绿 | `#[tokio::test]` 默认 `current_thread`，`tokio::spawn` 不产生真并发 | 并发用例必须写 `flavor = "multi_thread", worker_threads = 4` |
| 同端口启动「假成功」（2026-09-20 已修） | 目标端口被**另一个 memora 实例**占用时，新子进程因 `EADDRINUSE` 立即退出，父进程的健康探测却命中那个旧实例（响应同样是 `200 ok`）→ `start` 返回 0、打印 `memora started`，实例记录写向一个已死的 pid；调用方以为「刚起的实例」在服务，实际所有写入都打到旧实例 | 已在 spawn 前加端口试绑预检（`ensure_port_available`，地址取 `0.0.0.0` 与服务一致）——端口不可独占即硬失败。手工核验见自检清单第 8 项 |

---

## 8. L4 实测结论：运行形态与部署（2026-09-20）

| 项 | 结果 |
|---|---|
| `cargo test` | **228 passed; 0 failed** |
| `cargo clippy --all-targets` | **零告警** |
| `scripts/smoke-e2e.py` | **28/28 通过** |
| 跨项目隔离 | 项目 A 写入的实体在项目 B 的 `read_graph` 中不可见 |
| 退出码契约 | `stop` = 0、重复 `stop` = 0、未运行时 `status` = **3**、`stop` 后 PID 文件已删除 |
| 状态码语义 | 201 / 400 / 401 / 403 / 404 / 406 / 409 全部与文档一致 |

网页控制台（`/admin`）那一轮的实测结论见 §10。

---

## 9. L6：撤销部署（`deploy-local.sh --uninstall`）

测试部署必须能干净回退，否则「测完就留在机器上」会污染后续排查。撤销没有二进制层入口
（`memora` 只有 `run/start/stop/restart/status/init`），由脚本承担，因为它才知道客户端配置
该往哪看。

```bash
scripts/deploy-local.sh --uninstall --dry-run   # 先看计划，不改动
scripts/deploy-local.sh --uninstall --yes       # 执行
```

**沙箱化测试法**：整体重设 `HOME` 即可完全隔离——配置、数据、废纸篓、客户端配置全都落在
沙箱内，真实部署与真实废纸篓零接触：

```bash
S=$(mktemp -d); export HOME=$S/home
mkdir -p "$HOME/.Trash"                   # 无废纸篓时脚本会拒绝执行（这是预期行为）
export XDG_CONFIG_HOME=$HOME/.config XDG_DATA_HOME=$HOME/.local/share
```

### 验证矩阵（2026-09-20 实测，全部通过）

| # | 用例 | 判据 |
|---|---|---|
| 1 | 部署后 `--dry-run` | 打印 4 步计划与全部绝对路径，退出码 0，**磁盘无任何改动** |
| 2 | 非交互且未加 `--yes` | 退出码 1，报错提示加 `--yes` 或先 `--dry-run` |
| 3 | `--uninstall --yes` | 服务停止、端口释放、数据与配置目录消失、客户端条目已处理 |
| 4 | 客户端配置**仅含** memora | 整个文件移入废纸篓 |
| 5 | 客户端配置含 memora + 其它 server | 仅摘除 `mcpServers.memora`，同文件其它 server 与未知顶层键**原样保留**；原文件进废纸篓可恢复；新文件权限 600 |
| 6 | 幂等：连续执行两次 | 第二次全部走「不存在，跳过」分支，退出码仍为 0 |
| 7 | 配置不可读 | `data_dir` 退回内置默认值并明确提示「自定义 `--data-dir` 覆盖不到」 |
| 8 | 废纸篓不存在 | **在动手前**拒绝（步骤 1 之前），客户端配置未被触碰 |
| 9 | 范围护栏 | `data_dir` 为 `/`、`/tmp`、`$HOME`、`~/Documents`、仓库根 → 全部拒绝；`/opt/memora`、`~/Documents/memora` → 放行（护栏只拒绝这些目录**本身**，不拒绝其下用户自建的子目录） |
| 10 | 卸载后真实部署不受影响 | 另一个实例的端口、配置、客户端条目均未被触碰 |

### 护栏实现上踩到的两个坑（都是本轮实测发现并修复的）

| 坑 | 现象 | 根因与修法 |
|---|---|---|
| 护栏静默失效（一） | `data_dir = $HOME` 未被拦截 | macOS 上 `/tmp` 经 `pwd -P` 变成 `/private/tmp`。拿**解析后**的目标去比对**未解析**的 `${HOME}`，两者永不相等。修法：两侧都过同一个 `canon()` 规范化 |
| 护栏静默失效（二） | `data_dir = $HOME/Documents` 未被拦截 | `canon()` 只对**已存在**的路径做 realpath，未存在的路径原样返回，与同级已存在路径比对时出现「同一目录两种写法」。修法：先上溯到最近的已存在祖先，再拼回未存在的尾部 |
| 系统目录两种写法都拦不住 | `data_dir = /tmp` 绕过 | 黑名单必须同时列 `/tmp` 与 `/private/tmp`——它们是同一目录的两个写法，只列一个就会被另一条绕过 |

> **教训**：凡是「把解析后的路径与未解析的变量做比较」的校验，都要假定它会静默失效——
> 它不会报错，只会放行。这类缺陷只能靠**构造极端输入的正反用例**发现，正向测试永远绿。

---

## 10. L7：网页控制台（`/admin`）

控制台是「人」而不只是「脚本」的入口，因此它的失败模式与前面几层都不同：**页面打不开、登录不了、
点了没反应**——而服务端往往一切正常、日志里什么都没有。故单列一层。

### 沙箱起一个可丢弃的实例

```bash
S=$(mktemp -d /tmp/memora-console-e2e.XXXXXX); export HOME="$S/home"
mkdir -p "$HOME" "$S/data" "$S/conf"
./target/debug/memora init --config "$S/conf/config.toml" --data-dir "$S/data"   # 记下打印出的口令
./target/debug/memora run   --config "$S/conf/config.toml" --port 6799           # 换个端口，别碰真实实例
```

`init` 把随机口令**只打印一次**、把 Argon2id 哈希写进 `config.toml`（权限 600）。
后续用例都用打印出来的那份明文；配置文件里没有明文，所以抄完就不必再关心「文件里能不能找到口令」。

### 验证矩阵（2026-09-20 实测，全部通过）

| # | 用例 | 判据 |
|---|---|---|
| 1 | `GET /admin` | 200 + `text/html` + `no-store` + CSP；body 约 24KB |
| 2 | `GET /api/v1/session`（未登录） | `{console_login_enabled:true, authenticated:false, user:null}` |
| 3 | 错误口令登录 | **401**，body `invalid username or password` |
| 4 | 错误**用户名**登录 | **401，且响应体与 #3 逐字相同**——差异只出现在服务端日志的 `username_matched` 字段，不进响应 |
| 5 | 正确口令登录 | 200 + `Set-Cookie`，属性为 `Path=/; HttpOnly; SameSite=Strict; Max-Age=43200`；**明文链路下不带 `Secure`**（带上会被浏览器丢弃，表现为「登录成功但立刻又变回未登录」） |
| 6 | `GET /api/v1/projects`（带 Cookie） | 200 |
| 7 | `POST /api/v1/projects`（带 Cookie） | 201 + 明文 token（仅此一次） |
| 8 | 经 MCP 写入实体 / 关系 / 观测（项目 token） | `create_entities` / `create_relations`（回读带 id）/ `add_observations` 皆 200 |
| 9 | `GET /projects/{id}/graph`（带 Cookie） | 200，且能看到 #8 写入的全部内容（与 MCP 同源） |
| 10 | **跨源** POST（带 Cookie + `Origin: https://evil.example.com`） | **403** `cross_origin_rejected` |
| 11 | 会话 Cookie 打 `/mcp` | **401**——会话不参与 MCP 鉴权 |
| 12 | 项目 token 打 `/api/v1` | **401**——项目 token 不是管理凭据 |
| 13 | 伪造 Cookie（`memora_session=deadbeef…`） | 401 |
| 14 | `DELETE /api/v1/session` | 204 + `Max-Age=0` 的失效 Cookie |
| 15 | 登出后用旧 Cookie | 401 |
| 16 | 未配置口令时 `GET /api/v1/session` | `console_login_enabled:false`；登录返回 401 并给出「怎么修」的提示 |

> 用例 3 与 4 必须**成对**做：只测「口令错」测不出用户名枚举信道。用户名是本服务唯一的公开标识
> （默认 `admin`），一旦可枚举，爆破就只剩口令这一层——所以两条响应必须逐字相同。

### 无浏览器时的静态补偿（同样固化成了用例）

L7 的盲区是**浏览器端 JS 运行时错误**：它不进服务端日志、HTTP 用例全绿，页面却是一片空白。
本机（macOS 12）装不上 Playwright——`npm install playwright` 成功，但
`npx playwright install chromium` 报 `Playwright does not support chromium on mac12`。
故改用三条**可断言的静态检查**覆盖这一类缺陷：

| 用例 | 挡住的缺陷 |
|---|---|
| 脚本引用的每个 `$("id")` 必须存在于页面，且 id 不得重复 | 任何一个 id 拼错 → `addEventListener` 抛 `TypeError` → **整个脚本中断**，表现为「页面空白、连登录框都不出现」，且没有任何日志可查；id 重复同理（`getElementById` 只返回第一个，第二个永远绑不上，点了没反应） |
| 页面不含 `.innerHTML` / `.outerHTML` / `insertAdjacentHTML` / `document.write(` / `eval(` | 用 `innerHTML` 渲染记忆内容 = 在自己的管理界面里开一个存储型 XSS。断言里用带前导点的 `.innerHTML` 而非裸词，是为了不与注释中「绝不使用 innerHTML」这句话本身冲突 |
| 页面不引用任何外部资源；必须保留「token 只显示一次」的提示 | 自托管常无外网，外链还会引入供应链与隐私问题；而「明文 token 只在创建响应里出现一次」是创建流程里最容易造成不可逆损失的认知点 |

> **这三条替代不了真机浏览**：视觉、交互与浏览器兼容性仍需人工过一眼。它们覆盖的是
> 「静态可判定的那部分失败」，因此适合当 L7 的前置门禁，而不是与人眼核验二选一。
> 换到支持 Chromium 的机器后，应补一次真实浏览器回归，并据此下调这三条的「唯一防线」地位。

