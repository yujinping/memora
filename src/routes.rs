//! 路由装配与 HTTP handler。
//!
//! @author yujinping
//! @intent P3：`/mcp` 由统计占位替换为真实的 MCP Streamable HTTP 服务。
//!         装配顺序刻意保持「鉴权中间件在协议层之前」——未授权请求在进入 MCP 状态机
//!         之前就被拒掉，协议层因此可以完全无状态，不必为每个请求维护身份。
//!         handler 依旧只从请求扩展取 `ProjectRepos`（trait 对象），不引用任何具体后端。
//!         P4：`/api/v1` 补齐项目生命周期（增 / 删 / 统计），handler 实现在 `admin` 模块。

use crate::admin;
use crate::auth::{admin_auth, bearer_auth, AdminIdentity};
use crate::console;
use crate::mcp;
use crate::meta;
use crate::reply::internal_error;
use crate::state::AppState;
use axum::extract::{Extension, State};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use tower_http::limit::RequestBodyLimitLayer;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};

/// 健康检查（无需鉴权）。
async fn health() -> &'static str {
    "ok"
}

/// 项目列表：管理员见全部，普通账号只见自己名下的。
///
/// @intent 列表是**唯一**会因角色不同而形状不同的项目端点（单个项目的访问
///         由 `admin::visible_project` 统一判权）。这里必须显式分支：
///         若一律全量，租户会在「项目」页看到别人的项目名；若一律按 owner 过滤，
///         管理员就失去了「管理所有数据」的能力——而那是他的全部职责。
///
/// @intent 过滤下推到 SQL（`list_projects_of`）而非取全量后在内存里筛：
///         后者的功能结果相同，但「别人的数据压根没有被读出来」与
///         「读出来又丢掉了」是两种可信度完全不同的保证。
async fn list_projects(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
) -> Response {
    let result = if identity.is_admin() {
        meta::list_projects(&state.meta).await
    } else {
        meta::list_projects_of(&state.meta, identity.user()).await
    };

    match result {
        Ok(projects) => Json(projects).into_response(),
        Err(err) => internal_error(&err),
    }
}

/// HTTP 请求体上限（字节）。与 `normalize` 的逐字段上限共同构成「边缘 + 语义」双层防护：
/// 边缘先拦住明显超规的巨请求，语义层再校验字段长度，避免单 token 持有者撑爆内存 / 磁盘。
pub const MAX_REQUEST_BODY_BYTES: usize = 1_048_576;

/// 装配路由：
/// - `/health` 公开；
/// - `/admin` 公开：控制台 SPA 入口，不含任何数据，登录由页面自行发起；
/// - `/assets/*` 公开：控制台的构建产物（js / css），同样不含任何数据；
///   ⚠ 它们看起来「只是一堆静态文件」，但**不是**公开前缀之外的东西：
///   漏掉这一条的表现是「页面能打开、样式与脚本全 404」，而 HTML 本身 200，
///   排查时极易误判为前端构建问题。
/// - `/mcp` 为 MCP Streamable HTTP 端点，需 **Bearer 项目 token**；
/// - `/api/v1/session` 登录 / 登出 / 自检（登录必须公开，故单独成群、不挂鉴权中间件）；
/// - `/api/v1` 其余端点需管理凭据：`ADMIN_TOKEN` Bearer，或控制台会话 Cookie。
///
/// ⚠ 新增**公开前缀**时必须同步 `deploy/Caddyfile`（公网部署的反代白名单），
///   否则会出现「直连 localhost 正常、经 Caddy 反代 404」——两边各自看都没错。
///   这条约束由 `daemon::tests::reverse_proxy_template_covers_every_public_prefix` 守着。
///
/// 鉴权中间件通过 `from_fn_with_state` 直接持有 `AppState`，使整棵路由树状态一致（Router<()>），
/// 避免 Axum 对各子路由 `State<S>` 的推断冲突。MCP 服务作为嵌套服务挂载，
/// 中间件先于它执行并把 `ProjectRepos` 写入请求扩展，rmcp 再把该扩展透传给工具。
/// 管理面 handler 以 `State<AppState>` 取依赖（先 `route_layer` 挂鉴权，再 `with_state`
/// 落到 `Router<()>`），与 MCP 侧 `Router<()>` 的形态保持一致。
///
/// @intent 两套凭据的边界刻意划在路由分组上，而不是在 handler 里判断：
///         `/mcp` 只挂 `bearer_auth`（项目 token），`/api/v1` 只挂 `admin_auth`
///         （管理凭据）。会话 Cookie **不参与** `/mcp` 鉴权，项目 token 也**不是**
///         管理凭据——这条边界有专门的用例守着，防止将来「顺手复用中间件」把它抹掉。
///
/// @intent `/api/v1/session` 必须与其余管理端点分开注册：登录接口本身不可能先鉴权，
///         否则永远无法登录。若把它并进同一个 `route_layer`，`POST /session` 会
///         在进入 handler 前就被 401 拒掉——这是一个只在「第一次用浏览器」时才暴露的
///         装配错误，故有对应用例钉住。
/// @intent 控制台路由的兜底顺序（`/admin` → `/assets/*` → 其余 `/*path` 回退到入口 HTML）
///         是 SPA 的必要条件：Vue Router 使用 history 模式，用户刷新 `/admin/projects/foo`
///         时浏览器会真的请求那个路径，而服务端只有 `index.html` 一个页面。
///         兜底必须**排在 `/mcp`、`/api/v1`、`/assets` 之后**，否则一次手滑就会把
///         MCP 的 POST 吞进 HTML 里——那种缺陷在浏览器里看不出来，只在客户端 404。
pub fn create_app(state: AppState) -> Router {
    // MCP 端点：Bearer 鉴权 → 注入 ProjectRepos → 交给 rmcp 协议层
    let mcp_routes = Router::new()
        .nest_service("/mcp", mcp::build_service(&state.config))
        .route_layer(middleware::from_fn_with_state(state.clone(), bearer_auth));

    // 会话端点：登录必须公开；登出与自检的判定在 handler 内部完成（见 console 模块）
    let session_routes = Router::new()
        .route(
            "/session",
            get(console::status)
                .post(console::login)
                .delete(console::logout),
        )
        .with_state(state.clone());

    // 管理路由：ADMIN_TOKEN 或控制台会话。身份由 `admin_auth` 写入请求扩展，
    // handler 用 `Extension<AdminIdentity>` 取出后自行判权（见 `admin` 模块文档）。
    let api_routes = Router::new()
        .route("/projects", get(list_projects).post(admin::create_project))
        .route("/projects/{id}", delete(admin::delete_project))
        .route("/projects/{id}/stats", get(admin::project_stats))
        .route("/projects/{id}/graph", get(admin::project_graph))
        // token 轮换（P7）：旧 token 立即失效，明文只在新响应里出现一次
        .route(
            "/projects/{id}/token",
            post(admin::rotate_project_token),
        )
        // 账号管理（P7，P8 起仅管理员）：登录名是路径段，故含非法字符时由 handler 自行判定
        .route(
            "/users",
            get(admin::list_users).post(admin::create_user),
        )
        .route(
            "/users/{username}",
            delete(admin::delete_user).put(admin::update_user_password),
        )
        // 角色变更（P8，仅管理员）：与改口令分开注册，因为两者的授权规则不同
        // （改自己的口令人人可为，改角色只有管理员可为），合在一个 handler 里
        // 判权会让「只传了半个字段」的请求变得含糊。
        .route(
            "/users/{username}/role",
            put(admin::update_user_role),
        )
        // 交接（P4.3）：静态段 `pending` 必须先于 `{task_id}` 注册（matchit 对静态段
        // 优先，但先注册是防退化的自文档写法），并有专门用例钉住路由遮蔽。
        .route(
            "/projects/{id}/handoffs",
            post(admin::handoff_save).get(admin::handoff_list),
        )
        .route(
            "/projects/{id}/handoffs/pending",
            get(admin::handoff_pending),
        )
        .route(
            "/projects/{id}/handoffs/pending/ack",
            post(admin::handoff_ack_pending),
        )
        .route(
            "/projects/{id}/handoffs/{task_id}",
            get(admin::handoff_get).delete(admin::handoff_delete),
        )
        .route(
            "/projects/{id}/handoffs/{task_id}/ack",
            post(admin::handoff_ack),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), admin_auth))
        .with_state(state.clone());

    // 控制台静态资源：命中清单直接吐字节，未命中一律 404（不回退 HTML——
    // 否则一个写错的 js 路径会拿到 200 的 HTML，浏览器报语法错误，极难定位）
    let asset_routes = Router::new()
        .route("/assets/{*path}", get(console::asset))
        .route("/favicon.svg", get(console::icon));

    // 公开端点：健康检查、控制台入口、SPA 路由回退
    Router::new()
        .route("/health", get(health))
        .route("/admin", get(console::page))
        .route("/admin/{*path}", get(console::page))
        .merge(asset_routes)
        .merge(mcp_routes)
        .nest("/api/v1", session_routes.merge(api_routes))
        // 边缘 body 上限：先拦住明显超规的巨请求，再交给语义层（normalize）做字段级校验。
        // 放在最外层，使 /mcp 嵌套服务与 /api/v1 都受同一上限约束。
        // 用 `BodyLimitLayer` 而非 `DefaultBodyLimit`：后者只给 axum 自有提取器设默认上限，
        // 不会限制 rmcp 嵌套服务读取的原始 body（审计发现的盲区）。
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::EntityInput;
    use crate::meta::{create_project, init_meta_db, NewProject};
    use crate::storage::{SqliteFileBackend, StorageBackend, StorageRegistry};
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};
    use sea_orm::Database;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tower::ServiceExt;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 测试用控制台用户名。
    const CONSOLE_USER: &str = "console-admin";
    /// 测试用控制台口令（明文只出现在测试里；生产路径下它从不落盘）。
    const CONSOLE_PASSWORD: &str = "test-console-password";

    /// 控制台口令的哈希。
    ///
    /// @intent 只算一次并缓存：argon2 默认参数单次约数十毫秒，若每个用例现算会把
    ///         整套测试拖慢到难以接受。哈希本身与用例无关，缓存它不损失任何覆盖。
    fn console_password_hash() -> String {
        static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        HASH.get_or_init(|| crate::password::hash_password(CONSOLE_PASSWORD).unwrap())
            .clone()
    }

    /// 隔离的临时数据目录。
    fn temp_data_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("memora_p2_app_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 构建测试应用：临时元库 + 三个项目（两种可用后端 + 一个未注册后端）。
    ///
    /// 返回 (路由, 数据目录)，数据目录供测试直接落数据以验证隔离。
    async fn setup() -> (Router, PathBuf) {
        setup_with_console(false).await
    }

    /// 同上，但可播种一份控制台账号。
    ///
    /// @intent 控制台登录的真相源是 `users` 表，不是配置项——配置里的
    ///         `admin_user` / `admin_password_hash` 只在 `memora init` 时充当
    ///         **种子**。故此处直接往库里写账号，而不是改 `config`：
    ///         后者在改完之后「配置有口令但表是空的」，登录会一律 401，
    ///         测试会以「口令错误」的形式误报。
    ///
    /// @intent 口令哈希由 `console_password_hash` 缓存共享——argon2 默认参数单次约需
    ///         数十毫秒，每个用例现算会把整套测试拖慢到难以接受。
    async fn setup_with_console(with_console: bool) -> (Router, PathBuf) {
        let data_dir = temp_data_dir();
        let meta_path = data_dir.join("_meta.db");
        let url = format!("sqlite://{}?mode=rwc", meta_path.display());
        let db = Database::connect(&url).await.unwrap();
        init_meta_db(&db).await.unwrap();

        for (id, token, backend) in [
            ("demo", "secret", "sqlite_file"),
            ("memproj", "mem-token", "in_mem"),
            ("broken", "broken-token", "postgres"),
        ] {
            create_project(
                &db,
                &NewProject {
                    project_id: id.to_string(),
                    token: token.to_string(),
                    db_path: format!("{id}/mem.db"),
                    backend: backend.to_string(),
                    // @intent 归属留空 = 迁移前遗留的无主项目。用这个值而不是
                    //         某个具体用户名，是因为它让**每一条既有用例**都顺带
                    //         验证了「无主项目对管理员仍然可见」——升级后数据
                    //         像是丢了，是这类迁移最常见的故障形态。
                    owner: String::new(),
                },
            )
            .await
            .unwrap();
        }

        let mut config = Config::for_test(0, &data_dir.to_string_lossy(), "admin", "sqlite_file");
        // 注册表只依赖 `data_dir` 与默认后端，故趁控制台字段尚未改动时先由它构造，
        // 免得多造一份配置（两份配置容易在后续改动中悄悄分叉）。
        let storage = StorageRegistry::from_config(&config).unwrap();
        if with_console {
            // @intent 只改 `admin_user`（供 `status` 在 Bearer 登录时回报身份），
            //         口令则写进 `users` 表——即生产路径下 `memora init` 所做的那一步。
            config.admin_user = CONSOLE_USER.to_string();
            crate::users::create_with_role(
                &db,
                CONSOLE_USER,
                &console_password_hash(),
                crate::users::Role::Admin,
            )
            .await
            .unwrap();
        }

        let state = AppState::new(db, config, storage);
        (create_app(state), data_dir)
    }

    /// 读取响应体并尽量按 JSON 解析；非 JSON（如 403 的纯文本）返回 Null。
    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    /// 发起一次 MCP Streamable HTTP 请求。
    ///
    /// `Host` 必须显式给出：rmcp 的传输强制校验 Host（防 DNS 重绑定），而 `oneshot`
    /// 不经过 hyper，没有从 URI 合成 Host 的机会。默认用白名单内的 `localhost`。
    async fn mcp_send(
        app: Router,
        token: Option<&str>,
        host: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = body.map(|v| v.to_string()).unwrap_or_default();
        let resp = app
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    /// 以合法 Host 发起一次 MCP JSON-RPC 调用。
    async fn rpc(
        app: Router,
        token: Option<&str>,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        mcp_send(app, token, "localhost", Some(body)).await
    }

    /// 构造 `tools/call` 请求体。
    fn tool_call(id: i64, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        })
    }

    /// 构造 `method` 请求体（无参方法的 params 统一给 `{}`）。
    fn request(id: i64, method: &str, params: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    /// 取 `tools/call` 结果里的结构化内容（rmcp 的 `Json<T>` 出参）。
    fn structured(body: &serde_json::Value) -> serde_json::Value {
        body["result"]["structuredContent"].clone()
    }

    async fn get_with_token(
        app: Router,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder().uri(uri);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let resp = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    #[tokio::test]
    async fn health_is_public_and_ok() {
        let (app, _dir) = setup().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// 请求体超过上限必须在边缘被拒（413），不得进入协议层或落到 normalize 校验。
    ///
    /// @intent S2 审计：此前 `/mcp` 等端点无 body 上限，单 token 持有者可借此拖垮服务端。
    ///         本用例钉死「巨 body 在鉴权之后、解析之前被 413 拦下」。显式带 `Content-Length`
    ///         ——边缘层据此在调用 rmcp 之前即短路返回 413，而非让协议层读爆后兜底成 500。
    #[tokio::test]
    async fn oversized_request_body_is_rejected_with_413() {
        let (app, _dir) = setup().await;
        // 1.5 MiB 的合法 JSON-RPC 体外壳，远超 MAX_REQUEST_BODY_BYTES
        let big = "a".repeat(MAX_REQUEST_BODY_BYTES + 512 * 1024);
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": { "junk": big }
        })
        .to_string();
        let len = body.len();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "localhost")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header(header::CONTENT_LENGTH, len)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "超限 body 必须 413"
        );
    }

    /// 未授权请求必须在进入 MCP 协议层之前被拒（无 token 时连 Host 校验都不该走到）。
    #[tokio::test]
    async fn mcp_without_token_is_401() {
        let (app, _dir) = setup().await;
        let (status, _) = mcp_send(
            app,
            None,
            "localhost",
            Some(request(1, "tools/list", serde_json::json!({}))),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mcp_with_malformed_auth_header_is_401() {
        let (app, _dir) = setup().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::AUTHORIZATION, "Basic secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mcp_with_invalid_token_is_401() {
        let (app, _dir) = setup().await;
        let (status, _) = rpc(
            app,
            Some("wrong"),
            request(1, "tools/list", serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_without_token_is_401() {
        let (app, _dir) = setup().await;
        let (status, _) = get_with_token(app, "/api/v1/projects", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_with_valid_token_lists_projects_sorted() {
        let (app, _dir) = setup().await;
        let (status, body) = get_with_token(app, "/api/v1/projects", Some("admin")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["project_id"], "broken");
        assert_eq!(body[1]["project_id"], "demo");
        assert_eq!(body[2]["project_id"], "memproj");
        assert_eq!(body[2]["backend"], "in_mem");
    }

    /// 未注册的后端（本构建未编译 Postgres）应报 500，而非 401 或空结果。
    #[tokio::test]
    async fn mcp_with_unregistered_backend_is_500() {
        let (app, _dir) = setup().await;
        let (status, body) = rpc(
            app,
            Some("broken-token"),
            request(1, "tools/list", serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"], "internal_error");
    }

    /// initialize 握手：证明 Bearer 之后请求确实进入了 MCP 协议层，且服务自报身份。
    #[tokio::test]
    async fn mcp_initialize_reports_server_identity() {
        let (app, _dir) = setup().await;
        let (status, body) = rpc(
            app,
            Some("secret"),
            request(
                1,
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "memora-tests", "version": "1" }
                }),
            ),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        assert_eq!(body["result"]["serverInfo"]["name"], "memora");
        assert_eq!(
            body["result"]["capabilities"]["tools"],
            serde_json::json!({})
        );
    }

    /// `tools/list` 暴露的工具必须与文档承诺的一致（9 记忆 + 4 交接 = 13）。
    #[tokio::test]
    async fn mcp_tools_list_exposes_the_declared_tools() {
        let (app, _dir) = setup().await;
        let (status, body) = rpc(
            app,
            Some("secret"),
            request(2, "tools/list", serde_json::json!({})),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        let mut names: Vec<String> = body["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("tools 不是数组：{body}"))
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        let mut expected: Vec<String> = mcp::tools::TOOL_NAMES
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            names.len(),
            expected.len(),
            "工具数应与 TOOL_NAMES 声明一致（条数只在此处对表，不重复硬编码），响应体：{body}"
        );
        names.sort();
        expected.sort();
        assert_eq!(names, expected);
    }

    /// 端到端写读：`tools/call` 写入的记忆能被后续调用读到，关系回读带 id。
    #[tokio::test]
    async fn mcp_tool_call_writes_and_reads_project_memory() {
        let (app, _dir) = setup().await;

        let (status, created) = rpc(
            app.clone(),
            Some("secret"),
            tool_call(
                3,
                "create_entities",
                serde_json::json!({
                    "entities": [
                        { "name": "alpha", "entity_type": "person" },
                        { "name": "beta", "entity_type": "person" }
                    ]
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{created}");
        assert_eq!(structured(&created)["entities"][0]["name"], "alpha");
        assert_eq!(structured(&created)["entities"][1]["entity_type"], "person");
        // `Json<T>` 同时给出 text 与 structuredContent，兼顾只看 content 的老客户端
        assert!(
            !created["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "必须同时带 text 内容：{created}"
        );

        let (_, relations) = rpc(
            app.clone(),
            Some("secret"),
            tool_call(
                4,
                "create_relations",
                serde_json::json!({
                    "relations": [
                        { "from_name": "alpha", "to_name": "beta", "relation_type": "knows" }
                    ]
                }),
            ),
        )
        .await;
        let relation_id = structured(&relations)["relations"][0]["id"]
            .as_i64()
            .unwrap_or_else(|| panic!("关系未回读 id：{relations}"));
        assert!(relation_id > 0);

        let (_, graph) = rpc(
            app,
            Some("secret"),
            tool_call(5, "read_graph", serde_json::json!({})),
        )
        .await;
        let graph = structured(&graph);
        assert_eq!(graph["entities"].as_array().unwrap().len(), 2);
        assert_eq!(graph["relations"].as_array().unwrap().len(), 1);
    }

    /// 追加观测后能拿到观测 id，并可据此删除——这是记忆「可修订」的最小闭环。
    #[tokio::test]
    async fn mcp_observation_lifecycle_over_the_protocol() {
        let (app, _dir) = setup().await;

        rpc(
            app.clone(),
            Some("secret"),
            tool_call(
                1,
                "create_entities",
                serde_json::json!({ "entities": [{ "name": "alpha" }] }),
            ),
        )
        .await;

        let (_, added) = rpc(
            app.clone(),
            Some("secret"),
            tool_call(
                2,
                "add_observations",
                serde_json::json!({ "observations": [{ "entity_name": "alpha", "contents": ["偏好深色主题"] }] }),
            ),
        )
        .await;
        let obs = &structured(&added)["entities"][0]["observations"][0];
        let obs_id = obs["id"]
            .as_i64()
            .unwrap_or_else(|| panic!("未回读观测 id：{added}"));
        assert_eq!(obs["content"], "偏好深色主题");
        assert_eq!(
            structured(&added)["entities"][0]["entity_type"],
            "unknown",
            "省略 entity_type 时应规范化为 unknown"
        );

        let (_, searched) = rpc(
            app.clone(),
            Some("secret"),
            tool_call(3, "search_nodes", serde_json::json!({ "query": "深色" })),
        )
        .await;
        assert_eq!(structured(&searched)["entities"][0]["name"], "alpha");

        let (_, deleted) = rpc(
            app,
            Some("secret"),
            tool_call(
                4,
                "delete_observations",
                serde_json::json!({ "ids": [obs_id] }),
            ),
        )
        .await;
        assert_eq!(structured(&deleted)["ids"][0], obs_id);
    }

    /// 工具层错误需原样透出为 JSON-RPC 错误码，而不是被吞成 HTTP 500。
    #[tokio::test]
    async fn mcp_tool_error_surfaces_as_jsonrpc_error() {
        let (app, _dir) = setup().await;
        let (status, body) = rpc(
            app,
            Some("secret"),
            tool_call(
                1,
                "add_observations",
                serde_json::json!({ "observations": [{ "entity_name": "ghost", "contents": ["x"] }] }),
            ),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        assert_eq!(body["error"]["code"], -32602, "响应体：{body}");
        assert_eq!(body["id"], 1);
    }

    /// B：字段名写错必须在**调用当时**被拒并点名出错字段（模型据此可自我修正）。
    ///
    /// @intent 实测形状值得写下来：参数**反序列化**失败由 rmcp 报成 `result.isError = true`
    ///         + 文本消息，**不是** JSON-RPC `error` 对象；而工具语义层错误（如实体不存在）
    ///         返回的是 `error.code = -32602`。两者形状不同、都会让调用方看到原因，
    ///         但断言必须按各自的实际形状写——把两者混为一谈，钉住的就是一个不存在的契约。
    #[tokio::test]
    async fn mcp_rejects_unknown_arguments_instead_of_dropping_them() {
        let (app, _dir) = setup().await;
        let (status, body) = rpc(
            app,
            Some("secret"),
            tool_call(
                1,
                "create_entities",
                serde_json::json!({
                    "entities": [{ "name": "alpha", "observation": ["字段名写成了单数"] }]
                }),
            ),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        assert_eq!(
            body["result"]["isError"], true,
            "反序列化失败必须被报成工具错误，实际：{body}"
        );
        let text = body["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            text.contains("unknown field"),
            "错误应点明是「未知字段」，实际：{text}"
        );
        assert!(
            text.contains("observation"),
            "错误应点名出错字段，便于调用方自我修正，实际：{text}"
        );
    }

    /// A：协议层走一遍「一次调用建实体并附观测」，并**回读**确认真的落库。
    #[tokio::test]
    async fn mcp_create_entities_persists_the_observations_it_carries() {
        let (app, _dir) = setup().await;
        let (status, body) = rpc(
            app.clone(),
            Some("secret"),
            tool_call(
                1,
                "create_entities",
                serde_json::json!({
                    "entities": [{
                        "name": "alpha",
                        "entityType": "person",
                        "observations": ["事实一", "事实二"]
                    }]
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        assert_eq!(
            structured(&body)["entities"][0]["observations"][0]["content"],
            "事实一",
            "响应体：{body}"
        );

        let (_, graph) = rpc(
            app,
            Some("secret"),
            tool_call(2, "read_graph", serde_json::json!({})),
        )
        .await;
        let obs = structured(&graph)["entities"][0]["observations"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(obs.len(), 2, "回读应看到 2 条观测，实际：{graph}");
    }

    /// 注入链路端到端：绕过 HTTP 直写项目库的数据，能被 MCP 工具读到。
    #[tokio::test]
    async fn mcp_reads_data_written_directly_to_the_project_store() {
        let (app, dir) = setup().await;

        let backend = SqliteFileBackend::new(dir);
        let repos = backend.repositories_for("demo").await.unwrap();
        repos
            .memory
            .create_entities(vec![
                EntityInput::new("alpha", "person"),
                EntityInput::new("beta", "person"),
            ])
            .await
            .unwrap();
        repos
            .memory
            .create_relations(vec![crate::domain::RelationInput::new(
                "alpha", "beta", "knows",
            )])
            .await
            .unwrap();

        let (status, body) = rpc(
            app,
            Some("secret"),
            tool_call(1, "read_graph", serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        let graph = structured(&body);
        assert_eq!(graph["entities"].as_array().unwrap().len(), 2);
        assert_eq!(graph["relations"].as_array().unwrap().len(), 1);
    }

    /// 项目硬隔离：一个项目的数据不得出现在另一个项目的工具响应里。
    #[tokio::test]
    async fn projects_are_isolated_over_the_protocol() {
        let (app, dir) = setup().await;

        let backend = SqliteFileBackend::new(dir);
        backend
            .repositories_for("demo")
            .await
            .unwrap()
            .memory
            .create_entities(vec![EntityInput::new("only-demo", "person")])
            .await
            .unwrap();

        let (_, demo) = rpc(
            app.clone(),
            Some("secret"),
            tool_call(1, "read_graph", serde_json::json!({})),
        )
        .await;
        let (_, other) = rpc(
            app,
            Some("mem-token"),
            tool_call(2, "read_graph", serde_json::json!({})),
        )
        .await;

        assert_eq!(structured(&demo)["entities"].as_array().unwrap().len(), 1);
        assert!(
            structured(&other)["entities"]
                .as_array()
                .unwrap()
                .is_empty(),
            "另一项目不得看到 demo 的数据"
        );
    }

    /// DNS 重绑定防护：Host 不在白名单内必须被协议层拒绝——证明 `allowed_hosts` 真实生效。
    #[tokio::test]
    async fn mcp_rejects_disallowed_host() {
        let (app, _dir) = setup().await;
        let (status, _) = mcp_send(
            app,
            Some("secret"),
            "evil.example.com",
            Some(request(1, "tools/list", serde_json::json!({}))),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // ------------------------------------------------------------ 管理面（P4）

    /// 以 JSON 体发起管理接口请求（`ADMIN_TOKEN` 走 Authorization）。
    async fn send_json(
        app: Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let resp = app
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    /// 以 DELETE 方法发起管理接口请求。
    async fn delete_with_token(
        app: Router,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        send_json(app, "DELETE", uri, token, serde_json::json!({})).await
    }

    /// 字节序列是否包含某段模式（用于断言明文 token 未落库）。
    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// 元库全部相关文件（主库 + WAL / journal 附属）拼接后的字节流。
    ///
    /// @intent 只扫主库会漏掉尚未 checkpoint 的 WAL 内容，使「明文未落库」的断言
    ///         变成永真的空断言。
    fn meta_db_bytes(dir: &std::path::Path) -> Vec<u8> {
        let mut all = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name.starts_with("_meta.db") {
                all.extend(std::fs::read(&path).unwrap_or_default());
            }
        }
        all
    }

    /// 创建项目：返回可用的明文 token，且该 token 立刻能通过 MCP 鉴权。
    #[tokio::test]
    async fn admin_creates_project_and_the_token_works_on_mcp() {
        let (app, dir) = setup().await;
        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some("admin"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "响应体：{body}");

        let id = body["project_id"].as_str().unwrap().to_string();
        let token = body["token"].as_str().unwrap().to_string();
        assert!(id.starts_with("p_"), "自动生成的 id 应带可识别前缀：{id}");
        assert_eq!(token.len(), 64, "token 应为 256 bit 的十六进制串");
        assert_eq!(
            body["backend"], "sqlite_file",
            "省略后端时应取 STORAGE_BACKEND"
        );

        // 关键：签发的是「已登记的有效凭据」，而非一段随机串
        let (status, body) = rpc(
            app.clone(),
            Some(&token),
            request(
                1,
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "memora-tests", "version": "1" }
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "新 token 应立即可用：{body}");

        // 明文 token 绝不落库：库内只应有 SHA-256 摘要
        let raw = meta_db_bytes(&dir);
        assert!(!raw.is_empty(), "前置：元库应已写入（否则本断言是空断言）");
        assert!(
            !contains_bytes(&raw, token.as_bytes()),
            "明文 token 不得出现在元库任何文件中"
        );

        let (_, list) = get_with_token(app, "/api/v1/projects", Some("admin")).await;
        assert!(
            list.as_array()
                .unwrap()
                .iter()
                .any(|p| p["project_id"] == serde_json::json!(id)),
            "新项目应出现在列表中：{list}"
        );
    }

    /// 显式指定 project_id 与 backend。
    #[tokio::test]
    async fn admin_create_project_honours_explicit_id_and_backend() {
        let (app, _dir) = setup().await;
        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some("admin"),
            serde_json::json!({ "project_id": "custom", "backend": "in_mem" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "响应体：{body}");
        assert_eq!(body["project_id"], "custom");
        assert_eq!(body["backend"], "in_mem");

        let (status, body) =
            get_with_token(app, "/api/v1/projects/custom/stats", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        assert_eq!(body["backend"], "in_mem");
    }

    /// P6 端到端：sqlite_single 项目从建项目到 MCP 记忆与交接全链路可用。
    ///
    /// @intent 后端契约（bc_01–bc_07）钉的是存储层语义；本用例钉的是**协议链路**——
    ///         单库项目经 REST 创建、经 MCP 读写记忆、走完零 ID 交接三步、
    ///         stats 反映交接计数，共享库落在 DATA_DIR 下。
    #[tokio::test]
    async fn sqlite_single_project_serves_memory_and_handoffs_end_to_end() {
        let (app, dir) = setup().await;
        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some("admin"),
            serde_json::json!({ "project_id": "single-e2e", "backend": "sqlite_single" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "响应体：{body}");
        assert_eq!(body["backend"], "sqlite_single");
        let token = body["token"]
            .as_str()
            .expect("201 必须携带一次明文 token")
            .to_string();

        // 项目列表应反映单库形态的登记值（共享文件名，而非项目内路径）
        let (status, list) = get_with_token(app.clone(), "/api/v1/projects", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "响应体：{list}");
        let record = list
            .as_array()
            .and_then(|a| a.iter().find(|p| p["project_id"] == "single-e2e"))
            .expect("列表必须包含新项目");
        assert_eq!(record["db_path"], "mem-single.db");

        // MCP 交接链路：零 ID save → 零参数 load → ack
        let (status, resp) = rpc(
            app.clone(),
            Some(&token),
            tool_call(
                1,
                "handoff_save",
                serde_json::json!({
                    "goal": "P6 单库验收", "state": "端到端进行中",
                    "client": "workbuddy", "sessionId": "P6"
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{resp}");
        assert_eq!(structured(&resp)["task_id"], "current@workbuddy");

        let (status, resp) = rpc(
            app.clone(),
            Some(&token),
            tool_call(2, "handoff_load", serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{resp}");
        assert_eq!(structured(&resp)["status"], "found");

        let (status, resp) = rpc(
            app.clone(),
            Some(&token),
            tool_call(3, "handoff_ack", serde_json::json!({ "client": "zed" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{resp}");
        assert_eq!(structured(&resp)["won"], true);

        // MCP 记忆：写入 + 检索命中（FTS / 子串回退在单库上同样可用）
        let (status, resp) = rpc(
            app.clone(),
            Some(&token),
            tool_call(
                4,
                "create_entities",
                serde_json::json!({
                    "entities": [
                        { "name": "p6-note", "entityType": "note", "observations": ["单库后端端到端"] }
                    ]
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{resp}");

        let (status, resp) = rpc(
            app.clone(),
            Some(&token),
            tool_call(5, "search_nodes", serde_json::json!({ "query": "端到端" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{resp}");
        assert_eq!(structured(&resp)["entities"][0]["name"], "p6-note");

        // stats 的交接计数：save 一条、ack 不减计数
        let (status, body) =
            get_with_token(app, "/api/v1/projects/single-e2e/stats", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        assert_eq!(body["handoffs"], 1);

        // 共享库必须落在 DATA_DIR 下
        assert!(
            dir.join("mem-single.db").exists(),
            "共享库必须落在 DATA_DIR 下"
        );
    }

    /// P5.1 端到端：会话三工具经 MCP 协议层跑通写入 → 拉取 → 列表全链路。
    ///
    /// @intent 工具单测钉的是语义；本用例钉的是**协议面**——camelCase 别名
    ///         （`sessionId`）、入参 schema、`isError` 语义错误、以及
    ///         「轮次序号由服务端分配并回读」这条最容易被客户端违反的约定。
    #[tokio::test]
    async fn mcp_session_tools_round_trip_over_the_protocol() {
        let (app, _dir) = setup().await;
        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some("admin"),
            serde_json::json!({ "project_id": "session-e2e" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "响应体：{body}");
        let token = body["token"]
            .as_str()
            .expect("201 必须携带一次明文 token")
            .to_string();

        // 写三轮（含 camelCase 入参与 assistant / system 角色）
        let mut last_index = -1_i64;
        for (i, (role, content)) in [("user", "问题"), ("assistant", "回答"), ("system", "提示")]
            .into_iter()
            .enumerate()
        {
            let (status, resp) = rpc(
                app.clone(),
                Some(&token),
                tool_call(
                    i as i64 + 1,
                    "log_turn",
                    serde_json::json!({
                        "sessionId": "resume-main",
                        "role": role,
                        "content": content
                    }),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "响应体：{resp}");
            let view = structured(&resp);
            assert_eq!(view["session_id"], "resume-main");
            assert_eq!(
                view["turn_index"],
                serde_json::json!(i as i64),
                "轮次序号必须由服务端分配（0 起、连续）"
            );
            last_index = view["turn_index"].as_i64().unwrap();
        }
        assert_eq!(last_index, 2, "三轮写完最大序号必须是 2");

        // 拉回完整 transcript：逐字一致、按序
        let (status, resp) = rpc(
            app.clone(),
            Some(&token),
            tool_call(
                4,
                "get_session",
                serde_json::json!({ "sessionId": "resume-main" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{resp}");
        let turns = structured(&resp)["turns"]
            .as_array()
            .expect("必须返回 turns 数组")
            .clone();
        let got: Vec<(String, String)> = turns
            .iter()
            .map(|t| {
                (
                    t["role"].as_str().unwrap().into(),
                    t["content"].as_str().unwrap().into(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("user".to_string(), "问题".to_string()),
                ("assistant".to_string(), "回答".to_string()),
                ("system".to_string(), "提示".to_string()),
            ],
            "transcript 必须按轮次逐字还原"
        );

        // 缺失会话 = 空 transcript（正常返回，非 isError）
        let (status, resp) = rpc(
            app.clone(),
            Some(&token),
            tool_call(5, "get_session", serde_json::json!({ "sessionId": "nope" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{resp}");
        assert_eq!(structured(&resp)["turns"], serde_json::json!([]));

        // 列表 + 前缀过滤
        let (status, resp) = rpc(
            app.clone(),
            Some(&token),
            tool_call(6, "list_sessions", serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{resp}");
        assert_eq!(
            structured(&resp)["sessions"],
            serde_json::json!(["resume-main"])
        );

        // 非法角色 → JSON-RPC `-32602`（语义错误，调用方可修正），而不是服务端故障。
        // 形状约定见 mcp_tool_error_surfaces_as_jsonrpc_error 的 @intent 注释。
        let (status, resp) = rpc(
            app,
            Some(&token),
            tool_call(
                7,
                "log_turn",
                serde_json::json!({
                    "sessionId": "resume-main",
                    "role": "Tool",
                    "content": "x"
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{resp}");
        assert_eq!(resp["error"]["code"], -32602, "响应体：{resp}");
    }

    /// 重复 project_id 必须拒绝：静默覆盖会让既有 token 与新 token 指向同一项目。
    #[tokio::test]
    async fn admin_create_project_rejects_duplicate_id() {
        let (app, _dir) = setup().await;
        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some("admin"),
            serde_json::json!({ "project_id": "demo" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "响应体：{body}");
        assert_eq!(body["error"], "project_exists");
    }

    /// 非法 project_id（目录穿越 / 分隔符 / 空白）必须在管理面就被拦下。
    #[tokio::test]
    async fn admin_create_project_rejects_invalid_id() {
        let (app, _dir) = setup().await;
        for bad in ["../evil", "a/b", "a b", "a.b", ""] {
            let (status, body) = send_json(
                app.clone(),
                "POST",
                "/api/v1/projects",
                Some("admin"),
                serde_json::json!({ "project_id": bad }),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{bad:?} 应被拒绝，响应体：{body}"
            );
            assert_eq!(body["error"], "invalid_request");
        }
    }

    /// 未编译进本构建的后端必须报 400 并回传可用后端列表（调用方可自我修正）。
    #[tokio::test]
    async fn admin_create_project_rejects_unavailable_backend() {
        let (app, _dir) = setup().await;
        for bad in ["postgres", "mysql"] {
            let (status, body) = send_json(
                app.clone(),
                "POST",
                "/api/v1/projects",
                Some("admin"),
                serde_json::json!({ "project_id": format!("b-{bad}"), "backend": bad }),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "响应体：{body}");
            assert_eq!(body["error"], "invalid_request");
            assert!(
                body["available_backends"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|b| b == "sqlite_file"),
                "应回传可用后端：{body}"
            );
        }
    }

    /// 注销项目：token 立即失效、数据文件与目录一并删除、后续操作报 404。
    #[tokio::test]
    async fn admin_delete_project_unregisters_token_and_removes_data() {
        let (app, dir) = setup().await;
        let (status, created) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some("admin"),
            serde_json::json!({ "project_id": "doomed" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "响应体：{created}");
        let token = created["token"].as_str().unwrap().to_string();

        // 先写入记忆，确保「删掉的是真有数据的项目」
        let (status, _) = rpc(
            app.clone(),
            Some(&token),
            tool_call(
                1,
                "create_entities",
                serde_json::json!({ "entities": [{ "name": "temp" }] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let file = dir.join("doomed").join("mem.db");
        assert!(file.exists(), "前置：项目库应已落盘");

        let (status, body) =
            delete_with_token(app.clone(), "/api/v1/projects/doomed", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        assert_eq!(body["data_removed"], true);

        let (status, _) = rpc(
            app.clone(),
            Some(&token),
            request(1, "tools/list", serde_json::json!({})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "注销后原 token 必须立即失效"
        );
        assert!(!file.exists(), "项目库文件必须被删除");
        assert!(!dir.join("doomed").exists(), "项目目录必须被删除");

        let (status, _) =
            delete_with_token(app.clone(), "/api/v1/projects/doomed", Some("admin")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "重复注销应报 404");

        let (status, _) = get_with_token(app, "/api/v1/projects/doomed/stats", Some("admin")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "注销后统计应报 404");
    }

    /// 注销不存在的项目 → 404。
    #[tokio::test]
    async fn admin_delete_missing_project_is_404() {
        let (app, _dir) = setup().await;
        let (status, body) = delete_with_token(app, "/api/v1/projects/ghost", Some("admin")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "响应体：{body}");
        assert_eq!(body["error"], "not_found");
    }

    /// 后端未注册的项目仍应可注销登记，否则管理面会留下无法清理的死项目。
    ///
    /// @intent 物理数据无从删除（后端未编译进本构建），故响应必须**如实**标注
    ///         `data_removed=false`，而不是假装成功。
    #[tokio::test]
    async fn admin_delete_project_with_unregistered_backend_still_unregisters() {
        let (app, _dir) = setup().await;
        let (status, body) = delete_with_token(app, "/api/v1/projects/broken", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        assert_eq!(body["project_id"], "broken");
        assert_eq!(
            body["data_removed"], false,
            "后端不可用时必须如实报告数据未被删除"
        );
    }

    /// 用量统计：三个计数与实际写入一致。
    #[tokio::test]
    async fn admin_stats_reports_counts_for_a_project() {
        let (app, dir) = setup().await;
        let (status, _) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some("admin"),
            serde_json::json!({ "project_id": "stats-proj" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // 绕过 HTTP 直接写库：统计接口只应关心「库里有多少」，与写入路径无关
        let repos = SqliteFileBackend::new(dir)
            .repositories_for("stats-proj")
            .await
            .unwrap();
        repos
            .memory
            .create_entities(vec![
                EntityInput::new("s-a", "person"),
                EntityInput::new("s-b", "person"),
            ])
            .await
            .unwrap();
        repos
            .memory
            .create_relations(vec![crate::domain::RelationInput::new(
                "s-a", "s-b", "knows",
            )])
            .await
            .unwrap();
        repos
            .memory
            .add_observations(vec![
                crate::domain::ObservationInput::single("s-a", "fact one"),
                crate::domain::ObservationInput::single("s-b", "fact two"),
            ])
            .await
            .unwrap();
        drop(repos);

        let (status, body) =
            get_with_token(app, "/api/v1/projects/stats-proj/stats", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        assert_eq!(body["project_id"], "stats-proj");
        assert_eq!(body["backend"], "sqlite_file");
        assert_eq!(body["entities"], 2);
        assert_eq!(body["relations"], 1);
        assert_eq!(body["observations"], 2);
    }

    /// 未注册项目的统计 → 404（而非 500）。
    #[tokio::test]
    async fn admin_stats_missing_project_is_404() {
        let (app, _dir) = setup().await;
        let (status, body) =
            get_with_token(app, "/api/v1/projects/ghost/stats", Some("admin")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "响应体：{body}");
    }

    /// 管理面写操作与统计一律需要 ADMIN_TOKEN；项目 token 不得越权使用。
    #[tokio::test]
    async fn admin_endpoints_require_admin_token() {
        let (app, _dir) = setup().await;

        let (status, _) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects",
            None,
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some("wrong"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = delete_with_token(app.clone(), "/api/v1/projects/demo", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = get_with_token(app.clone(), "/api/v1/projects/demo/stats", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // 项目 token 不是管理凭据
        let (status, _) = get_with_token(app.clone(), "/api/v1/projects", Some("secret")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // 确认没有误删：unauthorized 请求不得产生副作用
        let (status, _) = get_with_token(app, "/api/v1/projects/demo/stats", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "被拒的请求不得影响既有项目");
    }

    // ------------------------------------------------------ 管理面交接（P4.3）

    /// POST `/handoffs`：无 ID 落 `current@<client>` 分片槽位；GET `/handoffs/{task_id}`
    /// 精确取最新修订。REST 与 MCP 的出参形状一致（复用同一组 dto 视图）。
    #[tokio::test]
    async fn admin_handoff_save_lands_in_sharded_slot_and_get_by_id() {
        let (app, _dir) = setup().await;
        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects/demo/handoffs",
            Some("admin"),
            serde_json::json!({
                "goal": "验收 REST 保存",
                "state": "第一条修订",
                "next_steps": ["接 CLI"],
                "client": "zed",
                "session_id": "A1"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "响应体：{body}");
        assert_eq!(body["task_id"], "current@zed", "无 ID 应落分片槽位：{body}");
        assert_eq!(body["revision"], 1);
        assert_eq!(body["goal"], "验收 REST 保存");
        assert_eq!(
            body["acked_at"],
            serde_json::Value::Null,
            "新修订必为待接手"
        );
        assert_eq!(body["next_steps"], serde_json::json!(["接 CLI"]));

        let (status, body) = get_with_token(
            app,
            "/api/v1/projects/demo/handoffs/current@zed",
            Some("admin"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        assert_eq!(body["task_id"], "current@zed");
        assert_eq!(body["state"], "第一条修订");
        assert_eq!(body["session_id"], "A1");
    }

    /// GET 不存在的 `task_id` 是 404（与 pending 的 204 严格区分）。
    #[tokio::test]
    async fn admin_handoff_get_missing_task_is_404() {
        let (app, _dir) = setup().await;
        let (status, body) = get_with_token(
            app,
            "/api/v1/projects/demo/handoffs/no-such-task",
            Some("admin"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "响应体：{body}");
    }

    /// 列表端点：缺省只列待接手、倒序、上限 10；`?unacked=0` 看历史；
    /// `?limit` 截断发生在排序之后；`?prefix` 只过滤不过滤排序。
    #[tokio::test]
    async fn admin_handoff_list_defaults_and_filters() {
        let (app, _dir) = setup().await;
        for (task, client) in [("t-1", "zed"), ("t-2", "cursor"), ("t-3", "zed")] {
            let (status, body) = send_json(
                app.clone(),
                "POST",
                "/api/v1/projects/demo/handoffs",
                Some("admin"),
                serde_json::json!({"goal": format!("{task} 的活"), "state": "待接", "client": client, "task_id": task}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "响应体：{body}");
        }
        // 接手 t-2，让它退出缺省列表但保留在历史里
        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects/demo/handoffs/t-2/ack",
            Some("admin"),
            serde_json::json!({"client": "zed"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "响应体：{body}");

        let (status, body) =
            get_with_token(app.clone(), "/api/v1/projects/demo/handoffs", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        let all: Vec<serde_json::Value> = body["handoffs"].as_array().unwrap().clone();
        let ids_of = |items: &[serde_json::Value]| -> Vec<String> {
            items
                .iter()
                .map(|s| s["task_id"].as_str().unwrap().to_string())
                .collect()
        };
        // 同秒 save 的顺序依赖 tie-break（task_id 升序），不硬编码断言；
        // 与契约 H11 同思路：断言「待接手集合 == {t-1, t-3} 且排序稳定」。
        let mut sorted_ids = ids_of(&all);
        sorted_ids.sort();
        assert_eq!(
            sorted_ids,
            vec!["t-1".to_string(), "t-3".to_string()],
            "缺省只列待接手：{body}"
        );
        for pair in all.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            assert!(
                a["created_at"].as_i64().unwrap() > b["created_at"].as_i64().unwrap()
                    || (a["created_at"] == b["created_at"]
                        && a["task_id"].as_str().unwrap() < b["task_id"].as_str().unwrap()),
                "排序应为 created_at 降序、同秒 task_id 升序：{body}"
            );
        }
        assert_eq!(all[0]["acked"], false);
        assert_eq!(all[0]["state_excerpt"], "待接");
        assert_eq!(all[0]["client"], "zed");

        let (status, body) = get_with_token(
            app.clone(),
            "/api/v1/projects/demo/handoffs?unacked=0",
            Some("admin"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let got: Vec<&str> = body["handoffs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["task_id"].as_str().unwrap())
            .collect();
        assert_eq!(got.len(), 3, "unacked=0 应包含已接手的：{body}");
        let t2 = body["handoffs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["task_id"] == "t-2")
            .unwrap();
        assert_eq!(t2["acked"], true, "已接手条目应带 acked 标记：{body}");
        assert_eq!(t2["acked_by"], "zed");

        let (status, body) = get_with_token(
            app.clone(),
            "/api/v1/projects/demo/handoffs?limit=1",
            Some("admin"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // 截断必须发生在排序之后：limit=1 的结果应等于全量列表的第一项（与 H11 同构）。
        let first = get_with_token(app.clone(), "/api/v1/projects/demo/handoffs", Some("admin"))
            .await
            .1;
        assert_eq!(
            body["handoffs"].as_array().unwrap().len(),
            1,
            "响应体：{body}"
        );
        assert_eq!(
            body["handoffs"][0]["task_id"], first["handoffs"][0]["task_id"],
            "limit 截断必须发生在排序之后"
        );

        let (status, body) = get_with_token(
            app.clone(),
            "/api/v1/projects/demo/handoffs?prefix=t-",
            Some("admin"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["handoffs"].as_array().unwrap().len(),
            2,
            "prefix 只过滤 task_id，缺省仍只列待接手（t-2 已接手）：{body}"
        );

        let (status, body) = get_with_token(
            app,
            "/api/v1/projects/demo/handoffs?prefix=t-&unacked=0",
            Some("admin"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["handoffs"].as_array().unwrap().len(),
            3,
            "prefix 与 unacked=0 可组合：{body}"
        );
    }

    /// 零 ID 入口 `GET /handoffs/pending`：无 → 204；唯一 → 200 完整记录；
    /// 多条 → 300 + 候选摘要（绝不替调用方挑）。
    #[tokio::test]
    async fn admin_handoff_pending_resolves_zero_id_entry() {
        let (app, _dir) = setup().await;

        let (status, body) = get_with_token(
            app.clone(),
            "/api/v1/projects/demo/handoffs/pending",
            Some("admin"),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "空仓库应 204：{body}");

        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects/demo/handoffs",
            Some("admin"),
            serde_json::json!({"goal": "唯一的活", "state": "待接", "client": "zed"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        let (status, body) = get_with_token(
            app.clone(),
            "/api/v1/projects/demo/handoffs/pending",
            Some("admin"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "唯一待接手应 200：{body}");
        assert_eq!(body["task_id"], "current@zed");
        assert_eq!(body["goal"], "唯一的活");

        let (status, _) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects/demo/handoffs",
            Some("admin"),
            serde_json::json!({"goal": "另一条", "state": "待接", "client": "cursor"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, body) =
            get_with_token(app, "/api/v1/projects/demo/handoffs/pending", Some("admin")).await;
        assert_eq!(status, StatusCode::MULTIPLE_CHOICES, "多条应 300：{body}");
        let got: Vec<&str> = body["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["task_id"].as_str().unwrap())
            .collect();
        assert_eq!(got.len(), 2, "候选应含两条：{body}");
        assert!(got.contains(&"current@zed") && got.contains(&"current@cursor"));
    }

    /// **路由遮蔽钉死**：存在名为 `pending` 的任务时，`GET /handoffs/pending` 与
    /// `POST /handoffs/pending/ack` 仍必须命中零 ID 入口，而不是被 `{task_id}` 吃掉。
    #[tokio::test]
    async fn admin_handoff_pending_route_is_not_shadowed_by_task_id() {
        let (app, _dir) = setup().await;
        for task in ["pending", "other"] {
            let (status, body) = send_json(
                app.clone(),
                "POST",
                "/api/v1/projects/demo/handoffs",
                Some("admin"),
                serde_json::json!({"goal": format!("{task} 的活"), "state": "待接", "task_id": task}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }

        // 若被 {task_id} 吃掉，这里会 200 返回单条 "pending" 任务，而非 300 候选
        let (status, body) = get_with_token(
            app.clone(),
            "/api/v1/projects/demo/handoffs/pending",
            Some("admin"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::MULTIPLE_CHOICES,
            "同名任务存在时零 ID 入口必须仍然生效：{body}"
        );

        let (status, body) = send_json(
            app,
            "POST",
            "/api/v1/projects/demo/handoffs/pending/ack",
            Some("admin"),
            serde_json::json!({"client": "zed"}),
        )
        .await;
        assert_eq!(status, StatusCode::MULTIPLE_CHOICES, "{body}");
        assert_eq!(body["won"], false, "歧义时 ack 不得抢到接手权：{body}");
    }

    /// ack 两个入口：`{task_id}/ack` 精确接手（重复 ack 幂等、won=false）；
    /// `pending/ack` 零 ID 接手（无待接手 → 204）。
    #[tokio::test]
    async fn admin_handoff_ack_endpoints() {
        let (app, _dir) = setup().await;
        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects/demo/handoffs",
            Some("admin"),
            serde_json::json!({"goal": "目标", "state": "待接", "task_id": "ack-me"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects/demo/handoffs/ack-me/ack",
            Some("admin"),
            serde_json::json!({"client": "zed"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["won"], true);
        assert_eq!(body["handoff"]["acked_by"], "zed");
        assert!(body["handoff"]["acked_at"].is_number());

        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects/demo/handoffs/ack-me/ack",
            Some("admin"),
            serde_json::json!({"client": "cursor"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "重复 ack 仍是正常返回：{body}");
        assert_eq!(body["won"], false, "已接手不得再次抢到");
        assert_eq!(body["handoff"]["acked_by"], "zed", "接手方不得被后到者覆盖");

        // 全部接手完毕后，零 ID 接手入口无活可干 → 204
        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects/demo/handoffs/pending/ack",
            Some("admin"),
            serde_json::json!({"client": "zed"}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "无待接手应 204：{body}");

        // 接手不存在的任务 → 404
        let (status, body) = send_json(
            app,
            "POST",
            "/api/v1/projects/demo/handoffs/ghost/ack",
            Some("admin"),
            serde_json::json!({"client": "zed"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    /// DELETE 幂等；删除后 GET 404；重复 DELETE 仍 200。
    #[tokio::test]
    async fn admin_handoff_delete_is_idempotent() {
        let (app, _dir) = setup().await;
        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects/demo/handoffs",
            Some("admin"),
            serde_json::json!({"goal": "待删", "state": "x", "task_id": "doomed"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        let (status, _) = delete_with_token(
            app.clone(),
            "/api/v1/projects/demo/handoffs/doomed",
            Some("admin"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = get_with_token(
            app.clone(),
            "/api/v1/projects/demo/handoffs/doomed",
            Some("admin"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) =
            delete_with_token(app, "/api/v1/projects/demo/handoffs/doomed", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "重复删除必须幂等");
    }

    /// 非法入参 400（与记忆写侧同源：整批先校验再落库）；项目不存在 404；无凭据 401。
    #[tokio::test]
    async fn admin_handoff_input_errors_are_actionable() {
        let (app, _dir) = setup().await;
        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects/demo/handoffs",
            Some("admin"),
            serde_json::json!({"goal": "", "state": "x"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects/demo/handoffs",
            Some("admin"),
            serde_json::json!({"goal": "g", "state": "x", "task_id": "bad/id"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects/ghost/handoffs",
            Some("admin"),
            serde_json::json!({"goal": "g", "state": "x"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        let (status, _) = send_json(
            app,
            "POST",
            "/api/v1/projects/demo/handoffs",
            None,
            serde_json::json!({"goal": "g", "state": "x"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// 列表端点的查询参数非法时必须 400，不得静默回退默认值。
    #[tokio::test]
    async fn admin_handoff_list_rejects_invalid_query_params() {
        let (app, _dir) = setup().await;
        for uri in [
            "/api/v1/projects/demo/handoffs?unacked=yes",
            "/api/v1/projects/demo/handoffs?limit=abc",
            "/api/v1/projects/demo/handoffs?limit=-1",
        ] {
            let (status, body) = get_with_token(app.clone(), uri, Some("admin")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} 响应体：{body}");
        }
    }

    /// `stats.handoffs` = 有交接的任务数（不区分是否已接手）。
    #[tokio::test]
    async fn admin_stats_includes_handoff_task_count() {
        let (app, _dir) = setup().await;
        for (task, ack_it) in [("s-1", false), ("s-2", true)] {
            let (status, body) = send_json(
                app.clone(),
                "POST",
                "/api/v1/projects/demo/handoffs",
                Some("admin"),
                serde_json::json!({"goal": format!("{task} 的活"), "state": "x", "task_id": task}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            if ack_it {
                let (status, body) = send_json(
                    app.clone(),
                    "POST",
                    "/api/v1/projects/demo/handoffs/s-2/ack",
                    Some("admin"),
                    serde_json::json!({"client": "zed"}),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{body}");
            }
        }

        let (status, body) =
            get_with_token(app, "/api/v1/projects/demo/stats", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["handoffs"], 2, "应统计全部有交接的任务：{body}");
    }

    // ------------------------------------------------------ 网页控制台（P4.4）

    /// 带 Cookie 发起一次请求。
    ///
    /// @intent 默认补上 `Host: localhost`：`oneshot` 不经过 hyper，不会自动合成 `Host`，
    ///         而跨站判据需要它。若不补，所有请求都会落进「有 Origin 无 Host」的分支，
    ///         同源用例会因**失败关闭**而被误判为跨站——这正是实测中暴露出来的形态。
    ///         显式给出 `Host` 也更贴近真实流量。
    async fn send_with_cookie(
        app: Router,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        origin: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value, axum::http::HeaderMap) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        let payload = body.map(|v| v.to_string()).unwrap_or_default();
        let resp = app
            .oneshot(builder.body(Body::from(payload)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        (status, body_json(resp).await, headers)
    }

    /// 从 `Set-Cookie` 响应头里取出 `name=value` 形式的 Cookie 对。
    fn session_cookie_pair(headers: &axum::http::HeaderMap) -> Option<String> {
        let raw = headers.get(header::SET_COOKIE)?.to_str().ok()?;
        let pair = raw.split(';').next()?.trim().to_string();
        if pair.starts_with("memora_session=") {
            Some(pair)
        } else {
            None
        }
    }

    /// 以指定账号登录并返回后续请求可直接使用的 Cookie 对。
    ///
    /// @intent 与 [`login`] 分开是为了多租户用例：那里必须能拿到**非管理员**
    ///         的会话，才能在同一个应用实例上验证两种角色的可见范围差异。
    async fn login_as(app: Router, username: &str, password: &str) -> String {
        let (status, body, headers) = send_with_cookie(
            app,
            "POST",
            "/api/v1/session",
            None,
            None,
            Some(serde_json::json!({
                "username": username,
                "password": password,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "登录应当成功：{body}");
        session_cookie_pair(&headers).expect("登录响应必须带会话 Cookie")
    }

    /// 以种子管理员登录并返回后续请求可直接使用的 Cookie 对。
    async fn login(app: Router) -> String {
        login_as(app, CONSOLE_USER, CONSOLE_PASSWORD).await
    }

    /// 新建账号（走真实的管理接口，不直接写库）。
    ///
    /// @intent 刻意不走 `users::create_with_role`：用例要验证的是「管理员开号的
    ///         那条链路」，直插数据库会绕过参数校验与角色解析，把接口本身的问题
    ///         藏在测试之外。
    async fn create_account(
        app: Router,
        admin_token: &str,
        username: &str,
        password: &str,
        role: &str,
    ) -> serde_json::Value {
        let (status, body) = send_json(
            app,
            "POST",
            "/api/v1/users",
            Some(admin_token),
            serde_json::json!({ "username": username, "password": password, "role": role }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "建账号失败：{body}");
        body
    }

    /// `/admin` 必须公开可访问：它只是一张登录表单，登录前就要能看到。
    #[tokio::test]
    async fn console_page_is_public_html() {
        let (app, _dir) = setup_with_console(true).await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
        assert!(
            resp.headers().contains_key("content-security-policy"),
            "页面必须带 CSP"
        );

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Memora"), "应返回控制台页面");
    }

    /// 没有控制台账号时，自检接口必须如实报告，且登录被拒。
    ///
    /// @intent 这是「功能不可用」与「口令错」的分野：前者页面会给修复指引，
    ///         后者只是提示重试。若两者都返回裸 401，用户只能看到一个无法登录、
    ///         且不说明原因的界面。
    #[tokio::test]
    async fn session_status_reports_missing_console_password() {
        let (app, _dir) = setup_with_console(false).await;

        let (status, body, _) =
            send_with_cookie(app.clone(), "GET", "/api/v1/session", None, None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["console_login_enabled"], false);
        assert_eq!(body["authenticated"], false);
        assert_eq!(body["users"], 0, "账号数应如实为 0：{body}");

        let (status, body, _) = send_with_cookie(
            app,
            "POST",
            "/api/v1/session",
            None,
            None,
            Some(serde_json::json!({ "username": "admin", "password": "whatever" })),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("no console account"),
            "应明确告知是「尚无账号」而非「口令错误」：{body}"
        );
    }

    /// 登录成功必须签发 Cookie，且该 Cookie 能通过管理面鉴权。
    ///
    /// @intent 同时钉住「登录接口本身不能挂鉴权中间件」——若把它误并进受保护的分组，
    ///         `POST /session` 会在进入 handler 前就 401，本用例即刻失败。
    #[tokio::test]
    async fn login_issues_a_cookie_that_authenticates_the_admin_api() {
        let (app, _dir) = setup_with_console(true).await;
        let cookie = login(app.clone()).await;

        let (status, body, _) =
            send_with_cookie(app, "GET", "/api/v1/projects", Some(&cookie), None, None).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "会话 Cookie 应能通过管理面鉴权：{body}"
        );
        assert!(body.as_array().unwrap().len() >= 3);
    }

    /// 错误口令与错误用户名都必须 401，且响应体不得指出是哪一个错了。
    #[tokio::test]
    async fn login_rejects_bad_credentials_without_enumeration() {
        let (app, _dir) = setup_with_console(true).await;

        for (user, password) in [
            (CONSOLE_USER, "wrong-password"),
            ("someone-else", CONSOLE_PASSWORD),
            ("", ""),
        ] {
            let (status, body, headers) = send_with_cookie(
                app.clone(),
                "POST",
                "/api/v1/session",
                None,
                None,
                Some(serde_json::json!({ "username": user, "password": password })),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{user:?} 应被拒绝");
            assert!(
                session_cookie_pair(&headers).is_none(),
                "失败的登录绝不能签发 Cookie"
            );
            assert_eq!(
                body["detail"], "invalid username or password",
                "不得区分用户名与口令哪个错：{body}"
            );
        }
    }

    /// 登出后原 Cookie 必须立即失效，且响应要清掉浏览器里的 Cookie。
    #[tokio::test]
    async fn logout_invalidates_the_session_and_clears_the_cookie() {
        let (app, _dir) = setup_with_console(true).await;
        let cookie = login(app.clone()).await;

        let (status, _, headers) = send_with_cookie(
            app.clone(),
            "DELETE",
            "/api/v1/session",
            Some(&cookie),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let cleared = headers
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            cleared.contains("Max-Age=0"),
            "必须让浏览器立即丢弃：{cleared}"
        );

        let (status, _, _) =
            send_with_cookie(app, "GET", "/api/v1/projects", Some(&cookie), None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "登出后会话必须不可用");
    }

    /// 伪造的会话 id 不得通过鉴权。
    #[tokio::test]
    async fn forged_session_cookie_is_rejected() {
        let (app, _dir) = setup_with_console(true).await;
        for forged in [
            "memora_session=deadbeef",
            "memora_session=",
            "memora_session=0000000000000000000000000000000000000000000000000000000000000000",
        ] {
            let (status, _, _) = send_with_cookie(
                app.clone(),
                "GET",
                "/api/v1/projects",
                Some(forged),
                None,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{forged} 不得通过");
        }
    }

    /// 会话 Cookie **不得**用于 `/mcp`：两套凭据必须隔离。
    ///
    /// @intent 这条边界一旦模糊，浏览器里的一次登录就等于对所有项目 token 的访问权，
    ///         而项目硬隔离是本服务的核心承诺。故单独用一条用例钉住。
    #[tokio::test]
    async fn session_cookie_does_not_authenticate_mcp() {
        let (app, _dir) = setup_with_console(true).await;
        let cookie = login(app.clone()).await;

        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::COOKIE, cookie)
            .body(Body::from(
                request(1, "tools/list", serde_json::json!({})).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "会话 Cookie 不是项目凭据，/mcp 必须拒绝"
        );
    }

    /// 项目 token 也不得用于管理面（反向隔离）。
    #[tokio::test]
    async fn project_token_is_not_an_admin_credential() {
        let (app, _dir) = setup_with_console(true).await;
        let (status, _, _) = send_with_cookie(
            app,
            "GET",
            "/api/v1/projects",
            Some("memora_session=secret"),
            None,
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "把项目 token 当会话 id 用必须被拒"
        );
    }

    /// CSRF：用会话 Cookie 发起的跨站写请求必须被拒，同源写请求必须放行。
    ///
    /// @intent 判据是「Origin 的 host 与 Host 是否一致」，故这里同时覆盖同源、异源
    ///         两种形态；缺 Origin 的形态见下一条用例。
    #[tokio::test]
    async fn cross_origin_writes_are_rejected_but_same_origin_is_allowed() {
        let (app, _dir) = setup_with_console(true).await;
        let cookie = login(app.clone()).await;

        // 异源 DELETE 必须被拒，且不得产生副作用
        let (status, body, _) = send_with_cookie(
            app.clone(),
            "DELETE",
            "/api/v1/projects/demo",
            Some(&cookie),
            Some("https://evil.example.com"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "跨站写请求必须被拒：{body}");
        assert_eq!(body["error"], "cross_origin_rejected");

        let (status, _) =
            get_with_token(app.clone(), "/api/v1/projects/demo/stats", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "被拒的跨站请求不得删掉项目");

        // 同源写请求放行
        let (status, body, _) = send_with_cookie(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some(&cookie),
            Some("http://localhost"),
            Some(serde_json::json!({ "project_id": "same-origin-ok" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "同源写请求应放行：{body}");
    }

    /// 缺少 `Origin` 的写请求放行：真正的跨站表单提交在现代浏览器上必带 Origin，
    /// 而 `curl` 带 Cookie 手工调试不该被误伤。
    #[tokio::test]
    async fn cookie_write_without_origin_is_allowed() {
        let (app, _dir) = setup_with_console(true).await;
        let cookie = login(app.clone()).await;

        let (status, body, _) = send_with_cookie(
            app,
            "POST",
            "/api/v1/projects",
            Some(&cookie),
            None,
            Some(serde_json::json!({ "project_id": "no-origin" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    /// 跨站检查只约束**会话**通路：Bearer 面向脚本，脚本没有 Origin，
    /// 也不存在「浏览器自动附带凭据」的问题，故不受限。
    #[tokio::test]
    async fn cross_origin_check_does_not_apply_to_bearer() {
        let (app, _dir) = setup_with_console(true).await;

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/projects")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, "Bearer admin")
            .header(header::ORIGIN, "https://evil.example.com")
            .body(Body::from(
                serde_json::json!({ "project_id": "bearer-with-origin" }).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "Bearer 通路不应受跨站检查约束"
        );
    }

    /// 安全方法（GET）即使异源也放行：它不改状态，且浏览器不会为一次读操作
    /// 提供任何额外权限。
    #[tokio::test]
    async fn cross_origin_reads_with_cookie_are_allowed() {
        let (app, _dir) = setup_with_console(true).await;
        let cookie = login(app.clone()).await;

        let (status, _, _) = send_with_cookie(
            app,
            "GET",
            "/api/v1/projects",
            Some(&cookie),
            Some("https://evil.example.com"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// 记忆浏览接口：返回实体（含观测）与关系，且项目不存在时报 404。
    #[tokio::test]
    async fn project_graph_exposes_entities_observations_and_relations() {
        let (app, dir) = setup_with_console(true).await;

        let repos = SqliteFileBackend::new(dir)
            .repositories_for("demo")
            .await
            .unwrap();
        repos
            .memory
            .create_entities(vec![
                EntityInput::new("alpha", "person"),
                EntityInput::new("beta", "person"),
            ])
            .await
            .unwrap();
        repos
            .memory
            .create_relations(vec![crate::domain::RelationInput::new(
                "alpha", "beta", "knows",
            )])
            .await
            .unwrap();
        repos
            .memory
            .add_observations(vec![crate::domain::ObservationInput::single(
                "alpha",
                "偏好深色主题",
            )])
            .await
            .unwrap();
        drop(repos);

        // 伪造的会话 id 不得通过鉴权
        let (status, _, _) = send_with_cookie(
            app.clone(),
            "GET",
            "/api/v1/projects/demo/graph",
            Some("memora_session=x"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, body) =
            get_with_token(app.clone(), "/api/v1/projects/demo/graph", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["project_id"], "demo");
        assert_eq!(body["entities"].as_array().unwrap().len(), 2);
        assert_eq!(body["relations"].as_array().unwrap().len(), 1);

        let alpha = body["entities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "alpha")
            .unwrap();
        assert_eq!(alpha["entity_type"], "person");
        let observations = alpha["observations"].as_array().unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0]["content"], "偏好深色主题");
        assert_eq!(observations[0]["entity_name"], "alpha");
        assert!(
            observations[0]["id"].as_i64().unwrap() > 0,
            "必须回传观测 id，否则控制台无法展示可定位的条目"
        );

        assert_eq!(body["relations"][0]["from_name"], "alpha");
        assert_eq!(body["relations"][0]["to_name"], "beta");
        assert_eq!(body["relations"][0]["relation_type"], "knows");

        // 与统计口径一致：控制台看到的实体数应等于 stats 报出的实体数
        let (_, stats) =
            get_with_token(app.clone(), "/api/v1/projects/demo/stats", Some("admin")).await;
        assert_eq!(
            stats["entities"],
            body["entities"].as_array().unwrap().len()
        );

        let (status, _) = get_with_token(app, "/api/v1/projects/ghost/graph", Some("admin")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "不存在的项目应报 404");
    }

    /// 记忆浏览接口的鉴权：无凭据一律 401。
    #[tokio::test]
    async fn project_graph_requires_admin_credentials() {
        let (app, _dir) = setup().await;
        let (status, _) = get_with_token(app.clone(), "/api/v1/projects/demo/graph", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = get_with_token(app, "/api/v1/projects/demo/graph", Some("secret")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "项目 token 不是管理凭据");
    }

    // ------------------------------------------------- 多租户与角色（P8）

    /// 项目列表按角色分支：管理员全量，普通账号只见自己名下的。
    ///
    /// @intent 这是多租户里唯一「形状随角色变化」的端点，也是最容易被写成
    ///         「一律全量」的一处。用例同时钉住两个方向：
    ///         - 租户看不到别人（含无主）的项目；
    ///         - 管理员看得到**无主**项目——否则升级后老数据会在控制台上像是丢了。
    #[tokio::test]
    async fn project_listing_is_scoped_by_role() {
        let (app, _dir) = setup_with_console(true).await;
        create_account(app.clone(), "admin", "alice", "alice-password", "user").await;

        // 三个预置项目均为无主（迁移中间态），管理员应全部可见
        let (status, list) = get_with_token(app.clone(), "/api/v1/projects", Some("admin")).await;
        assert_eq!(status, StatusCode::OK);
        let ids: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["project_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 3, "管理员应看到全部项目：{list}");
        assert!(
            list.as_array().unwrap().iter().all(|p| p["owner"] == ""),
            "迁移前遗留项目应如实报出空归属：{list}"
        );

        // 租户一个也看不到：无主项目不属于任何人
        let cookie = login_as(app.clone(), "alice", "alice-password").await;
        let (status, list, _) =
            send_with_cookie(app.clone(), "GET", "/api/v1/projects", Some(&cookie), None, None)
                .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            list.as_array().unwrap().len(),
            0,
            "租户不得看到任何无主项目：{list}"
        );

        // 租户自建一个项目，从此只看到它
        let (status, body, _) = send_with_cookie(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some(&cookie),
            Some("http://localhost"),
            Some(serde_json::json!({ "project_id": "alice-notes" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "租户应能自建项目：{body}");
        assert_eq!(
            body["owner"], "alice",
            "项目归属必须是创建者本人，而不是配置里的 admin_user"
        );

        let (_, list, _) =
            send_with_cookie(app.clone(), "GET", "/api/v1/projects", Some(&cookie), None, None)
                .await;
        let ids: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["project_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["alice-notes"], "租户只应看到自己的项目：{list}");

        // 管理员依旧能看到四个（三个无主 + alice 的）
        let (_, list) = get_with_token(app, "/api/v1/projects", Some("admin")).await;
        assert_eq!(list.as_array().unwrap().len(), 4, "管理员应看到全部：{list}");
    }

    /// 跨租户访问单个项目一律 404，且与「真不存在」在结构与文案模板上完全一致。
    ///
    /// @intent 断言的是**响应模板一致**而不只是状态码相同：若越权返回 403、
    ///         不存在返回 404，则「403 与否」就成了一个项目名枚举接口。
    ///         这里逐项比对 `error` 标识与 `detail` 的模板（把项目名换成占位符后
    ///         必须完全相同），防止将来有人「为了让报错更友好」给越权分支
    ///         加一句额外说明——那句说明本身就是泄漏点。
    #[tokio::test]
    async fn projects_of_another_tenant_are_indistinguishable_from_missing() {
        let (app, _dir) = setup_with_console(true).await;
        create_account(app.clone(), "admin", "alice", "alice-password", "user").await;
        create_account(app.clone(), "admin", "bob", "bob-password", "user").await;

        let alice = login_as(app.clone(), "alice", "alice-password").await;
        let bob = login_as(app.clone(), "bob", "bob-password").await;

        // alice 建项目，bob 去访问
        let (status, _, _) = send_with_cookie(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some(&alice),
            Some("http://localhost"),
            Some(serde_json::json!({ "project_id": "alice-private" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        /// 把 `detail` 里的项目名替换成占位符，得到可跨项目名比较的「模板」。
        fn template(body: &serde_json::Value, project_id: &str) -> String {
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .replace(project_id, "<id>")
        }

        for uri in [
            "/api/v1/projects/{id}/stats",
            "/api/v1/projects/{id}/graph",
            "/api/v1/projects/{id}/handoffs",
        ] {
            let target = uri.replace("{id}", "alice-private");
            let ghost = uri.replace("{id}", "ghost-project");

            let (status, body, _) =
                send_with_cookie(app.clone(), "GET", &target, Some(&bob), None, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{target} 越权应为 404：{body}");

            let (ghost_status, ghost_body, _) =
                send_with_cookie(app.clone(), "GET", &ghost, Some(&bob), None, None).await;
            assert_eq!(ghost_status, status, "{uri}：越权与不存在的状态码必须相同");
            assert_eq!(body["error"], ghost_body["error"], "{uri}：错误标识必须相同");
            assert_eq!(
                template(&body, "alice-private"),
                template(&ghost_body, "ghost-project"),
                "{uri}：越权与不存在的文案模板必须相同，否则可用来枚举项目名——\
                 越权响应：{body}，不存在响应：{ghost_body}"
            );
        }

        // 删除与轮换同样不可越权（写操作更危险）
        let (status, _, _) = send_with_cookie(
            app.clone(),
            "POST",
            "/api/v1/projects/alice-private/token",
            Some(&bob),
            Some("http://localhost"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "越权轮换 token 必须失败");

        let (status, _, _) = send_with_cookie(
            app.clone(),
            "DELETE",
            "/api/v1/projects/alice-private",
            Some(&bob),
            Some("http://localhost"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "越权注销必须失败");

        // owner 本人与管理员都仍能访问，证明 404 只针对越权者
        let (status, _, _) = send_with_cookie(
            app.clone(),
            "GET",
            "/api/v1/projects/alice-private/stats",
            Some(&alice),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "owner 应能访问自己的项目");
        let (status, _) =
            get_with_token(app, "/api/v1/projects/alice-private/stats", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "管理员应能访问任何项目");
    }

    /// 账号管理接口仅管理员可用；租户一律 403（不是 404——账号名不是敏感资源，
    /// 且这里的目标是「你没这个权限」，不是「这东西不存在」）。
    #[tokio::test]
    async fn account_management_requires_admin() {
        let (app, _dir) = setup_with_console(true).await;
        create_account(app.clone(), "admin", "alice", "alice-password", "user").await;
        let alice = login_as(app.clone(), "alice", "alice-password").await;

        let (status, body, _) =
            send_with_cookie(app.clone(), "GET", "/api/v1/users", Some(&alice), None, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "租户不得列账号：{body}");
        assert_eq!(body["error"], "role_required");

        let cases = [
            (
                "POST",
                "/api/v1/users",
                Some(serde_json::json!({ "username": "eve", "password": "eve-password" })),
            ),
            (
                "PUT",
                "/api/v1/users/bob/role",
                Some(serde_json::json!({ "role": "admin" })),
            ),
            ("DELETE", "/api/v1/users/console-admin", None),
        ];
        for (method, uri, payload) in cases {
            let (status, body, _) = send_with_cookie(
                app.clone(),
                method,
                uri,
                Some(&alice),
                Some("http://localhost"),
                payload,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} {uri} 应仅限管理员：{body}"
            );
        }

        // 租户可以改自己的口令（这是该接口对他存在的理由）
        let (status, body, _) = send_with_cookie(
            app.clone(),
            "PUT",
            "/api/v1/users/alice",
            Some(&alice),
            Some("http://localhost"),
            Some(serde_json::json!({ "password": "alice-new-password" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "租户应能改自己的口令：{body}");

        // 但不能改别人的
        let (status, body, _) = send_with_cookie(
            app,
            "PUT",
            "/api/v1/users/bob",
            Some(&alice),
            Some("http://localhost"),
            Some(serde_json::json!({ "password": "hijacked-password" })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "租户不得改他人口令：{body}"
        );
        assert_eq!(body["error"], "role_required");
    }

    /// 改角色必须立即生效：会话里的角色快照被原地刷新，不需要重新登录。
    ///
    /// @intent 若只改库不刷新会话，降权的旧会话还能用满 12 小时——那等于
    ///         「权限变更没生效」。此用例同时覆盖提权与降权两个方向。
    #[tokio::test]
    async fn role_changes_take_effect_on_live_sessions() {
        let (app, _dir) = setup_with_console(true).await;
        create_account(app.clone(), "admin", "alice", "alice-password", "user").await;
        let alice = login_as(app.clone(), "alice", "alice-password").await;

        // 初始：租户，看不到账号页
        let (_, body, _) =
            send_with_cookie(app.clone(), "GET", "/api/v1/session", Some(&alice), None, None)
                .await;
        assert_eq!(body["role"], "user");
        let (status, _, _) =
            send_with_cookie(app.clone(), "GET", "/api/v1/users", Some(&alice), None, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // 提权：同一个 Cookie 立刻变成管理员
        let (status, body) = send_json(
            app.clone(),
            "PUT",
            "/api/v1/users/alice/role",
            Some("admin"),
            serde_json::json!({ "role": "admin" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["role"], "admin");
        assert_eq!(
            body["sessions_updated"], 1,
            "alice 有一个在线会话，必须被刷新：{body}"
        );

        let (_, body, _) =
            send_with_cookie(app.clone(), "GET", "/api/v1/session", Some(&alice), None, None)
                .await;
        assert_eq!(body["role"], "admin", "提权应立刻反映在会话自检里");
        let (status, body, _) =
            send_with_cookie(app.clone(), "GET", "/api/v1/users", Some(&alice), None, None).await;
        assert_eq!(status, StatusCode::OK, "提权后应立即能管账号：{body}");

        // 降权：立刻失去管理权限，但会话本身还在（不踢人）
        let (status, body) = send_json(
            app.clone(),
            "PUT",
            "/api/v1/users/alice/role",
            Some("admin"),
            serde_json::json!({ "role": "user" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body, _) =
            send_with_cookie(app.clone(), "GET", "/api/v1/session", Some(&alice), None, None)
                .await;
        assert_eq!(status, StatusCode::OK, "降权不应销毁会话");
        assert_eq!(body["role"], "user");
        let (status, _, _) =
            send_with_cookie(app, "GET", "/api/v1/users", Some(&alice), None, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "降权后应立即失去管理权限");
    }

    /// 唯一管理员不可被删除、也不可被降级。
    ///
    /// @intent 判据是「管理员数」而不是「账号数」：只剩一群普通账号时，
    ///         删掉/降级最后一个管理员会让管理面永久锁死，而唯一的补救手段是
    ///         手工改库或删掉 `_meta.db`（后者连带丢掉全部项目登记）。
    /// @intent 顺带钉住「先提一个管理员，再动原来那个就合法了」这条逃生路径——
    ///         否则用户看到 409 会以为系统坏了。
    #[tokio::test]
    async fn the_only_admin_can_be_neither_deleted_nor_demoted() {
        let (app, _dir) = setup_with_console(true).await;
        create_account(app.clone(), "admin", "bob", "bob-password", "user").await;

        let (status, body) = delete_with_token(
            app.clone(),
            &format!("/api/v1/users/{CONSOLE_USER}"),
            Some("admin"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "last_admin");

        let (status, body) = send_json(
            app.clone(),
            "PUT",
            &format!("/api/v1/users/{CONSOLE_USER}/role"),
            Some("admin"),
            serde_json::json!({ "role": "user" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "last_admin");

        // 普通账号删除不受此限（他本来就不是管理员）
        let (status, body) =
            delete_with_token(app.clone(), "/api/v1/users/bob", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "删除普通账号不应被拦：{body}");

        // 有第二个管理员之后，原来的就可以动了
        create_account(app.clone(), "admin", "carol", "carol-password", "admin").await;
        let (status, body) = send_json(
            app.clone(),
            "PUT",
            &format!("/api/v1/users/{CONSOLE_USER}/role"),
            Some("admin"),
            serde_json::json!({ "role": "user" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "存在第二个管理员后应允许降级：{body}");

        // 于是 carol 成了唯一管理员，她不能被删
        let (status, body) =
            delete_with_token(app, "/api/v1/users/carol", Some("admin")).await;
        assert_eq!(status, StatusCode::CONFLICT, "唯一管理员保护应随之转移：{body}");
        assert_eq!(body["error"], "last_admin");
    }

    /// 删除账号后，它名下的项目不会消失，只是变成「仅管理员可见」。
    ///
    /// @intent 这是刻意的保守选择：自动转交或自动删除都是在替用户做不可逆的决定。
    ///         用例同时钉住「管理面不能因此丢数据」和「别人也拿不到这些数据」。
    #[tokio::test]
    async fn deleting_an_account_orphans_its_projects_without_losing_them() {
        let (app, _dir) = setup_with_console(true).await;
        create_account(app.clone(), "admin", "alice", "alice-password", "user").await;
        let alice = login_as(app.clone(), "alice", "alice-password").await;

        let (status, _, _) = send_with_cookie(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some(&alice),
            Some("http://localhost"),
            Some(serde_json::json!({ "project_id": "alice-notes" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, body) =
            delete_with_token(app.clone(), "/api/v1/users/alice", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["sessions_revoked"], 1,
            "删除账号必须同时销毁它的会话，否则 Cookie 还能用满 12 小时：{body}"
        );

        // 旧 Cookie 立即失效
        let (status, _, _) =
            send_with_cookie(app.clone(), "GET", "/api/v1/session", Some(&alice), None, None)
                .await;
        assert_eq!(status, StatusCode::OK);
        // 会话自检本身是公开的，这里靠 authenticated 判定
        let (_, body, _) =
            send_with_cookie(app.clone(), "GET", "/api/v1/session", Some(&alice), None, None)
                .await;
        assert_eq!(body["authenticated"], false, "删除账号后旧 Cookie 必须失效");

        // 项目还在，管理员看得见，归属仍是那个已不存在的账号名
        let (status, body) =
            get_with_token(app.clone(), "/api/v1/projects/alice-notes/stats", Some("admin"))
                .await;
        assert_eq!(status, StatusCode::OK, "项目数据不得随账号一起消失：{body}");
        let (_, list) = get_with_token(app.clone(), "/api/v1/projects", Some("admin")).await;
        let orphan = list
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["project_id"] == "alice-notes")
            .expect("项目必须仍在列表里");
        assert_eq!(
            orphan["owner"], "alice",
            "应如实报出失效归属，供管理员处置"
        );

        // 别的租户依旧看不到
        create_account(app.clone(), "admin", "bob", "bob-password", "user").await;
        let bob = login_as(app.clone(), "bob", "bob-password").await;
        let (status, _, _) =
            send_with_cookie(app.clone(), "GET", "/api/v1/projects/alice-notes/stats", Some(&bob), None, None)
                .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "失效归属的项目对任何租户都不可见"
        );
    }

    /// 新建账号省略 `role` 时必须是普通账号，非法角色值必须是 400。
    ///
    /// @intent 「省略即管理员」会把「顺手多开一个号」变成「顺手多开一个管理员」，
    ///         而这类事故没有任何提示。缺省值落在低权限上，是有意的。
    #[tokio::test]
    async fn new_accounts_default_to_tenant_and_reject_unknown_roles() {
        let (app, _dir) = setup_with_console(true).await;

        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/users",
            Some("admin"),
            serde_json::json!({ "username": "dave", "password": "dave-password" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["role"], "user", "省略 role 必须落在普通账号：{body}");

        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/users",
            Some("admin"),
            serde_json::json!({
                "username": "mallory",
                "password": "mallory-password",
                "role": "superuser"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "未知角色不得被猜成某种权限：{body}");

        // 前缀匹配/大小写都不算数——只有精确的 admin 才是管理员。
        // @intent 读路径（`Role::parse`）容忍空白与大小写，写路径不容忍：
        //         入参位置宽容等于把拼错的值升级成最高权限。
        for sloppy in ["admin ", " Admin", "ADMIN"] {
            let (status, body) = send_json(
                app.clone(),
                "POST",
                "/api/v1/users",
                Some("admin"),
                serde_json::json!({
                    "username": "trent",
                    "password": "trent-password",
                    "role": sloppy
                }),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "角色值 `{sloppy}` 不得被宽容接受：{body}"
            );
        }

        // 账号列表如实报出角色
        let (_, list) = get_with_token(app, "/api/v1/users", Some("admin")).await;
        let dave = list
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["username"] == "dave")
            .expect("dave 应在列表里");
        assert_eq!(dave["role"], "user");
        let admin = list
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["username"] == CONSOLE_USER)
            .expect("种子管理员应在列表里");
        assert_eq!(admin["role"], "admin");
    }

    /// `ADMIN_TOKEN`（脚本路径）恒为管理员，且不属于任何租户。
    ///
    /// @intent 这条不是「权限更宽」的顺手设计，而是兼容性要求：`memora-handoff`
    ///         等脚本只用 `ADMIN_TOKEN`，若它被降级或受 owner 过滤，所有脚本会在
    ///         一次升级后集体坏掉，且报错形态是「项目不存在」——极难定位。
    #[tokio::test]
    async fn admin_token_acts_as_a_tenantless_administrator() {
        let (app, _dir) = setup_with_console(true).await;
        create_account(app.clone(), "admin", "alice", "alice-password", "user").await;
        let alice = login_as(app.clone(), "alice", "alice-password").await;
        let (_, _, _) = send_with_cookie(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some(&alice),
            Some("http://localhost"),
            Some(serde_json::json!({ "project_id": "alice-notes" })),
        )
        .await;

        // 自检里身份是配置里的 admin_user，角色是 admin
        let (status, body) =
            get_with_token(app.clone(), "/api/v1/session", Some("admin")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["authenticated"], true);
        assert_eq!(body["user"], CONSOLE_USER);
        assert_eq!(body["role"], "admin");

        // 能读别人名下的项目
        let (status, body) =
            get_with_token(app.clone(), "/api/v1/projects/alice-notes/stats", Some("admin"))
                .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["project_id"], "alice-notes");

        // 能用账号管理
        let (status, _) = get_with_token(app.clone(), "/api/v1/users", Some("admin")).await;
        assert_eq!(status, StatusCode::OK);

        // 但它自己「创建」的项目归属是 admin_user 这个主体名，不是空串
        let (status, body) = send_json(
            app.clone(),
            "POST",
            "/api/v1/projects",
            Some("admin"),
            serde_json::json!({ "project_id": "script-project" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(
            body["owner"], CONSOLE_USER,
            "脚本建的项目归属应是 admin_user，空串会被下一次启动的认领逻辑重写"
        );

        // 普通账号拿不到脚本项目的任何东西
        let (status, _, _) = send_with_cookie(
            app,
            "GET",
            "/api/v1/projects/script-project/stats",
            Some(&alice),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
