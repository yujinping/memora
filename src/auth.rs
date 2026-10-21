//! Bearer 鉴权中间件与工具函数。
//!
//! @author yujinping
//! @intent P2：中间件职责扩展为「token → 项目登记项 → 仓库集合」，并把结果作为
//!          `ProjectContext` 注入请求扩展；此后所有 handler 只依赖 trait 对象。
//! @intent P4.4：管理面（`/api/v1`）新增**第二条**凭据通路——网页控制台的会话 Cookie。
//!         两条通路的取舍不同：Bearer 面向脚本（无状态、可写进配置文件与 CI 密钥），
//!         Cookie 面向浏览器（登录一次、自动携带）。二者共用同一个鉴权入口
//!         [`authenticate_admin`]，以免两处各自判定而出现「一条通路做了 CSRF 检查、
//!         另一条忘了做」这类不对称缺陷。
//!         `/mcp` 一侧**只有** Bearer 一条通路，会话 Cookie 不参与——见 `routes` 的装配。
//!
//! @intent P8：管理面的身份从「有没有凭据」细化为「是谁 + 什么角色」。
//!         [`AdminIdentity`] 因此携带 `user` 与 `role`，并把结果写入请求扩展，
//!         让每个 handler 都能取到它（而不是各自重新解析一遍请求头）。
//!         授权判断统一走 [`AdminIdentity::require_admin`] 与
//!         [`AdminIdentity::can_access`]——**分散判断是越权的主要来源**。

use crate::error::{AuthError, ProjectResolveError};
use crate::reply::{internal_error, unauthorized};
use crate::session;
use crate::state::AppState;
use crate::users::Role;
use axum::extract::{Request, State};
use axum::http::{header::AUTHORIZATION, HeaderMap, HeaderValue, Method};
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

/// 管理面的鉴权结果。
///
/// @intent 保留「凭据来自哪条通路」这一信息，是为了让中间件能对**会话通路**施加
///         CSRF 检查：携带会话 Cookie 的是浏览器，而浏览器会自动附带该 Cookie，
///         于是跨站请求也能通过鉴权；Bearer 通路不存在这个问题，故不受该检查约束。
///
/// @intent 两个变体都携带 `user`：授权需要一个**主体**来回答「这个名字的数据
///         你能看吗」。只有 Bearer 通路没有天然主体，它取配置里的 `admin_user`
///         ——那正是「配置里那对凭据的人化形态」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminIdentity {
    /// 来自 `Authorization: Bearer <ADMIN_TOKEN>`（脚本 / 无状态调用方）
    ///
    /// @intent 恒为 [`Role::Admin`]：`ADMIN_TOKEN` 能建项目、能删项目、能读写任何项目
    ///         的数据，它比任何控制台账号权限都大。若把它降级成普通账号，
    ///         `memora-handoff` 这类只用 `ADMIN_TOKEN` 的脚本会突然看不见项目。
    BearerToken {
        /// 主体名（取配置里的 `admin_user`）
        user: String,
    },
    /// 来自有效会话 Cookie（网页控制台）
    Session {
        /// 登录用户名
        user: String,
        /// 签发时的角色快照（见 `session` 模块文档）
        role: Role,
    },
}

impl AdminIdentity {
    /// 主体名（授权与日志用）。
    pub fn user(&self) -> &str {
        match self {
            AdminIdentity::BearerToken { user } => user,
            AdminIdentity::Session { user, .. } => user,
        }
    }

    /// 角色。
    pub fn role(&self) -> Role {
        match self {
            AdminIdentity::BearerToken { .. } => Role::Admin,
            AdminIdentity::Session { role, .. } => *role,
        }
    }

    /// 是否管理员。
    pub fn is_admin(&self) -> bool {
        self.role().is_admin()
    }

    /// 是否可访问某人名下的资源。
    ///
    /// @intent 这是整个权限模型的**唯一一处**判断：管理员放行一切，其余只放行
    ///         自己名下的。所有项目类端点都经 `admin::visible_project` 调用它，
    ///         而不是各自写 `if identity.is_admin() || owner == identity.user()`——
    ///         后者的每一份拷贝都是一次可能写错方向的机会。
    pub fn can_access(&self, owner: &str) -> bool {
        self.is_admin() || self.user() == owner
    }

    /// 要求管理员身份；非管理员返回可直接回传的 403。
    ///
    /// @intent 返回 `Result<(), Response>` 而不是 `bool`：调用方拿到 `Err` 时
    ///         必须把它原样返回，这样「忘写返回值」不会变成「静默放行」
    ///         （`bool` 的 `unwrap_or(false)` 很容易被写成 `unwrap_or(true)`）。
    ///
    /// @intent 错误侧装箱（`Box<Response>`）：`Response` 有 128 字节，直接内联会让
    ///         每个 `Result` 都按它的大小搬运。这里与 `admin::visible_project`
    ///         保持同一形态——同一个模块里两种装箱风格只是给后来者添乱。
    ///         多一次分配只发生在**已经要构造 JSON 响应体**的拒绝路径上。
    pub fn require_admin(&self) -> Result<(), Box<Response>> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(Box::new(crate::reply::forbidden(
                "role_required",
                "this endpoint requires an administrator account",
            )))
        }
    }
}

/// 定长数据的常量时间比较。
///
/// @intent 逐字节异或后统一判定，不做提前返回：`==` 会在首个不同的字节处停下，
///         于是比较耗时随「匹配了多少个前缀字符」变化，理论上可被用于逐字节爆破凭据。
///         长度不等时立即返回 `false`——凭据长度是公开且固定的，泄漏它没有意义。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 解析管理面身份：Bearer 优先，其次会话 Cookie。
///
/// @intent 「Bearer 存在但无效」时**直接失败**，不继续尝试 Cookie。理由：调用方显式
///         提供了凭据却被拒绝，若此时回退到环境里自动携带的会话 Cookie 并放行，
///         会让「配置里的 token 早已失效」这一事实被静默掩盖——而这正是最难排查的
///         一类问题（脚本在配了错的 token 时反而表现正常）。
pub fn authenticate_admin(state: &AppState, headers: &HeaderMap) -> Option<AdminIdentity> {
    if let Some(token) = extract_bearer(headers.get(AUTHORIZATION)) {
        let expected = state.config.admin_token.as_bytes();
        // 空 ADMIN_TOKEN 表示「未配置管理凭据」，此时任何 Bearer 都不该被接受。
        if !expected.is_empty() && constant_time_eq(token.as_bytes(), expected) {
            return Some(AdminIdentity::BearerToken {
                user: state.config.admin_user.clone(),
            });
        }
        return None;
    }

    let id = session::cookie_value(headers, session::COOKIE_NAME)?;
    let found = state.sessions.get(&id)?;
    Some(AdminIdentity::Session {
        user: found.user,
        role: found.role,
    })
}

/// 管理面鉴权中间件：`/api/v1` 的写操作与统计。
///
/// @intent 会话通路上追加跨站检查（见 [`AdminIdentity`]）。这里返回 403 而非 401：
///         凭据本身是有效的，被拒的是「这次请求来自别的站点」，换个凭据也不会变好——
///         属于与 `/mcp` 的 Host 白名单同类的「请求来源不被接受」。
///
/// @intent 身份写入请求扩展，handler 用 `Extension<AdminIdentity>` 取出。
///         这比让每个 handler 自己解析请求头可靠得多：解析逻辑只有一份，
///         而且「某个 handler 忘了解析」会变成编译期缺少提取器，而不是运行期越权。
pub async fn admin_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let identity = match authenticate_admin(&state, req.headers()) {
        Some(identity) => identity,
        None => return auth_failure(AuthError::InvalidToken),
    };

    if matches!(identity, AdminIdentity::Session { .. })
        && is_state_changing(req.method())
        && session::is_cross_origin(req.headers())
    {
        tracing::warn!(
            method = %req.method(),
            path = %req.uri().path(),
            user = %identity.user(),
            "rejected a cross-origin state-changing request authenticated by session cookie"
        );
        return crate::reply::forbidden(
            "cross_origin_rejected",
            "state-changing requests authenticated by a session cookie must originate from \
             the same host; use the admin token for scripts",
        );
    }

    req.extensions_mut().insert(identity);
    next.run(req).await
}

/// 该方法是否会改变服务端状态（从而需要 CSRF 防护）。
///
/// @intent 按「不是安全方法」判定，而不是枚举 POST/DELETE：将来新增 PUT/PATCH 时
///         自动被覆盖，不必记得回来改这里——漏改的后果是**静默失去防护**。
fn is_state_changing(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
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

    /// 常量时间比较必须是真值函数：等值放行，任何差异都必须被发现。
    #[test]
    fn constant_time_eq_is_an_exact_equality_check() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"admin-token", b"admin-token"));

        assert!(
            !constant_time_eq(b"admin-token", b"admin-tokeN"),
            "末位差异"
        );
        assert!(
            !constant_time_eq(b"admin-token", b"Admin-token"),
            "首位差异"
        );
        assert!(
            !constant_time_eq(b"admin-token", b"admin-token "),
            "长度差异"
        );
        assert!(!constant_time_eq(b"short", b"longer-value"));

        // 全字节取值下的差异（含 0x00 与 0xff 的边界）
        assert!(constant_time_eq(&[0x00, 0xff], &[0x00, 0xff]));
        assert!(!constant_time_eq(&[0x00, 0xff], &[0x00, 0xfe]));
    }

    /// 会改变服务端状态的方法判定：安全方法放行，其余一律要求 CSRF 检查。
    ///
    /// @intent 这条档的是「将来新增 PUT/PATCH 却忘了纳入检查」——那会表现为
    ///         **静默失去防护**，而功能测试全部通过。
    #[test]
    fn state_changing_methods_are_classified_by_default_deny() {
        for safe in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(!is_state_changing(&safe), "{safe} 不应被要求 CSRF 检查");
        }
        for unsafe_method in [
            Method::POST,
            Method::DELETE,
            Method::PUT,
            Method::PATCH,
            Method::CONNECT,
        ] {
            assert!(
                is_state_changing(&unsafe_method),
                "{unsafe_method} 必须要求 CSRF 检查"
            );
        }
    }

    // ------------------------------------------------------ 身份与角色（P8）

    /// Bearer 通路恒为管理员，主体名取配置里的 `admin_user`。
    #[test]
    fn bearer_token_identity_is_always_an_admin() {
        let identity = AdminIdentity::BearerToken {
            user: "root".to_string(),
        };
        assert_eq!(identity.user(), "root");
        assert_eq!(identity.role(), Role::Admin);
        assert!(identity.is_admin());
        assert!(identity.require_admin().is_ok());
    }

    /// 会话通路的角色来自签发快照，不得被「猜」成管理员。
    #[test]
    fn session_identity_keeps_the_issued_role() {
        let tenant = AdminIdentity::Session {
            user: "alice".to_string(),
            role: Role::User,
        };
        assert_eq!(tenant.role(), Role::User);
        assert!(!tenant.is_admin());
        assert!(
            tenant.require_admin().is_err(),
            "普通账号必须被账号管理类端点拒绝"
        );

        let admin = AdminIdentity::Session {
            user: "boss".to_string(),
            role: Role::Admin,
        };
        assert!(admin.is_admin());
    }

    /// 数据范围的唯一判断点：管理员全通，其余只看自己名下的。
    ///
    /// @intent 这条断言是越权防护的锚点。它必须同时验证**两个方向**：
    ///         管理员能访问别人的（否则管理面无意义），普通账号不能访问别人的
    ///         （否则隔离是假的）。只测一个方向会让另一个方向的回归静默通过。
    #[test]
    fn access_is_owner_scoped_except_for_admins() {
        let admin = AdminIdentity::Session {
            user: "boss".to_string(),
            role: Role::Admin,
        };
        let alice = AdminIdentity::Session {
            user: "alice".to_string(),
            role: Role::User,
        };

        assert!(admin.can_access("alice"), "管理员可见任何人的项目");
        assert!(admin.can_access("boss"));
        assert!(alice.can_access("alice"), "普通账号可见自己的项目");
        assert!(!alice.can_access("bob"), "普通账号不得见他人的项目");
        assert!(
            !alice.can_access(""),
            "无主项目只对管理员可见（空串不等于任何用户名）"
        );
        assert!(
            admin.can_access(""),
            "迁移期遗留的无主项目必须对管理员可见，否则升级后数据像是丢了"
        );

        // Bearer 通路也走同一个判断（它是管理员，故全通）
        let script = AdminIdentity::BearerToken {
            user: "root".to_string(),
        };
        assert!(script.can_access("anyone-else"));
    }
}
