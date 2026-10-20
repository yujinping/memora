# Memora v0.3.0

轻量自托管 AI 长期记忆服务（MCP + SQLite），单二进制。

本次版本重写网页控制台：前端换成 Vue 3 + Vite + Tailwind CSS v4，构建产物在编译期内嵌进可执行文件——**发布形态仍是单文件**，部署时不需要 Node、不需要额外的静态目录。控制台账号从「配置文件里的一行口令哈希」升级成数据库里的账号表，支持多账号自助管理。

## 新增

- **控制台前端工程化（Vue 3 + Vite + Tailwind CSS v4）**：`web/` 为独立前端工程（Vue 3.5 `<script setup>` + vue-router 4 + TypeScript strict + vue-tsc 类型校验），`scripts/build-web.sh` 一键构建，产物落到 `src/console_dist/`。`build.rs` 在编译期扫描该目录，生成 `$OUT_DIR/console_assets.rs`，再由 `include_bytes!` 拷进二进制——**产物缺失时编译期直接失败并提示修复命令**，不会发布出一个只有后端的包
- **控制台账号入库（`_meta.db` 的 `users` 表）**：`/admin` 不复读 `admin_password_hash` 的明文语义。`memora init`（或首次启动）把配置里的 `admin_user` / `admin_password_hash` 作为**种子**写进 `users` 表，此后改口令、加账号、删账号都改库，不会被重启回滚。支持中文 / 邮箱形式用户名（≤64 字符，禁空白与控制字符）
- **多账号管理**：`GET/POST /api/v1/users`、`PUT/DELETE /api/v1/users/{username}`。弱口令 400（`weak_password`）、重名 409（`user_exists`）、**删最后一个账号 409（`last_user`）**；删除账号会同步吊销该账号所有在线会话
- **Token 轮换**：`POST /api/v1/projects/{id}/token` 只换 `token_hash`，旧 token 立即失效，项目数据与 ID 不变。控制台项目页的「重置 token」即走此端点
- **控制台内浏览记忆与交接**：记忆列表 + 实体关系、交接列表 + 详情（详情单独请求，因为 `state_excerpt` 在列表里是截断的，不含 `next_steps` / `decisions` / `open_questions`）
- **`scripts/build-web.sh`**：`--no-install` 可跳过依赖安装；默认 `pnpm install --frozen-lockfile`，构建时关闭 pnpm 11 的隐式 install（`verify-deps-before-run=false`），避免 CI 里静默改锁文件

## 功能

- 标准 MCP（Streamable HTTP）协议，**16 个工具**（9 记忆 + 4 交接 + 3 会话），接入 Zed / WorkBuddy / Cursor / Claude 等客户端
- 按项目硬隔离的长期记忆存储（SQLite，`sqlite_file` / `sqlite_single` 双后端可选）
- 网页控制台（`/admin`）：项目与 token 管理、记忆浏览、交接浏览、账号管理
- 单实例 XDG 绝对路径 TOML 配置；自守护进程管理（daemon 模式，Linux / macOS / Windows）
- CLI 子命令直连服务端（`--server`，HTTPS 走 rustls），零安装即可读写 / 交接

## 安全

- **双凭据体系**：`/mcp` 只认项目 Bearer token；`/api/v1` 认 `ADMIN_TOKEN` Bearer 或会话 Cookie（`memora_session`，`HttpOnly` + `SameSite=Strict` + 绝对 12h TTL，不滑动续期）
- **CSRF 双防线**：SameSite=Strict 之外再校验 `is_cross_origin`——有 `Origin` 无 `Host` 时 fail-closed，跨源写直接 403
- **账号枚举时序防护**：账号不存在时仍跑一次 Argon2id（`dummy_hash()` 用 `OnceLock` 缓存，只算一次），登录耗时与真实账号不可区分
- **CSP 收紧**：去掉 `'unsafe-inline'`（`default-src 'none'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self' data:; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`）。为此把 `<img src="/favicon.svg">` 改成内联 SVG 组件，样式全部走打包后的 CSS 文件
- **缓存策略反向隔离**：`/assets/*` → `public, max-age=31536000, immutable`（文件名带内容哈希）；入口 HTML → `no-store`（否则升级后用户拿着旧 HTML 去请求已删除的旧 chunk）。`/assets/*` 未命中返回 **404 而不是回退 HTML**，避免 MIME 错配导致的 script 解析失败
- 资源响应统一带 `X-Content-Type-Options: nosniff`；`Content-Type` 由 `build.rs` 的扩展名映射决定，不信任客户端

## 升级说明

**从 v0.2.x 升级是平滑的**，无需手工迁移：

1. 启动时若 `users` 表为空，用配置里的 `admin_user` / `admin_password_hash` 播种为第一个账号，日志会打印 `console account seeded`
2. 表非空则跳过播种（`admin_password_hash` 此后仅作冷启动兜底，改它不会影响已有账号）——**此时改口令请走控制台或 `PUT /api/v1/users/{username}`**
3. 已有的项目、token、记忆、交接数据完全不受影响

如果忘了控制台口令：停服务，删除 `_meta.db` 里 `users` 表的行（或整表），再用配置里的 `admin_password_hash` 重启即可重新播种。**注意**：`_meta.db` 同时存着 `projects` 表，删表请只删 `users`。

## 安装

各平台二进制见下方 Assets（Linux / macOS x64 / macOS arm64 / Windows x64），解压即用。

部署模板（systemd / Caddyfile / config.toml）见仓库 `deploy/` 目录。

反代需放行的前缀：`/admin`、`/assets/`、`/favicon.svg`、`/mcp`、`/api/v1`、`/health`。

## 支持平台

| 平台 | 架构 |
|------|------|
| Linux | x86_64 |
| macOS | x86_64 / arm64 |
| Windows | x86_64 |