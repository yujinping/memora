//! MCP 工具层：把领域仓库暴露为 9 个标准记忆工具（Streamable HTTP 传输）。
//!
//! @author yujinping
//! @intent P3：以 rmcp（Rust MCP SDK）实现标准 MCP 服务端，语义对齐官方 MCP Memory，
//!         使 Cursor / Claude / Cline / WorkBuddy 等客户端零改造接入。
//!         分层上刻意做成三段：`dto`（协议数据契约）→ `tools`（后端无关语义）
//!         → `server`（rmcp 协议适配）。只有 `server` 依赖 rmcp，
//!         故工具语义可以脱离传输层、直接用内存后端做快速单测。

pub mod dto;
pub mod server;
pub mod tools;

pub use server::MemoraServer;

use crate::config::Config;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

/// 构造 `/mcp` 的 Streamable HTTP 服务。
///
/// @intent 刻意选用**无状态**传输（`legacy_session_mode = false` + `json_response = true`），
/// 理由有三：
/// 1. 9 个工具都是「一问一答」，服务端无需向客户端推送，会话带来的只有状态与内存开销；
/// 2. 无会话即无「会话 id 被另一项目 token 复用」的越权面，隔离只由 token 决定；
/// 3. 2GB 服务器的目标要求常驻内存尽量低，省略会话表是直接收益。
///
/// 代价是每次请求都要重新解析 token → 项目（元库单次索引查询 + 后端连接缓存命中），
/// 相对一次记忆读写的开销可忽略。
///
/// `allowed_hosts` 由配置注入：rmcp 默认只信回环 Host 以防 DNS 重绑定，
/// 而公网部署经 Caddy 反代会透传真实域名，故必须可配（见 `Config::effective_mcp_allowed_hosts`）。
pub fn build_service(config: &Config) -> StreamableHttpService<MemoraServer, LocalSessionManager> {
    // 启动自检：把「对外承诺的工具面」与宏实际注册的工具对齐。
    // 两者不一致只可能是装配漏接（编程错误），故在服务启动阶段直接失败，
    // 而不是等某个客户端发现少了个工具——启动即失败优于运行期才发现。
    assert_tool_manifest();

    let mcp_config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_allowed_hosts(config.effective_mcp_allowed_hosts());

    StreamableHttpService::new(|| Ok(MemoraServer::new()), Default::default(), mcp_config)
}

/// 校验宏注册的工具与 [`tools::TOOL_NAMES`] 完全一致。
///
/// @intent 该不变量同时被单测覆盖；放在这里是为了让它在**真实启动路径**上生效，
///         而非仅在测试构建里成立。
fn assert_tool_manifest() {
    let actual = MemoraServer::new().tool_names();
    tracing::info!(tools = ?actual, "MCP tools ready");

    for name in tools::TOOL_NAMES {
        assert!(
            actual.iter().any(|a| a == name),
            "MCP 工具未注册：{name}（实际注册：{actual:?}）"
        );
    }
    assert_eq!(
        actual.len(),
        tools::TOOL_NAMES.len(),
        "MCP 工具数量与清单不符（实际注册：{actual:?}）"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 装配不 panic，且 Host 白名单来自配置（而非 rmcp 的默认回环）。
    #[test]
    fn service_uses_hosts_from_config() {
        let mut config = Config::for_test(0, ".", "admin", "sqlite_file");
        config.mcp_allowed_hosts = vec!["mem.example.com".to_string()];

        let service = build_service(&config);
        assert_eq!(service.config.allowed_hosts, vec!["mem.example.com"]);
        assert!(
            !service.config.legacy_session_mode,
            "必须是无状态传输：有会话就多一处越权面与状态开销"
        );
        assert!(
            service.config.session_store.is_none(),
            "无状态模式下不应配置会话持久化"
        );
    }

    /// 未配置时回退到回环白名单，避免默认把服务暴露给任意 Host。
    #[test]
    fn service_falls_back_to_loopback_hosts() {
        let config = Config::for_test(0, ".", "admin", "sqlite_file");
        let service = build_service(&config);
        assert_eq!(
            service.config.allowed_hosts,
            crate::config::DEFAULT_MCP_ALLOWED_HOSTS
                .iter()
                .map(|h| h.to_string())
                .collect::<Vec<_>>()
        );
    }
}
