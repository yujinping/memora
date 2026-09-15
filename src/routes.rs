//! 路由装配与 HTTP handler。
//!
//! @author yujinping
//! @intent P3：`/mcp` 由统计占位替换为真实的 MCP Streamable HTTP 服务。
//!         装配顺序刻意保持「鉴权中间件在协议层之前」——未授权请求在进入 MCP 状态机
//!         之前就被拒掉，协议层因此可以完全无状态，不必为每个请求维护身份。
//!         handler 依旧只从请求扩展取 `ProjectRepos`（trait 对象），不引用任何具体后端。
//!         P4：`/api/v1` 补齐项目生命周期（增 / 删 / 统计），handler 实现在 `admin` 模块。

use crate::admin;
use crate::auth::{admin_auth, bearer_auth};
use crate::mcp;
use crate::meta;
use crate::reply::internal_error;
use crate::state::AppState;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use sea_orm::DatabaseConnection;

/// 健康检查（无需鉴权）。
async fn health() -> &'static str {
    "ok"
}

/// 管理员：项目列表。
async fn list_projects(meta: DatabaseConnection) -> Response {
    match meta::list_projects(&meta).await {
        Ok(projects) => Json(projects).into_response(),
        Err(err) => internal_error(&err),
    }
}

/// 装配路由：
/// - `/health` 公开；
/// - `/mcp` 为 MCP Streamable HTTP 端点，需 Bearer（按 token 隔离到项目）；
/// - `/api/v1` 需 ADMIN_TOKEN：项目列表 / 创建 / 注销 / 统计。
///
/// 鉴权中间件通过 `from_fn_with_state` 直接持有 `AppState`，使整棵路由树状态一致（Router<()>），
/// 避免 Axum 对各子路由 `State<S>` 的推断冲突。MCP 服务作为嵌套服务挂载，
/// 中间件先于它执行并把 `ProjectRepos` 写入请求扩展，rmcp 再把该扩展透传给工具。
/// 管理面 handler 以 `State<AppState>` 取依赖（先 `route_layer` 挂鉴权，再 `with_state`
/// 落到 `Router<()>`），与 MCP 侧 `Router<()>` 的形态保持一致。
pub fn create_app(state: AppState) -> Router {
    // MCP 端点：Bearer 鉴权 → 注入 ProjectRepos → 交给 rmcp 协议层
    let mcp_routes = Router::new()
        .nest_service("/mcp", mcp::build_service(&state.config))
        .route_layer(middleware::from_fn_with_state(state.clone(), bearer_auth));

    // 管理路由：ADMIN_TOKEN 鉴权；项目列表的 db 连接经闭包注入，避免与路由状态耦合
    let meta_conn = state.meta.clone();
    let api_routes = Router::new()
        .route(
            "/projects",
            get(move || {
                let conn = meta_conn.clone();
                async move { list_projects(conn).await }
            })
            .post(admin::create_project),
        )
        .route("/projects/{id}", delete(admin::delete_project))
        .route("/projects/{id}/stats", get(admin::project_stats))
        .route_layer(middleware::from_fn_with_state(state.clone(), admin_auth))
        .with_state(state.clone());

    // 健康检查（公开）
    Router::new()
        .route("/health", get(health))
        .merge(mcp_routes)
        .nest("/api/v1", api_routes)
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
    use std::sync::Arc;
    use tower::ServiceExt;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

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
                },
            )
            .await
            .unwrap();
        }

        let config = Config::for_test(0, data_dir.to_string_lossy(), "admin", "sqlite_file");
        let state = AppState {
            meta: db,
            storage: Arc::new(StorageRegistry::from_config(&config).unwrap()),
            config,
        };
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
    async fn rpc(app: Router, token: Option<&str>, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
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

    async fn get_with_token(app: Router, uri: &str, token: Option<&str>) -> (StatusCode, serde_json::Value) {
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
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// 未授权请求必须在进入 MCP 协议层之前被拒（无 token 时连 Host 校验都不该走到）。
    #[tokio::test]
    async fn mcp_without_token_is_401() {
        let (app, _dir) = setup().await;
        let (status, _) = mcp_send(app, None, "localhost", Some(request(1, "tools/list", serde_json::json!({})))).await;
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
        let (status, _) = rpc(app, Some("wrong"), request(1, "tools/list", serde_json::json!({}))).await;
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
        assert_eq!(body["result"]["capabilities"]["tools"], serde_json::json!({}));
    }

    /// `tools/list` 暴露的工具必须与文档承诺的 9 个一致。
    #[tokio::test]
    async fn mcp_tools_list_exposes_the_nine_tools() {
        let (app, _dir) = setup().await;
        let (status, body) = rpc(app, Some("secret"), request(2, "tools/list", serde_json::json!({}))).await;

        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        let mut names: Vec<String> = body["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("tools 不是数组：{body}"))
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names.len(), 9, "响应体：{body}");
        names.sort();

        let mut expected: Vec<String> = mcp::tools::TOOL_NAMES.iter().map(|s| s.to_string()).collect();
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

        let (_, graph) = rpc(app, Some("secret"), tool_call(5, "read_graph", serde_json::json!({}))).await;
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
            tool_call(1, "create_entities", serde_json::json!({ "entities": [{ "name": "alpha" }] })),
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
        let obs_id = obs["id"].as_i64().unwrap_or_else(|| panic!("未回读观测 id：{added}"));
        assert_eq!(obs["content"], "偏好深色主题");
        assert_eq!(
            structured(&added)["entities"][0]["entity_type"], "unknown",
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
            tool_call(4, "delete_observations", serde_json::json!({ "ids": [obs_id] })),
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

        let (status, body) = rpc(app, Some("secret"), tool_call(1, "read_graph", serde_json::json!({}))).await;
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

        let (_, demo) = rpc(app.clone(), Some("secret"), tool_call(1, "read_graph", serde_json::json!({}))).await;
        let (_, other) = rpc(app, Some("mem-token"), tool_call(2, "read_graph", serde_json::json!({}))).await;

        assert_eq!(structured(&demo)["entities"].as_array().unwrap().len(), 1);
        assert!(
            structured(&other)["entities"].as_array().unwrap().is_empty(),
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
        assert!(
            !raw.is_empty(),
            "前置：元库应已写入（否则本断言是空断言）"
        );
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

        let (status, body) = get_with_token(app, "/api/v1/projects/custom/stats", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        assert_eq!(body["backend"], "in_mem");
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

        let (status, body) = delete_with_token(app.clone(), "/api/v1/projects/doomed", Some("admin")).await;
        assert_eq!(status, StatusCode::OK, "响应体：{body}");
        assert_eq!(body["data_removed"], true);

        let (status, _) = rpc(app.clone(), Some(&token), request(1, "tools/list", serde_json::json!({}))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "注销后原 token 必须立即失效");
        assert!(!file.exists(), "项目库文件必须被删除");
        assert!(!dir.join("doomed").exists(), "项目目录必须被删除");

        let (status, _) = delete_with_token(app.clone(), "/api/v1/projects/doomed", Some("admin")).await;
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
            .create_relations(vec![crate::domain::RelationInput::new("s-a", "s-b", "knows")])
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

        let (status, body) = get_with_token(app, "/api/v1/projects/stats-proj/stats", Some("admin")).await;
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
        let (status, body) = get_with_token(app, "/api/v1/projects/ghost/stats", Some("admin")).await;
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
}
