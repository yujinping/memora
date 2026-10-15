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
    /// 批量创建实体（每个实体可一并携带观测）。同名实体已存在时忽略（不覆盖其类型），整体幂等。
    #[tool(description = "Create entities in this project's memory graph. \
                       Each entity may carry observations, which are persisted together with it. \
                       Re-creating an existing name is ignored (its type is preserved). \
                       Returns the persisted state of the requested names.")]
    async fn create_entities(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::CreateEntitiesRequest>,
    ) -> Result<Json<dto::EntitiesResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::create_entities(&*repos.memory, req).await?))
    }

    /// 批量创建关系。(from_name, to_name, relation_type) 重复时跳过，返回实际落库的关系（含 id）。
    #[tool(description = "Create relations between existing entities. \
                       Duplicate (from_name, to_name, relation_type) triples are skipped. \
                       Returns the stored relations including their ids.")]
    async fn create_relations(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::CreateRelationsRequest>,
    ) -> Result<Json<dto::RelationsResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::create_relations(&*repos.memory, req).await?))
    }

    /// 为已存在实体追加观测；实体不存在时报 -32602。
    #[tool(description = "Append observations (facts) to existing entities. \
                       The entity must already exist. \
                       Returns the affected entities including the new observation ids.")]
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
        Ok(Json(tools::delete_observations(&*repos.memory, req).await?))
    }

    /// 按 id 删除关系（幂等）。
    #[tool(description = "Delete relations by id (ids come from read_graph). \
                       Deleting a non-existent id is silently ignored.")]
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

    /// 保存一条工作交接（检查点），供另一个客户端 / 机器接手。
    #[tool(
        description = "Save a work handoff at a checkpoint, so another client or machine can \
                       continue this work. Pass client set to your own name (e.g. \"zed\", \
                       \"workbuddy\"): it keeps your pending handoff separate from other \
                       clients'. Omit task_id to continue the workstream you most recently \
                       saved; pass back the task_id returned by handoff_load / handoff_save \
                       when you are continuing that same workstream. If the work item you are \
                       handing off is NOT the one you loaded, give it a short task_id of your \
                       own — never ask the user to invent one. Returns the stored revision and \
                       its final task_id."
    )]
    async fn handoff_save(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::HandoffSaveRequest>,
    ) -> Result<Json<dto::HandoffSaveResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::handoff_save(&*repos.handoff, req).await?))
    }

    /// 读取一条交接：不给 task_id 就是「接手当前待接手的那条」。
    #[tool(
        description = "Load a handoff to resume work. Omit task_id to load the pending one: if \
                       exactly one is pending you get it, if several are pending you get their \
                       summaries (status=\"ambiguous\") and must pick one by task_id. \
                       status=\"none\" means nothing is waiting — that is a normal state, not \
                       an error. Reading never marks anything as taken."
    )]
    async fn handoff_load(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::HandoffLoadRequest>,
    ) -> Result<Json<dto::HandoffLoadResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::handoff_load(&*repos.handoff, req).await?))
    }

    /// 列出待接手的交接（接手选择界面的数据来源）。
    #[tool(
        description = "List handoffs waiting to be taken over, newest first, with a recognizable \
                       summary (goal, state excerpt, client, session label, time). Defaults to \
                       pending ones only; pass unacked_only=false to look at history. This call \
                       has no side effects — listing never marks work as taken."
    )]
    async fn handoff_list(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::HandoffListRequest>,
    ) -> Result<Json<dto::HandoffListResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::handoff_list(&*repos.handoff, req).await?))
    }

    /// 标记接手（CAS）：会从**其他所有客户端**的待接手列表里移除这一条。
    #[tool(
        description = "Mark a handoff as taken over. This removes the entry from every other \
                       client's pending list, so only call it once the user has agreed to take \
                       that work. Omit task_id to take the single pending handoff; if several \
                       are pending the call returns the candidates and takes nothing \
                       (won=false). won=false with a handoff means someone else took it first — \
                       report that instead of proceeding."
    )]
    async fn handoff_ack(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::HandoffAckRequest>,
    ) -> Result<Json<dto::HandoffAckResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::handoff_ack(&*repos.handoff, req).await?))
    }

    /// 追加一轮对话到会话 transcript（`turn_index` 由服务端分配并回读）。
    #[tool(
        description = "Append one turn (role + content) to a session transcript. The server \
                       assigns the turn index and returns it in the result — never number the \
                       turns yourself. Use this when the full conversation detail (not just a \
                       summary) must survive to another machine; for checkpoint-style handoff \
                       prefer handoff_save."
    )]
    async fn log_turn(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::LogTurnRequest>,
    ) -> Result<Json<dto::TurnView>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::log_turn(&*repos.conversation, req).await?))
    }

    /// 拉回完整会话 transcript（轮次升序）。
    #[tool(
        description = "Fetch the full transcript of a session, turns in order. A session that \
                       was never written returns an empty list — that is a normal answer, not \
                       an error. Combine with list_sessions to find which sessions exist."
    )]
    async fn get_session(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::GetSessionRequest>,
    ) -> Result<Json<dto::GetSessionResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::get_session(&*repos.conversation, req).await?))
    }

    /// 列出会话标识（最近活跃降序，可按前缀过滤）。
    #[tool(
        description = "List session ids, most recently active first. Pass prefix to narrow the \
                       list. This call has no side effects; pick a session id from the result \
                       and fetch it with get_session."
    )]
    async fn list_sessions(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(req): Parameters<dto::ListSessionsRequest>,
    ) -> Result<Json<dto::ListSessionsResponse>, ErrorData> {
        let repos = repos_from(&ctx)?;
        Ok(Json(tools::list_sessions(&*repos.conversation, req).await?))
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
                 to restore relevant memory before answering. \
                 HANDOFF RESUME: at the start of a session, call handoff_list (unacked only) \
                 to see what is waiting. Nothing pending: say nothing about it. One pending: \
                 mention it in one line and take it over once the user agrees. Several \
                 pending: show the summaries and let the user choose — or pick silently when \
                 the user's request already makes it obvious. If the user only wants to know \
                 what is pending, list and stop. Only handoff_ack marks work as taken, and \
                 that removes the entry from every other client's pending list: never ack \
                 before the user has agreed to take that work. Never ask the user for an id. \
                 HANDOFF SAVE: when a task reaches a meaningful checkpoint, or the user says \
                 they will continue from another client or machine, call handoff_save with \
                 goal/state/next_steps and pass client set to your own name (e.g. \"zed\", \
                 \"workbuddy\") — it keeps your pending handoff separate from other clients'. \
                 Omit task_id to continue the workstream you most recently saved — pass back \
                 the task_id returned by handoff_load / handoff_save when you are continuing \
                 that same workstream. If the work item you are handing off is NOT the one you \
                 loaded, give it a short task_id of your own; never ask the user to invent one. \
                 SESSION TRANSCRIPTS: when the user wants the full conversation (not just a \
                 summary) to survive to another machine, log each turn with log_turn — the \
                 server assigns turn indices, never number them yourself. On the new machine, \
                 list_sessions to find the session, then get_session to replay it verbatim.",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 路由表暴露的工具必须与 `mcp::tools::TOOL_NAMES` 完全一致，
    /// 否则「文档写了 13 个工具、实际只注册 12 个」这类问题会静默存在。
    #[test]
    fn router_exposes_exactly_the_declared_tools() {
        let server = MemoraServer::new();
        let mut names = server.tool_names();
        names.sort();

        let mut expected: Vec<String> = tools::TOOL_NAMES.iter().map(|s| s.to_string()).collect();
        expected.sort();

        assert_eq!(names, expected);
        assert_eq!(names.len(), 16, "9 记忆 + 4 交接 + 3 会话");
    }

    /// 每个工具都必须带描述与可解析的入参 schema——描述是代理决定是否调用它的全部依据。
    #[test]
    fn every_tool_advertises_description_and_input_schema() {
        let server = MemoraServer::new();
        for tool in server.tool_router.list_all() {
            let name = tool.name.to_string();
            assert!(
                tool.description
                    .as_deref()
                    .is_some_and(|d| !d.trim().is_empty()),
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

    /// 回归 #7「零安装自足性」：`instructions` 必须自带判方向所需的全部措辞。
    ///
    /// @intent 这条防的是一种隐蔽退化：把关键规则挪进可选的 skill 文件（它需要落盘才能
    ///         被发现、且在支持 skills 的客户端里也是按需加载），于是**所有测试照样绿、
    ///         零安装路径却已失效**。四条措辞分别对应四类必需行为：会话开始先看待接手、
    ///         检查点/换机器要保存、只想看看则只读、绝不向人要 ID。
    #[test]
    fn instructions_are_self_sufficient_for_the_install_free_path() {
        let info = MemoraServer::new().get_info();
        let text = info.instructions.unwrap_or_default();
        for (needle, why) in [
            ("handoff_list", "会话开始先看有无待接手"),
            ("handoff_save", "检查点 / 换机器时要保存"),
            ("list and stop", "只想看看时只读、不改状态"),
            ("Never ask the user for an id", "绝不向用户索要 ID"),
        ] {
            assert!(
                text.contains(needle),
                "instructions 缺少「{why}」所需的措辞：{needle}"
            );
        }
    }

    /// 技能文件（可选质量层）的机械属性：存在、frontmatter 可解析、
    /// `name` / `description` 非空且 `description` 以 `Use when` 起头
    /// （对起头有约定才能在对的时机被客户端按需加载，docs §5.6.2、§11）。
    ///
    /// @intent 技能行为本身无法被服务端单测覆盖——它是给模型看的文本；
    ///         能钉的只有「文件存在且格式正确」这一层。
    ///
    /// **双模（§11 回归 #7 的可执行形态）**：`skills/` 整个缺席时本测试不失败，
    /// 转而断言零安装路径（instructions 自足）仍成立——skill 是可选增强，
    /// 删掉它交接功能必须完全可用，这条回归必须能真的跑。常规 checkout 下
    /// 文件随仓库存在，走严格 frontmatter 校验，缺字段照样红。
    #[test]
    fn skill_file_exists_with_valid_frontmatter() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/memora-handoff/SKILL.md");
        let Ok(text) = std::fs::read_to_string(&path) else {
            // 零安装分支：skill 缺席是允许的形态，此时唯一要求是
            // instructions（随连接即得的通道）自足。
            let instructions = MemoraServer::new()
                .get_info()
                .instructions
                .unwrap_or_default();
            assert!(
                instructions.contains("Never ask the user for an id"),
                "skill 缺席时 instructions 必须仍自足（零安装路径）"
            );
            return;
        };

        let body = text
            .strip_prefix("---\n")
            .and_then(|t| t.split_once("\n---"))
            .unwrap_or_else(|| panic!("SKILL.md 应以 `---\\n...\\n---` frontmatter 开头"))
            .0;
        let mut name = None;
        let mut description = None;
        for line in body.lines() {
            if let Some(v) = line.strip_prefix("name:") {
                name = Some(v.trim().to_string());
            }
            if let Some(v) = line.strip_prefix("description:") {
                description = Some(v.trim().to_string());
            }
        }
        assert!(
            name.is_some_and(|n| !n.is_empty()),
            "frontmatter 缺少非空 name"
        );
        let description = description.expect("frontmatter 缺少 description");
        assert!(
            description.starts_with("Use when"),
            "description 必须以 `Use when` 起头才能在对的时机被加载，实际：{description}"
        );
    }
}
