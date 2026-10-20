# Memora v0.3.1

轻量自托管 AI 长期记忆服务（MCP + SQLite），单二进制。

这是一次**安全修复版本**，补齐 v0.3.0 遗留的三项审计问题（S2 / S4 / S5）。功能与配置无变化，
升级只需替换二进制并重启，无需迁移数据、无需改配置文件、无需改反代。

## 安全修复

- **请求体上限（S2）**：此前 `/mcp` 等端点没有任何请求体大小限制，单个项目 token 持有者可以发送
  超大请求把服务端内存撑爆——在多项目共享的实例上这是可达的拒绝服务面。现在整棵路由树外层挂了
  1 MiB 上限（`MAX_REQUEST_BODY_BYTES`），超限在**鉴权之后、协议解析之前**即被 `413` 短路返回，
  不会进入 rmcp 读爆内存。
  > 用 `RequestBodyLimitLayer` 而非 axum 的 `DefaultBodyLimit`：后者只给 axum 自有提取器设默认上限，
  > **不限制嵌套服务（rmcp）读取的原始 body**——审计发现的盲区正在这里。

- **记忆写路径的容量上限**：此前记忆写入只校验「非空」，无任何长度 / 条数上限。现补齐字段级契约常量：
  `MEMORY_NAME_MAX_BYTES`(256) / `MEMORY_TYPE_MAX_BYTES`(128) / `MEMORY_SOURCE_MAX_BYTES`(256) /
  `MEMORY_OBSERVATION_MAX_BYTES`(16 KiB) / `MEMORY_OBSERVATIONS_PER_ENTITY_MAX`(200) /
  `MEMORY_ENTITIES_MAX`(100) / `MEMORY_RELATIONS_MAX`(200) / `MEMORY_OBSERVATIONS_MAX`(500)。
  与上一条构成「边缘 + 语义」双层防护。

- **`drop_project` 与取库的并发竞态（S4）**：旧实现「先摘缓存、再删目录」，两步之间并发
  `repositories_for` 会走慢路径建池，此时 `remove_dir_all` 把它刚建好的文件删掉，表现为
  `(code: 14) unable to open database file`。现在两者共用同一把建池闸门串行化。
  回归用例以 2000 次交错循环钉死（必须 `multi_thread`，单线程下会假绿）。

- **明文 HTTP 告警（S5）**：CLI 以 `http://` 连接**非本机**地址时，`ADMIN_TOKEN` 与全部交接数据
  都以明文过网。现在启动即打印醒目告警并建议改用 https 或 SSH 隧道（`ssh -L`）。
  刻意**只告警不拒绝**——内网与隧道转发是既定用法，交由用户判断。

## 升级说明

**从 v0.3.0 升级是直接的**，无需任何手工步骤：

1. 停服务 → 替换二进制 → 启动
2. 配置、`_meta.db`、项目库、记忆与交接数据全部不受影响（本次无表结构变更）
3. 反代前缀与 v0.3.0 相同：`/mcp`、`/api/v1`、`/admin*`、`/assets/*`、`/favicon.svg`、`/health`

从 v0.2.x 直接升级到本版请先读 v0.3.0 的说明（控制台账号入库，配置项降级为种子）。

## 验证

- `cargo test`：365 passed，0 failed
- `cargo clippy --all-targets -- -D warnings`：零告警
- 新增用例 `oversized_request_body_is_rejected_with_413`、`drop_project_racing_repositories_for_does_not_corrupt_db`