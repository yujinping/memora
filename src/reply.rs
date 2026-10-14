//! 统一 HTTP 错误响应。
//!
//! @author yujinping
//! @intent P2：把「客户端错误」与「服务端错误」的响应构造收口到一处，
//!          并确保内部错误详情只进日志、不回传客户端。
//!          P4 补充管理面所需的 400 / 404 / 409：这三类都属于「调用方可自行修正」，
//!          因此响应体带上具体原因，与 500 的通用响应体形成明确分野。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// 401：缺少 / 无效 token。
pub fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized" })),
    )
        .into_response()
}

/// 400：入参非法。响应体带原因，便于调用方自我修正。
pub fn bad_request(error: &str, detail: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": error, "detail": detail })),
    )
        .into_response()
}

/// 404：目标资源不存在。
pub fn not_found(error: &str, detail: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": error, "detail": detail })),
    )
        .into_response()
}

/// 409：资源已存在（重复创建）。
pub fn conflict(error: &str, detail: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": error, "detail": detail })),
    )
        .into_response()
}

/// 403：凭据有效，但这次请求的来源不被接受（跨站写请求 / 非白名单 Host）。
///
/// @intent 与 401 的分野是刻意的：401 表示「换个凭据再来」，403 表示「换凭据也没用」。
///         把它们混为一谈会让调用方在错误的方向上反复重试。
pub fn forbidden(error: &str, detail: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": error, "detail": detail })),
    )
        .into_response()
}

/// 401 的可选带原因版本。
///
/// @intent 仅用于**登录接口**：那里缺凭据本身就是「用户名或口令不对」，
///         不存在 `unauthorized()` 所防范的「token 探测」问题（用户名不是秘密，
///         口令是否正确才是）。其余鉴权路径一律继续用无原因的 [`unauthorized`]，
///         避免把「凭据是否存在」透给攻击者。
pub fn unauthorized_with(detail: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized", "detail": detail })),
    )
        .into_response()
}

/// 500：内部错误。详情仅记入日志，响应体保持通用，避免泄漏存储细节。
pub fn internal_error(detail: &dyn std::fmt::Display) -> Response {
    tracing::error!(error = %detail, "internal error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal_error" })),
    )
        .into_response()
}
