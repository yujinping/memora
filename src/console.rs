//! 网页控制台：`/admin` 页面本体，以及它使用的登录 / 登出 / 自检接口。
//!
//! @author yujinping
//! @intent 背景：管理面最初只有 `POST /api/v1/projects` 这类接口，创建项目必须 `curl`
//!         并手工拼 `Authorization` 头。接口本身没问题——问题是设计时**从未立项
//!         人类界面**（设计文档里检索「控制台 / 登录 / 浏览器」为零命中），
//!         于是「怎么建一个项目」这件事只有脚本能回答。本模块补上这条路。
//!
//! @intent 页面以 `include_str!` 内嵌进二进制，不引入前端构建步骤、不新增依赖，
//!         与 `deploy/` 模板的做法一致；仍是单二进制，拷过去就能用。
//!         代价是改动页面需要重新编译——对一个自托管服务而言，这比引入
//!         node 工具链划算得多。
//!
//! @intent 页面**不携带任何凭据**：它自己在浏览器里请求 `/api/v1/session` 登录，
//!         凭据存在会话 Cookie 中（HttpOnly，脚本读不到）。因此 `/admin` 本身
//!         无需鉴权——它只是一张登录表单，没有任何数据。
//!
//! 状态码约定（与 `admin` 模块一致）：
//! - 401：口令错误，或未配置登录口令（凭据无法成立）
//! - 403：来源不被接受（跨站写请求）
//! - 500：服务端故障（熵源不可用）

use crate::auth::{authenticate_admin, AdminIdentity};
use crate::password;
use crate::reply;
use crate::session;
use crate::state::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

/// `/admin` 页面本体（内嵌）。
const CONSOLE_HTML: &str = include_str!("console.html");

/// 登录请求体。
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// 用户名
    pub username: String,
    /// 口令（明文，仅在本次请求的内存中存在）
    pub password: String,
}

/// 登录成功响应体。
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    /// 登录用户名
    pub user: String,
    /// 会话有效期（秒）
    pub expires_in_secs: u64,
}

/// 会话自检响应体（页面用它决定显示登录表单还是控制台）。
///
/// @intent `console_login_enabled` 会暴露给未登录访客。这是刻意的：页面必须知道
///         该不该渲染登录框，否则只能给一个必然失败的输入界面。它泄漏的信息
///         仅限于「这台 memora 是否配了控制台口令」，对攻击者没有实际帮助。
#[derive(Debug, Serialize)]
pub struct SessionStatus {
    /// 是否已配置控制台口令（未配置时登录不可用）
    pub console_login_enabled: bool,
    /// 本次请求是否已通过鉴权
    pub authenticated: bool,
    /// 已登录时的用户名
    pub user: Option<String>,
}

/// 返回控制台页面。
///
/// @intent `Cache-Control: no-store`：页面本身不含数据，但浏览器缓存它反而会造成
///         「已更新二进制、浏览器仍跑旧页面」的困惑，而这不属于值得排查的问题。
pub async fn page() -> Response {
    (
        StatusCode::OK,
        [
            ("content-type", "text/html; charset=utf-8"),
            ("cache-control", "no-store"),
            // 页面不含任何外部资源与内联事件处理器之外的脚本来源，故可以收紧到
            // `self`；`'unsafe-inline'` 仅用于页面内的 <script> 与 <style>。
            (
                "content-security-policy",
                "default-src 'none'; style-src 'self' 'unsafe-inline'; \
                 script-src 'self' 'unsafe-inline'; connect-src 'self'; base-uri 'none'; \
                 form-action 'none'",
            ),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "same-origin"),
        ],
        CONSOLE_HTML,
    )
        .into_response()
}

/// 会话自检（公开）。
///
/// @intent 之所以公开：它是页面的启动调用，必须在登录之前就能问出「要不要显示登录框」。
///         它不返回任何项目数据，只回答「你是谁 / 要不要登录」。
pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let identity = authenticate_admin(&state, &headers);
    Json(SessionStatus {
        console_login_enabled: state.config.console_login_enabled(),
        authenticated: identity.is_some(),
        user: match identity {
            Some(AdminIdentity::Session { user }) => Some(user),
            Some(AdminIdentity::BearerToken) => Some(state.config.admin_user.clone()),
            None => None,
        },
    })
    .into_response()
}

/// 登录：校验用户名与口令，成功后签发会话 Cookie。
///
/// @intent 用户名不匹配时**仍然执行**一次口令校验。否则「用户名错」会因跳过 argon2
///         而快出一个数量级，形成可利用的用户名枚举信道；而用户名恰恰是本服务唯一
///         的公开标识（默认 `admin`），一旦可枚举，爆破就只需面对口令这一层。
///         因此两个条件都先求值、再合并判定。
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    if !state.config.console_login_enabled() {
        tracing::warn!(
            config_file = %state.config.config_path.display(),
            "login attempt rejected: no admin password is configured"
        );
        return reply::unauthorized_with(
            "console login is not configured; set admin_password_hash in the config file \
             (or run `memora init`)",
        );
    }

    let user_ok = crate::auth::constant_time_eq(
        req.username.trim().as_bytes(),
        state.config.admin_user.as_bytes(),
    );
    // 无论用户名对错都要跑完：见本函数的 @intent。
    let password_ok = password::verify_password(&req.password, &state.config.admin_password_hash);

    if !(user_ok && password_ok) {
        tracing::warn!(
            user = %req.username.trim(),
            username_matched = user_ok,
            "console login failed"
        );
        return reply::unauthorized_with("invalid username or password");
    }

    let id = match state.sessions.create(&state.config.admin_user) {
        Ok(id) => id,
        Err(err) => return reply::internal_error(&err),
    };

    let secure = session::request_is_secure(&headers);
    let mut response_headers = HeaderMap::new();
    session::attach_cookie(
        &mut response_headers,
        &session::build_session_cookie(&id, session::SESSION_TTL, secure),
    );

    tracing::info!(user = %state.config.admin_user, "console login succeeded");
    (
        StatusCode::OK,
        response_headers,
        Json(LoginResponse {
            user: state.config.admin_user.clone(),
            expires_in_secs: session::SESSION_TTL.as_secs(),
        }),
    )
        .into_response()
}

/// 登出：销毁会话并让浏览器立即丢弃 Cookie。
///
/// @intent 不要求已登录：未登录时登出应幂等成功，而不是报 401——「我想退出」被拒绝
///         是毫无意义的失败。Cookie 无论如何都被清掉。
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(id) = session::cookie_value(&headers, session::COOKIE_NAME) {
        if state.sessions.remove(&id) {
            tracing::info!("console session removed");
        }
    }

    let secure = session::request_is_secure(&headers);
    let mut response_headers = HeaderMap::new();
    session::attach_cookie(
        &mut response_headers,
        &session::expired_session_cookie(secure),
    );

    (StatusCode::NO_CONTENT, response_headers, "").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内嵌页面必须真的存在、非空，且引用了控制台所需的接口。
    ///
    /// @intent `include_str!` 的路径错误会在编译期报错，但**内容走样**（例如误改了
    ///         文件名指向一个空文件）不会。这条断言把「页面被打包进二进制」钉住。
    ///
    /// @intent 断言的是「API 前缀 + 路径片段」，而不是拼好的完整 URL：页面里写的是
    ///         `const API = "/api/v1"` 与 `api("GET", "/session")`，完整串
    ///         `/api/v1/session` 在源码中根本不存在。按完整串断言会得到一个
    ///         永远失败的用例，而永远失败的用例的归宿是被删掉——测试必须描述
    ///         实现里真实存在的东西。这一点在实测中踩到过。
    #[test]
    fn embedded_console_page_is_present_and_self_contained() {
        assert!(CONSOLE_HTML.len() > 1024, "页面内容过短，疑似未正确内嵌");
        assert!(
            CONSOLE_HTML.contains("<!DOCTYPE html") || CONSOLE_HTML.contains("<html"),
            "应是一份完整的 HTML 文档"
        );

        assert!(
            CONSOLE_HTML.contains(r#"const API = "/api/v1""#),
            "页面必须把管理接口统一挂在 /api/v1 前缀下"
        );
        for endpoint in ["/session", "/projects", "/graph"] {
            assert!(
                CONSOLE_HTML.contains(endpoint),
                "页面必须调用 {endpoint} 接口"
            );
        }

        // 不得引用任何外部资源：自托管常无外网，且外链会引入供应链与隐私问题。
        for forbidden in ["http://cdn", "https://cdn", "//unpkg", "googleapis"] {
            assert!(
                !CONSOLE_HTML.contains(forbidden),
                "页面不得依赖外部资源：{forbidden}"
            );
        }
    }

    /// 页面必须包含「token 只显示一次」的提示——这是创建流程里最容易造成不可逆
    /// 损失的认知点（明文 token 只在创建响应里出现一次）。
    #[test]
    fn embedded_page_warns_that_the_token_is_shown_once() {
        assert!(
            CONSOLE_HTML.contains("只显示一次") || CONSOLE_HTML.contains("仅显示一次"),
            "页面必须明示 token 只出现一次"
        );
    }

    /// 脚本引用的每一个元素 id 都必须真实存在，且 id 不得重复。
    ///
    /// @intent 这是**无浏览器**条件下最有价值的一条断言。控制台的脚本在文档末尾一次性
    ///         绑定全部按钮与输入框，任何一个 id 拼错都会在 `addEventListener` 处抛
    ///         `TypeError: Cannot read properties of null`，**且脚本就此中断**——表现是
    ///         「页面打开后一片空白，登录框都不出现」，而服务端日志里没有任何异常
    ///         （浏览器端错误不会回传），端到端 HTTP 用例也全绿。
    ///         这类缺陷在没有浏览器的环境里几乎只能靠肉眼逐行比对，故固化下来。
    ///
    /// @intent id 重复同样致命：`getElementById` 只返回第一个匹配元素，
    ///         第二个永远不会被绑定，症状与拼错 id 完全一样（点了没反应）。
    #[test]
    fn every_dom_reference_in_the_page_resolves() {
        let ids = collect_ids(CONSOLE_HTML);
        assert!(!ids.is_empty(), "页面里应至少有一个带 id 的元素");

        let mut missing = Vec::new();
        for reference in collect_id_references(CONSOLE_HTML) {
            if !ids.contains(&reference) {
                missing.push(reference);
            }
        }
        assert!(
            missing.is_empty(),
            "脚本引用了不存在的元素 id：{missing:?}（页面共有 {ids:?}）"
        );
    }

    /// 展示不可信内容时只许走 `textContent`，不得用任何解析 HTML 的写入口。
    ///
    /// @intent 控制台展示的实体名与观测内容都来自 AI 客户端写入的记忆库，是不可信数据。
    ///         用 `innerHTML` 渲染等于在自己的管理界面里开一个存储型 XSS——而这一条
    ///         用肉眼审查极难持续保证（任何一个新字段都可能顺手写成 innerHTML）。
    ///         断言里用带前导点的 `.innerHTML` 而非裸词，是为了不与注释中
    ///         「绝不使用 innerHTML」这句话本身冲突。
    #[test]
    fn page_renders_untrusted_text_without_html_parsing_apis() {
        for forbidden in [
            ".innerHTML",
            ".outerHTML",
            "insertAdjacentHTML",
            "document.write(",
            "eval(",
        ] {
            assert!(
                !CONSOLE_HTML.contains(forbidden),
                "页面不得使用 {forbidden}：记忆内容是不可信数据，必须经 textContent 写入"
            );
        }
        assert!(
            CONSOLE_HTML.contains("node.textContent = String(opts.text)"),
            "页面应存在一个统一经 textContent 写文本的建元素助手"
        );
    }

    /// 收集 HTML 里全部 `id="..."` 取值；重复即断言失败。
    fn collect_ids(html: &str) -> std::collections::BTreeSet<String> {
        let mut ids = std::collections::BTreeSet::new();
        let mut rest = html;
        while let Some(at) = rest.find("id=\"") {
            rest = &rest[at + 4..];
            let Some(end) = rest.find('"') else { break };
            let id = &rest[..end];
            assert!(
                ids.insert(id.to_string()),
                "元素 id 重复：{id}——getElementById 只会命中的第一个，另一个绑不上事件"
            );
            rest = &rest[end..];
        }
        ids
    }

    /// 收集脚本里 `$("...")` 形式的元素引用。
    ///
    /// @intent 页面的取元素助手固定叫 `$` 且只接受字符串字面量，故这种朴素扫描足够；
    ///         若将来引入 `$(变量)` 的用法，本函数会静默漏掉它——那时需要改成
    ///         真正的解析而不是正则。
    fn collect_id_references(html: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = html;
        while let Some(at) = rest.find("$(\"") {
            rest = &rest[at + 3..];
            let Some(end) = rest.find('"') else { break };
            found.push(rest[..end].to_string());
            rest = &rest[end..];
        }
        found
    }
}
