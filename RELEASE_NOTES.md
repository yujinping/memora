# Memora v0.1.0

轻量自托管 AI 长期记忆服务（MCP + SQLite），单二进制。

## 功能

- 标准 MCP（Streamable HTTP）协议，接入 Zed / WorkBuddy / Cursor / Claude 等客户端
- 按项目物理隔离的长期记忆存储（SQLite）
- 单实例 XDG 绝对路径 TOML 配置
- 自守护进程管理（daemon 模式）

## 安装

各平台二进制见下方 Assets（Linux / macOS x64 / macOS arm64），解压即用。

部署模板（systemd / Caddyfile / config.toml）见仓库 `deploy/` 目录。

## 支持平台

| 平台 | 架构 |
|------|------|
| Linux | x86_64 |
| macOS | x86_64 / arm64 |