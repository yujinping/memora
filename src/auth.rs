//! Bearer 鉴权中间件与工具函数。
//!
//! @author yujinping
//! @intent P2：中间件职责扩展为「token → 项目登记项 → 仓库集合」，并把结果作为
//!          `ProjectContext` 注入请求扩展；此后所有 handler 只依赖 trait 对象。

use crate::error::{AuthError, ProjectResolveError};
use crate::reply::{internal_error, unauthorized};
use crate::state::AppState;
use axum::extract::{Request, State};
use axum::http::{header::AUTHORIZATION, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use sha2::{Digest, Sha256};

/// 从 Authorization 头提取 Bearer token（前缀大小写不敏感，去除首尾空白）。
pub fn extract_bearer(hdr: Option<&HeaderValue>) -> Option<String> {
    let h = hdr?.to_str().ok()?;
    let prefix = "bearer ";
    if h.to_ascii_lowercase().starts_with(prefix) {
        let token = h[prefix.len()..].trim();
        if token.is_empty() {
            None
        } else {
            Some(token.to_string())
        }
    } else {
        None
    }
}

/// 计算字符串的 SHA-256 十六进制摘要（token 入库前散列）。
pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex_encode(&hasher.finalize())
}

/// 字节序列 → 小写十六进制字符串。
///
/// @intent 摘要与随机 token 共用同一编码，避免两处各写一遍格式化逻辑而漂移。
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 项目 token 的熵源字节数（256 bit）。
///
/// @intent 与 PEM / 会话密钥同量级；取值偏大是为了让 token 在任何情况下都不属于
///         可暴力枚举的空间，因为它是项目数据的唯一凭据。
pub const TOKEN_BYTES: usize = 32;

/// 生成随机项目 token（64 个十六进制字符）。
///
/// @intent 明文 token 只在 `POST /api/v1/projects` 的响应中出现一次，库内仅存 SHA-256
///         （见 [`sha256_hex`]）。熵源不可用属系统性故障，故显式返回错误而非回退到
///         弱随机——回退会让「看似正常但可预测」的 token 悄悄上线。
pub fn generate_token() -> anyhow::Result<String> {
    random_hex(TOKEN_BYTES)
}

/// 取系统熵并编码为小写十六进制串。
///
/// @intent 项目中所有随机量（项目 token、自动生成的 project_id）共用同一处熵源访问，
///         避免各调用点各自处理「熵源不可用」这一故障分支。
pub fn random_hex(bytes: usize) -> anyhow::Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("system entropy unavailable: {e}"))?;
    Ok(hex_encode(&buf))
}

/// 鉴权失败 → 401 响应（现阶段两类失败对外表现一致，避免 token 探测）。
fn auth_failure(err: AuthError) -> Response {
    tracing::debug!(reason = ?err, "rejected request");
    unauthorized()
}

/// Bearer 鉴权中间件：解析 token → 定位项目 → 把仓库集合注入请求扩展。
///
/// @intent 注入的是 `ProjectRepos` 本身（自带 project_id 与 backend），
///         而非再包一层上下文结构——避免同一信息存在两个来源。
///         此后所有 handler 只依赖 `Arc<dyn MemoryRepository>`，不认识具体后端。
pub async fn bearer_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let token = match extract_bearer(req.headers().get(AUTHORIZATION)) {
        Some(token) => token,
        None => return auth_failure(AuthError::MissingToken),
    };

    match state.repositories_for_token(&token).await {
        Ok(repos) => {
            // 记录归属：一旦发生「跨项目误写」，日志要能直接回答这次调用落到哪个项目、
            // 哪个存储后端（`ProjectRepos` 的这两个字段就是为诊断而存在的，见 docs §12.3）。
            tracing::debug!(
                project_id = %repos.project_id,
                backend = repos.backend.as_str(),
                "resolved project repositories"
            );
            req.extensions_mut().insert(repos);
            next.run(req).await
        }
        Err(ProjectResolveError::Auth(err)) => auth_failure(err),
        Err(ProjectResolveError::Storage(err)) => internal_error(&err),
    }
}

/// 管理员鉴权中间件：比对 ADMIN_TOKEN。
pub async fn admin_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    match extract_bearer(req.headers().get(AUTHORIZATION)) {
        Some(t) if t == state.config.admin_token => next.run(req).await,
        Some(_) => auth_failure(AuthError::InvalidToken),
        None => auth_failure(AuthError::MissingToken),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::http::HeaderValue;

    #[test]
    fn extracts_bearer_variants() {
        let h = HeaderValue::from_str("Bearer abc123").unwrap();
        assert_eq!(extract_bearer(Some(&h)), Some("abc123".to_string()));

        let h2 = HeaderValue::from_str("bearer  token-xyz  ").unwrap();
        assert_eq!(extract_bearer(Some(&h2)), Some("token-xyz".to_string()));

        let h3 = HeaderValue::from_str("Basic abc").unwrap();
        assert_eq!(extract_bearer(Some(&h3)), None);

        assert_eq!(extract_bearer(None), None);
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// 十六进制编码的边界：空输入、前导零、逐字节补零。
    #[test]
    fn hex_encode_pads_every_byte() {
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex_encode(&[0x01]), "01");
    }

    /// 生成的 token 必须是 256 bit 熵、小写十六进制、且不重复。
    ///
    /// @intent 重复性用批量断言而非两两比较：256 bit 空间的碰撞若真发生，
    ///         说明熵源被固定（例如误用常量种子），必须直接被测试抓住。
    #[test]
    fn generated_tokens_are_full_length_lowercase_hex_and_unique() {
        let token = generate_token().unwrap();
        assert_eq!(token.len(), TOKEN_BYTES * 2, "应为 64 字符：{token}");
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "只允许小写十六进制字符：{token}"
        );

        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            assert!(seen.insert(generate_token().unwrap()), "token 出现重复");
        }
    }
}
