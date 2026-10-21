# Memora v0.4.0

轻量自托管 AI 长期记忆服务（MCP + SQLite），单二进制。

这是一次**功能版本**：把 memora 从「单租户、谁登录谁是管理员」改造为**多租户**。
每个项目有了归属账号，普通账号只能管自己的账号与数据，管理员能管全部。
升级只需替换二进制并重启——**老库自动迁移，无需手工操作、无需改配置、无需改反代**。

## 多租户

- **项目有了归属**。`projects` 新增 `owner` 列，记录创建者。记忆、观测、关系、交接
  **不带** owner——它们经所属项目继承归属。归属只在项目这一层定义：一条观测属于谁，
  取决于它写进了谁的库。记录级 owner 需要每表加列、每次写入校验、每次查询过滤，
  且必然出现「项目 owner 与某条观测 owner 不一致」这种无法自洽的状态。
- **两档角色**。`users` 新增 `role` 列：

  | 能力 | `admin` | `user`（租户） |
  |---|---|---|
  | 项目列表 | 全部（含无主 / 归属失效） | 仅自己名下 |
  | 项目读写 / 统计 / 记忆浏览 / 交接 / 轮换 token | 任意项目 | 仅自己名下 |
  | 创建项目 | 可以（归属为自己） | 可以（归属为自己） |
  | 账号管理（列表 / 新建 / 删除 / 改角色） | 可以 | `403 role_required` |
  | 改口令 | 任何人 | 仅自己 |

  没有中间档。中间档（例如「能看全部但不能删」）需要立刻回答「它能不能删项目 /
  能不能改别人口令」等一串没人有答案的问题。

- **越权返回 404，不返回 403**。403 会变成项目名枚举接口。所有项目类端点经
  `admin::visible_project` 单一判断点，越权时返回与该 id 真不存在**逐字相同模板**的 404
  （有对应用例比对两者模板）。警告另记服务端日志，供安全审计。
- **权限收口在三处**，不在每个 handler 里各自判断（分散判断是越权的常见来源）：
  `AdminIdentity::can_access`（归属）、`admin::visible_project`（项目可见性）、
  `AdminIdentity::require_admin`（角色）。
- **项目列表按归属过滤下推到 SQL**（`list_projects_of`）而非取全量后在内存里筛。
  功能结果相同，但「别人的数据压根没有被读出来」与「读出来又丢掉了」是两种可信度
  完全不同的保证。

## 角色变更即时生效

会话记录携带签发时的角色快照（避免每请求查库），改角色时**原地刷新该账号的全部在线会话**：

- 降权若滞后 12 小时，等于权限变更没生效；
- 提权滞后则让用户对着「说我是管理员但什么也看不到」的界面反复刷新。

响应回传 `sessions_updated`，如实报告刷新了几个会话（0 = 对方当前未登录）。
刷新**不销毁会话**——降权的人不需要重新登录。

## 最后一个管理员保护

删除账号与降级角色都以**管理员数量**（而非账号数量）为判据，`409 last_admin` 拒绝。
只剩一群普通账号时删掉最后一个管理员，会让管理面永久锁死，唯一补救手段是手工改库或
删掉 `_meta.db`（后者连带丢掉全部项目登记）。

逃生路径：先提升另一个账号为管理员，原管理员即可处置。控制台把这条路径写在禁用按钮的
提示里，而不是让人点一个必然失败的按钮再去猜原因。

## 存量库自动迁移

全部走**幂等加列**（`PRAGMA table_info` 检查后 `ALTER TABLE ... ADD COLUMN ... DEFAULT`），
可反复执行：

| 列 | 默认值 | 理由 |
|---|---|---|
| `projects.owner` | `''` | 显式的「无主」中间态，而不是某个人。启动时由 `claim_unowned_projects` 回填给首个管理员（按 username 升序取首个 admin，确定性） |
| `users.role` | `admin` | **忠实承接**旧语义：此前「凡能登录者皆管理员」。若回填成 `user`，升级瞬间所有账号集体失去管理权，且再也无法恢复（没人能进账号页） |

首次启动会打印 `claimed legacy projects without an owner for the first administrator projects=N`。
之后重启不再重复认领（只处理 `owner = ''` 的行）。
迁移期间**不加外键**（`owner` 指向 `users.username`）：SQLite 的 `PRAGMA foreign_keys` 默认
关闭，加了也不生效，只会给人「有约束保护」的错觉。归属校验在应用层完成。

## 兼容性

- **`ADMIN_TOKEN` 恒为管理员、不属于任何租户**。这是脚本兼容性要求：`memora-handoff` 等
  工具只带它，若被降权或受归属过滤，所有脚本会在一次升级后集体坏掉，且报错形态是
  「项目不存在」——极难定位。
- **MCP 通道完全未变**。项目 token 仍只访问自己的库，与账号体系解耦：删除账号不会让
  该项目的 token 失效（记忆数据仍在，由管理员接手处置）。
- **响应新增字段，未删改旧字段**：`GET /projects` 各项多 `owner`，
  `POST /projects` 与 `POST /projects/{id}/token` 多 `owner`，
  `POST /session` 与 `GET /session` 多 `role`，`GET /users` 各项多 `role`。
  老客户端（含交接 CLI）不受影响——`owner` 带 `#[serde(default)]`。
- **`409 last_user` 改名为 `409 last_admin`**，判据也相应从「账号数」改为「管理员数」。
  若有脚本依赖旧错误码，需同步改。

## 控制台

- 导航按角色裁剪：普通账号看不到「账号」页（路由守卫同时拦手输 URL，静默回项目页）。
- 顶栏显示角色徽标与数据范围（「管理员」/「普通账号」）。
- 项目表新增**归属**列。管理员另看到两种异常标记：
  - **遗留**：`owner` 为空（迁移中间态，通常只在首次启动前出现）；
  - **失效**：`owner` 指向已删除的账号。数据完整保留、仅管理员可见，
    转交或注销由人决定——这是刻意的保守选择，自动处置等于替用户做不可逆的决定。
- 账号页可建账号时选角色、随时升降级。删除与降级的禁用判据是管理员数量。
- 建账号的 `role` 缺省为普通账号，且取值**逐字**匹配 `admin` / `user`——
  `"Admin"` 与 `"admin "` 一律 400。入参宽容等于把拼错的值升级成最高权限
  （读库路径仍容忍空白与大小写，因为手工编辑过的存量值不该让管理面失效）。

## 工程注意

- **`pnpm build` 需要 `web/pnpm-workspace.yaml` 的 `allowBuilds: {esbuild: true}`**。
  pnpm 11 起不再读取 `package.json` 的 `pnpm` 字段，且默认忽略构建脚本并以非零状态退出——
  症状是 `pnpm build` 在真正调用 `vue-tsc` 之前就失败，错误信息只有一串 pnpm 内部栈，
  看不出是白名单问题。只放行 esbuild 一个（它只校验平台二进制，无任意代码执行面）。
- **`require_admin` 与 `parse_role` 的错误侧装箱**（`Box<Response>`）：`Response` 有 128 字节，
  直接内联会让每个 `Result` 按它的大小搬运，`clippy::result_large_err` 会拦下。

## 验证

- `cargo test`：**388 passed，0 failed**（此前 365；新增 23 项）
- `cargo clippy --all-targets -- -D warnings`：**零告警**
- `pnpm build`（`vue-tsc -b` 类型检查 + `vite build`）：**通过**
- 运行时冒烟（真实进程 + 真实 Cookie，非 `oneshot`）已覆盖：租户建项目归属自己 →
  租户看不到他人项目 → 越权与不存在的 404 模板一致 → 租户 `/users` 403 →
  提权后旧 Cookie 立刻可管账号 → 降权后立刻失去权限 → `last_admin` 双向拒绝 →
  删号后旧 Cookie 失效且项目仍在（归属显示为失效）→ MCP 写入与检索正常 →
  P7 遗留库启动即完成加列与认领，重启幂等
- 新增端到端用例：`project_listing_is_scoped_by_role`、
  `projects_of_another_tenant_are_indistinguishable_from_missing`、
  `account_management_requires_admin`、`role_changes_take_effect_on_live_sessions`、
  `the_only_admin_can_be_neither_deleted_nor_demoted`、
  `deleting_an_account_orphans_its_projects_without_losing_them`、
  `new_accounts_default_to_tenant_and_reject_unknown_roles`、
  `admin_token_acts_as_a_tenantless_administrator`

## 升级

```bash
memora stop
# 替换二进制
memora start
```

无需改 `config.toml`、无需改 Caddyfile、无需迁移数据。建议升级前备份 `_meta.db`：

```bash
cp ~/.local/share/memora/_meta.db ~/memora-backup/_meta-$(date +%Y%m%d-%H%M%S).db
```

从 v0.3.x 升级是直接的。从 v0.2.x 直接升级请先读 v0.3.0 的说明（控制台账号入库，
配置项降级为种子）。