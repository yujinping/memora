//! rmcp 协议适配层：把 `mcp::tools` 的语义实现暴露为标准 MCP 工具。
//!
//! @author yujinping
//! @intent P3：本模块是全项目**唯一**依赖 rmcp 的地方。它只做三件事：
//!         1. 用宏声明 9 个工具并生成 JSON Schema（出参 schema 由 `Json<T>` 参与推导）；
//!         2. 从请求上下文取出当前 token 对应项目的仓库集合；
//!         3. 把语义层错误原样透出为 JSON-RPC 错误。
//!         业务语义一律不留在此处，故工具行为可脱离 MCP 传输单独测试（见 `tools`）。
//!
//! 关于「仓库从哪来」：Streamable HTTP 传输会把 HTTP 请求的 `Parts`（含 Bearer 中间件
//! 注入的 `ProjectRepos`）挂到 JSON-RPC 请求的扩展上。因此工具实现无需持有任何全局
//! 状态或可变会话，就能拿到「本次请求所属项目的仓库」——这正是本服务选择**无状态**
//! 传输的收益：没有会话，也就不存在「会话被另一项目 token 复用」的越权面。

use crate::mcp::{dto, tools};
use crate::storage::ProjectRepos;
use axum::http::request::Parts;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler};

/// Memora 的 MCP 服务端。
///
/// @intent 无字段可变的轻量对象：每次请求由工厂新建一个，工具的路由表在构造时装配。
///         项目相关的状态不在对象里，而在请求扩展里（见模块文档）。
#[derive(Clone)]
pub struct MemoraServer {
    /// 由 `#[tool_router]` 生成的工具分发表
    tool_router: ToolRouter<Self>,
}

impl MemoraServer {
    /// 构造服务端（装配工具路由表）。
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// 已注册的工具名（顺序与 `tools/list` 一致），供诊断与测试使用。
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect()
    }
}

impl Default for MemoraServer {
    fn default() -> Self {
        Self::new()
    }
}

/// 从请求上下文取出本项目已解析好的仓库集合。
///
/// @intent 上下文里没有仓库只有一种可能：请求没经过 Bearer 中间件就到达了工具层，
///         属于服务端装配错误（而非调用方问题），故报 -32603 而不是 -32602。
fn repos_from(ctx: &RequestContext<RoleServer>) -> Result<ProjectRepos, ErrorData> {
    ctx.extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<ProjectRepos>())
        .cloned()
        .ok_or_else(|| {
            ErrorData::internal_error(
                "project context is missing on the MCP request".to_string(),
                None,
            )
        })
}

#[tool_router]
impl MemoraServer {
    /// 批量创建实体。同名实体已存在时忽略（不覆盖其类型），整体幂等。
    #[tool(
        description = "Create entities in this project's memory graph. \
                       Re-creating an existing name is ignored (its type is preserved). \
                       Returns the persisted state of the requested names."
    )]
    async fn create_entities(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::CreateEntitiesRequest>,
    ) -> Result<Json<dto::EntitiesResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::create_entities(&*repos.memory, req).await?))
    }

    /// 批量创建关系。(from_name, to_name, relation_type) 重复时跳过，返回实际落库的关系（含 id）。
    #[tool(
        description = "Create relations between existing entities. \
                       Duplicate (from_name, to_name, relation_type) triples are skipped. \
                       Returns the stored relations including their ids."
    )]
    async fn create_relations(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::CreateRelationsRequest>,
    ) -> Result<Json<dto::RelationsResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::create_relations(&*repos.memory, req).await?))
    }

    /// 为已存在实体追加观测；实体不存在时报 -32602。
    #[tool(
        description = "Append observations (facts) to existing entities. \
                       The entity must already exist. \
                       Returns the affected entities including the new observation ids."
    )]
    async fn add_observations(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::AddObservationsRequest>,
    ) -> Result<Json<dto::EntitiesResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::add_observations(&*repos.memory, req).await?))
    }

    /// 按名删除实体，级联清除其观测、关联关系与检索索引。
    #[tool(
        description = "Delete entities by name. Their observations, attached relations \
                       and search index entries are removed as well. \
                       Returns the names that actually existed and were deleted."
    )]
    async fn delete_entities(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::DeleteEntitiesRequest>,
    ) -> Result<Json<dto::DeleteEntitiesResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::delete_entities(&*repos.memory, req).await?))
    }

    /// 按 id 删除观测（幂等）。
    #[tool(
        description = "Delete observations by id (ids come from read_graph / search_nodes / \
                       open_nodes). Deleting a non-existent id is silently ignored."
    )]
    async fn delete_observations(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::DeleteObservationsRequest>,
    ) -> Result<Json<dto::DeleteObservationsResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(
            tools::delete_observations(&*repos.memory, req).await?,
        ))
    }

    /// 按 id 删除关系（幂等）。
    #[tool(
        description = "Delete relations by id (ids come from read_graph). \
                       Deleting a non-existent id is silently ignored."
    )]
    async fn delete_relations(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::DeleteRelationsRequest>,
    ) -> Result<Json<dto::DeleteRelationsResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::delete_relations(&*repos.memory, req).await?))
    }

    /// 返回本项目的全量记忆图（实体含观测 + 关系）。
    #[tool(
        description = "Read the whole memory graph of this project: all entities with their \
                       observations, and all relations. Start a new session with this call to \
                       restore context."
    )]
    async fn read_graph(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<dto::GraphResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::read_graph(&*repos.memory).await?))
    }

    /// 关键词检索：命中实体名、实体类型或观测内容，命中实体带出其完整观测集。
    #[tool(
        description = "Search memory by keyword. Matches entity names, entity types and \
                       observation contents. Results are ranked by relevance and each hit \
                       carries its complete observation set."
    )]
    async fn search_nodes(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::SearchNodesRequest>,
    ) -> Result<Json<dto::EntitiesResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::search_nodes(&*repos.memory, req).await?))
    }

    /// 按名展开实体；未命中的名字被跳过。
    #[tool(
        description = "Open specific entities by name. Names that do not exist are skipped. \
                       Use it when you already know which entities you need."
    )]
    async fn open_nodes(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::OpenNodesRequest>,
    ) -> Result<Json<dto::EntitiesResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::open_nodes(&*repos.memory, req).await?))
    }
}

#[tool_handler]
impl ServerHandler for MemoraServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "memora",
                env!("CARGO_PKG_VERSION").to_string(),
            ))
            .with_instructions(
                "Memora (忆庐) — a self-hosted long-term memory store for AI agents. \
                 Memory is scoped to the project bound to your bearer token. \
                 PROACTIVE WRITE: whenever the conversation reveals a user preference, a \
                 project convention, a key decision or a todo, persist it with \
                 create_entities / add_observations. \
                 PROACTIVE READ: at the start of a session, call read_graph or search_nodes \
                 to restore relevant memory before answering.",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 路由表暴露的工具必须与 `mcp::tools::TOOL_NAMES` 完全一致，
    /// 否则「文档写了 9 个工具、实际只注册 8 个」这类问题会静默存在。
    #[test]
    fn router_exposes_exactly_the_nine_declared_tools() {
        let server = MemoraServer::new();
        let mut names = server.tool_names();
        names.sort();

        let mut expected: Vec<String> = tools::TOOL_NAMES.iter().map(|s| s.to_string()).collect();
        expected.sort();

        assert_eq!(names, expected);
    }

    /// 每个工具都必须带描述与可解析的入参 schema——描述是代理决定是否调用它的全部依据。
    #[test]
    fn every_tool_advertises_description_and_input_schema() {
        let server = MemoraServer::new();
        for tool in server.tool_router.list_all() {
            let name = tool.name.to_string();
            assert!(
                tool.description.as_deref().is_some_and(|d| !d.trim().is_empty()),
                "工具 {name} 缺少描述"
            );
            assert_eq!(
                tool.input_schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "工具 {name} 的入参 schema 必须是 object"
            );
        }
    }

    /// 服务器信息需自报身份，便于客户端在工具列表页展示来源。
    #[test]
    fn server_info_identifies_memora() {
        let info = MemoraServer::new().get_info();
        assert_eq!(info.server_info.name, "memora");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(
            info.instructions
                .as_deref()
                .is_some_and(|i| i.contains("create_entities")),
            "接入指令需给出主动写入/读取的提示，否则记忆库会长期为空"
        );
    }
}
