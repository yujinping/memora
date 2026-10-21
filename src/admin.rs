//! REST 管理面：项目生命周期（创建 / 注销 / 用量统计）、账号与交接。
//!
//! @author yujinping
//! @intent P4：把「项目登记」从手工改元库提升为受 `ADMIN_TOKEN` 保护的接口。
//!         分层上与 MCP 工具层同构——本模块只依赖 `StorageRegistry` 与
//!         `MemoryRepository` trait，不引用任何具体后端；「项目的物理数据在哪、
//!         怎么删」由后端自己回答（`StorageBackend::drop_project`），
//!         因此新增后端时本文件无需改动。
//!
//! @intent P8 多租户：本模块是**授权收敛点**。所有项目类 handler 都经
//!         [`visible_project`] 取得登记行，它把「这个人能不能动这个项目」收敛成
//!         一处判断；所有账号类 handler 都在开头调 `identity.require_admin()`。
//!         这样做的理由是可验证的：越权几乎从不源于「判断写错」，而源于
//!         「某个 handler 忘了判断」。把判断放进必经路径后，漏写就变成编译错误
//!         （缺少身份提取器）或类型错误（拿不到 ProjectRecord）。
//!
//! 状态码约定（与 `reply` 模块一致）：
//! - 400：入参可修正（非法 project_id、后端未编译进本构建、弱口令）
//! - 403：凭据有效但角色不足（普通账号访问账号管理 / 改他人口令）
//! - 404：项目不存在**或不属于当前账号**（两种情况刻意合并，见 [`visible_project`]）
//! - 409：project_id 已被占用 / 拒绝删除最后一个管理员
//! - 500：服务端故障（元库 / 后端不可用、熵源不可用）

use crate::auth::{self, AdminIdentity};
use crate::error::StorageError;
use crate::meta::{self, NewProject, ProjectRecord};
use crate::reply;
use crate::state::AppState;
use crate::storage::{validate_project_id, BackendKind, ProjectRepos};
use crate::users::{self, Role};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// 自动生成 project_id 时使用的随机字节数（6 字节 → 12 位十六进制）。
///
/// @intent 48 bit 对「一个自托管实例的项目数量」而言碰撞概率可忽略，
///         同时短到便于在配置里手抄。
const GENERATED_ID_BYTES: usize = 6;

/// 自动生成 project_id 的前缀。
///
/// @intent 与用户自定义 id 区分，便于运维识别「这是系统发的」；
///         且满足存储层对 project_id 的字符集约束（`[A-Za-z0-9_-]`）。
const GENERATED_ID_PREFIX: &str = "p_";

/// 创建项目请求体（两个字段均可省略）。
#[derive(Debug, Deserialize)]
pub struct CreateProjectRequest {
    /// 项目标识；省略时自动生成 `p_<12 位十六进制>`
    #[serde(default)]
    pub project_id: Option<String>,
    /// 存储后端标识；省略时取配置项 `STORAGE_BACKEND`
    #[serde(default)]
    pub backend: Option<String>,
}

/// 创建项目响应体。
///
/// @intent `token` 是全流程中唯一一次明文出现的机会——库内只存 SHA-256，
///         之后无法找回，只能重新创建项目。
#[derive(Debug, Serialize)]
pub struct CreateProjectResponse {
    /// 项目标识
    pub project_id: String,
    /// 明文项目 token（仅此一次）
    pub token: String,
    /// 生效的存储后端标识（规范化后）
    pub backend: String,
    /// 归属账号（即创建者；管理员可代建，此时为管理员自己）
    pub owner: String,
}

/// 注销项目响应体。
#[derive(Debug, Serialize)]
pub struct DeleteProjectResponse {
    /// 项目标识
    pub project_id: String,
    /// 是否确实删除了持久化数据；后端未注册时为 `false`（如实报告，不假装成功）
    pub data_removed: bool,
}

/// 轮换项目 token 的响应体。
///
/// @intent 形状与 [`CreateProjectResponse`] 刻意保持一致（`project_id` / `token` /
///         `backend` / `owner`），使控制台可以复用同一段「一次性展示 token」的界面。
///         唯一的语义差别是 `token` 是**新**的、旧 token 已立即失效。
#[derive(Debug, Serialize)]
pub struct RotateTokenResponse {
    /// 项目标识
    pub project_id: String,
    /// 新明文 token（仅此一次；旧 token 已失效）
    pub token: String,
    /// 该项目使用的存储后端标识
    pub backend: String,
    /// 归属账号
    pub owner: String,
}

/// 轮换项目 token：生成新 token → 覆盖摘要 → 返回明文（仅此一次）。
///
/// @intent 与「删除 + 重建项目」的区别是本接口存在的全部理由：重建会连同记忆一起
///         丢掉，而轮换只换凭据。token 泄漏时的正确处置是轮换，不是销毁数据。
///
/// @intent 不做「旧 token 宽限期」：一次轮换之后旧 token 立刻 401。宽限期看起来
///         更平滑，实际是把「泄漏的 token 还能用一段时间」做成默认行为——
///         而用户按下「轮换」时的预期恰恰相反。
///
/// @intent 租户可以轮换**自己**项目的 token：token 泄漏的责任人正是项目的
///         使用者，要求他去找管理员才能在事故中止血，是把权限设计成了故障源。
pub async fn rotate_project_token(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path(project_id): Path<String>,
) -> Response {
    if let Err(err) = validate_project_id(&project_id) {
        return reply::bad_request("invalid_request", &err.to_string());
    }

    let record = match visible_project(&state, &identity, &project_id).await {
        Ok(record) => record,
        Err(response) => return *response,
    };

    let token = match auth::generate_token() {
        Ok(token) => token,
        Err(err) => return reply::internal_error(&err),
    };

    match meta::update_token(&state.meta, &project_id, &token).await {
        Ok(true) => {}
        // 查到了又更新不到：另一个请求刚删掉了它。按 404 报告，与「本来就没有」同解。
        Ok(false) => return project_not_found(&project_id),
        Err(err) => return reply::internal_error(&err),
    }

    tracing::info!(project_id = %project_id, user = %identity.user(), "project token rotated");
    Json(RotateTokenResponse {
        project_id,
        token,
        backend: record.backend,
        owner: record.owner,
    })
    .into_response()
}

// ---------------------------------------------------------------- 控制台账号（P7）

/// 创建账号请求体。
#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    /// 登录名（不得含空白，见 `users::validate_username`）
    pub username: String,
    /// 明文口令（仅在本次请求的内存中存在，入库前哈希）
    pub password: String,
    /// 角色：`admin` / `user`；省略时按普通账号创建
    ///
    /// @intent 省略时取**普通账号**（与 `users::create` 的缺省一致）：建账号是
    ///         高频动作（给每个团队成员开号），建管理员是低频且需要额外确认的动作。
    ///         让高频动作的缺省值落在低权限上，可以把「顺手多开了个管理员」
    ///         这类事故的概率压到零。
    #[serde(default)]
    pub role: Option<String>,
}

/// 修改口令请求体。
#[derive(Debug, Deserialize)]
pub struct UpdatePasswordRequest {
    /// 新明文口令
    pub password: String,
}

/// 修改角色请求体。
#[derive(Debug, Deserialize)]
pub struct UpdateRoleRequest {
    /// 新角色：`admin` / `user`
    pub role: String,
}

/// 账号的对外形态。
///
/// @intent **不含** `password_hash`：它属于「永远不该出现在响应里」的字段，
///         而漏掉一次就是一个离线爆破的起点。单独定义视图而不是给
///         `users::UserRecord` 加 `Serialize`，是为了让「加一个字段」不会
///         顺手把哈希也带上——那样必须显式改这里。
#[derive(Debug, Serialize)]
pub struct UserView {
    /// 登录名
    pub username: String,
    /// 角色标识（`admin` / `user`）
    pub role: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 最近一次口令变更时间（Unix 秒）
    pub updated_at: i64,
}

impl From<users::UserRecord> for UserView {
    fn from(record: users::UserRecord) -> Self {
        Self {
            username: record.username,
            role: record.role.as_str().to_string(),
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

/// 账号列表（仅管理员）。
pub async fn list_users(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
) -> Response {
    if let Err(response) = identity.require_admin() {
        return *response;
    }
    match users::list(&state.meta).await {
        Ok(records) => Json(records.into_iter().map(UserView::from).collect::<Vec<_>>())
            .into_response(),
        Err(err) => reply::internal_error(&err),
    }
}

/// 解析并校验请求体里的角色标识。
///
/// @intent 未知角色返回 400 而不是回退到缺省值：调用方显式写了 `role`，
///         拼错成 `"adminn"` 却得到一个普通账号，会让「为什么他看不到数据」
///         变成一次无从下手的排查。
///
/// @intent 这里刻意**逐字匹配**，不复用 [`crate::users::Role::parse`]：后者为
///         「读库 / 读配置」而设计，容忍首尾空白与大小写（手工编辑过的存量值
///         不该让整个管理面失效）。但在**入参**位置上，宽容的代价是不对称的——
///         `"admin "` 或 `"Admin"` 被接受就等于把一个拼错的值升级成最高权限，
///         而调用方永远收不到「我写错了」的反馈。读路径宽容、写路径严格，
///         是这两处判定必须分开的全部理由（有对应用例钉住）。
///
/// @intent 错误侧装箱，理由同 [`crate::auth::AdminIdentity::require_admin`]。
fn parse_role(raw: &str) -> Result<Role, Box<Response>> {
    Role::ALL
        .into_iter()
        .find(|role| role.as_str() == raw)
        .ok_or_else(|| {
            Box::new(reply::bad_request(
                "invalid_request",
                &format!(
                    "`role` 只接受 {:?}，实际 `{raw}`",
                    Role::ALL.map(|r| r.as_str())
                ),
            ))
        })
}

/// 创建账号（201，仅管理员）。
///
/// @intent 口令强度在**能拿到明文的唯一时刻**校验（与 `config` 模块对
///         `--admin-password` 的处置同源）：入库存的是哈希，之后再无从判断强度。
///         弱口令不会让任何检查失败，只会在某天被爆破。
pub async fn create_user(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Json(req): Json<CreateUserRequest>,
) -> Response {
    if let Err(response) = identity.require_admin() {
        return *response;
    }

    let username = match users::validate_username(&req.username) {
        Ok(name) => name,
        Err(reason) => return reply::bad_request("invalid_request", &reason),
    };

    let role = match req.role.as_deref() {
        Some(raw) => match parse_role(raw) {
            Ok(role) => role,
            Err(response) => return *response,
        },
        None => Role::default(),
    };

    if let Some(reason) = crate::password::weak_password_reason(&req.password) {
        return reply::bad_request("weak_password", &reason);
    }

    // 先查后插：与 `create_project` 同一取向——在此拦下能给出 409，而唯一约束
    // 在库层兜底（并发插入时后到者得到 500，而不是静默覆盖原口令）。
    match users::find(&state.meta, &username).await {
        Ok(Some(_)) => {
            return reply::conflict(
                "user_exists",
                &format!("user `{username}` already exists"),
            )
        }
        Ok(None) => {}
        Err(err) => return reply::internal_error(&err),
    }

    let hash = match crate::password::hash_password(&req.password) {
        Ok(hash) => hash,
        Err(err) => return reply::internal_error(&err),
    };
    if let Err(err) = users::create_with_role(&state.meta, &username, &hash, role).await {
        return reply::internal_error(&err);
    }

    tracing::info!(
        user = %username,
        role = role.as_str(),
        by = %identity.user(),
        "console user created"
    );
    match users::find(&state.meta, &username).await {
        Ok(Some(record)) => (StatusCode::CREATED, Json(UserView::from(record))).into_response(),
        Ok(None) => reply::internal_error(&"user vanished right after being created"),
        Err(err) => reply::internal_error(&err),
    }
}

/// 删除账号（仅管理员）。
///
/// @intent **拒绝删除最后一个管理员**：删掉它会让「账号管理」永久无人可用，
///         而唯一的补救手段是手工改库或删掉 `_meta.db`——后者会连带
///         丢掉全部项目登记。注意判据是「管理员数」而不是「账号数」：
///         剩一群普通账号时删掉最后一个管理员，管理面同样是锁死的。
///
/// @intent 顺带**销毁该账号的所有会话**：不这么做的话，被删账号手里的 Cookie
///         还能继续用到会话过期（最长 12 小时），「删账号」看起来生效了一半。
///
/// @intent 不自动处理其名下的项目：项目归属会指向一个不存在的账号，于是
///         只有管理员能看到它们（[`AdminIdentity::can_access`] 里空串与
///         不存在用户名都不等于任何租户）。这是**刻意的保守选择**——
///         自动转交或自动删除都是在替用户做不可逆的决定。控制台上会把这类项目
///         标为「归属已失效」，提示管理员处置。
pub async fn delete_user(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path(username): Path<String>,
) -> Response {
    if let Err(response) = identity.require_admin() {
        return *response;
    }

    let target = match users::find(&state.meta, &username).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return reply::not_found("not_found", &format!("user `{username}` not found"))
        }
        Err(err) => return reply::internal_error(&err),
    };

    if target.role.is_admin() {
        match users::count_admins(&state.meta).await {
            Ok(total) if total <= 1 => {
                return reply::conflict(
                    "last_admin",
                    "refusing to delete the only administrator: it would leave no way to \
                     manage accounts; promote another account first",
                )
            }
            Ok(_) => {}
            Err(err) => return reply::internal_error(&err),
        }
    }

    match users::delete(&state.meta, &username).await {
        Ok(true) => {
            let removed = state.sessions.remove_user(&username);
            tracing::info!(
                user = %username,
                sessions = removed,
                by = %identity.user(),
                "console user deleted"
            );
            Json(json!({ "username": username, "sessions_revoked": removed })).into_response()
        }
        Ok(false) => reply::not_found("not_found", &format!("user `{username}` not found")),
        Err(err) => reply::internal_error(&err),
    }
}

/// 修改口令。
///
/// @intent 管理员可改任何人；普通账号**只能改自己**。后者不是「多一层保护」，
///         而是这条接口对普通账号存在的唯一理由——否则他连自己的口令都换不了，
///         只能找管理员代设（那等于让管理员知道所有人的口令）。
///
/// @intent 改口令**不踢掉已有会话**（与删除账号相反）：用户改口令的动机通常是
///         「想换一个」，而不是「怀疑被入侵」。若怀疑被入侵，正确动作是删除账号；
///         把两者混在一起会让「改个口令」变成一次强制重新登录，收益为零。
pub async fn update_user_password(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path(username): Path<String>,
    Json(req): Json<UpdatePasswordRequest>,
) -> Response {
    if !identity.is_admin() && identity.user() != username {
        return reply::forbidden(
            "role_required",
            "only administrators can change another account's password",
        );
    }

    if let Some(reason) = crate::password::weak_password_reason(&req.password) {
        return reply::bad_request("weak_password", &reason);
    }
    let hash = match crate::password::hash_password(&req.password) {
        Ok(hash) => hash,
        Err(err) => return reply::internal_error(&err),
    };

    match users::update_password(&state.meta, &username, &hash).await {
        Ok(true) => {
            tracing::info!(user = %username, by = %identity.user(), "console password updated");
            Json(json!({ "username": username })).into_response()
        }
        Ok(false) => reply::not_found("not_found", &format!("user `{username}` not found")),
        Err(err) => reply::internal_error(&err),
    }
}

/// 修改角色（仅管理员）。
///
/// @intent **拒绝把最后一个管理员降级**：与删除同一个理由——管理面会永久锁死。
///
/// @intent 改角色后立刻刷新该账号全部会话的角色快照（[`crate::session::SessionStore::refresh_role`]）：
///         降权若滞后 12 小时，等于「权限变更没生效」；提权滞后则让用户对着
///         一个「说我是管理员但什么也看不到」的界面反复刷新。
#[derive(Debug, Serialize)]
pub struct UpdateRoleResponse {
    /// 登录名
    pub username: String,
    /// 新角色标识
    pub role: String,
    /// 被刷新角色快照的会话数（0 表示对方当前未登录）
    pub sessions_updated: usize,
}

pub async fn update_user_role(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path(username): Path<String>,
    Json(req): Json<UpdateRoleRequest>,
) -> Response {
    if let Err(response) = identity.require_admin() {
        return *response;
    }

    let role = match parse_role(&req.role) {
        Ok(role) => role,
        Err(response) => return *response,
    };

    let target = match users::find(&state.meta, &username).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return reply::not_found("not_found", &format!("user `{username}` not found"))
        }
        Err(err) => return reply::internal_error(&err),
    };

    if target.role.is_admin() && !role.is_admin() {
        match users::count_admins(&state.meta).await {
            Ok(total) if total <= 1 => {
                return reply::conflict(
                    "last_admin",
                    "refusing to demote the only administrator: it would leave no way to \
                     manage accounts; promote another account first",
                )
            }
            Ok(_) => {}
            Err(err) => return reply::internal_error(&err),
        }
    }

    match users::update_role(&state.meta, &username, role).await {
        Ok(true) => {
            let sessions_updated = state.sessions.refresh_role(&username, role);
            tracing::info!(
                user = %username,
                role = role.as_str(),
                sessions = sessions_updated,
                by = %identity.user(),
                "console user role updated"
            );
            Json(UpdateRoleResponse {
                username,
                role: role.as_str().to_string(),
                sessions_updated,
            })
            .into_response()
        }
        Ok(false) => reply::not_found("not_found", &format!("user `{username}` not found")),
        Err(err) => reply::internal_error(&err),
    }
}

/// 项目用量统计。
#[derive(Debug, Serialize)]
pub struct ProjectStats {
    /// 项目标识
    pub project_id: String,
    /// 该项目使用的存储后端标识
    pub backend: String,
    /// 实体数
    pub entities: usize,
    /// 关系数
    pub relations: usize,
    /// 观测总数（跨实体累加）
    pub observations: usize,
    /// 有交接的任务数（不区分是否已接手）
    pub handoffs: usize,
}

/// 一张观测的对外形态。
#[derive(Debug, Serialize)]
pub struct ObservationView {
    /// 观测主键（可据此调用 `delete_observations`）
    pub id: i64,
    /// 事实内容
    pub content: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 写入来源标识，空串表示未声明
    pub source: String,
    /// 所属实体名
    pub entity_name: String,
}

/// 一个实体的对外形态（含其观测）。
#[derive(Debug, Serialize)]
pub struct EntityView {
    /// 实体名
    pub name: String,
    /// 实体类型
    pub entity_type: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 登记来源标识，空串表示未声明
    pub source: String,
    /// 该实体的观测，按 id 升序
    pub observations: Vec<ObservationView>,
}

/// 一条关系的对外形态。
#[derive(Debug, Serialize)]
pub struct RelationView {
    /// 关系主键
    pub id: i64,
    /// 起点实体名
    pub from_name: String,
    /// 终点实体名
    pub to_name: String,
    /// 关系类型
    pub relation_type: String,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 写入来源标识，空串表示未声明
    pub source: String,
}

/// 全图快照的对外形态（网页控制台的「记忆浏览」用它）。
///
/// @intent 单独定义视图结构而不给领域模型加 `Serialize`：领域模型是存储层与工具层
///         共用的内部契约，把它的字段直接暴露成 HTTP 响应，等于让「改一个字段名」
///         同时成为一次不兼容的 API 变更。视图结构把这两件事解耦，代价是几行映射代码。
#[derive(Debug, Serialize)]
pub struct GraphView {
    /// 项目标识
    pub project_id: String,
    /// 实体（含观测），按 name 升序
    pub entities: Vec<EntityView>,
    /// 关系，按 id 升序
    pub relations: Vec<RelationView>,
}

/// 创建项目：校验 → 发 token → 登记（返回 201）。
///
/// @intent 归属取**当前登录者**，不接受请求体指定 owner：让调用方自选归属，
///         就等于允许普通账号把项目塞到别人名下（或把别人的名字写进自己的
///         项目），而「代建项目」本身没有真实需求——管理员要给别人建项目时，
///         让对方自己登录建一次更简单，也不会留下归属与责任不一致的数据。
pub async fn create_project(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Json(req): Json<CreateProjectRequest>,
) -> Response {
    let project_id = match req.project_id {
        Some(id) => id.trim().to_string(),
        None => match auth::random_hex(GENERATED_ID_BYTES) {
            Ok(suffix) => format!("{GENERATED_ID_PREFIX}{suffix}"),
            Err(err) => return reply::internal_error(&err),
        },
    };

    // 管理面第一道校验：与存储层的 `validate_project_id` 同一套规则，
    // 但在此拦下可给出 400 而非等到写文件时才失败。
    if let Err(err) = validate_project_id(&project_id) {
        return reply::bad_request("invalid_request", &err.to_string());
    }

    let requested = req
        .backend
        .unwrap_or_else(|| state.config.storage_backend.clone());
    let kind = match BackendKind::parse(&requested) {
        Ok(kind) => kind,
        Err(err) => return unavailable_backend(&state, &err),
    };
    let backend = match state.storage.get(kind) {
        Ok(backend) => backend,
        Err(err) => return unavailable_backend(&state, &err),
    };

    // 先查后插：管理面是低频人工操作，此处的竞态窗口可接受；
    // 唯一约束仍在库层兜底（并发插入时后到者会得到 500，而非悄然覆盖）。
    //
    // @intent 存在性检查是**全局**的（不看归属）：project_id 是全局主键，
    //         别人的同名项目同样会冲突。此时返回的 409 与「自己重名」逐字相同
    //         ——否则「这个名字被占用了」会泄漏「别人有一个叫这名字的项目」。
    match meta::find_project(&state.meta, &project_id).await {
        Ok(Some(_)) => {
            return reply::conflict(
                "project_exists",
                &format!("project `{project_id}` already exists"),
            )
        }
        Ok(None) => {}
        Err(err) => return reply::internal_error(&err),
    }

    let token = match auth::generate_token() {
        Ok(token) => token,
        Err(err) => return reply::internal_error(&err),
    };

    let owner = identity.user().to_string();
    let new = NewProject {
        project_id: project_id.clone(),
        token: token.clone(),
        // 记录值由后端给出（文件后端 = 项目内相对路径；单库 = 共享文件名；内存 = 无文件），
        // 实际位置以各后端推导为准（见 docs §12.3 / §12.5），此处不构成第二个真相源
        db_path: backend.db_path_label(&project_id),
        backend: kind.as_str().to_string(),
        owner: owner.clone(),
    };
    if let Err(err) = meta::create_project(&state.meta, &new).await {
        return reply::internal_error(&err);
    }

    tracing::info!(
        project_id = %project_id,
        backend = kind.as_str(),
        owner = %owner,
        "project created"
    );
    (
        StatusCode::CREATED,
        Json(CreateProjectResponse {
            project_id,
            token,
            backend: kind.as_str().to_string(),
            owner,
        }),
    )
        .into_response()
}

/// 注销项目：清物理数据 → 删登记行。
pub async fn delete_project(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path(project_id): Path<String>,
) -> Response {
    if let Err(err) = validate_project_id(&project_id) {
        return reply::bad_request("invalid_request", &err.to_string());
    }

    let record = match visible_project(&state, &identity, &project_id).await {
        Ok(record) => record,
        Err(response) => return *response,
    };

    // 顺序刻意是「先物理数据、后登记行」。反序时若删文件失败，会留下
    // 「登记行已消失、数据仍在盘上」的不可见残留——既违背删除承诺，又无法通过重试自愈。
    // 本顺序的最坏情况是「文件已删、登记行尚在」，重试一次 DELETE 即可收敛。
    let data_removed = match BackendKind::parse(&record.backend) {
        Ok(kind) => match state.storage.get(kind) {
            Ok(backend) => match backend.drop_project(&project_id).await {
                Ok(()) => true,
                // 物理删除失败：中止，保留登记行，让调用方可重试
                Err(err) => return reply::internal_error(&err),
            },
            // 后端未编译进本构建：物理数据无从删除，但仍允许注销登记，
            // 否则管理面会永久残留一个既用不了也删不掉的死项目。
            Err(err) => {
                tracing::warn!(
                    project_id = %project_id,
                    backend = %record.backend,
                    error = %err,
                    "backend unavailable: project unregistered but data files remain on disk"
                );
                false
            }
        },
        Err(err) => {
            tracing::warn!(
                project_id = %project_id,
                backend = %record.backend,
                error = %err,
                "unknown backend recorded: project unregistered but data files remain on disk"
            );
            false
        }
    };

    if let Err(err) = meta::delete_project(&state.meta, &project_id).await {
        return reply::internal_error(&err);
    }

    tracing::info!(
        project_id = %project_id,
        data_removed,
        by = %identity.user(),
        "project deleted"
    );
    Json(DeleteProjectResponse {
        project_id,
        data_removed,
    })
    .into_response()
}

/// 用量统计：实体 / 关系 / 观测三个计数。
///
/// @intent 与 MCP 工具层走同一条路径（`MemoryRepository::read_graph`），
///         因此统计口径与客户端看到的数据必然一致，不会出现两套计数逻辑。
pub async fn project_stats(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path(project_id): Path<String>,
) -> Response {
    let repos = match repositories_of(&state, &identity, &project_id).await {
        Ok(repos) => repos,
        Err(response) => return *response,
    };
    let graph = match repos.memory.read_graph().await {
        Ok(graph) => graph,
        Err(err) => return reply::internal_error(&err),
    };
    let handoffs = match repos.handoff.list_handoffs(None, false, usize::MAX).await {
        Ok(items) => items.len(),
        Err(err) => return reply::internal_error(&err),
    };

    Json(ProjectStats {
        project_id,
        backend: repos.backend.as_str().to_string(),
        entities: graph.entity_count(),
        relations: graph.relation_count(),
        observations: graph.observation_count(),
        handoffs,
    })
    .into_response()
}

/// 全图快照：网页控制台「记忆浏览」的后端。
///
/// @intent 与 MCP 的 `read_graph` **同一个仓库方法**，因此控制台看到的与 AI 客户端
///         写入的是同一份数据，不存在「两套读取逻辑互相漂移」的问题。
///         本接口是只读的：控制台不提供改记忆的能力，写入仍只经 MCP 工具，
///         以免在管理面引入第二套写入语义（判重规则、来源字段等）。
pub async fn project_graph(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path(project_id): Path<String>,
) -> Response {
    let repos = match repositories_of(&state, &identity, &project_id).await {
        Ok(repos) => repos,
        Err(response) => return *response,
    };
    let graph = match repos.memory.read_graph().await {
        Ok(graph) => graph,
        Err(err) => return reply::internal_error(&err),
    };

    let entities = graph
        .entities
        .iter()
        .map(|entity| EntityView {
            name: entity.name.clone(),
            entity_type: entity.entity_type.clone(),
            created_at: entity.created_at,
            source: entity.source.clone(),
            observations: entity
                .observations
                .iter()
                .map(|obs| ObservationView {
                    id: obs.id,
                    content: obs.content.clone(),
                    created_at: obs.created_at,
                    source: obs.source.clone(),
                    entity_name: obs.entity_name.clone(),
                })
                .collect(),
        })
        .collect();

    let relations = graph
        .relations
        .iter()
        .map(|relation| RelationView {
            id: relation.id,
            from_name: relation.from_name.clone(),
            to_name: relation.to_name.clone(),
            relation_type: relation.relation_type.clone(),
            created_at: relation.created_at,
            source: relation.source.clone(),
        })
        .collect();

    Json(GraphView {
        project_id,
        entities,
        relations,
    })
    .into_response()
}

// ---------------------------------------------------------------- 交接（P4.3）

// REST 交接出参**复用 MCP 的 dto 视图**（`mcp::dto`）。
//
// @intent CLI、curl 与 AI 客户端看到的是**同一份 JSON 形状**——「CLI 看到的和 AI
//         看到的不一样」这类漂移在结构上不可能发生（docs §5.2、§5.3）。
//         与 graph 端点自建视图不同：那里视图是领域模型与 HTTP 的解耦层，
//         而 dto 本来就是对外协议契约，无需再包一层。

// 交接保存请求体：与 MCP `handoff_save` 入参同构（含 camelCase 别名与
// `deny_unknown_fields`，字段拼错在解析期就被 400 拒绝）。
use crate::mcp::dto::HandoffSaveRequest;

/// 接手请求体；两个字段都可省略，`client` 只进 `acked_by` 诊断信息。
#[derive(Debug, Default, Deserialize)]
pub struct AckHandoffBody {
    /// 接手方标识；省略落空串 = 未声明
    #[serde(default)]
    pub client: Option<String>,
}

/// `GET .../handoffs` 的查询参数（全部可省）。
#[derive(Debug, Deserialize)]
pub struct HandoffListQuery {
    /// `1`（缺省）只列待接手；`0` 看历史（含已接手）
    #[serde(default)]
    pub unacked: Option<String>,
    /// 最多返回多少条；缺省 10（与 MCP `handoff_list` 同源）
    #[serde(default)]
    pub limit: Option<String>,
    /// 只列 `task_id` 以该前缀开头的条目
    #[serde(default)]
    pub prefix: Option<String>,
}

/// POST `/projects/{id}/handoffs`：保存修订，返回 201 + 完整记录。
///
/// @intent 校验失败 400（调用方改参数即可），与 MCP 的 `-32602` 同源分流；
///         「落哪个后端、哪张表」由 `repositories_of` 决定，本 handler 对后端盲视。
pub async fn handoff_save(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path(project_id): Path<String>,
    Json(req): Json<HandoffSaveRequest>,
) -> Response {
    let repos = match repositories_of(&state, &identity, &project_id).await {
        Ok(repos) => repos,
        Err(response) => return *response,
    };
    let input = crate::domain::HandoffInput {
        task_id: req.task_id,
        goal: req.goal,
        state: req.state,
        next_steps: req.next_steps,
        files: req.files,
        decisions: req.decisions,
        open_questions: req.open_questions,
        client: req.client,
        session_id: req.session_id,
    };
    match repos.handoff.save_handoff(input).await {
        Ok(h) => (
            StatusCode::CREATED,
            Json(crate::mcp::tools::handoff_view(h)),
        )
            .into_response(),
        Err(StorageError::InvalidInput(detail)) => reply::bad_request("invalid_input", &detail),
        Err(err) => reply::internal_error(&err),
    }
}

/// GET `/projects/{id}/handoffs`：列摘要（接手选择界面）。
///
/// @intent 查询参数非法必须 400 而不是静默回退默认值——与配置解析「非法值硬失败」
///         同一口径；`unacked` 只接受 `1`/`0`，`limit` 只接受非负整数。
pub async fn handoff_list(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path(project_id): Path<String>,
    Query(q): Query<HandoffListQuery>,
) -> Response {
    let repos = match repositories_of(&state, &identity, &project_id).await {
        Ok(repos) => repos,
        Err(response) => return *response,
    };
    let unacked = match q.unacked.as_deref() {
        None | Some("1") => true,
        Some("0") => false,
        Some(other) => {
            return reply::bad_request(
                "invalid_request",
                &format!("`unacked` 只接受 1 或 0，实际 `{other}`"),
            )
        }
    };
    let limit = match q.limit.as_deref() {
        None => crate::mcp::dto::DEFAULT_HANDOFF_LIST_LIMIT,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                return reply::bad_request(
                    "invalid_request",
                    &format!("`limit` 必须是非负整数，实际 `{raw}`"),
                )
            }
        },
    };
    match repos
        .handoff
        .list_handoffs(q.prefix.as_deref(), unacked, limit)
        .await
    {
        Ok(items) => {
            let handoffs: Vec<_> = items
                .into_iter()
                .map(crate::mcp::tools::handoff_summary_view)
                .collect();
            Json(json!({ "handoffs": handoffs })).into_response()
        }
        Err(err) => reply::internal_error(&err),
    }
}

/// GET `/projects/{id}/handoffs/pending`：**零 ID 入口**。
///
/// @intent 状态码三分流是本端点的全部语义：200 = 命中唯一一条（返回完整记录）；
///         204 = 没有待接手（正常状态，不是 404——404 留给「指定 task_id 不存在」）；
///         300 = 多条待接手，响应体给候选摘要，服务端绝不替调用方挑（docs §5.2）。
pub async fn handoff_pending(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path(project_id): Path<String>,
) -> Response {
    let repos = match repositories_of(&state, &identity, &project_id).await {
        Ok(repos) => repos,
        Err(response) => return *response,
    };
    match repos.handoff.resolve_handoff(None).await {
        Ok(crate::domain::HandoffResolution::Found(h)) => {
            Json(crate::mcp::tools::handoff_view(*h)).into_response()
        }
        Ok(crate::domain::HandoffResolution::None) => StatusCode::NO_CONTENT.into_response(),
        Ok(crate::domain::HandoffResolution::Ambiguous(list)) => {
            let candidates: Vec<_> = list
                .into_iter()
                .map(crate::mcp::tools::handoff_summary_view)
                .collect();
            (
                StatusCode::MULTIPLE_CHOICES,
                Json(json!({ "status": "ambiguous", "candidates": candidates })),
            )
                .into_response()
        }
        Err(StorageError::InvalidInput(detail)) => reply::bad_request("invalid_input", &detail),
        Err(err) => reply::internal_error(&err),
    }
}

/// GET `/projects/{id}/handoffs/{task_id}`：精确取最新修订；不存在 404。
pub async fn handoff_get(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path((project_id, task_id)): Path<(String, String)>,
) -> Response {
    let repos = match repositories_of(&state, &identity, &project_id).await {
        Ok(repos) => repos,
        Err(response) => return *response,
    };
    match repos.handoff.resolve_handoff(Some(&task_id)).await {
        Ok(crate::domain::HandoffResolution::Found(h)) => {
            Json(crate::mcp::tools::handoff_view(*h)).into_response()
        }
        Ok(crate::domain::HandoffResolution::None) => {
            reply::not_found("not_found", &format!("handoff `{task_id}` not found"))
        }
        // 显式 ID 不会产生歧义（歧义只发生在无 ID 解析）
        Ok(crate::domain::HandoffResolution::Ambiguous(_)) => {
            reply::internal_error(&"unreachable: explicit id resolved to ambiguous")
        }
        Err(StorageError::InvalidInput(detail)) => reply::bad_request("invalid_input", &detail),
        Err(err) => reply::internal_error(&err),
    }
}

/// ack 的共用执行体：`task_id = None`（零 ID）与显式 ID 走同一条仓库方法（CAS）。
///
/// @intent 与 `resolve_handoff` 同源：寻址规则只有一份实现。返回值按 resolution
///         分流——Found 200（回传 won 事实）、None 由调用方决定（204 或 404）、
///         Ambiguous 300 且 `won=false`（不产生任何副作用）。
async fn ack_response(repos: &ProjectRepos, task_id: Option<&str>, client: &str) -> Response {
    match repos.handoff.ack_handoff(task_id, client).await {
        Ok(outcome) => {
            let (status, handoff, candidates) =
                crate::mcp::tools::resolution_parts(outcome.resolution);
            match status {
                crate::mcp::dto::HandoffStatus::Found => (
                    StatusCode::OK,
                    Json(json!({
                        "status": "found",
                        "won": outcome.won,
                        "handoff": handoff,
                    })),
                )
                    .into_response(),
                crate::mcp::dto::HandoffStatus::Ambiguous => (
                    StatusCode::MULTIPLE_CHOICES,
                    Json(json!({
                        "status": "ambiguous",
                        "won": outcome.won,
                        "candidates": candidates,
                    })),
                )
                    .into_response(),
                crate::mcp::dto::HandoffStatus::None => {
                    // 显式 ID 找不到任务是 404；零 ID 没有「当前待接手」是 204。
                    match task_id {
                        Some(id) => {
                            reply::not_found("not_found", &format!("handoff `{id}` not found"))
                        }
                        None => StatusCode::NO_CONTENT.into_response(),
                    }
                }
            }
        }
        Err(StorageError::InvalidInput(detail)) => reply::bad_request("invalid_input", &detail),
        Err(err) => reply::internal_error(&err),
    }
}

/// 解析 ack 请求体：空 body 或缺 `client` 都按「未声明」处理。
///
/// @intent 用 `Bytes` + 空体判断区分「没有 body」（正常，取默认值）与「body 不是
///         合法 JSON」（400）——两者合并成默认值会让拼错的请求被静默当成功，违反
///         「非法值不得静默通过」的项目口径。
async fn ack_body(bytes: axum::body::Bytes) -> Result<String, Box<Response>> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(String::new());
    }
    match serde_json::from_slice::<AckHandoffBody>(&bytes) {
        Ok(body) => Ok(body.client.unwrap_or_default()),
        Err(err) => Err(Box::new(reply::bad_request(
            "invalid_request",
            &format!("请求体不是合法 JSON：{err}"),
        ))),
    }
}

/// POST `/projects/{id}/handoffs/pending/ack`：**零 ID 接手**。
pub async fn handoff_ack_pending(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path(project_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let repos = match repositories_of(&state, &identity, &project_id).await {
        Ok(repos) => repos,
        Err(response) => return *response,
    };
    let client = match ack_body(body).await {
        Ok(client) => client,
        Err(response) => return *response,
    };
    ack_response(&repos, None, &client).await
}

/// POST `/projects/{id}/handoffs/{task_id}/ack`：精确接手（CAS；重复 ack 幂等）。
pub async fn handoff_ack(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path((project_id, task_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    let repos = match repositories_of(&state, &identity, &project_id).await {
        Ok(repos) => repos,
        Err(response) => return *response,
    };
    let client = match ack_body(body).await {
        Ok(client) => client,
        Err(response) => return *response,
    };
    ack_response(&repos, Some(&task_id), &client).await
}

/// DELETE `/projects/{id}/handoffs/{task_id}`：幂等删除（不存在视为成功）。
pub async fn handoff_delete(
    State(state): State<AppState>,
    Extension(identity): Extension<AdminIdentity>,
    Path((project_id, task_id)): Path<(String, String)>,
) -> Response {
    let repos = match repositories_of(&state, &identity, &project_id).await {
        Ok(repos) => repos,
        Err(response) => return *response,
    };
    match repos.handoff.delete_handoff(&task_id).await {
        Ok(()) => Json(json!({ "task_id": task_id })).into_response(),
        Err(err) => reply::internal_error(&err),
    }
}

/// 取得当前身份可见的项目登记行；不可见时给出 404。
///
/// @intent **「不存在」与「不属于你」刻意返回同一个 404**。若越界时返回 403，
///         调用方就能通过「403 还是 404」枚举出「系统里存在一个叫 x 的项目」——
///         项目名往往是泄露最多信息的东西（`iptv-gx`、`payroll`、`acme-nda`）。
///         这与 `/mcp` 用统一 401 掩盖 token 是否有效是同一条原则。
///
/// @intent 这是项目类授权的唯一入口：先 `validate` 再查库再判权，
///         三步都在这里，调用方拿不到未过滤的 `ProjectRecord`。
async fn visible_project(
    state: &AppState,
    identity: &AdminIdentity,
    project_id: &str,
) -> Result<ProjectRecord, Box<Response>> {
    if let Err(err) = validate_project_id(project_id) {
        return Err(Box::new(reply::bad_request(
            "invalid_request",
            &err.to_string(),
        )));
    }

    let record = match meta::find_project(&state.meta, project_id).await {
        Ok(Some(record)) => record,
        Ok(None) => return Err(Box::new(project_not_found(project_id))),
        Err(err) => return Err(Box::new(reply::internal_error(&err))),
    };

    if !identity.can_access(&record.owner) {
        // 记日志但不改变响应：越权尝试是安全上值得留痕的事件，
        // 而响应体必须与「真不存在」逐字一致（见本函数的 @intent）。
        tracing::warn!(
            project_id = %project_id,
            user = %identity.user(),
            owner = %record.owner,
            "rejected access to a project owned by another account"
        );
        return Err(Box::new(project_not_found(project_id)));
    }

    Ok(record)
}

/// 解析项目标识 → 登记行 → 存储后端 → 仓库集合，失败时给出可回传的响应。
/// @intent 抽成一处是因为「登记行不存在」「不属于当前账号」「后端不可用」在多处接口里
///         必须映射成**同样**的状态码；各写一遍迟早出现一处返回 404、另一处返回 500
///         的不一致，而调用方会据此写出不同的重试策略。
///
/// @intent 状态码分野与 `reply` 模块一致：项目不存在 / 不可见 → 404（调用方可修正，
///         例如换个 project_id）；登记的后端无法解析或未编译进本构建 → 500
///         （服务端 / 构建问题，调用方改不了）。
///
/// @intent 错误侧刻意装箱（`Box<Response>`）：`Response` 有 128 字节，直接内联会让
///         每个 `Result` 都按它的大小搬运，clippy 的 `result_large_err` 正是指这一点。
///         装箱多一次分配，但只发生在**已经要构造 JSON 响应体**的错误路径上——
///         不在热路径，代价可忽略。这不是为了讨好 lint：错误路径本来就慢，
///         而每次成功调用都白扛一个 128 字节返回值的成本是确定的。
async fn repositories_of(
    state: &AppState,
    identity: &AdminIdentity,
    project_id: &str,
) -> Result<ProjectRepos, Box<Response>> {
    let record = visible_project(state, identity, project_id).await?;

    // 登记的 backend 值无法解析属数据损坏，调用方改不了 ⇒ 500
    let kind = match BackendKind::parse(&record.backend) {
        Ok(kind) => kind,
        Err(err) => return Err(Box::new(reply::internal_error(&err))),
    };
    let backend = match state.storage.get(kind) {
        Ok(backend) => backend,
        Err(err) => return Err(Box::new(reply::internal_error(&err))),
    };
    match backend.repositories_for(project_id).await {
        Ok(repos) => Ok(repos),
        Err(err) => Err(Box::new(reply::internal_error(&err))),
    }
}

/// 404：项目不存在。
fn project_not_found(project_id: &str) -> Response {
    reply::not_found("not_found", &format!("project `{project_id}` not found"))
}

/// 400：请求的后端不可用，并回传本构建可用的后端列表。
///
/// @intent 回传可用列表是刻意的：调用方（运维 / 脚本）无需翻文档即可自我修正，
///         这是「400 属于可修正错误」这一约定的具体兑现。
fn unavailable_backend(state: &AppState, err: &StorageError) -> Response {
    let available: Vec<&str> = state
        .storage
        .available_kinds()
        .iter()
        .map(|k| k.as_str())
        .collect();

    tracing::debug!(error = %err, "rejected unavailable backend");
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "invalid_request",
            "detail": err.to_string(),
            "available_backends": available,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 自动生成的 project_id 必须满足存储层的字符集约束、长度固定且不重复。
    #[test]
    fn generated_ids_are_valid_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let suffix = auth::random_hex(GENERATED_ID_BYTES).unwrap();
            let id = format!("{GENERATED_ID_PREFIX}{suffix}");

            assert_eq!(id.len(), GENERATED_ID_PREFIX.len() + GENERATED_ID_BYTES * 2);
            assert!(id.starts_with(GENERATED_ID_PREFIX));
            assert!(
                validate_project_id(&id).is_ok(),
                "自动生成的 id 必须能通过存储层校验：{id}"
            );
            assert!(seen.insert(id), "自动生成的 id 出现重复");
        }
    }
}
