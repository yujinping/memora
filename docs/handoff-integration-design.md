# Memora（忆庐）· 交接（Handoff）集成设计

- **@author** yujinping
- **@date** 2026-10-14
- **@intent** 把原先独立、以 git 为存储的 handoff CLI 并入 Memora：复用其 MCP 端点、项目隔离与鉴权体系，新增「任务交接」这一等公民，使跨客户端接力不再需要第二套工具、第二份配置、第二个数据目录。
- **@relation** 本文是 [`memory-system-design.md`](memory-system-design.md)（下称**主设计**）的补充篇，不修改其既有结论；主设计 §5.4 的会话工具仍然有效，与本篇的分工见 §9.1。

---

## 0. 结论摘要

1. **能做，且整合成本低**。交接所需的全部基础设施——跨客户端可达的端点、项目级隔离、鉴权、持久化后端抽象、单二进制运维——Memora 已经具备，缺的只是一张表、一个仓库契约、四个 MCP 工具和一组 CLI 子命令。
2. **不移植 git**。已确认原 CLI 中 git 只是「顺手当文件存储」，不是需求。版本能力由「追加式不可变修订」自然获得，无需引入 git 的仓库 / 提交 / 推送概念。
3. **交接不是记忆，也不是 transcript**，三者必须分开存（§2）。这是本设计最核心的一条约束：一旦把交接塞进记忆图谱，每次接力都会往 FTS5 里灌一批噪音，`search_nodes` 的记忆召回质量会持续劣化。
4. **默认路径零 ID**（§3.7）。这是本设计对「跨客户端、跨电脑接力」体验的核心承诺：写入方不必给任务起名（缺省落按写入方分片的槽位 `current@<client>`），接手方连一个参数都不用传（`handoff_load{}` / `handoff_ack{}`）。
5. **接手的主流程是「列出最近待接手 → 选一条」**（§3.7）：`handoff_list`（缺省只看待接手、按时间倒序、上限 10）给出可辨认的摘要——`goal` + 状态片段 + `client` + **`session_id`** + 时间。0 条如实说没有；恰好 1 条一句话告知、**你点头后再 ack**；≥2 条**列出来让人选**。人记得的会话标签（如「A1」）只是列表里的一列，不是要输入的地址；ID 只在服务端与代理之间流转。
6. **接手方零安装**（§5.6.1、§5.8）：新电脑上填 **HTTPS 的 MCP 地址 + 项目 token** 两行即可用，不装二进制、不改服务端。**skill 与 CLI 都是可选增强，不是门槛**——往客户端目录丢一个文本文件、或放一个二进制，两者都不算「安装客户端」，装了只是更好用。服务端唯一前置是 `.env` 的 `MCP_ALLOWED_HOSTS` 含对外域名（否则协议层 403 且报错不指向配置项）。`/mcp` 已随服务启动，**不需要「开启 MCP 服务」这种开关**。
7. **流程装在连接里，而不是靠人记命令**（§5.5、§5.6）：MCP `instructions` 随 `initialize` 下发、连接即得，因此它**必须自足**——规则（会话开始先看有无待接手、检查点/换机器保存、只想看看则只读、绝不索要 ID）全部在这里。理由是**通道可用性**：不少 MCP 客户端没有 skills 概念，而即使在支持 skills 的客户端里它也是按需加载，不能把「会话开始就要判方向」押在一个可能不被加载的通道上。`skills/memora-handoff/SKILL.md` 只放质量指导，**删掉它功能完全可用**。人说的是「接着弄」/「换台机器」，不是命令。
8. **「该交还是该接」的推断属于会话内部，CLI 做不了**（§5.6、§10）：CLI 是无状态进程，看不到会话进展，只能靠旁证文件而那是另一个谓词、两个方向都会判错。所以命令面保持 5 个显式子命令（脚本要确定性），方向推断交给代理。**命令面只减少「要记的东西」，不承担猜测。**
9. **一个契约、三种入口**：`HandoffRepository::resolve_handoff` 是唯一寻址语义来源，MCP 工具（模型按需）、REST（CLI / curl / 脚本）、CLI 子命令（人切换客户端时的确定性兜底）都只是它的适配层。
10. **CLI 是可选的第二入口**（§5.3、§5.8）：偏好命令行/脚本/自动化的人、以及代理没按指令行事时，它就是正路。为支持「在任意机器上装一个二进制就能接手」，引入 `ureq`（blocking + rustls）直连 HTTPS，二进制约 +1.5–2.5MB；这种场景恰恰最不可能有一个配好 CA 的 `curl`。
11. **落点建议：P4.3**，插在 P6（多后端）之前。理由与协同要求见 §9.2。

---

## 1. 背景与动机

### 1.1 旧 handoff CLI 的问题

| 问题 | 具体表现 |
|---|---|
| 每台机器都要配一套 | 仓库地址、凭据、工作副本、`.gitignore`；换台机器先 clone |
| 与记忆库两套体系 | 两套凭据、两个目录、两处备份，且互不知道对方存在 |
| 交接内容无法检索 | 内容躺在提交历史里，找「上次那个关于鉴权的决定」只能翻 `git log` |
| 多出三步手工动作 | `add` → `commit` → `push` / `pull`，漏一步就静默丢失接力 |
| 版本能力几乎没人用 | 有 commit 历史，但从没有人去读 diff |

### 1.2 为什么并进 Memora 成本低

| 交接需要 | Memora 已有 |
|---|---|
| 多客户端 / 多机器到达同一份状态 | MCP over HTTP，配同一个 token 即同一份数据 |
| 按「项目」隔离 | `token → project_id` → `data/{project}/mem.db`，物理隔离 |
| 凭据体系 | 项目 token（MCP）+ `ADMIN_TOKEN`（管理面），已成型 |
| 持久化 | `StorageBackend` / `Repository` 抽象 + 共享契约测试 |
| 运维 | 单二进制 + `start/stop/status/init`，无外部服务 |
| 写入来源可追溯 | 上一提交引入的 `source` 字段（同一思路沿用到交接的 `client` 字段） |

### 1.3 git 的真实角色（已确认）

原 CLI 用 git，仅仅是因为「手边有个版本控制工具，顺手当文件存储」。**结论：不移植 git。**

如果将来确实需要「交接内容怎么演变的」这一类审计能力，应走主设计 §11「记忆版本与回滚」的路线（在 Memora 内部做修订链），而不是把 git 重新拉回来——把 git 拉回来等于同时接受 §1.1 的全部五条问题。

---

## 2. 概念边界：记忆 / 交接 / transcript

这是本项目最容易做错的一处，故单列一节。

| 维度 | 长期记忆 | **任务交接** | 会话 transcript |
|---|---|---|---|
| 回答的问题 | 「这个项目/这个人是什么样的」 | 「这活干到哪了、下一步做什么」 | 「当时到底说了什么」 |
| 生命周期 | 长（月～永久） | 短（小时～天），被接手即完成使命 | 中（按需回放） |
| 写入者 | 代理按需（主动写入规则） | 代理在关键节点 / 用户切客户端时 | 代理每轮（P5.1） |
| 体量 | 小条目，多条累积 | 单条几 KB 的结构化摘要 | 大，整段对话 |
| 是否需要被检索 | **是**（`search_nodes` 的主战场） | **否**（按 `task_id` 精确取用） | 弱（按 `session_id` 取） |
| 消费方式 | 反复读取、长期引用 | 读一次 → ack → 继续推进 | 回放 |
| 存放 | `entities` / `observations` / `relations` | **`handoffs`**（新增） | `conversations`（P5.1） |

**为什么不能混**：若把交接写成 observation，则每条交接都会进入 `memory_fts`。跑几次接力后，`search_nodes("重构")` 会被历次交接里反复出现的「重构」二字淹没，而真正的项目约定被挤到结果末尾。**交接的高频写入特性与记忆的低频精炼特性是冲突的**，必须分表。

**边界规则（写进工具描述，避免模型用错）**：

- 交接里出现的**结论性、长期有效**的内容（例如「鉴权统一用 ADMIN_TOKEN」），应在 `handoff_save` 的同时用 `create_entities` / `add_observations` 落进记忆——交接是**过程**，记忆是**沉淀**。
- 交接里不要粘贴 transcript。要回放原始对话，用 P5.1 的会话工具。

---

## 3. 数据模型

### 3.1 `handoffs` 表（`sqlite_file` 后端，位于 `data/{project}/mem.db`）

```sql
-- 任务交接：一次 save = 追加一条不可变修订
CREATE TABLE IF NOT EXISTS handoffs (
  task_id        TEXT    NOT NULL,             -- 任务标识（项目内唯一，人手可写）
  revision       INTEGER NOT NULL,             -- 修订号，从 1 单调递增
  goal           TEXT    NOT NULL,             -- 这活要达成什么
  state          TEXT    NOT NULL,             -- 现在到哪了
  next_steps     TEXT    NOT NULL,             -- JSON 数组：下一步要做什么
  files          TEXT    NOT NULL,             -- JSON 数组：关键文件 / 位置
  decisions      TEXT    NOT NULL,             -- JSON 数组：已定但可能未沉淀进记忆的
  open_questions TEXT    NOT NULL,             -- JSON 数组：需要人拍板的
  client         TEXT    NOT NULL DEFAULT '',  -- 写入方（zed / cursor / workbuddy ...）
  session_id     TEXT    NOT NULL DEFAULT '',  -- 写入方会话标识（便于回查 transcript）
  created_at     INTEGER NOT NULL,             -- Unix 秒
  acked_at       INTEGER,                      -- NULL = 尚未被接手
  acked_by       TEXT    NOT NULL DEFAULT '',  -- 接手方标识
  PRIMARY KEY (task_id, revision)
);

-- list_handoffs 取「每任务最新修订」时走此索引
CREATE INDEX IF NOT EXISTS idx_handoffs_task ON handoffs(task_id, revision DESC);
```

**建表方式**：追加进 `sqlite_file.rs` 顶部那份 `CREATE TABLE IF NOT EXISTS` DDL 数组即可，**无需写迁移函数**。该 DDL 在每次建库/打开时执行，新库与存量库都会补齐这张新表——这与 `source` 列的情况不同（那是给已存在的表加列，才需要 `ensure_source_columns`）。

### 3.2 字段语义

| 字段 | 必填 | 说明 |
|---|---|---|
| `task_id` | **可选** | 任务标识。缺省即落到按写入方分片的默认槽位（§3.7）。字符集 `[A-Za-z0-9._@-]`，长度 1–128 |
| `goal` | ✅ | 一句话说明目标。用于新客户端判断「是不是我要接的活」 |
| `state` | ✅ | 当前进展。**不写过程，只写结论性状态** |
| `next_steps` | ✅（可为空数组） | 交接的核心价值所在。空数组 = 明确的「无事可做」 |
| `files` | ✅（可为空数组） | 接手方最需要的东西：去哪看。相对项目根的路径或模块名 |
| `decisions` | ✅（可为空数组） | 已经定了、但接手方不知道就会重开讨论的事 |
| `open_questions` | ✅（可为空数组） | 需要人拍板的。接手方据此决定要不要先问人 |
| `client` | 可选（但**保存时建议填**） | 写入方标识，空串 = 未声明。它是零 ID 保存的**分片键**（`current@<client>`，见 §3.7），同时并入记忆的 `source` 语义 |
| `session_id` | 可选 | 写入方会话标识；P5.1 落地后可据此拉到原始 transcript |

**`task_id` 校验与 `project_id` 的关键差异**：`project_id` 禁止 `.`，因为它会被拼进文件路径（目录穿越纵深防御）；`task_id` **从不参与路径拼接**（它只是行主键），故允许 `.` 与 `@`——`.` 便于 `v1.2-api-refactor` 这类命名，`@` 是服务端分片槽位 `current@<client>` 的分隔符（§3.7）。两者共用一套长度上限与「拒绝空 / 空白」的规则，但**不共用字符集**——这一点必须在 `normalize.rs` 里分开实现，不要图省事复用一个校验函数。

### 3.3 为什么用 JSON 数组列，而不是 `handoff_items` 子表

| 方案 | 优点 | 缺点 | 结论 |
|---|---|---|---|
| JSON 数组列（本文） | 一次取整条，无 join；数组保序；实现小 | 无法按单条 item 做 SQL 查询 | ✅ 采用 |
| 子表 `handoff_items(task_id, revision, kind, ord, text)` | 可按 item 查询 / 过滤 | 每次读写都要 join + 排序重组；为「永远不会用到」的查询能力付费 | ❌ 否决 |

判断依据：**交接永远是整条读取的**。没有任何已知场景需要「只查所有交接里提到 `auth.rs` 的那一条」。若将来需要这种检索，正确的做法是让 `handoff_save` 同时往记忆里写一条，由 `search_nodes` 承担检索——而不是把交接表改造成可查询的结构。

序列化用 `serde_json`（已是依赖）。空数组序列化为 `[]`，**不允许 NULL**：`NULL` 与 `[]` 的区别对调用方毫无意义，只会制造两种「空」的分支。

### 3.4 为什么不复用记忆图谱

见 §2 的噪音论证。补充一条实现层面的理由：记忆图谱的写侧是**幂等合并**语义（重名忽略、三元组去重、观测追加），而交接的写侧是**追加修订**语义。两者对同一张表的要求相反，硬塞进去会让 `create_entities` 的幂等性变得无法解释。

### 3.5 校验与容量上限

| 规则 | 值 | 理由 |
|---|---|---|
| `task_id` 字符集 / 长度 | `[A-Za-z0-9._@-]`，1–128 | 见 §3.2；`@` 供分片槽位使用 |
| 单字段（`goal` / `state`）长度 | ≤ 16 KiB | 一句话要说清的事，超过就是没想清 |
| 数组元素数 / 单元素长度 | ≤ 64 项 / 每项 ≤ 4 KiB | 交接是摘要，不是清单倾倒 |
| 整条上限 | ≤ 64 KiB | 超限说明用错了工具，应改走 P5.1 transcript |

校验一律「**整批先校验、再落库**」，与记忆写侧同源（主设计 §9 P2 取舍 1）：非法输入不产生半条修订。

### 3.6 其他后端的对应形态

| 后端 | 形态 |
|---|---|
| `sqlite_file` | 见 §3.1；`project_id` 由「库文件」承担，表内**不加**该列（与 `memory` 模块一致） |
| `sqlite_single`（P6） | 加 `project_id TEXT NOT NULL`，主键扩为 `(project_id, task_id, revision)`，所有查询附带租户条件（与 `entity.rs::single` 的既有做法一致） |
| `in_mem` | `HashMap<task_id, Vec<Handoff>>`，仅供契约测试与临时验证 |
| `postgres`（预留） | 同 `sqlite_single` 的租户列或 schema 隔离 |

### 3.7 寻址模型：默认路径必须「零 ID」

**设计原则：标识符是给程序寻址用的，不是给人的记忆负担。** 交接最常见的场景是「我在 A 机器上停下，到 B 机器上接着弄」——此时人的脑子里只有一个念头「接着上次那个」，**没有任何标识符**。若接口要求他输入或回忆一个 ID，这个功能就已经失败了：跨设备场景下他连上次的 ID 都无从查起。

因此把 ID 从**主路径**上彻底移除，只在**并行工作流**这种需要精确指认的场景才出现：

| 角色 | 默认路径 | 需要 ID 吗 |
|---|---|---|
| 写入方（代理） | `handoff_save{goal, state, next_steps, ...}`，不给 `task_id` | ❌ 不需要，也不该被要求起名 |
| 写入方（人，CLI） | `memora handoff save --project memora < handoff.json` | ❌ 不需要 |
| 接手方（代理，MCP） | `handoff_list` → 选中 → `handoff_load` + `handoff_ack`（只有一条时可走 `handoff_load{}` / `handoff_ack{}` 直取） | ❌ 一个参数都不用传；选中哪条从**列表里看**，不靠记忆 |
| 接手方（人，CLI） | `memora handoff list` → `load --task <抄下来的>` → `ack --task <同一个>` | ❌ 同上；`task_id` 是从列表**抄**的，不是回忆的 |
| 并行工作流 | `--task v1.2-api-refactor` 显式指定 | ✅ 仅在**刻意**并行时才需要 |

**MCP 路径为什么能做到一个参数都不传**：`project_id` 由 Bearer token 解析得出（主设计 §5.3），客户端配置里本来就带着 token，所以「是哪个项目」根本不需要人告诉它——这也正是 Memora 把项目放进 token 而非 URL 的既有收益。

**先澄清一个容易混的概念：交接挂在「工作项」上，不挂在「会话」上。**

| 概念 | 是什么 | 例子 |
|---|---|---|
| 会话（session / transcript） | 这件事**在哪发生的** | Zed 今天下午那个窗口；WorkBuddy 电脑 B 上的新对话 |
| 工作项（task / workstream） | **在做什么** | 「把 handoff 并进 memora」；「修鉴权的 401」 |

同一件工作**必然**跨多个会话、多个客户端、多台机器——这正是需要交接的原因。所以不存在「交接哪个会话」，只有「交接哪件工作」。把交接绑到会话上会直接自相矛盾：接手方开的是新会话，它的 `session_id` 与写入方永远不同。`session_id` 因此只是**可选溯源信息**（「这份交接是在哪个会话里写的」，便于回查 transcript），不参与寻址。

**接手的主流程：先列出「最近待接手」，再选。** 这是默认交互，不是兜底——对齐真实设想：在 B 电脑上执行一条 `latest` / `list`，看到最近若干条待接手，然后在 AI 界面里挑一条。

| 待接手条数 | 行为 |
|---|---|
| 0 | 如实告诉人「没有待接手的」，不猜、不编 |
| 1 | **一句话告知**「有一条待接手：X，要接着弄吗」；**你说要就直接接，不再追问细节**。会话刚开、你还什么都没说时**不要抢先把 ack 打出去**——那会把这条从别的客户端的待接手列表里抹掉（§7.4） |
| ≥2 | **列出摘要让人选**，绝不自动挑一条 |

注意上表第 2 行与「读/改分离」的关系：**告知是读、ack 是改**。唯一的候选也不构成 ack 的授权——授权只来自你说了「接着弄」。这一条同时解释了为什么 `handoff_list` 本身永远不产生副作用（§5.6）。

列表字段的取舍标准是「**认得出是哪一条**」，而不是「信息完整」：`goal` + 状态片段 + `client` + `session_id` + 时间，一屏 5 条足够（默认 `limit = 10`，可调）。正文（`next_steps` / `decisions`）**不进摘要**——否则一屏放不下，也没必要。

三处值得点明的设计含义：

- **用户口中的「A1」不需要是地址，只需要是一列。** 人会记得「a 电脑 zed 上那个会话」，但若要求他把 A1 敲进来，功能就已经失败了。让 `session_id` 出现在列表里，识别问题就退化成「看一眼、点一下」。这就是为什么 §4.2 的 `HandoffSummary` 带 `session_id` 与 `state_excerpt`——它们是**展示列**，不是寻址键。
- **`handoff_load{}` 零参数是加速器，不是主路径**：只有一条时它等价于「列表 + 自动选第一条」；多条时它退化为返回候选列表（与 `handoff_list` 同源）。两条路都通向「列表 + 选择」，**不存在「必须先知道 ID」的入口**。
- **这就是「列表选择」而非「ID 输入」**：与 `claude --resume`、「打开 IDE 的最近项目」同构。ID 只在服务端与代理之间流转一次，从不要求人记忆。

**默认槽位按写入方分片**：`task_id` 缺省时落到 `current@<client>`（`client` 也缺省时为 `current`）。槽位名由服务端拼出，人从不输入。

- 同一写入方连续 save → 追加同一槽位的修订（§7.1），**不遮蔽、不产生歧义**；
- 不同写入方各自持有待办 → 接收方读上面的列表挑选；
- 跨客户端接力依然是零 ID：Zed 写 `current@zed` → WorkBuddy `handoff_load{}` 只见到一条待接手 → `ack` 接手 → WorkBuddy 继续推进时写 `current@workbuddy`。

**为什么不能把所有无 ID 的 save 都塞进同一个 `current`**：第二个写入方无 ID 的 save 会变成 `current` 的新修订，而按 §7.2「待接手候选只看每个任务的最新修订」，**第一条工作流会被静默遮蔽**——接手方永远看不到它，直到有人显式删除都不会有人知道它存在过。分片是修掉这个遮蔽的最小改动，代价只是槽位名多一个 `@`。

**能力边界（明写，不掩盖）**：同一个 `client` 标识下**真的并行**两条工作流时，必须显式给 `task_id`。这不是实现偷懒，而是信息论上的必然——「这是两件不同的事」只存在于调用方，服务端无法凭空发明一个区分。此时标签由**代理**生成与回传（`handoff_save` 的响应里就带着 `task_id`），人依然不需要记忆。

因此代理侧指令要写清一条：**「若你正在推进的工作项与 `handoff_load` 得到的那条不是同一件，必须给它一个简短的 `task_id`。」** 把这条边界转化为代理的行为，而不是转化为人的负担。

**ID 的生命周期（代理侧同样不需要人参与）**：`handoff_save` 与 `handoff_load` 的响应都返回 `task_id`；代理在后续 `save` 时原样带回，即「继续同一工作项」。新会话接手时从 `handoff_load` 拿到它，之后一路带着走。**ID 只在工具结果里流转，从不经过人的输入。**

**无 ID 解析算法（`task_id == None`，契约阶段 R1）**：

1. 候选 = 每个任务的**最新修订**中 `acked_at IS NULL` 的那些（旧修订即使未被接手也不再候选——它已被新修订取代，见 §7.2）；
2. 按 `created_at` 降序、`task_id` 升序排序；
3. 0 条 → `HandoffResolution::None`；1 条 → `Found`；≥2 条 → `Ambiguous(摘要列表)`。

歧义时**绝不替调用方猜**（按时间猜「最新的那条」看着聪明，但在并行工作流下会把两个客户端的活接串）。返回摘要列表让对方挑，然后带 ID 精确取——对代理而言这一步不需要人参与，它自己读完摘要就能选。

**跨设备清单**：接手设备需要准备的东西，全部与交接无关——只有「添加 Memora 的 MCP 配置（url + token）」这一件本来就必需的事。ID、服务器地址、数据目录都不需要人记住，**也不需要在那台机器上安装 memora 二进制**：代理走的是纯 HTTP 的 MCP，客户端配置里几行 JSON 即可。

只有在模型没按指令调用工具、需要人用 CLI 兜底时，才需要那台机器上有 `memora` 二进制——它是同一个单二进制，且 CLI 的 `load` / `ack` 同样零 ID（§5.3）。

**CLI 的项目参数也能省**（见 §5.3）：`--project` 缺省时依次取 `MEMORA_PROJECT` 环境变量 / `.env`、然后「服务端只有一个项目」时自动选定并打印选定结果，都取不到才报错。这样人在 B 机器上敲的第一条命令可以短到 `memora handoff load`。

---

## 4. 仓库契约

### 4.1 `HandoffRepository`（后端无关）

```rust
// src/storage/repo.rs（与 MemoryRepository 并列）
#[async_trait]
pub trait HandoffRepository: Send + Sync {
    /// 追加一条不可变修订，返回落库后的完整记录。
    /// `input.task_id == None` 时落到 `current@<client>` 分片槽位（见 §3.7）；
    /// revision 由后端分配（从 1 单调递增）；新修订的 acked_at 恒为 NULL。
    async fn save_handoff(&self, input: HandoffInput) -> Result<Handoff, StorageError>;

    /// 解析一条交接。**这是 load / ack 共用的唯一寻址入口**，避免两处各自实现
    /// 「无 ID 时挑哪条」而出现语义漂移。
    /// - `task_id == Some(id)`：精确取该任务的最新修订
    /// - `task_id == None`：取「待接手」的那一条（算法见 §4.3 R1）
    /// 三种结果都是正常返回，没有一种是错误（见 §5.1 错误分流）。
    async fn resolve_handoff(
        &self,
        task_id: Option<&str>,
    ) -> Result<HandoffResolution, StorageError>;

    /// 列出任务摘要（每任务只给最新修订的摘要）。**接手选择界面的数据来源。**
    ///
    /// - `unacked_only = true`（推荐缺省）：只列待接手的，即「有哪些活等着我」；
    /// - `limit`：最多返回多少条（缺省 10），使「最近 5 条」这种用法不必全量扫描；
    /// - 排序：`created_at` 降序，同秒时 `task_id` 升序（稳定，便于测试与人工扫读）。
    async fn list_handoffs(
        &self,
        prefix: Option<&str>,
        unacked_only: bool,
        limit: usize,
    ) -> Result<Vec<HandoffSummary>, StorageError>;

    /// 标记接手，寻址规则与 [`resolve_handoff`] 完全一致（同样接受 `None`）。
    /// 仅当目标修订尚未被接手时生效；已接手时为幂等的空操作。
    async fn ack_handoff(
        &self,
        task_id: Option<&str>,
        client: &str,
    ) -> Result<AckOutcome, StorageError>;

    /// 删除某任务的全部修订（幂等；不存在视为成功）。
    async fn delete_handoff(&self, task_id: &str) -> Result<(), StorageError>;
}
```

> `resolve_handoff` 刻意**不给 `load` / `ack` 各写一份寻址逻辑**：一旦两处实现略有差异，就会出现「`load` 拿到 A、`ack` 却标记了 B」——这种缺陷在生产里极难复现，却会让接手状态彻底错乱。

### 4.2 领域类型（`src/domain.rs`）

```rust
pub struct HandoffInput {
    /// `None` = 落到默认槽位（见 §3.7）。交接者无需为一个「临时标记」起名。
    pub task_id: Option<String>,
    pub goal: String,
    pub state: String,
    pub next_steps: Vec<String>,
    pub files: Vec<String>,
    pub decisions: Vec<String>,
    pub open_questions: Vec<String>,
    pub client: String,      // 空串 = 未声明
    pub session_id: String,  // 空串 = 未声明
}

pub struct Handoff {
    pub task_id: String,     // 落库后总有值（默认槽位或调用方指定）
    pub revision: i64,
    pub goal: String,
    pub state: String,
    pub next_steps: Vec<String>,
    pub files: Vec<String>,
    pub decisions: Vec<String>,
    pub open_questions: Vec<String>,
    pub client: String,
    pub session_id: String,
    pub created_at: i64,
    pub acked_at: Option<i64>,
    pub acked_by: String,
}

/// list 用的轻量摘要：省掉正文，只留「**认得出是哪一条**」所需的信息。
///
/// @intent 这是选择界面的数据来源，字段取舍的标准不是「信息完整」而是「可辨认」：
///         用户/AI 要在一屏 5 条里认出「就是那个」，靠的是 goal + 状态片段 +
///         谁写的 + 什么时候 + 会话标签。正文（next_steps / decisions）不进摘要，
///         否则一屏放不下、也没必要。
pub struct HandoffSummary {
    pub task_id: String,
    pub revision: i64,
    pub goal: String,
    /// 状态首行截断（≤ 120 字符）；goal 相近时靠它区分
    pub state_excerpt: String,
    pub client: String,
    /// 写入方会话标签（如 "A1"）。**不是地址，只是一列可辨认的展示信息**
    pub session_id: String,
    pub created_at: i64,
    pub acked: bool,
    pub acked_by: String,
    pub next_step_count: usize,
}

/// 寻址结果：三种都是**正常返回**，没有一种是错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffResolution {
    /// 命中唯一一条
    Found(Handoff),
    /// 没有可接手的交接（项目里从来没有，或全部都已被接手）
    None,
    /// 有多条同时待接手，无法替调用方决定。带摘要让调用方挑选后再用 ID 精确取
    Ambiguous(Vec<HandoffSummary>),
}

/// ack 的结果：`won` 说明本次调用是否真的抢到了接手权。
pub struct AckOutcome {
    pub resolution: HandoffResolution,
    pub won: bool,
}
```

### 4.3 语义契约（由共享契约测试钉住）

新增 `contract::run_handoff_contract(repo: &dyn HandoffRepository)`，纳入 `storage/contract.rs`（`#[cfg(test)]`），与 `run_all` / `run_backend_contract` 并列。**每个后端各跑一遍**，因而 `sqlite_file` 与 `in_mem` 的行为必须完全一致。

| # | 阶段 | 断言 |
|---|---|---|
| H1 | 空仓库 | `resolve_handoff(Some("nope"))` → `None`；`resolve_handoff(None)` → `None`（均**不报错**） |
| H2 | 首次 save（无 ID） | `client="zed"` 时落库 `task_id == "current@zed"`（分片槽位）；`revision == 1`；六个正文字段回读一致；数组**保序**；空数组回读为空数组 |
| H3 | 同一写入方再次 save | `revision == 2`；`resolve(Some("current@zed"))` 返回修订 2；修订 1 仍在 |
| H4 | 无 ID 解析 / 唯一 | 槽位 `current@zed` 待接手时 `resolve(None)` → `Found`，`task_id == "current@zed"`（**零 ID 可接手**） |
| H5 | 无 ID 解析 / 已接手 | `ack(None)` 后 `resolve(None)` → `None`（已接手的不再被当作待接手） |
| H6 | 无 ID 解析 / 歧义 | 显式建两个任务 `alpha` / `beta` 均未接手 → `resolve(None)` → `Ambiguous`，摘要按 `created_at` 降序、`task_id` 升序 |
| H7 | 显式 ID 精确寻址 | `resolve(Some("beta"))` 命中 `beta`，不受 `alpha` 影响（歧义时调用方挑选的路径可用） |
| H8 | **分片不遮蔽**（修掉 §3.7 的缺陷） | `zed` 与 `workbuddy` 各自无 ID save 一条 → 落为**两个不同槽位**；两者都未接手时 `resolve(None)` → `Ambiguous` 且 `candidates` 含两者。**绝不能出现「后者把前者遮蔽成一个修订」** |
| H9 | list 语义 | 每任务只出现一次；`created_at` 降序稳定；`unacked_only=true` 只返回未接手；`acked` / `acked_by` 反映最新修订 |
| H10 | list 摘要可辨认性 | 摘要含 `goal` / **`state_excerpt`** / `client` / **`session_id`** / `created_at`；`state_excerpt` 超长时按约定截断（≤ 120 字符）；`next_step_count` 与实际数组长度一致 |
| H11 | list limit | `limit = 5` 时最多返回 5 条，且返回的是**最新的 5 条**（截断发生在排序之后，不是之前）；`limit = 0` 的处理在实现前定死（建议「返回空」而非「视为无限」，后者是静默的语义陷阱） |
| H12 | list prefix | 前缀过滤只影响 task_id 匹配，不影响排序，也不影响 `limit` 的计数口径 |
| H13 | ack 首次（显式 ID） | `won == true`；`acked_at` 落值、`acked_by` 为调用方标识 |
| H14 | ack 重复 | `won == false`；`acked_at` / `acked_by` **保持首次值不变**（幂等，不覆盖） |
| H15 | ack 无 ID 歧义 | 两个任务均未接手时 `ack(None)` → `Ambiguous` 且 `won == false`（**绝不替调用方猜**） |
| H16 | save 重置接手态 | 已接手的槽位再 save → 修订 2 的 `acked_at` 为 `NULL`，且 `resolve(None)` 重新 `Found`——内容变了，上一次接手不算数 |
| H17 | delete 幂等 | 删除后 `resolve` → `None`、`list` 不含；重复 delete 成功 |
| H18 | 非法输入原子性 | 空 `goal`、非法 `task_id`、超长字段 → 报错且**不产生任何修订** |

**R1（无 ID 解析规则）是这张表里最该被单独盯住的一条**：H4 / H5 / H6 / H15 合起来钉死「零 ID 主路径可用，歧义时不猜」；**H8 单独钉死「多写入方并行时互不遮蔽」**——它正是「多个会话同时存在」这一场景在契约层的保障，也是 §3.7 那个缺陷不再复发的唯一硬约束。**H10 / H11 钉死「列表能认出人、且只截断在排序之后」**——上一条是交互质量，后一条是真实缺陷的高发点（先在 SQL 里 `LIMIT` 再排序会返回最旧的几条，而这个错误在小数据量下测不出来）。

**跨项目隔离不在 `run_handoff_contract` 内**：仓库级契约只拿得到一个项目的 repo，测不了隔离。该断言属于**后端级契约**——在 `run_backend_contract` 中追加（`repositories_for("a")` 与 `("b")` 各写一条交接，互不可见）。对 `sqlite_file` / `in_mem` 这是平凡成立（各自独立容器），对 `sqlite_single` 才是真正有意义的租户过滤断言。

### 4.4 ack 的并发实现（CAS）

`ack_handoff` 必须是**原子比较并交换**，否则两个客户端同时接手时会双双认为自己是接手人：

- SQLite：`UPDATE handoffs SET acked_at = ?, acked_by = ? WHERE task_id = ? AND revision = ? AND acked_at IS NULL`，随后按 `changes()`（受影响行数）判断是否 `won`，再读回当前最新修订。
- 内存后端：在单把锁内完成「判断 + 写入」，天然满足。

**为什么 `won` 必须回传**：交锋的双方需要知道「是不是我先接的」。后到者看到 `won=false` + `acked_by=zed`，就能决定是停下来问人，而不是两个客户端同时改同一份代码。

---

## 5. 对外接口

### 5.1 MCP 工具（新增 4 个）

命名采用**家族前缀** `handoff_*`，而非续用 `动词_名词` 风格。理由：工具面从「单一官方家族（9 个）」扩为「两个家族（13 个）」，前缀让模型与人在 `tools/list` 里一眼分组；`create_handoff` 这种命名会混进既有动词序列，反而更难分辨。既有 9 个工具**一个都不改名**。

| 工具 | 关键参数 | 行为 | 返回 |
|---|---|---|---|
| `handoff_save` | `goal, state, next_steps[], files[], decisions[], open_questions[], task_id?, client?, session_id?` | 追加一条修订（缺 `task_id` 时落 `current@<client>` 分片槽位；新修订接手态重置） | `{handoff, task_id, revision}` |
| `handoff_load` | `task_id?` | **不给 `task_id` = 取当前待接手的那条**；给了则精确取 | `{status, handoff?, candidates[]}`，`status ∈ {found, none, ambiguous}` |
| `handoff_list` | `unacked_only?`（缺省 **true**）, `limit?`（缺省 10）, `prefix?` | **列出「有哪些活等着我」**，接手选择界面的数据来源 | `{handoffs[]}`，每项是含 `goal` / `state_excerpt` / `client` / `session_id` / `created_at` 的摘要 |
| `handoff_ack` | `task_id?, client?` | 标记接手（CAS）；**不给 `task_id` = 接手当前待接手的那条** | `{status, won, handoff?, candidates[]}` |

**接手的主路径是 `handoff_list` → 选 → `handoff_load` + `handoff_ack`**（§3.7 的三分支行为）。`handoff_list` 的 `unacked_only` **缺省为 true**：这个工具的主场景是「选一条接手」，已接手的不该混进来；想考古（「我上周交接的是什么」）显式传 `false`。

**`handoff_load{}` 零参数是加速器**：只有一条待接手时等价于「列表 + 自动选第一条」，多条时与 `handoff_list` 一样返回候选。`candidates[]` 仅在 `status=ambiguous` 时非空，携带摘要供模型挑选后再用 `task_id` 精确取。

**`client` 在两个工具里的地位不同**，这点必须在描述里写清，否则代理会以为它无关紧要：

- 在 `handoff_save` 里，`client` 是**分片键**——它决定无 `task_id` 时落到哪个槽位（`current@zed` vs `current@workbuddy`），因此是**语义参数**，省略会导致不同客户端撞进同一个项目级 `current` 槽位；
- 在 `handoff_ack` / `handoff_load` 里，它只影响 `acked_by` 的诊断信息，省略落空串（沿用本项目「空串 = 未声明」的既有约定，与记忆的 `source` 同源），**不影响寻址与接手语义**。

**工具清单常量**要拆开，避免「对外承诺」与「官方对齐」两个概念混在一个数组里：

```rust
// src/mcp/tools.rs
pub const MEMORY_TOOL_NAMES: [&str; 9] = [ /* 现有 9 个，原样 */ ];
pub const HANDOFF_TOOL_NAMES: [&str; 4] =
    ["handoff_save", "handoff_load", "handoff_list", "handoff_ack"];
/// 对外承诺的完整工具面 = 两个家族之和（供 `mcp::mod::assert_tool_manifest` 使用）
pub const TOOL_NAMES: [&str; 13] = [ /* 上面两者拼接 */ ];
```

- `mcp/mod.rs::assert_tool_manifest` 逻辑不变（比对完整清单与宏实注册数），只是数组变长——启动自检依然成立。
- 现有测试 `tool_names_cover_the_nine_memory_tools` 改为断言 `MEMORY_TOOL_NAMES`，并**新增**用例断言两个家族无重名、总数 13。这样「9 个官方语义工具没被污染」这一承诺仍然被钉住。

**错误码分流（沿用主设计原则，但多出第 4 类）**：

| 情形 | 码 | 理由 |
|---|---|---|
| 入参非法（空 `goal`、非法 `task_id`、超长） | `-32602` | 调用方改参数即可 |
| 后端故障 / 项目 id 非法 | `-32603` | 服务端问题 |
| **`task_id` 不存在** | **不是错误**，`status="none"` | 「第一次接手时还没有交接」是正常状态，不是调用方错误。与 `open_nodes` 跳过未知名字同源 |
| **多条同时待接手** | **不是错误**，`status="ambiguous"` + `candidates[]` | 服务端不替调用方决定接哪条（§3.7 R1）；让调用方看完摘要自己选 |
| ack 时已被他人接手 | **不是错误**，`won=false` | 是正常的竞态结果，回传事实即可 |

### 5.2 REST 管理面（新增 7 个路由）

挂在既有 `/api/v1` 之下，鉴权沿用 `ADMIN_TOKEN`：

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/api/v1/projects/{id}/handoffs` | 保存修订，`201`，返回完整记录；体中 `task_id` 可省略 |
| `GET` | `/api/v1/projects/{id}/handoffs` | **列摘要（接手选择界面）**：`?unacked=1`（缺省即 1，传 `0` 看历史）、`?limit=5`（缺省 10）、`?prefix=` |
| `GET` | `/api/v1/projects/{id}/handoffs/pending` | **零 ID 入口**：解析当前待接手的交接，`200`；无则 `204`；歧义则 `300`+候选摘要 |
| `GET` | `/api/v1/projects/{id}/handoffs/{task_id}` | 精确取最新修订；不存在 `404` |
| `POST` | `/api/v1/projects/{id}/handoffs/pending/ack` | **零 ID 接手**，体 `{"client":"..."}`；歧义则 `300` |
| `POST` | `/api/v1/projects/{id}/handoffs/{task_id}/ack` | 精确接手，体 `{"client":"..."}` |
| `DELETE` | `/api/v1/projects/{id}/handoffs/{task_id}` | 幂等删除，`200` |

**该用哪个**：CLI 的 `list` / `load` / `ack` 分别走「列表」「pending 或 `{task_id}`」「pending/ack 或 `{task_id}/ack`」。列表与解析两类端点与 MCP 的 `handoff_list` / `handoff_load` / `handoff_ack` **共用同一条仓库契约**，因此「列什么、无 ID 挑哪条」的规则各只有一处实现。

**状态码语义**与主设计 §5.2 完全一致（400 可修正 / 401 凭据 / 404 不存在 / 500 服务端），另加两种上述分流：
- `204 No Content` = 没有待接手的交接（正常状态，不是 404——404 留给「指定的 `task_id` 不存在」）。
- `300 Multiple Choices` = 多条待接手，响应体给出候选摘要。选它是为了不与既有 400/404 的语义混淆：这不是调用方写错了什么，而是需要他做一次选择。

`GET /api/v1/projects/{id}/stats` 的响应增加 `handoffs` 计数（有交接的任务数）。

### 5.3 CLI（`memora handoff ...`）

```
memora handoff save [--task <id>] [--file <path>] [--client <name>]
                                  # 缺省读 stdin；--task 省略即落 current@<client>
memora handoff list [--limit <n>] [--all] [--prefix <p>] [--json]
                                  # 缺省只列待接手、按时间倒序、上限 10；--all 含已接手
memora handoff load [--task <id>] [--json]
                                  # 省略 --task = 取当前待接手的那条
memora handoff ack  [--task <id>] [--client <name>]
                                  # 省略 --task = 接手当前待接手的那条
memora handoff rm   --task <id>

项目：--project <id>   可省略：依次取 MEMORA_PROJECT（环境变量 / .env）、
                       服务端只有一个项目时自动选定（会打印选定了哪个）
全局：--server <URL>   默认 http://127.0.0.1:<PORT>；支持 https://（见 §5.4）
      --timeout <sec>  缺省 10；连接与整体读共用
```

**最短可用路径**（B 机器上接着干，一个 ID 都不用记）——这就是设想里的「latest」：

```bash
memora handoff list            # 最近 10 条待接手，倒序；看目标/状态/谁写的/会话/时间
memora handoff load --task <从上面选中那条的 task_id>   # 或把 task_id 交给代理
memora handoff ack  --task <同一个 task_id>
```

`list` 的默认行为**刻意就是「待接手 + 倒序 + 上限 10」**，无需任何参数即等价于设想中的 `latest`；因此不额外加 `latest` 子命令——一个 flag 组合不该占一个动词位（若你更想要这个字面量，加别名是一行的事，见 §13）。

**人会怎么用**：`memora handoff list` 印出带序号的表格，选中哪条由**人看一眼**决定——这与在 AI 界面里让模型列出候选再由人点选是同一份数据（§4.2 的 `HandoffSummary`）。**两种入口共用一份摘要，不会出现「CLI 看到的和 AI 看到的不一样」。**

**输入契约**：`save` 只接受 **JSON**（与 REST / MCP 同构），不提供 `--goal/--state/--next` 这类拼装选项。理由：选项面越小越好（本项目的既有取舍，见主设计 §9 P4.2 取舍 5）；交接的主要写入者是代理（走 MCP），CLI 是确定性兜底，手工 JSON 的轻微不便换来解析器与文档的显著简化。

```bash
# 示例：切客户端前手动存一次交接（不给 task_id，client=zed → 落到 current@zed）
cat <<'JSON' | memora handoff save --project memora --client zed
{
  "goal": "把 handoff CLI 并入 memora，作为 P4.3 落地",
  "state": "设计文档已完成，尚未写代码",
  "next_steps": ["domain.rs 增加 Handoff 类型", "sqlite_file 建表并实现 HandoffRepository"],
  "files": ["docs/handoff-integration-design.md", "src/storage/repo.rs"],
  "decisions": ["交接独立建表，不混入记忆图谱", "ack 是可见状态而非删除"],
  "open_questions": ["同一 client 并行两条工作流时标签怎么起"],
  "client": "zed",
  "session_id": "A1"
}
JSON
```

**CLI 不直连存储**：CLI 只经 HTTP 调 REST 端点，与 `daemon` 一样**不触碰 storage / routes**。这条边界很重要——它保证 CLI 对「用哪个后端、数据在哪个文件」完全盲视，将来换 `sqlite_single` / `postgres` 时 CLI 零改动。

**解析器改动（这是集成中最容易被低估的一处）**：现有 `cli.rs::parse` 的规则是「首个位置参数为子命令，其余位置参数一律拒绝」（见该文件模块注释第 2 条）。`memora handoff save` 有两个位置参数，因此必须把命令模型改为两级：

```rust
pub enum Command {
    // ... 现有 8 个变体不变
    Handoff { action: HandoffAction },   // 新增
}

pub enum HandoffAction { Save, Load, List, Ack, Remove }
```

解析规则改为：首个位置参数 → 父命令；**仅当父命令是 `Handoff` 时**，允许第二个位置参数作为 action（缺失则报错并列出候选）；第三个及以后仍拒绝。新增 flag：`--project` / `--task` / `--limit` / `--all` / `--prefix` / `--client` / `--server` / `--timeout` / `--file` / `--json`。

**必填校验刻意放宽到「几乎都可不填」**（这是 §3.7 在 CLI 侧的兑现）：

| 参数 | 规则 |
|---|---|
| `--project` | 可省略，按「`MEMORA_PROJECT` → 服务端唯一项目」解析；都失败才报错，且错误信息给出两种解决办法 |
| `--task` | **`save` / `list` 完全可省；`load` / `ack` 可省（走 R1 解析）；仅 `rm` 必填**——删除是有意为之的破坏性操作，必须明确指认，不能靠「猜当前待接手的」 |
| `--limit` / `--all` | 均可省（缺省 10 条、只看待接手）。`--all` 是 `--unacked=0` 的语义化写法，**不提供 `--unacked`**——正向 flag 与默认值重合时，读命令的人要多想一步 |
| `--client` | **`save` 时建议填**（它是分片键，决定零 ID 交接落到哪个槽位，见 §5.1）；`load` / `ack` 时可省略（仅影响 `acked_by` 诊断）。缺省落空串 = 未声明 |

现有子命令的行为与退出码语义**不变**。新增退出码一档：`list` / `load` / `ack` 遇到「无待接手交接」返回 `0`（正常无事可做）并在 stderr 提示一句，**不**视为错误；遇到「歧义」返回非 0 并打印候选列表（因为此时脚本需要人来选，而人需要看到候选才能选）。

**CLI 执行代码放在新模块 `src/handoff_cli.rs`**，依赖方向为 `main → handoff_cli → config`（+ 共享的 `http` 模块），与 `daemon` 同级，**不依赖 storage / routes**。

### 5.4 HTTP 客户端（支持 HTTPS，已定案）

**决策（已定）：CLI 必须能直连公网 HTTPS**，因此交接 CLI 不再使用 `daemon` 那套手写客户端（它只支持本机明文 HTTP）。

| 用途 | 实现 | 理由 |
|---|---|---|
| `daemon` 的本机健康探测 | **保持手写的 `http_get`**（零依赖、已有测试） | 只探 `127.0.0.1`，TLS 无用；改动它有回归风险而无收益 |
| `handoff_cli` 的 REST 调用 | **新模块 `src/http.rs`，基于 `ureq`（blocking）** | 需要通用 URL（自定义 host/port）、POST JSON、**TLS** |

**为什么选 `ureq` 而不是 `reqwest`**：CLI 是**同步**的（与 `daemon` 同风格），`reqwest` 会拉入 async 栈（tokio + hyper），而 `ureq` 是阻塞式、依赖面小得多，更贴合这里的形态。两者都能满足需求，但 `ureq` 的代价更小。

```toml
# Cargo.toml 新增（具体版本与特性名以实现时 crates.io 当前文档为准）
ureq = { version = "3", default-features = false, features = ["rustls", "json"] }
```

- 根证书用 `rustls` 的 WebPKI 根证书（不依赖系统 CA 配置），与 Let's Encrypt 签发的服务端证书兼容；
- 保留 `Connection: close` 语义即可，无需自己解析 chunked；
- **超时**：连接与整体读各设一个（`--timeout`，缺省 10s），否则 `memora handoff load` 打不通时会像卡死。

**代价要如实承认**：TLS 客户端不可能零体积，预计二进制 +1.5–2.5MB（`ring`/`aws-lc` 系加密库是大头）。但这笔代价**恰恰是在维护「单二进制」承诺**——反过来依赖运行环境里恰好装了一个支持 HTTPS 的 `curl`，才是真正破坏了「单文件交付」。

**`--server` 语法**：`http://host:port` 或 `https://host[:port]`（https 缺省 443，http 缺省取配置的 `PORT`）。缺省 `http://127.0.0.1:<PORT>`。若部署在子路径下（如 `https://mem.example.com/memora`），`--server` 允许带路径前缀，请求时拼到 `/api/v1/...` 之前。

**§6.2 的 `curl` 命令仍然保留**，但定位从「公网唯一出路」降为「脚本 / 无 CLI 环境下的等价用法」。

### 5.5 代理侧接入指令（必须落地，否则功能等于不存在）

与主设计 §5.3 的两条记忆规则同源，在 `server.rs` 的 `get_info().with_instructions(...)` 与 README 中追加两条交接规则。**措辞刻意不出现任何 ID**——指令里一旦出现 `task_id`，模型就会倾向于向用户索要它，零 ID 设计随即失效：

```text
HANDOFF RESUME: at the start of a session, call handoff_list (unacked only) to see
what is waiting. Nothing pending: say nothing about it. One pending: mention it in
one line and take it over once the user agrees. Several pending: show the summaries
and let the user choose — or pick silently when the user's request already makes it
obvious. If the user only wants to know what is pending, list and stop. Only
handoff_ack marks work as taken, and that removes the entry from every other client's
pending list: never ack before the user has agreed to take that work. Never ask the
user for an id.
HANDOFF SAVE: when a task reaches a meaningful checkpoint, or the user says they
will continue from another client or machine, call handoff_save with
goal/state/next_steps and pass client set to your own name (e.g. "zed",
"workbuddy") — it keeps your pending handoff separate from other clients'.
Omit task_id to continue the workstream you most recently saved — pass back the
task_id returned by handoff_load / handoff_save when you are continuing that same
workstream. If the work item you are handing off is NOT the one you loaded, give
it a short task_id of your own; never ask the user to invent one.
```

**这段文本必须自足**——它是接手方**零安装**路径上唯一会到达模型的交接知识（连接即得，见 §5.6、§5.8）。因此「判方向所需的最小规则」全部在这里，包括「只想看看 = 只读」和「换机器 = 保存」两条分支；skill 只放提高质量的长尾指导，**删掉 skill 功能不受影响**。

五处措辞都是刻意的：

- `never ask the user for an id` 阻止模型反问你；
- `Nothing pending: say nothing about it` 避免每次开新会话都报告一次「当前没有交接」——那是噪音，不是信息；
- `One pending: mention it in one line and take it over once the user agrees` 让唯一候选时**不追问细节**（在唯一选项里提问是纯摩擦），但也不抢跑——**这句话是「读/改分离」在指令层的落实**（§3.7）；
- `If the user only wants to know what is pending, list and stop` 把「看」与「接」分开：读没有副作用，所以可以随时答应；
- `show the summaries and let the user choose` / `or pick silently when the user's request already makes it obvious` 把**真正需要人判断的那一步交回给人**，同时避免走向另一个极端。**消歧的默认顺序是「先看能否从用户话里判断，判断不了再问人」，而不是「一律问人」或「一律自己猜」。**

`instructions` 的体量成本是一次性的：它在 MCP `initialize` 时下发（[server.rs:219](src/mcp/server.rs)），进入客户端会话上下文，**不是每次工具调用都付**。上面的记忆两条规则已占用约 5 行，加上交接 8 行仍在同一个量级内。

### 5.6 流程的载体：自足的 `instructions` + 可选的质量层

**先纠正一个我自己的框架错误**：上一轮我把「一条命令按会话新旧自动判方向」当成 CLI 的问题来讨论。它不是——**这个推断 CLI 永远做不了，而会话内的代理能做**。原因很简单：

| 判方向需要的信息 | CLI 看得到吗 | 会话内的代理看得到吗 |
|---|---|---|
| 这个会话已经有多少进展 | ❌ 无状态进程，每次都是新的 | ✅ 它就在会话里 |
| 用户此刻想交出去还是接过来 | ❌ 只能靠参数 | ✅ 用户刚说的话就是答案 |

CLI 只能靠「本地 marker 文件」之类的旁证，而那是**另一个谓词**（「这个目录以前交接过」≠「这个会话有进展」），两个方向都会判错。所以：**命令面保持 5 个显式子命令（脚本要确定性），方向推断交给代理。**

**顺带确认：你原本的规则本身是安全的。** 「新会话 → 列出预配置条数 → 选一条接手」——列出来让你选，ack 发生在你选定之后，**不存在自动 ack**。我上一轮担心的「猜错就静默把别人的待接手标记掉」在你这版规则里并不存在。

#### 5.6.1 内容该放哪一层：按「通道是否存在」划分

**先修正一个论证错误**：我一度把「skill 必须落文件才能被发现」当成门槛论据，由此推出「关键规则不能只放在 skill 里」。**这个理由不成立**——往客户端已经在扫描的目录里丢一个文本文件，没有二进制、没有常驻进程、没有服务端改动，**那不是安装客户端**。它与「装 CLI 二进制」同属可选增强，谁都不构成入门门槛。

**结论仍然成立，但真正的理由是通道可用性，与安装成本无关**：

1. **很多 MCP 客户端根本没有 skills 这个概念**。它们只认 `tools` + `instructions`。对这些客户端，`instructions` 是唯一通道——不是「更省钱」，而是**只有这一条**。
2. **即使客户端支持 skills，它也是按需加载**：加载与否由模型依据 `description` 自行判断。而「会话开始时要判方向」这件事，不能押在一个**可能不被加载**的通道上。

所以 `instructions` 必须自足，`skill` 只能是加法：

| 载体 | 到达方式 | 放什么 | 缺了会怎样 |
|---|---|---|---|
| MCP `instructions` | **连接即得**（`initialize` 下发，[server.rs:219](src/mcp/server.rs)） | 判方向所需的**最小完整规则**（含「只想看看 = 只读」「换机器 = 保存」两条分支） | ❌ 功能不可用——模型不知道该调什么、什么时候调 |
| `skills/memora-handoff/SKILL.md` | 丢一个文本文件到客户端 skills 目录 | **质量指导**（怎么写才让人认得出、多候选怎么呈现、什么算检查点） | ✅ 功能照常，只是写得糙 |

**因此我上一版「skill 承载完整决策表、instructions 只放三条不变量」的划分仍然是错的**——它让不支持 skills 的客户端缺了判方向的分支规则。现在的划分是：**instructions 自足，skill 只做锦上添花**。删掉 `skills/` 目录，交接功能必须完全可用——这是一条可验收的约束（§11 回归 #7）。

这也和本项目「同一个事实只说一次」的原则不冲突：**同一事实仍然只说一次**，只是基础事实的落点从 skill 移回了 instructions；skill 里放的是另一类内容（质量指导），不是基础规则的副本。

**三个层次，逐个可选**（都写进 §5.8 清单）：

| 层 | 要装什么 | 适合谁 | 不装的影响 |
|---|---|---|---|
| **基线** | 只填 MCP 地址 + token | 所有人 | — |
| 增强 · skill | 丢一个文本文件 | 支持 Agent Skills 的客户端；想让列表更好读、少点废话 | 无（功能照常） |
| 增强 · CLI | 一个二进制 | 偏好命令行 / 脚本 / 自动化的人；或代理不按指令行事时 | 无（代理路径不受影响） |

#### 5.6.2 技能：可选的质量层

**SKILL.md 草案**（frontmatter 用 Agent Skills 标准，Claude Code 与 DSH 按同一形状发现）：

```markdown
---
name: memora-handoff
description: Use when writing or reviewing a handoff at a work checkpoint, or when
  presenting several pending handoffs for the user to choose from, to make the
  entries recognizable and the choice easy.
---

- **Make the entry recognizable.** The next person reads a one-line list, not your
  full context. Write `goal` as what the work IS ("把 handoff 并入 memora"), not
  what you did ("改了 repo.rs")。Put the blocking detail in the first line of
  `state` — that is the excerpt shown in the list.
- **`next_steps` must be executable by someone with no memory of this session**:
  name files by path, name the command to run, name the check that proves it worked.
- **Prefer a few concrete steps over a summary.** Three actionable steps beat ten
  vague ones; the reader wants to start, not to review.
- **Pass a stable `session_id`** (e.g. `zed-20261014-2130`) so the entry is
  distinguishable from your other sessions in someone else's list.
- **Presenting candidates**: show a numbered table with goal, the state excerpt,
  client, session label and time; ask the user to pick a number. Never make the user
  read a task_id out loud.
- **A meaningful checkpoint** is: a sub-task finished, a decision made that changes
  direction, or you are about to run something long. Not after every tool call.
```

**SKILL.md 跨客户端可移植**：它遵循 Agent Skills 标准（`name` + `description` + 正文），`~/.claude/skills/`、DSH 的 skill 目录都按同一形状发现；`description` 以 `Use when` 起头才能在对的时机被加载（DSH 内置技能即此约定）。仓库只需在 `skills/memora-handoff/` 放一份，README 说明「可选安装」。

**测试的诚实边界**：技能与 instructions 的行为都**无法被服务端单测覆盖**——它们是给模型看的文本。能测的只有机械属性（见 §11），真正的验收是 §5.7 的人工走查与 §5.8 的零安装清单。

### 5.7 端到端示例：A 电脑 Zed（会话 A1）→ B 电脑 WorkBuddy

对齐设想的完整走法，注意**人这一侧从头到尾没有任何标识符**：

```text
[A 电脑 · Zed · 会话 A1]
用户：「先做到这，我换台机器接着弄」
模型：handoff_save{goal, state, next_steps, files, decisions, open_questions,
                   client:"zed", session_id:"A1"}                    ← 不给 task_id
     → task_id="current@zed", revision=1, acked_at=NULL

[B 电脑 · WorkBuddy · 新会话]
用户：「接着上次的弄」
模型（依指令）：handoff_list{unacked_only:true, limit:5}              ← 相当于 latest
     → {handoffs:[
          {task_id:"current@zed", goal:"把 handoff 并入 memora",
           state_excerpt:"设计文档已完成…", client:"zed",
           session_id:"A1", created_at:…, acked:false},
          ... 最多 5 条
        ]}
     ┌─ 恰好 1 条  → 一句话告知「有一条待接手：把 handoff 并入 memora（zed/A1），
     │                要接着弄吗」；用户说「接着弄」→ handoff_load{task_id:"current@zed"}
     │                + handoff_ack{client:"workbuddy"}
     └─ 多于 1 条  → 模型把这张表念给用户，用户选「A1 那个」
                    → handoff_load{task_id:"current@zed"}（或候选里选中的那个）
                    → handoff_ack{task_id:"current@zed", client:"workbuddy"}
     → {status:"found", won:true, handoff:{acked_by:"workbuddy"}}
模型：按 next_steps 继续；对其中长期有效的结论调用 create_entities
```

若模型没有按指令调用工具（真实世界必然发生），人在 B 电脑上敲：

```bash
memora handoff list            # 看到 A1 那条（goal / 状态 / client / session_id / 时间）
memora handoff load --task current@zed
memora handoff ack  --task current@zed
```

——`current@zed` 是**从列表里抄下来的**，不是从记忆里回忆出来的。这正是「A1 是展示列、不是地址」的兑现（§3.7）。

全程 B 电脑上事先要准备的只有 Memora 的 MCP 配置（url + token），而那是使用记忆功能本来就需要的。

---

### 5.8 接手方零安装清单（「降低门槛」的验收标准）

**新电脑上要做的全部事情，两行**：

1. 在任意支持 MCP 的客户端里加一个 MCP server：URL `https://mem.example.com/mcp`，Header `Authorization: Bearer <项目 token>`；
2. 完成。

**基线不需要**：memora 二进制、CLI、skill 文件、数据库客户端、SSH 登服务器、改 Caddy 配置。

| 项 | 接手方（新电脑） | 服务端（一次性） |
|---|---|---|
| MCP 端点 | 填 `https://<域名>/mcp` | `/mcp` 已随服务启动，**无需新增开关**（[routes.rs:47](src/routes.rs)） |
| 凭据 | 填项目 token（创建项目时给的那串） | 只存 SHA-256，服务端也取不回明文 |
| HTTPS | 无（客户端自己处理） | Caddy 自动签发证书（[Caddyfile](deploy/Caddyfile)） |
| Host 白名单 | **无** | ⚠ **`.env` 的 `MCP_ALLOWED_HOSTS` 必须含对外域名** |
| 交接知识 | **无**（连接即得） | 交接规则随 `initialize` 的 `instructions` 下发（§5.5） |

**两个可选增强**，都不属于「安装客户端」，装了只是更好用：

| 增强 | 装法 | 得到什么 | 不装的代价 |
|---|---|---|---|
| skill | 往客户端 skills 目录放一个文本文件 | 列表更可辨认、候选呈现更清楚、少点废话 | 无（功能照常，写作质量靠模型的自觉） |
| CLI | 放一个二进制（可选直连 HTTPS，§5.4） | 命令行/脚本/自动化的确定性入口；代理不按指令时的兜底 | 无（代理路径完全不受影响） |

**那条 ⚠ 是零安装路径上唯一的隐藏依赖**：rmcp 的 Streamable HTTP 传输默认只信回环 Host（防 DNS 重绑定），Caddy 会透传真实域名，白名单未命中时请求被**协议层 403 拒绝，而错误信息不指向配置项**。Caddyfile 与 `env.template` 都已就此警告过（[env.template:29](deploy/env.template)），本设计把它提到清单里，因为它决定「接手方零安装」这个承诺成不成立。

**CLI 的定位不是「没有 AI 客户端时的兜底」**，而是**与 skill 并列的可选第一路径**：偏好命令行的人、脚本与自动化、以及代理恰好没按指令行事时，它都是正路。§5.4 的 HTTPS 直连因此不只是保险——它是让「在任意一台机器上装一个二进制就能接手」这个选项成立的前提，而那 1.5–2.5MB 花的正是这种场景（这种机器最不可能有一个配好 CA 的 `curl`）。

---

## 6. 鉴权与部署影响

### 6.1 CLI 为什么必须用 `ADMIN_TOKEN`，而不是项目 token

这是一个硬约束，不是偏好：**明文项目 token 只在创建项目时出现一次，服务端只存 SHA-256（主设计 §5.2），因此 CLI 无法「就地查出」项目 token**。运行 `memora handoff` 时手边唯一可用的凭据是 `.env` 里的 `ADMIN_TOKEN`。

由此推出两条设计结论：

1. CLI 走 **REST 管理面**（`/api/v1/...`，`ADMIN_TOKEN`），而不是走 MCP 的 `tools/call`——尽管后者能省掉 7 个路由。省下端点的代价是「CLI 必须持有项目 token」，而它拿不到。
2. REST 路径**显式携带 `{project_id}`**，因为 `ADMIN_TOKEN` 不与任何项目绑定。这是 CLI 上唯一还需要人指定一次的东西，故在 §5.3 给了两级缺省（`MEMORA_PROJECT` → 服务端唯一项目）。

**安全影响**：`ADMIN_TOKEN` 因此从「管理面凭据」升格为「管理面 + 交接读写凭据」。这不改变它的信任等级（管理面本就能删项目、读统计），但要在 README 中如实写明，并保持「项目 token 不能当管理凭据用」的既有隔离不变。

### 6.2 HTTPS（**已定：CLI 直连**）

**决策**：CLI 支持 `https://`，引入阻塞式 TLS HTTP 客户端 `ureq`（见 §5.4）。

| 方案 | 支持范围 | 代价 | 结论 |
|---|---|---|---|
| A. CLI 仅 `http://` | 本机、内网、SSH 隧道 | 跨公网必须绕道 `curl`；且依赖运行环境恰好有可用的 curl/CA | ❌ 否决 |
| **B. CLI 支持 `https://`** | 直连公网 | 引入 TLS 客户端与根证书，二进制 +1.5–2.5MB | ✅ **采用** |

**为什么这笔代价是值得的**：单二进制交付的承诺，本身就包含了「不假设目标机器上有什么」。要求用户先确认 `curl` 支持 HTTPS、系统 CA 配置正确、再拼一串 header，与「一个二进制、一条命令」的定位相冲。把 TLS 打进二进制是一次性成本，而每次跨设备接手都要依赖环境则是持续成本。

**`curl` 命令保留**，定位降为「脚本 / 无 CLI 环境下的等价用法」（与 CLI 走同一 REST 契约，语义不会漂移）。接手侧同样是零 ID：

```bash
# 看有没有待接手的交接（204 = 没有；300 = 多条，响应体给候选摘要）
curl -s -i https://mem.example.com/api/v1/projects/memora/handoffs/pending \
  -H "Authorization: Bearer $ADMIN_TOKEN"

# 接手
curl -s -X POST https://mem.example.com/api/v1/projects/memora/handoffs/pending/ack \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H "Content-Type: application/json" \
  -d '{"client":"laptop-b"}'

# 存一次交接
curl -s -X POST https://mem.example.com/api/v1/projects/memora/handoffs \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H "Content-Type: application/json" \
  -d @handoff.json
```

### 6.3 部署影响

- **Caddy 无需改动**：`/api/v1/*` 与 `/mcp*` 都已在放行前缀内（[Caddyfile](deploy/Caddyfile)），交接端点与 MCP 通道自动可达。
- **不需要「开启 MCP 服务」这种开关**：`/mcp` 目前随服务无条件挂载（[routes.rs:47](src/routes.rs)）。若将来给 MCP 加开关，交接工具会随之一起消失——**这一点要在开关的文档里写清**，否则会出现「配了地址却没有任何交接工具」的迷惑现象。
- **`memora init` 模板无需改动**；唯一前置是 `MCP_ALLOWED_HOSTS` 含对外域名（§5.8 的 ⚠），而模板与 Caddyfile 已就此警告。
- 新增统计字段 `handoffs` 属于响应扩展，向后兼容。

---

## 7. 生命周期与并发

### 7.1 追加式不可变修订

每次 `save` 追加一条新修订，**从不就地覆盖**。这不是为了「版本控制」（用户已确认不需要），而是为了三件具体的事：

1. **写入原子性**：一条修订是完整写完的，不存在「读到一个改了一半的交接」。
2. **并发安全**：两个客户端同时 save 得到 revision 1 与 2，无覆盖争议；后写者胜出是确定的（revision 大者）。
3. **近零成本的审计**：出问题时可回看上一条交接写了什么——这几乎不花代码，却能在「接手方看错了状态」时快速定位。

代价是存储增长。交接单条 ≤ 64 KiB、save 频率以「人切换客户端」计（一天几次），增长可忽略。

### 7.2 新修订重置接手状态

修订 N+1 的 `acked_at` 恒为 `NULL`。理由：**交接内容变了，上一次的接手就不算数**。否则会出现「A 接手了修订 1，B 更新了修订 2，A 永远不知道自己手上的信息已过期」。

这条规则同时是零 ID 解析（§3.7 R1）能保持简单的前提：**待接手候选只看每个任务的最新修订**，旧修订即使 `acked_at IS NULL` 也不再作为候选（它已被新修订取代）。于是默认槽位 `current` 无论追加多少修订，任何时候都只有 0 或 1 条待接手。

### 7.3 ack 竞态

见 §4.4。要点：ack 是 CAS，`won` 回传事实，`acked_by` 保持首次值不被覆盖（幂等）。

### 7.4 为什么不做过期删除（TTL）

- 静默过期比「过期但可见」危险得多：接手方以为「没有交接」，而实际上有，只是被删了。
- 模型无法可靠推理 TTL，能推理的是「`acked_at` 有值 → 已被接手」。
- 因此：**ack 是可见状态，不是删除动作**。清理交给显式的 `handoff rm` / `DELETE`。
- 陈旧度以 `created_at` 如实呈现，由接手方自行判断；如将来确需提示，可加「超过 N 天未接手」的告警字段，而不是自动删除（列入 §12 后续扩展）。

**不做 TTL 的直接收益**：`list_handoffs` 的结果是确定性的，契约测试不需要时间旅行（不依赖 fake clock），实现与测试都显著变简单。

---

## 8. 改动点清单

| 文件 | 改动 | 备注 |
|---|---|---|
| `src/domain.rs` | 新增 `HandoffInput`（`task_id: Option<String>`）/ `Handoff` / `HandoffSummary` / `HandoffResolution` / `AckOutcome`；`default_task_id(client)` 分片槽位函数 | 与记忆领域类型并列，不混用 |
| `Cargo.toml` | 新增 `ureq`（blocking + rustls，见 §5.4） | 为 HTTPS 直连；预计二进制 +1.5–2.5MB |
| `src/storage/repo.rs` | 新增 `HandoffRepository` trait（**`resolve_handoff` 为唯一寻址入口**） | 后端无关 |
| `src/storage/mod.rs` | `ProjectRepos` 增加 `handoff: Arc<dyn HandoffRepository>` | **编译器会强制提示所有构造点**：`sqlite_file` / `mem` / 测试 |
| `src/storage/normalize.rs` | 新增 `task_id` 校验（**不复用** `project_id` 的字符集）+ 交接字段与容量校验 | 见 §3.2 差异说明 |
| `src/entity.rs` | 新增 `memory::handoffs` 实体；P6 落地后同步 `single::handoffs` | sea-orm 仅作后端内部 SQL 层 |
| `src/storage/sqlite_file.rs` | DDL 数组追加 `handoffs` 表与索引；实现 `HandoffRepository`（含默认槽位、R1 解析、ack 的 CAS） | 无迁移函数 |
| `src/storage/mem.rs` | 内存实现 | 契约测试参照实现 |
| `src/storage/contract.rs` | 新增 `run_handoff_contract`（H1–H18，含 R1、**H8 分片不遮蔽**、**H10/H11 摘要与 limit**）；`run_backend_contract` 追加跨项目隔离断言 | 两个后端各跑一遍 |
| `src/mcp/dto.rs` | 4 个工具的入参 / 出参契约（serde + JsonSchema）；`task_id` 为 `Option`；`unacked_only` 缺省 true、`limit` 缺省 10；出参摘要含 `state_excerpt` / `session_id` | 入参别名规则同样适用 |
| `src/mcp/tools.rs` | 4 个工具的语义实现；拆分 `MEMORY_TOOL_NAMES` / `HANDOFF_TOOL_NAMES` / `TOOL_NAMES` | 只依赖 trait |
| `src/mcp/server.rs` | 注册 4 个工具；`instructions` 追加两条交接规则（**措辞零 ID**） | 唯一依赖 rmcp 之处 |
| `src/mcp/mod.rs` | `assert_tool_manifest` 逻辑不变（数组变长）；模块与 `build_service` 的注释由「9 个工具」改为「9 记忆 + 4 交接」 | 注释是与代码同寿命的承诺表述 |
| `src/admin.rs` | 7 个 REST handler（含 `pending` 与 `pending/ack`）；列表端点解析 `unacked`（缺省 1）/ `limit`（缺省 10）；`stats` 增加 `handoffs` 计数 | 只经 `StorageRegistry` 取 trait |
| `src/routes.rs` | 挂载 `/api/v1/projects/{id}/handoffs*` 路由组 | 注意 `pending` 需先于 `{task_id}` 注册（否则被当作 task 名匹配） |
| `src/cli.rs` | 两级命令模型 `Command::Handoff { action }` + 10 个新 flag（含 `--limit` / `--all` / `--server` / `--timeout`）；`--project` / `--task` 缺省解析 | 见 §5.3 |
| `src/http.rs`（新） | **`ureq` 封装的通用客户端**：支持 `http`/`https`、自定义 host、POST JSON、超时、可选路径前缀 | `daemon` 继续用自己的手写本机探测，不被替换（§5.4） |
| `src/handoff_cli.rs`（新） | CLI 执行：组装 JSON → HTTP → 渲染；`--json` 输出；项目缺省解析 | **不依赖 storage / routes** |
| `src/main.rs` | 分发 `Command::Handoff` | — |
| `README.md` | 工具表（13 个）、CLI 用法、路线图、代理指令、**技能安装说明**（§5.6） | — |
| `skills/memora-handoff/SKILL.md`（新） | 质量增强层：让条目可辨认、候选呈现方式、检查点定义（§5.6.2）；**不装也能用** | 可选安装；纯文本，无编译期依赖 |
| `docs/memory-system-design.md` | §5.4 标注会话工具顺延为 P5.1；其余不动 | — |

> **路由注册顺序是这里唯一一个静默失败点**：Axum 的 `/handoffs/pending` 与 `/handoffs/{task_id}` 会被同一路径模式匹配，若 `{task_id}` 先注册，`pending` 会被解析成任务名并返回 404，而错误信息完全不指向注册顺序。必须用一条路由测试把「`GET /handoffs/pending` 命中零 ID 入口」钉死。

> 当前工作区有一份未提交的 P6 改动（`src/entity.rs` 的 `single` 模块）。本篇设计与它不冲突，但**不要在同一提交里混合**——交接落地时若 `sqlite_single` 已存在，须一并补 `single::handoffs` 实体与其实现。

---

## 9. 与现有期的关系

### 9.1 与 P5 会话（transcript）的分工

主设计 §5.4 定义了 `conversations` 表与 `log_turn` / `get_session` / `list_sessions`。两者关系：

| | 交接（本篇） | 会话（P5.1） |
|---|---|---|
| 内容 | 蒸馏后的状态 | 原始对话 |
| 体量 | 几 KB | 可能 MB 级 |
| 解决 | 「接着干什么」 | 「当时说了什么」 |
| 依赖 | 无 | 无 |

**建议顺序：先交接，后会话。** 交接更小、价值更直接、测试更简单；而且交接不依赖 transcript，反过来 transcript 还能借 `handoff.session_id` 指回具体会话。因此把本篇作为 **P4.3** 落地，主设计 §5.4 的会话工具顺延为 **P5.1**，其工具名（`log_turn` 等）与语义不变。

### 9.2 与 P6 多后端的协同

`handoffs` 是一张**新增表**，因此：

- 若 P6 (`sqlite_single`) 先落地：交接落地时必须同步补 `single::handoffs` 与 `sqlite_single` 的实现，否则「换后端」承诺出现缺口（`sqlite_single` 项目上交接功能 500）。
- 若交接先落地（本文建议）：P6 落地时把 `handoffs` 一并纳入 `sqlite_single` 的建表与租户过滤，`run_handoff_contract` 自动覆盖两者的一致性。

无论顺序如何，**`run_handoff_contract` 必须被每个后端各调用一次**——这正是主设计 §12.5「业务层零改动」承诺在交接功能上的兑现方式。

---

## 10. 关键取舍（含被否决方案）

| 决策 | 取舍与理由 |
|---|---|
| **交接独立建表，不混入记忆图谱** | 高频过程性写入会污染 FTS5，使记忆召回质量持续劣化；且图谱的幂等合并语义与交接的追加修订语义相反（§2、§3.4） |
| **不移植 git** | 已确认 git 只是顺手当文件存储；移植它等于连同 §1.1 的五条问题一起接受。版本需求由追加修订低成本满足（§1.3、§7.1） |
| **追加式修订，而非就地覆盖** | 换取写入原子性、无覆盖争议与零成本审计；代价是可忽略的存储增长（§7.1） |
| **不做 TTL 自动过期** | 静默删除比「可见但陈旧」危险；也让契约测试不必依赖假时钟（§7.4） |
| **ack 是状态而非删除** | 保留「谁接手了」这一事实，使并发接手可被察觉（`won=false`）；删除交给显式操作（§4.4、§7.3） |
| **默认路径零 ID**（`task_id` 可省、`load`/`ack` 可无参） | 接力的真实场景是「我在另一台机器上接着弄」，此时人脑里没有标识符；要求输入 ID 等于让功能不可用。代价是多一条 R1 解析规则与歧义分流（§3.7） |
| **交接挂工作项，不挂会话** | 接手方开的是新会话，把交接绑到 `session_id` 会直接自相矛盾（双方永远不同）；`session_id` 只作可选溯源（§3.7） |
| **多工作项靠「读短列表」消解，不靠背 ID** | 与 `claude --resume`、「最近项目」同构；ID 只在服务端与代理之间流转一次，人只需说「接着弄」（§3.7） |
| **接手主流程是「列表优先」，不是「零参自动」** | 列表是**可核对的**（看到 goal / 状态 / 谁写的 / 会话 / 时间），零参自动是加速器。默认顺序不能反过来，否则多条候选时会静默接错（§3.7、§5.1） |
| **`handoff_list` 的 `unacked_only` 缺省为 true** | 该工具的主场景是「选一条接手」，已接手的不该混进来；考古走显式 `--all`。反向 flag（`--unacked`）与默认值重合就不提供（§5.1、§5.3） |
| **`session_id` 进摘要作展示列，但绝不参与寻址** | 人记得的是会话标签（「A1」），要求他输入就失败了；放进列表让他「看一眼点一下」即可。同时明确它**不是键**——接手方开的是新会话，拿它寻址会自相矛盾（§3.7、§4.2） |
| **不新增 `latest` 子命令** | `list` 的缺省行为（待接手 + 倒序 + 上限 10）已等价于「latest」；一个 flag 组合不值得占一个动词位（§5.3） |
| **方向推断（该交还是该接）放进 skill，不放进 CLI** | 判方向要的是「会话有多少进展 + 用户此刻想干什么」，CLI 是无状态进程，两样都看不到；靠 marker 文件旁证是另一个谓词，两个方向都会判错。skill 就在会话里，天然知道（§5.6） |
| **命令面保持显式，不追求「一条命令自动判方向」** | 命令面减少的应该是「要记的东西」，不该承担猜测；而猜测的收益（少打两个词）远小于猜错的代价（错误 ack 会从所有其他客户端的待接手列表里移除该条）。**人说的话本身就是最好的信号源**——交给模型理解，比交给 CLI 推断可靠得多（§5.6、§7.4） |
| **`instructions` 自足，skill 只做质量层** | 判方向所需的规则全部放进连接即得的 `instructions`，理由是**通道可用性**：不少 MCP 客户端没有 skills 概念（只有 tools + instructions），而支持 skills 的客户端里它也是按需加载——不能把「会话开始就要判方向」押在可能不存在的通道上。**注意这不是安装成本论证**：丢一个文本文件不算安装客户端（§5.6.1）。skill 只放「装了更好用」的质量指导，**删掉 `skills/` 目录交接功能必须完全可用**（可验收，§11 回归 #7） |
| **无 ID 的 save 落到按写入方分片的槽位 `current@<client>`** | 固定单一 `current` 会让第二个写入方**静默遮蔽**第一条交接（后者成为新修订、旧修订不再是待接手候选）。分片是修掉遮蔽的最小改动（§3.7、§4.3 H8） |
| **同一 `client` 并行两条工作流必须给 `task_id`** | 信息论上的必然：「这是两件不同的事」只存在于调用方。代价由代理承担（它自己生成并回传 ID），不转嫁给人（§3.7） |
| **无 ID 解析歧义时返回候选，绝不替调用方猜时间最新的一条** | 「取最新」在并行工作流下会把两个客户端的活接串；让调用方看摘要挑选，对代理而言不需要人参与（§3.7 R1、§4.3 H13） |
| **`load` / `ack` 共用 `resolve_handoff`，不各写一份寻址** | 两处实现一旦有差异，会出现「`load` 拿到 A、`ack` 标记了 B」——极难复现却会让接手状态彻底错乱（§4.1） |
| **`load` 不存在时不算错误** | 「首次接手时没有交接」是正常状态。与 `open_nodes` 跳过未知名字同源；MCP 返回 `status="none"`，REST 返回 `204`（`404` 留给「指定 ID 不存在」） |
| **JSON 数组列，而非子表** | 交接永远整条读取，子表 join 是为永不发生的查询付费（§3.3） |
| **`task_id` 字符集含 `.` 与 `@`，`project_id` 不含** | 前者不参与路径拼接（无穿越风险）：`.` 便于 `v1.2-xxx` 命名，`@` 作分片槽位分隔符；后者参与路径，必须更严。两者**不复用校验函数**（§3.2） |
| **工具用 `handoff_*` 前缀** | 工具面扩为两个家族后，前缀便于分组识别；既有 9 个工具一个不改名（§5.1） |
| **CLI 走 REST + `ADMIN_TOKEN`，而非 MCP** | 明文项目 token 不可反查，CLI 拿不到它；`ADMIN_TOKEN` 是 CLI 唯一合法凭据。代价是 7 个新路由（§6.1） |
| **CLI 只收 JSON 输入** | 缩小选项面与解析器复杂度；主要写入者是代理（走 MCP），CLI 是兜底（§5.3） |
| **CLI 的 `--project` / `--task` 也尽量可省** | 跨设备时人只该敲 `memora handoff load`；项目按 `MEMORA_PROJECT` → 服务端唯一项目解析，任务按 R1 解析。代价是 CLI 多一次 `GET /projects` 探测（§5.3） |
| **CLI 不直连存储，只经 HTTP** | 与 `daemon` 同级解耦，使 CLI 对后端与文件布局完全盲视；换后端时零改动（§5.3） |
| **引入 `ureq` 支持 HTTPS 直连** | 「单二进制」的承诺包含「不假设目标机器上有什么」；依赖环境里的 `curl` + CA 是持续成本，打进二进制是一次性成本。代价 +1.5–2.5MB（§5.4、§6.2） |
| **选 `ureq` 而非 `reqwest`** | CLI 是同步的，`ureq` 阻塞式、依赖面小；`reqwest` 会拉入 async 栈（tokio + hyper）而此处用不到（§5.4） |
| **`daemon` 的手写本机探测不替换** | 它只探 `127.0.0.1`，TLS 无用；改动它有回归风险而无收益。两套客户端职责不同，重复是有界的（§5.4） |
| **`assert_tool_manifest` 逻辑不变，只换数组** | 启动自检这一安全网继续生效，无需重写（§5.1） |

---

## 11. 测试计划（TDD）

沿用主设计「先写测试、观察失败、再实现」的方式，按层推进：

| 层 | 用例 | 数量级 |
|---|---|---|
| 契约（`run_handoff_contract`） | H1–H18（含 R1 四条：唯一 / 已接手 / 歧义 / ack 歧义不猜；含 **H8 分片不遮蔽**、**H10 摘要字段**、**H11 limit 截断在排序之后**），`sqlite_file` + `in_mem` 各跑一遍 | 36（18 × 2） |
| 后端契约（`run_backend_contract` 扩展） | 跨项目交接不可见 | 1× 每后端 |
| 规范化（`normalize.rs`） | `task_id` 合法 / 遍历 / 空白 / 超长 / 含 `@`；缺省分片槽位拼接；各字段容量上限；整条上限；空数组合法；非法输入整批拒绝 | 13+ |
| MCP 语义（`tools.rs`，内存后端，脱离协议栈） | **`handoff_list` 缺省只列待接手、倒序、受 `limit` 约束**；摘要含 `state_excerpt` / `session_id`；**`handoff_load{}` 零参数取到待接手**；`handoff_ack{}` 零参数接手；**两个 client 各存一条 → 歧义候选含两者**；save 省略 `task_id` 落 `current@<client>`；错误码分流 | 16+ |
| MCP 协议（`routes.rs` 端到端） | `tools/list` 含 13 个工具；`tools/call` 走通「**list 看到候选 → 选中 → load → ack**」与「零参数 save → 零参数 load → 零参数 ack」两条链路；非法入参 `-32602` | 9+ |
| MCP 清单不变量（`server.rs`） | 13 个工具与 `TOOL_NAMES` 一致；9 个记忆工具未被改名；两个家族无重名 | 3 |
| REST 管理面（`routes.rs`） | 7 个路由的状态码（201 / 200 / 204 / 300 / 404 / 400 / 401）；**`GET /handoffs/pending` 命中零 ID 入口而非被 `{task_id}` 吃掉**；列表端点的 `unacked` 缺省为 1、`limit` 缺省 10；`stats` 含 `handoffs`；跨项目不可见 | 14+ |
| CLI 解析（`cli.rs`） | `handoff save/load/list/ack/rm` 解析；缺 action 报候选；`--task` 可省（仅 `rm` 必填）；`--limit` / `--all` 缺省值；`--project` 可省的两级解析；`--server` 的 http/https 与路径前缀解析；既有子命令行为不变（回归） | 16+ |
| CLI 执行（`handoff_cli.rs`） | **`memora handoff list`（无参数）在唯一项目下列出待接手并带序号**；**`memora handoff load`（无 `--project` 无 `--task`）走通**；`--json` 渲染；无待接手时退出码 0；歧义时非 0 并打印候选；连接失败错误信息含 server 地址 | 9+ |
| HTTP 客户端（`http.rs`） | `http` 与 `https` 两类 URL 解析（含端口与路径前缀）；POST JSON 的 body / Content-Type / Authorization；超时；非 2xx 处理 | 7+ |
| 流程载体（`server.rs` + `skills/`） | **`instructions` 自足性**：字符串同时包含「会话开始先看待接手」「检查点/换机器保存」「只想看看则只读」「**绝不索要 ID**」四类措辞；`SKILL.md` frontmatter 可解析、`name`/`description` 非空且以 `Use when` 起头 | 5+ |

**技能行为无法被服务端单测覆盖**——`SKILL.md` 是给模型看的文本，它是否被正确遵守只能在真实会话里观察。上表能钉的只有「文件存在、格式正确、instructions 里没有反向措辞」这三件事。**这条边界必须如实标注**，否则测试计划会给人「流程已被覆盖」的错觉；真正的验收是 §5.7 的人工端到端走查。

**必须包含的七条回归**：

1. **既有 9 个工具名与语义零变化**（对官方 MCP Memory 的兼容承诺）。
2. **既有 CLI 子命令与退出码零变化**（`run`/`start`/`stop`/`restart`/`status`/`init`）。
3. **`search_nodes` 不受交接影响**：写入多条交接后，记忆检索结果与写入前一致（钉死 §2 的分表决策）。
4. **零 ID 主路径端到端可用**：一条用例从「`handoff_save` 不给 `task_id`」一路走到「另一客户端 `handoff_load{}` + `handoff_ack{}` 成功接手」，中途**不出现任何标识符**。这条用例是 §3.7 这个 UX 承诺的唯一硬保障——它一旦被后人改坏，功能会悄悄退回「要输 ID」的形态，而单测层面一切仍然通过。
5. **多写入方不互相遮蔽**：两个不同 `client` 各用零 ID save 一条，随后 `handoff_load{}` 必须返回 `ambiguous` 且候选含两者（对应 H8）。**这条是「多个会话同时存在」这一场景的端到端保障**——它是上一版设计里真实存在的缺陷，必须由用例钉住而不是靠约定。
6. **列表可用性端到端**：A 客户端存一条（带 `session_id:"A1"`）→ B 客户端 `handoff_list` 必须**认得出它**（摘要含 `session_id` 与 `state_excerpt`）→ 按列表里的 `task_id` 完成 `load` + `ack`。**这条钉死「人在列表里认、不靠记忆」这一交互**；若某次重构把 `session_id` 从摘要里删掉，功能仍能跑通而体验已破坏，只有这条用例会失败。
7. **零安装自足性**：`initialize` 响应的 `instructions` 非空且包含判方向所需的四类措辞；**删除 `skills/` 目录后全部测试仍然通过**。这条钉死「skill 是可选增强、不是必需依赖」——否则某次重构可能把关键规则挪进 skill，而所有测试照样绿，零安装路径却已经失效。

---

## 12. 非目标

- **不移植 git**：无分支、无 diff、无提交、无远端仓库。
- **不做交接内容的关键词检索**：检索是 `search_nodes` 的职责；长期有效的结论应沉淀为记忆（§2）。
- **不做自动过期 / 自动清理**（§7.4）。
- **不做跨项目搬运交接**：交接归属项目，跨项目转移属于「迁移」而非「接力」。
- **不做 Web 看板**：交接通过 MCP / CLI / curl 使用。
- **不做交接的向量检索**。
- **不做多节点集群下的交接一致性**（与主设计一致的既有非目标）。

**后续扩展（非本次）**：超过 N 天未接手的陈旧告警字段；`handoff_save` 与 `add_observations` 的联动提示（保存时若 `decisions` 非空则提醒沉淀）；交接的 TTL 只作为「展示用陈旧标记」而非删除。

---

## 13. 决策记录

**已定**（不再需要你确认）：

| # | 决策点 | 结论 |
|---|---|---|
| 1 | CLI 直连公网 **HTTPS** | ✅ **支持**。引入 `ureq`（blocking + rustls），`--server` 接受 `http`/`https`（§5.4、§6.2） |
| 2 | 多工作项怎么消解 | ✅ **读短列表选择**，不背 ID；无 ID 且多条时返回候选、不替调用方猜（§3.7 R1） |
| 3 | 无 ID 的 save 落到哪 | ✅ 按写入方分片的 `current@<client>`，修掉「第二个写入方静默遮蔽第一条」的缺陷（§3.7、H8） |
| 4 | 同一 `client` 并行两条工作流的处理 | ✅ 必须给 `task_id`；**由代理生成并回传**，不要求人输入（§3.7） |
| 5 | 接手主流程的形态 | ✅ **列表优先**（`handoff_list` → 选 → `load` + `ack`）；零参 `handoff_load{}` 作加速器（§3.7、§5.1） |
| 6 | 多候选时谁做选择 | ✅ **人**（代理把摘要摆出来让人选；用户话里已明确时可自行选）。真正需要判断的一步交回给人（§5.5） |
| 7 | 「A1」这类会话标签怎么用 | ✅ 进摘要作**展示列**（`session_id`），**不参与寻址**（§3.7、§4.2） |
| 8 | 「该交还是该接」的推断放在哪 | ✅ **放在会话内的代理**（它看得到会话进展与用户的话），CLI 保持 5 个显式子命令、不做方向推断（§5.6、§10） |
| 9 | 流程的载体 | ✅ 两层，按「**通道是否存在**」划分：`instructions` **自足**（连接即得，承载判方向所需规则）；`skills/memora-handoff/SKILL.md` **可选**（只做质量层，删掉功能不减）（§5.6.1、§5.6.2） |
| 10 | 「新会话」时的行为 | ✅ **列出预配置条数的待接手供选择**；确认你原本的规则本身不含自动 ack，因此不存在「猜错就静默标记」的风险（§5.6） |
| 11 | **接手方零安装** | ✅ **是**：新电脑只填 `https://<域名>/mcp` + 项目 token 两行。基线不装二进制、不改服务端；`/mcp` 无需开关（§5.8） |
| 12 | 零安装的服务端前置 | ✅ 只有一条：`.env` 的 `MCP_ALLOWED_HOSTS` 含对外域名。未命中时协议层 403 且**报错不指向配置项**，故写入 §5.8 清单（§5.8、§6.3） |
| 13 | skill 与 CLI 的性质 | ✅ 都**不是门槛**：往客户端目录丢一个文本文件、或放一个二进制，都不是「安装客户端」。二者平行可选、装了更好用；**skill 不承载必需规则**的理由是通道可用性（很多客户端没有 skills，且它按需加载），不是安装成本（§5.6.1、§5.8） |
| 14 | CLI 直连 HTTPS 的定位 | ✅ 支持 `ureq`（+1.5–2.5MB），定位为**与 skill 并列的可选第一路径**（命令行/脚本/自动化、代理不按指令时），而非「没有 AI 客户端时的兜底」（§5.4、§5.8） |

**仍需你确认**（都有建议值，不阻塞开工）：

| # | 决策点 | 本文建议 | 影响 |
|---|---|---|---|
| 15 | 交接修订的**保留策略** | 全量保留（单条 ≤ 64 KiB、低频写入） | 若担心增长，可只保留最近 N 条修订，但会削弱「回看上一条」的能力 |
| 16 | 工具命名前缀 | `handoff_save` / `handoff_load` / `handoff_list` / `handoff_ack` | 若偏好与官方一致的 `动词_名词`，改为 `save_handoff` 等；不影响其余设计 |
| 17 | 是否同时落地 P4.3（交接）与 P5.1（会话） | **只做交接**，会话顺延 | 同时做会让本期范围与测试面翻倍 |
| 18 | TLS 客户端选型 | `ureq`（阻塞、依赖面小） | 若更看重生态熟悉度可换 `reqwest` blocking；体积略大 |
| 19 | `client` 标识从哪来 | 代理显式传入（如 `zed` / `workbuddy`）；缺省落空串，此时分片槽位退化为 `current` | 不传 `client` 就回到「项目内单一槽位」，多写入方会撞车；instructions 与 skill 都会要求代理传 |
| 20 | **会话开始且恰好 1 条待接手时** | **一句话告知，等你点头再接手**；你说「接着弄」时才 ack（§5.6） | 若你觉得多余，可改成「1 条时直接接手」——代价是「只想看看」也会标记掉别人的待接手 |
| 21 | 列表里的 `state_excerpt` 截断长度 | 120 字符 | 纯展示；太长则一屏放不下，太短则两条相似 goal 难区分 |
| 22 | 是否加 `latest` 别名 | **不加**（`list` 缺省已等价） | 若你就是习惯敲 `latest`，加一行别名即可 |
| 23 | 可选增强（skill）怎么分发 | `skills/memora-handoff/SKILL.md` + README 写「可选安装」 | 它是增强不是门槛，所以即便不做也不阻塞；若要「装上就会」可另做客户端插件，但需按客户端各写一份 |

---

*本文档为交接功能的实现蓝图；确认上述决策后按 §11 的 TDD 顺序落地。*