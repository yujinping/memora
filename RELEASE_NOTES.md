# Memora v0.2.1

轻量自托管 AI 长期记忆服务（MCP + SQLite），单二进制。

## 新增

- **任务交接（handoff，P4.5）**：跨客户端 / 跨机器工作交接。4 个 MCP 工具（`handoff_save` / `handoff_load` / `handoff_list` / `handoff_ack`）+ 7 个管理 REST 路由 + `memora handoff` CLI 子命令。**零 ID 主路径**：save 不带 task_id 自动落 `current@<client>` 槽位，load/ack 不带参数接当前待接手；多任务待接手时返回候选清单，从不让用户猜 ID。追加式修订 + CAS ack，接手状态可见可追溯
- **会话工具（P5.1）**：3 个 MCP 工具（`log_turn` / `get_session` / `list_sessions`）逐轮记录会话 transcript，轮次序号由服务端在会话内单调分配；缺失会话返回空集；列表按最近活跃降序。配合交接工具实现「A 机写、B 机恢复」
- **单库存储后端 `sqlite_single`（P6）**：`storage_backend = "sqlite_single"` 让所有项目共用一个 SQLite 文件（`data/mem-single.db`），租户隔离靠 `project_id` 列过滤——适合项目多、inode/文件句柄受限的场景。默认仍为 `sqlite_file`（每项目一库）
- CLI 子命令直连服务端（`--server`，HTTPS 走 rustls），零安装即可读写 / 交接

## 功能

- 标准 MCP（Streamable HTTP）协议，**16 个工具**（9 记忆 + 4 交接 + 3 会话），接入 Zed / WorkBuddy / Cursor / Claude 等客户端
- 按项目硬隔离的长期记忆存储（SQLite，双后端可选）
- 网页控制台（`/admin`，Argon2id 口令 + CSRF 双防）
- 单实例 XDG 绝对路径 TOML 配置；自守护进程管理（daemon 模式，Linux / macOS / Windows）

## 安装

各平台二进制见下方 Assets（Linux / macOS x64 / macOS arm64 / Windows x64），解压即用。

部署模板（systemd / Caddyfile / config.toml）见仓库 `deploy/` 目录。

## 支持平台

| 平台 | 架构 |
|------|------|
| Linux | x86_64 |
| macOS | x86_64 / arm64 |
| Windows | x86_64 |
