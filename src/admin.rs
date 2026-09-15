//! REST 管理面：项目生命周期（创建 / 注销 / 用量统计）。
//!
//! @author yujinping
//! @intent P4：把「项目登记」从手工改元库提升为受 `ADMIN_TOKEN` 保护的接口。
//!         分层上与 MCP 工具层同构——本模块只依赖 `StorageRegistry` 与
//!         `MemoryRepository` trait，不引用任何具体后端；「项目的物理数据在哪、
//!         怎么删」由后端自己回答（`StorageBackend::drop_project`），
//!         因此新增后端时本文件无需改动。
//!
//! 状态码约定（与 `reply` 模块一致）：
//! - 400：入参可修正（非法 project_id、后端未编译进本构建）
//! - 404：项目不存在
//! - 409：project_id 已被占用
//! - 500：服务端故障（元库 / 后端不可用、熵源不可用）

use crate::auth;
use crate::error::StorageError;
use crate::meta::{self, NewProject};
use crate::reply;
use crate::state::AppState;
use crate::storage::{validate_project_id, BackendKind};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
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
}

/// 注销项目响应体。
#[derive(Debug, Serialize)]
pub struct DeleteProjectResponse {
    /// 项目标识
    pub project_id: String,
    /// 是否确实删除了持久化数据；后端未注册时为 `false`（如实报告，不假装成功）
    pub data_removed: bool,
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
}

/// 创建项目：校验 → 发 token → 登记（返回 201）。
pub async fn create_project(
    State(state): State<AppState>,
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
    if let Err(err) = state.storage.get(kind) {
        return unavailable_backend(&state, &err);
    }

    // 先查后插：管理面是低频人工操作，此处的竞态窗口可接受；
    // 唯一约束仍在库层兜底（并发插入时后到者会得到 500，而非悄然覆盖）。
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

    let new = NewProject {
        project_id: project_id.clone(),
        token: token.clone(),
        // 记录用相对路径：实际位置由后端推导（见 docs §12.3），此处不构成第二个真相源
        db_path: format!("{project_id}/mem.db"),
        backend: kind.as_str().to_string(),
    };
    if let Err(err) = meta::create_project(&state.meta, &new).await {
        return reply::internal_error(&err);
    }

    tracing::info!(project_id = %project_id, backend = kind.as_str(), "project created");
    (
        StatusCode::CREATED,
        Json(CreateProjectResponse {
            project_id,
            token,
            backend: kind.as_str().to_string(),
        }),
    )
        .into_response()
}

/// 注销项目：清物理数据 → 删登记行。
pub async fn delete_project(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> Response {
    if let Err(err) = validate_project_id(&project_id) {
        return reply::bad_request("invalid_request", &err.to_string());
    }

    let record = match meta::find_project(&state.meta, &project_id).await {
        Ok(Some(record)) => record,
        Ok(None) => return project_not_found(&project_id),
        Err(err) => return reply::internal_error(&err),
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

    tracing::info!(project_id = %project_id, data_removed, "project deleted");
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
    Path(project_id): Path<String>,
) -> Response {
    if let Err(err) = validate_project_id(&project_id) {
        return reply::bad_request("invalid_request", &err.to_string());
    }

    let record = match meta::find_project(&state.meta, &project_id).await {
        Ok(Some(record)) => record,
        Ok(None) => return project_not_found(&project_id),
        Err(err) => return reply::internal_error(&err),
    };

    // 登记的 backend 值无法解析属数据损坏，调用方改不了 ⇒ 500
    let kind = match BackendKind::parse(&record.backend) {
        Ok(kind) => kind,
        Err(err) => return reply::internal_error(&err),
    };
    let backend = match state.storage.get(kind) {
        Ok(backend) => backend,
        Err(err) => return reply::internal_error(&err),
    };
    let repos = match backend.repositories_for(&project_id).await {
        Ok(repos) => repos,
        Err(err) => return reply::internal_error(&err),
    };
    let graph = match repos.memory.read_graph().await {
        Ok(graph) => graph,
        Err(err) => return reply::internal_error(&err),
    };

    Json(ProjectStats {
        project_id,
        backend: kind.as_str().to_string(),
        entities: graph.entity_count(),
        relations: graph.relation_count(),
        observations: graph.observation_count(),
    })
    .into_response()
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
