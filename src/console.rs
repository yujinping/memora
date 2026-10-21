//! 网页控制台：会话接口（登录 / 登出 / 自检）与内嵌前端产物。
//!
//! @author yujinping
//! @intent 背景：管理面最初只有 `POST /api/v1/projects` 这类接口，创建项目必须 `curl`
//!         并手工拼 `Authorization` 头。接口本身没问题——问题是设计时**从未立项
//!         人类界面**（设计文档里检索「控制台 / 登录 / 浏览器」为零命中），
//!         于是「怎么建一个项目」这件事只有脚本能回答。本模块补上这条路。
//!
//! @intent P7：控制台从一张手写 HTML（638 行内联脚本）升级为 **Vue3 + Vite + Tailwind**
//!         构建的 SPA。构建产物由 `build.rs` 收集、经 `include_bytes!` 内嵌，
//!         运行时由 [`asset`] 按路径直接吐字节——没有 `dist` 目录、没有运行时解压，
//!         仍然是一个可执行文件拷过去就能用的单二进制。
//!         代价是改动前端需要先 `pnpm build`；`build.rs` 会在产物缺失时用占位页替换，
//!         使「只改后端」的场景不必装 node 工具链（见 `build.rs` 的文档）。
//!
//! @intent 页面**不携带任何凭据**：它自己在浏览器里请求 `/api/v1/session` 登录，
//!         凭据存在会话 Cookie 中（HttpOnly，脚本读不到）。因此 `/admin` 本身
//!         无需鉴权——它只是一份空壳 HTML，没有任何数据。
//!
//! @intent 账号来源在 P7 换成了 `_meta.db.users`（见 [`crate::users`]）：本模块的登录
//!         只负责「查库校验 → 开会话」，不再直接读配置里的口令哈希。配置文件里的
//!         `admin_user` / `admin_password_hash` 退化为**首次启动的种子**。
//!
//! 状态码约定（与 `admin` 模块一致）：
//! - 401：口令错误，或系统中还没有任何可登录账号（凭据无法成立）
//! - 403：来源不被接受（跨站写请求）
//! - 500：服务端故障（元库不可用、熵源不可用）

use crate::auth::authenticate_admin;
use crate::reply;
use crate::session;
use crate::state::AppState;
use crate::users;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

/// 前端构建产物清单（由 `build.rs` 生成）。
///
/// @intent 用「生成一个 Rust 源文件」而不是「生成的 JSON + 运行时解析」：后者要求
///         字节序列以 `&'static [u8]` 之外的形式存在（例如 `Vec<u8>` 常量），
///         等于在启动时多做一次无意义的拷贝与解析；而 `include_bytes!` 直接把
///         文件内容编进只读数据段，零运行时代价。
mod manifest {
    include!(concat!(env!("OUT_DIR"), "/console_assets.rs"));
}

/// 单条内嵌资源。
pub struct Asset {
    /// 去掉前导 `/` 的请求路径（如 `assets/index-abc123.js`）
    pub path: &'static str,
    /// 文件内容
    pub bytes: &'static [u8],
    /// 响应用的 MIME 类型
    pub content_type: &'static str,
    /// 是否是可以长期缓存的指纹资源（文件名含内容哈希）
    pub immutable: bool,
}

/// 全部内嵌资源（含 `index.html`）。
pub static ASSETS: &[Asset] = manifest::ASSETS;

/// SPA 入口文件（服务端渲染不了路由，所有未知路径都回退到它）。
pub static INDEX_HTML: &[u8] = manifest::INDEX_HTML;

/// 按请求路径查找资源。
///
/// @intent 线性扫描而非哈希表：资源数量是个位数（入口 HTML + 一个 js + 一个 css），
///         构建一个 `HashMap` 的成本远高于扫这几次字符串比较，而且它得是
///         `LazyLock`——启动时多一次分配，换来的收益为零。
pub fn find_asset(path: &str) -> Option<&'static Asset> {
    let trimmed = path.trim_start_matches('/');
    ASSETS.iter().find(|asset| asset.path == trimmed)
}

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
    /// 角色标识（`admin` / `user`）
    pub role: String,
    /// 会话有效期（秒）
    pub expires_in_secs: u64,
}

/// 会话自检响应体（页面用它决定显示登录表单还是控制台）。
///
/// @intent `console_login_enabled` 会暴露给未登录访客。这是刻意的：页面必须知道
///         该不该渲染登录框，否则只能给一个必然失败的输入界面。它泄漏的信息
///         仅限于「这台 memora 是否已建过任何账号」，对攻击者没有实际帮助。
#[derive(Debug, Serialize)]
pub struct SessionStatus {
    /// 系统中是否存在可登录账号（无账号时登录不可用）
    pub console_login_enabled: bool,
    /// 本次请求是否已通过鉴权
    pub authenticated: bool,
    /// 已登录时的用户名
    pub user: Option<String>,
    /// 已登录时的角色标识；未登录为 `None`
    ///
    /// @intent 角色必须由服务端给出，**不能**让前端从账号名猜测：控制台的导航
    ///         （是否显示「账号」页）与若干按钮的可见性都依赖它。前端一旦自行判断，
    ///         就会出现「页面显示可点、后端 403」的错位，而那类错位最容易被误读成
    ///         后端 bug。注意它只影响**界面呈现**，真正的授权始终在服务端做。
    pub role: Option<String>,
    /// 账号总数（凭证来自库，故此处顺带给出规模，便于「为什么进不去」的判断）
    ///
    /// @intent 这与 `console_login_enabled` 是同一件事的两种精度，看似冗余，
    ///         但它们在页面上的用途不同：前者决定「是否渲染登录框」，
    ///         后者用于在「一个账号都没有」时给出**可执行**的修复指引
    ///         （`memora init` 还是手工建账号，取决于是否曾经播种过）。
    pub users: usize,
}

/// 返回控制台页面（SPA 入口）。
///
/// @intent `Cache-Control: no-store`：入口 HTML 的指纹资源名会随后端版本变化，
///         而浏览器缓存 HTML 反而会造成「已更新二进制、浏览器仍跑旧页面」的困惑，
///         这不属于值得排查的问题。指纹资源本身则走 [`asset_response`] 的
///         `immutable` 长缓存。
pub async fn page() -> Response {
    html_response(INDEX_HTML)
}

/// `GET /assets/{path}`：返回一份构建产物。
///
/// @intent 未命中时返回 **404 而不是回退入口 HTML**。回退看起来更"友好"，
///         实际会让一个写错的 js 路径拿到 `200 text/html`，浏览器随后报
///         `Unexpected token '<'`——错误信息指向 HTML 内容而不是缺失的文件。
///         静态资源就该有静态资源的语义失败。
pub async fn asset(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    match find_asset(&format!("assets/{path}")) {
        Some(asset) => asset_response(asset),
        None => {
            tracing::debug!(path = %path, "console asset not found");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// `GET /favicon.svg`：站点图标。
///
/// @intent 单独一条路由而不是让前端产物带一个二进制 ico：SVG 是纯文本、
///         体积小到可以直接写在源码里，且 `favicon` 缺失会在每个浏览器里留一条
///         404 噪声日志——正好掩盖真正需要看的那条。
pub async fn icon() -> Response {
    const FAVICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32">
<rect width="32" height="32" rx="8" fill="#4c8dff"/>
<path d="M9 22V10h3l4 6 4-6h3v12h-3v-7l-4 6-4-6v7z" fill="#fff"/>
</svg>"##;
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE.as_str(), "image/svg+xml"),
            (header::CACHE_CONTROL.as_str(), "public, max-age=86400"),
        ],
        FAVICON,
    )
        .into_response()
}

/// 以控制台入口的响应头返回一份 HTML。
///
/// @intent CSP 收紧到 `'self'`：前端产物已无任何内联脚本与内联样式
///         （Vite 把 js/css 都产出为独立文件），故 `'unsafe-inline'` 可以彻底去掉。
///         这是把控制台换成构建产物的一个附带收益——手写页面时代它必须留着
///         `'unsafe-inline'`，否则页面自身的 `<script>` 与 `<style>` 都跑不起来。
///
/// @intent `img-src 'self' data:`：允许内联 SVG 图标（`data:` 不可执行，
///         且比放开一整类外部域安全得多）。
fn html_response(body: &'static [u8]) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE.as_str(), "text/html; charset=utf-8"),
            (header::CACHE_CONTROL.as_str(), "no-store"),
            (
                header::CONTENT_SECURITY_POLICY.as_str(),
                "default-src 'none'; style-src 'self'; script-src 'self'; \
                 connect-src 'self'; img-src 'self' data:; font-src 'self'; \
                 base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            ),
            (header::X_CONTENT_TYPE_OPTIONS.as_str(), "nosniff"),
            (header::REFERRER_POLICY.as_str(), "same-origin"),
        ],
        body,
    )
        .into_response()
}

/// 返回一份静态资源。
///
/// @intent 缓存策略分两档，判据是「文件名是否含内容哈希」而不是「是不是 HTML」：
///         Vite 产出的 `assets/index-<hash>.js` 内容与名字一一对应，可以永久缓存；
///         入口 HTML 与 `favicon.ico` 这类固定名字的资源必须 `no-store`，
///         否则升级后浏览器会拿旧 HTML 去引用已不存在的旧 js，页面白屏。
pub fn asset_response(asset: &Asset) -> Response {
    let cache = if asset.immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-store"
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE.as_str(), asset.content_type),
            (header::CACHE_CONTROL.as_str(), cache),
            (header::X_CONTENT_TYPE_OPTIONS.as_str(), "nosniff"),
        ],
        asset.bytes,
    )
        .into_response()
}

/// 会话自检（公开）。
///
/// @intent 之所以公开：它是页面的启动调用，必须在登录之前就能问出「要不要显示登录框」。
///         它不返回任何项目数据，只回答「你是谁 / 要不要登录」。
///
/// @intent 账号数为 0 时 `console_login_enabled` 为 `false`，而不是「True 但登录必失败」。
///         这两种表达对页面的含义完全不同：前者可以给出修复指引，后者只能让用户
///         对着一个永远 401 的登录框反复重试。
pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let identity = authenticate_admin(&state, &headers);
    let total = match users::count(&state.meta).await {
        Ok(total) => total,
        Err(err) => return reply::internal_error(&err),
    };
    let (user, role) = match &identity {
        Some(identity) => (
            Some(identity.user().to_string()),
            Some(identity.role().as_str().to_string()),
        ),
        None => (None, None),
    };
    Json(SessionStatus {
        console_login_enabled: total > 0,
        authenticated: identity.is_some(),
        user,
        role,
        users: total,
    })
    .into_response()
}

/// 登录：校验用户名与口令，成功后签发会话 Cookie。
///
/// @intent 校验**完全**交给 [`users::authenticate`]：那里已经处理了「账号不存在时
///         也要跑一次哈希」这一时序要求（否则会形成账号枚举信道），
///         而此处若自己拼 `find` + `verify`，这条约束就会在重构中悄悄丢失。
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    let total = match users::count(&state.meta).await {
        Ok(total) => total,
        Err(err) => return reply::internal_error(&err),
    };
    if total == 0 {
        tracing::warn!(
            config_file = %state.config.config_path.display(),
            "login attempt rejected: no console user exists yet"
        );
        return reply::unauthorized_with(
            "no console account exists yet; run `memora init` to seed the initial admin \
             (it uses admin_user / admin_password_hash from the config file)",
        );
    }

    let record = match users::authenticate(&state.meta, &req.username, &req.password).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            tracing::warn!(
                user = %req.username.trim(),
                "console login failed"
            );
            return reply::unauthorized_with("invalid username or password");
        }
        Err(err) => return reply::internal_error(&err),
    };

    let id = match state.sessions.create(&record.username, record.role) {
        Ok(id) => id,
        Err(err) => return reply::internal_error(&err),
    };

    let secure = session::request_is_secure(&headers);
    let mut response_headers = HeaderMap::new();
    session::attach_cookie(
        &mut response_headers,
        &session::build_session_cookie(&id, session::SESSION_TTL, secure),
    );

    tracing::info!(
        user = %record.username,
        role = record.role.as_str(),
        "console login succeeded"
    );
    (
        StatusCode::OK,
        response_headers,
        Json(LoginResponse {
            user: record.username,
            role: record.role.as_str().to_string(),
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

    /// 内嵌资源必须真的存在、非空，且入口 HTML 引用了指纹资源。
    ///
    /// @intent `include_bytes!` 的路径错误会在编译期报错，但**内容走样**
    ///         （例如 `build.rs` 吐了一个占位页）不会。这条断言把「前端产物真的
    ///         被打包进二进制」钉住——占位页的构建也必须能跑通测试，所以这里
    ///         断言的是一份**真产物**独有的特征：`<script type="module">` 引用了
    ///         `/assets/` 下的指纹文件。
    #[test]
    fn embedded_console_is_a_real_vite_build() {
        let html = String::from_utf8_lossy(INDEX_HTML);
        assert!(html.len() > 256, "入口页面过短，疑似占位页：{html}");
        assert!(html.contains("<!DOCTYPE html") || html.contains("<!doctype html"));
        assert!(
            html.contains("/assets/"),
            "入口页面必须引用 /assets/ 下的构建产物：{html}"
        );

        let scripts: Vec<&Asset> = ASSETS
            .iter()
            .filter(|a| a.content_type.contains("javascript"))
            .collect();
        let styles: Vec<&Asset> = ASSETS
            .iter()
            .filter(|a| a.content_type.contains("css"))
            .collect();
        // @intent 不断言「js 恰好一个」：路由用 `() => import(...)` 懒加载页面，
        //         每个页面因此成为一个独立分块（当前约 9 个）。钉死数量会让
        //         「新增一个页面」变成一次测试失败，而它并不是错误。
        //         真正需要钉住的是「入口引用的 js/css 都在清单里」与
        //         「css 只有一份」——后者是因为 Vite 的样式产物不会分块，
        //         出现两份通常意味着 `@theme` 被某处重复引入。
        assert!(!scripts.is_empty(), "应当至少内嵌一个 js 产物");
        assert_eq!(styles.len(), 1, "应当恰好内嵌一个 css 产物");
        assert!(
            scripts.iter().any(|a| a.path == "assets/index.js"),
            "入口分块缺失，清单：{:?}",
            ASSETS.iter().map(|a| a.path).collect::<Vec<_>>()
        );

        for asset in ASSETS {
            assert!(!asset.bytes.is_empty(), "资源为空：{}", asset.path);
            assert!(
                !asset.path.starts_with('/'),
                "清单里的路径不得带前导斜杠：{}",
                asset.path
            );
        }
    }

    /// 入口 HTML 引用的每个 `/assets/...` 都必须在清单里能找到。
    ///
    /// @intent 这是**无浏览器**条件下最有价值的一条断言：`build.rs` 若只收集了
    ///         一部分文件（例如正则漏掉了 `.css`），页面会在浏览器里静默丢掉样式，
    ///         而服务端日志毫无异常、HTTP 用例也全绿。这类缺陷只能靠比对清单钉住。
    #[test]
    fn every_asset_referenced_by_the_entry_page_is_embedded() {
        let html = String::from_utf8_lossy(INDEX_HTML);
        let mut missing = Vec::new();
        let mut rest = html.as_ref();
        while let Some(at) = rest.find("/assets/") {
            rest = &rest[at..];
            let end = rest
                .find(|c: char| c == '"' || c == '\'' || c == '>' || c.is_whitespace())
                .unwrap_or(rest.len());
            let reference = &rest[..end];
            if find_asset(reference).is_none() {
                missing.push(reference.to_string());
            }
            rest = &rest[end..];
        }
        assert!(
            missing.is_empty(),
            "入口页面引用了未内嵌的资源：{missing:?}（清单：{:?}）",
            ASSETS.iter().map(|a| a.path).collect::<Vec<_>>()
        );
    }

    /// 指纹资源必须长缓存，入口页面必须不缓存。
    ///
    /// @intent 两者写反的后果不对称且都很隐蔽：指纹资源 `no-store` 只是浪费带宽，
    ///         而入口 HTML 被缓存会让**升级后白屏**——旧 HTML 指向已不存在的旧 js。
    #[test]
    fn cache_policy_follows_content_addressing() {
        let html = Asset {
            path: "index.html",
            bytes: b"x",
            content_type: "text/html",
            immutable: false,
        };
        let js = Asset {
            path: "assets/index-abc.js",
            bytes: b"x",
            content_type: "application/javascript",
            immutable: true,
        };
        // @intent 这里只断言「空路径不命中」与「前导斜杠会被归一化」，
        //         不断言 `index.html` 能被 `find_asset` 命中——入口页面走的是
        //         独立的 `INDEX_HTML` 常量与 `page` handler，**不在** `ASSETS`
        //         清单里（见 `build.rs` 里 `if relative == ENTRY { continue }`）。
        //         断言它会命中，等于要求清单里存两份入口页面。
        assert!(find_asset("").is_none(), "空路径不得命中任何资源");
        assert!(
            find_asset("assets/index.js").is_some(),
            "入口分块应在清单中，清单：{:?}",
            ASSETS.iter().map(|a| a.path).collect::<Vec<_>>()
        );
        assert!(
            find_asset("/assets/index.js").is_some(),
            "带前导斜杠也应命中（服务端需与浏览器给出的路径形态无关）"
        );
        assert!(
            find_asset("assets/missing.js").is_none(),
            "清单外的资源必须落空，交由 handler 返回 404"
        );

        // 通过响应头验证策略（不进 axum 状态机，直接看构造结果）
        let js_resp = asset_response(&js);
        let policy = js_resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(policy.contains("immutable"), "指纹资源应可永久缓存：{policy}");

        let html_resp = html_response(html.bytes);
        let policy = html_resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert_eq!(policy, "no-store", "入口 HTML 不得被缓存");

        let csp = html_resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            !csp.contains("unsafe-inline"),
            "构建产物不应再需要 unsafe-inline：{csp}"
        );
    }
}