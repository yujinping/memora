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

/// 500：内部错误。详情仅记入日志，响应体保持通用，避免泄漏存储细节。
pub fn internal_error(detail: &dyn std::fmt::Display) -> Response {
    tracing::error!(error = %detail, "internal error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal_error" })),
    )
        .into_response()
}
