//! 交接 CLI 执行层（P4.3）。
//!
//! @author yujinping
//! @intent 见 docs/handoff-integration-design.md §5.3：只经 HTTP 调 REST 管理面
//!         （ADMIN_TOKEN），与 daemon 同级、不触碰 storage / routes，
//!         对「用哪个后端、数据在哪个文件」完全盲视——换后端时本模块零改动。
//!
//! 退出码契约（§5.3）：list / load / ack 遇「无待接手」返回 0（正常无事可做，
//! stderr 提示一句）；遇「歧义」返回 1 并打印候选（人需要看到候选才能选）；
//! 连接类故障走 `Err`，由 `main::finish` 收敛为非零退出码。

use crate::cli::HandoffOpts;
use crate::http::{Client, Endpoint};
use std::io::Read;
use std::time::Duration;

/// CLI 执行所需的连接信息。
pub struct HandoffConfig {
    /// 服务端地址（`--server` 或本机缺省）
    pub endpoint: Endpoint,
    /// `ADMIN_TOKEN`（CLI 唯一合法凭据：明文项目 token 不可反查，CLI 拿不到它）
    pub token: String,
}

/// 项目解析：显式 `--project` → `MEMORA_PROJECT` → 服务端唯一项目。
///
/// @intent 返回 `(project_id, auto_selected)`：自动选定时调用方要**打印选定了哪个**，
///         否则用户不知道命令作用在哪份数据上（docs §5.3）。
///         独立成纯函数是为让两级缺省规则可被精确测试（不经网络）。
pub fn resolve_project_id(
    explicit: Option<&str>,
    env: Option<&str>,
    projects: &[String],
) -> Result<(String, bool), String> {
    if let Some(id) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok((id.to_string(), false));
    }
    if let Some(id) = env.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok((id.to_string(), false));
    }
    match projects {
        [only] => Ok((only.clone(), true)),
        [] => Err(
            "server has no projects; create one (POST /api/v1/projects) or set \
             MEMORA_PROJECT / pass --project"
                .to_string(),
        ),
        many => Err(format!(
            "server has multiple projects ({}); pass --project or set MEMORA_PROJECT",
            many.join(", ")
        )),
    }
}

/// 把 Unix 秒格式化为 `yyyy-MM-dd HH:MM`（按时区偏移）。
///
/// @intent 人类可读的时间是列表可辨认性的一部分（docs §3.7）；偏移作为参数注入，
///         让格式化本身可确定性测试（本地时区因机器而异）。
fn format_time(secs: i64, offset: time::UtcOffset) -> String {
    let fmt = time::format_description::parse_borrowed::<2>("[year]-[month]-[day] [hour]:[minute]");
    match (time::OffsetDateTime::from_unix_timestamp(secs), fmt) {
        (Ok(dt), Ok(fmt)) => {
            let local = dt.checked_to_offset(offset).unwrap_or(dt);
            local.format(&fmt).unwrap_or_else(|_| secs.to_string())
        }
        _ => secs.to_string(),
    }
}

/// 当前本地时区；不可得（罕见平台）时退回 UTC——宁可时间偏 8 小时，不可崩。
fn local_offset() -> time::UtcOffset {
    time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC)
}

/// 执行 `memora handoff <action>`，返回进程退出码。
///
/// @intent `env_project` 由 `main` 注入（读 `MEMORA_PROJECT`），本模块不做环境
///         访问——两级缺省规则因此可被确定性测试。
pub fn run(
    action: crate::cli::HandoffAction,
    opts: &HandoffOpts,
    cfg: &HandoffConfig,
    env_project: Option<&str>,
) -> Result<i32, String> {
    let timeout = Duration::from_secs(opts.timeout.unwrap_or(10));
    let client = Client::new(cfg.endpoint.clone(), cfg.token.clone(), timeout);

    let (project, auto) =
        resolve_project_via_server(&client, opts.project.as_deref(), env_project)?;
    if auto {
        eprintln!("server has exactly one project; using `{project}`");
    }

    match action {
        crate::cli::HandoffAction::List => cmd_list(&client, &project, opts),
        crate::cli::HandoffAction::Load => cmd_load(&client, &project, opts),
        crate::cli::HandoffAction::Ack => cmd_ack(&client, &project, opts),
        crate::cli::HandoffAction::Save => cmd_save(&client, &project, opts),
        crate::cli::HandoffAction::Remove => cmd_remove(&client, &project, opts),
    }
}

/// 项目两级解析（先本地规则，缺省再问服务端拿项目列表）。
fn resolve_project_via_server(
    client: &Client,
    explicit: Option<&str>,
    env: Option<&str>,
) -> Result<(String, bool), String> {
    if explicit.map(str::trim).filter(|s| !s.is_empty()).is_some()
        || env.map(str::trim).filter(|s| !s.is_empty()).is_some()
    {
        // 本地已定，不需要网络
        return resolve_project_id(explicit, env, &[]);
    }
    let resp = client.send("GET", "/api/v1/projects", None)?;
    if resp.status != 200 {
        return Err(format!(
            "GET /api/v1/projects failed ({}): {}",
            resp.status, resp.body
        ));
    }
    let ids: Vec<String> = serde_json::from_str::<Vec<serde_json::Value>>(&resp.body)
        .map_err(|err| format!("project list is not valid JSON: {err}"))?
        .into_iter()
        .filter_map(|v| v["project_id"].as_str().map(str::to_string))
        .collect();
    resolve_project_id(explicit, env, &ids)
}

/// percent-encode 路径 / 查询段中的保留字符（最小实现：只转义会破坏结构的字符）。
fn url_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'@' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn cmd_list(client: &Client, project: &str, opts: &HandoffOpts) -> Result<i32, String> {
    let mut query = format!("unacked={}", if opts.all { 0 } else { 1 });
    if let Some(limit) = opts.limit {
        query.push_str(&format!("&limit={limit}"));
    }
    if let Some(prefix) = &opts.prefix {
        query.push_str(&format!("&prefix={}", url_encode(prefix)));
    }
    let resp = client.send(
        "GET",
        &format!("/api/v1/projects/{project}/handoffs?{query}"),
        None,
    )?;
    match resp.status {
        200 => {
            if opts.json {
                println!("{}", resp.body);
                return Ok(0);
            }
            let items = parse(&resp.body)["handoffs"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                eprintln!("没有待接手的交接");
                return Ok(0);
            }
            let offset = local_offset();
            for (i, s) in items.iter().enumerate() {
                println!("{}", render_summary(i + 1, s, offset));
            }
            Ok(0)
        }
        other => Err(format!("list failed ({other}): {}", resp.body)),
    }
}

fn cmd_load(client: &Client, project: &str, opts: &HandoffOpts) -> Result<i32, String> {
    let path = match &opts.task {
        Some(task) => format!("/api/v1/projects/{project}/handoffs/{}", url_encode(task)),
        None => format!("/api/v1/projects/{project}/handoffs/pending"),
    };
    let resp = client.send("GET", &path, None)?;
    match resp.status {
        200 => {
            if opts.json {
                println!("{}", resp.body);
            } else {
                let v = parse(&resp.body);
                let task_id = v["task_id"].as_str().unwrap_or("");
                let goal = v["goal"].as_str().unwrap_or("");
                let state = v["state"].as_str().unwrap_or("");
                println!("{goal}  ({task_id})\n\n进展：{state}");
                print_array(&v, "next_steps", "下一步");
                print_array(&v, "files", "关键文件");
                print_array(&v, "decisions", "已定事项");
                print_array(&v, "open_questions", "待拍板");
            }
            Ok(0)
        }
        204 => {
            eprintln!("没有待接手的交接");
            Ok(0)
        }
        300 => {
            print_candidates(&parse(&resp.body));
            Ok(1)
        }
        404 => Err(format!(
            "handoff `{}` not found",
            opts.task.as_deref().unwrap_or("")
        )),
        other => Err(format!("load failed ({other}): {}", resp.body)),
    }
}

fn cmd_ack(client: &Client, project: &str, opts: &HandoffOpts) -> Result<i32, String> {
    let path = match &opts.task {
        Some(task) => {
            format!(
                "/api/v1/projects/{project}/handoffs/{}/ack",
                url_encode(task)
            )
        }
        None => format!("/api/v1/projects/{project}/handoffs/pending/ack"),
    };
    let body = serde_json::json!({ "client": opts.client.clone().unwrap_or_default() });
    let resp = client.send("POST", &path, Some(&body.to_string()))?;
    match resp.status {
        200 => {
            let v = parse(&resp.body);
            if v["won"].as_bool().unwrap_or(false) {
                let task_id = v["handoff"]["task_id"].as_str().unwrap_or("");
                println!("已接手 {task_id}；按 next_steps 继续推进");
                Ok(0)
            } else {
                let acked_by = v["handoff"]["acked_by"].as_str().unwrap_or("");
                eprintln!(
                    "该交接已被他人接手（acked_by={acked_by}）；如需接手请先用 handoff list 确认"
                );
                Ok(1)
            }
        }
        204 => {
            eprintln!("没有待接手的交接");
            Ok(0)
        }
        300 => {
            print_candidates(&parse(&resp.body));
            Ok(1)
        }
        404 => Err(format!(
            "handoff `{}` not found",
            opts.task.as_deref().unwrap_or("")
        )),
        other => Err(format!("ack failed ({other}): {}", resp.body)),
    }
}

fn cmd_save(client: &Client, project: &str, opts: &HandoffOpts) -> Result<i32, String> {
    let raw = read_input(opts)?;
    // 先本地校验是合法 JSON 对象：不合法就不发请求（CLI 端最快的自我修正回路）
    let value: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|err| format!("input is not valid JSON: {err}"))?;
    if !value.is_object() {
        return Err("input must be a JSON object (goal / state / ...)".to_string());
    }
    let resp = client.send(
        "POST",
        &format!("/api/v1/projects/{project}/handoffs"),
        Some(raw.trim()),
    )?;
    match resp.status {
        201 => {
            if opts.json {
                println!("{}", resp.body);
            } else {
                let v = parse(&resp.body);
                println!(
                    "已保存：{} (revision {})",
                    v["task_id"].as_str().unwrap_or(""),
                    v["revision"]
                );
            }
            Ok(0)
        }
        other => Err(format!("save failed ({other}): {}", resp.body)),
    }
}

fn cmd_remove(client: &Client, project: &str, opts: &HandoffOpts) -> Result<i32, String> {
    // 解析层已强制 rm 必带 --task；这里再防一次（run 可能被直接调用）
    let task = opts
        .task
        .as_deref()
        .ok_or_else(|| "handoff rm requires --task".to_string())?;
    let resp = client.send(
        "DELETE",
        &format!("/api/v1/projects/{project}/handoffs/{}", url_encode(task)),
        None,
    )?;
    match resp.status {
        200 => {
            println!("已删除 {task}");
            Ok(0)
        }
        other => Err(format!("rm failed ({other}): {}", resp.body)),
    }
}

fn parse(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or(serde_json::Value::Null)
}

fn print_array(v: &serde_json::Value, key: &str, label: &str) {
    if let Some(items) = v[key].as_array().filter(|a| !a.is_empty()) {
        let joined: Vec<String> = items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect();
        println!("{label}: {}", joined.join("；"));
    }
}

/// 歧义候选的 stderr 呈现：绝不替人挑，列出来让人选（docs §5.3）。
fn print_candidates(v: &serde_json::Value) {
    eprintln!("存在多条待接手，请用 `memora handoff list` 查看后用 --task 指定：");
    if let Some(items) = v["candidates"].as_array() {
        let offset = local_offset();
        for (i, s) in items.iter().enumerate() {
            eprintln!("{}", render_summary(i + 1, s, offset));
        }
    }
}

/// 从 `--file` 或 stdin 读取 save 的 JSON 输入。
fn read_input(opts: &HandoffOpts) -> Result<String, String> {
    match &opts.file {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|err| format!("cannot read {}: {err}", path.display())),
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|err| format!("cannot read stdin: {err}"))?;
            Ok(buf)
        }
    }
}

/// 摘要渲染（列表选择界面）：`N) goal [client/session] task_id  状态片段  时间`。
///
/// @intent 字段取舍 = 「认得出是哪一条」（docs §4.2）：goal 打头，client / session_id
///         紧随，task_id 供「抄下来」交给 load / ack，时间列辅助判断陈旧度。
fn render_summary(index: usize, s: &serde_json::Value, offset: time::UtcOffset) -> String {
    let goal = s["goal"].as_str().unwrap_or("");
    let client = s["client"].as_str().unwrap_or("");
    let session = s["session_id"].as_str().unwrap_or("");
    let task_id = s["task_id"].as_str().unwrap_or("");
    let excerpt = s["state_excerpt"].as_str().unwrap_or("");
    let time = s["created_at"]
        .as_i64()
        .map(|t| format_time(t, offset))
        .unwrap_or_default();
    let who = if client.is_empty() && session.is_empty() {
        String::new()
    } else {
        format!(" [{client}/{session}]")
    };
    format!("{index}) {goal}{who} {task_id}  {excerpt}  {time}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};

    /// 极简 REST 桩：循环应答，按「路径包含」匹配 canned 响应。
    ///
    /// @intent 覆盖 CLI 的**整条执行链**（解析 REST 响应 → 渲染 / 退出码），
    ///         而不是 mock 某一层；顺序应答即可，CLI 是顺序请求。
    fn serve(routes: Vec<(&'static str, u16, &'static str)>) -> Endpoint {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream: TcpStream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 2048];
                loop {
                    let n = std::io::Read::read(&mut stream, &mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some(header_end) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().ok())
                            })
                            .flatten()
                            .unwrap_or(0);
                        if buf.len() >= header_end + len {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buf);
                let mut parts = text.split_whitespace();
                let method = parts.next().unwrap_or("");
                let full_path = parts.next().unwrap_or("");
                let path_only = full_path.split('?').next().unwrap_or("");
                // 匹配规则：method 相等 + 路径以后缀结尾。`ends_with` 而非 `contains`：
                // "/api/v1/projects" 会吃掉所有 handoffs 子路径；后缀与 method 组合
                // 才能区分 `GET /handoffs/{task}` 与 `DELETE /handoffs/{task}`。
                let hit = routes.iter().find(|(key, _, _)| match key.split_once(' ') {
                    Some((m, suffix)) => m == method && path_only.ends_with(suffix),
                    None => path_only.ends_with(key),
                });
                let (status, body) = match hit {
                    Some((_, s, b)) => (*s, *b),
                    None => (404, "{}"),
                };
                let reason = match status {
                    200 => "OK",
                    201 => "Created",
                    204 => "No Content",
                    300 => "Multiple Choices",
                    404 => "Not Found",
                    _ => "OK",
                };
                let payload = if status == 204 {
                    // 204 也必须是完整的 HTTP 响应（有头无体）；直接关连接会让
                    // 客户端报「Peer disconnected」而不是拿到状态码
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                } else {
                    format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                };
                stream.write_all(payload.as_bytes()).ok();
            }
        });
        Endpoint::parse(&format!("http://{addr}"), 0).unwrap()
    }

    fn run_load(opts: &HandoffOpts, cfg: &HandoffConfig) -> Result<i32, String> {
        run(crate::cli::HandoffAction::Load, opts, cfg, None)
    }

    fn run_list(opts: &HandoffOpts, cfg: &HandoffConfig) -> Result<i32, String> {
        run(crate::cli::HandoffAction::List, opts, cfg, None)
    }

    fn run_ack(opts: &HandoffOpts, cfg: &HandoffConfig) -> Result<i32, String> {
        run(crate::cli::HandoffAction::Ack, opts, cfg, None)
    }

    fn run_save(opts: &HandoffOpts, cfg: &HandoffConfig) -> Result<i32, String> {
        run(crate::cli::HandoffAction::Save, opts, cfg, None)
    }

    fn run_rm(opts: &HandoffOpts, cfg: &HandoffConfig) -> Result<i32, String> {
        run(crate::cli::HandoffAction::Remove, opts, cfg, None)
    }

    const TOKEN: &str = "admin-tok";

    // ------------------------------------------------------ resolve_project_id

    #[test]
    fn explicit_project_wins_over_everything() {
        let (id, auto) =
            resolve_project_id(Some(" explicit "), Some("env"), &["a".into(), "b".into()]).unwrap();
        assert_eq!(id, "explicit");
        assert!(!auto);
    }

    #[test]
    fn env_project_is_second_choice() {
        let (id, auto) = resolve_project_id(None, Some("from-env"), &["a".into()]).unwrap();
        assert_eq!(id, "from-env");
        assert!(!auto);
    }

    #[test]
    fn single_project_is_auto_selected() {
        let (id, auto) = resolve_project_id(None, None, &["solo".into()]).unwrap();
        assert_eq!(id, "solo");
        assert!(auto, "自动选定时必须提示调用方打印选定结果");
    }

    #[test]
    fn zero_or_multiple_projects_are_actionable_errors() {
        let err = resolve_project_id(None, None, &[]).unwrap_err();
        assert!(err.contains("MEMORA_PROJECT"), "{err}");
        let err = resolve_project_id(None, None, &["a".into(), "b".into()]).unwrap_err();
        assert!(err.contains("a") && err.contains("b"), "{err}");
    }

    // ------------------------------------------------------ format_time

    #[test]
    fn time_is_formatted_as_utc_date_when_offset_is_utc() {
        // 2026-10-14 16:00:00 UTC
        assert_eq!(
            format_time(1791993600, time::UtcOffset::UTC),
            "2026-10-14 16:00"
        );
    }

    #[test]
    fn invalid_timestamp_falls_back_to_raw_digits() {
        assert_eq!(
            format_time(i64::MAX, time::UtcOffset::UTC),
            i64::MAX.to_string()
        );
    }

    // ------------------------------------------------------ run（整链，经桩服务）

    #[test]
    fn list_without_args_auto_selects_single_project_and_renders_table() {
        let ep = serve(vec![
            ("GET /api/v1/projects", 200, r#"[{"project_id":"solo"}]"#),
            (
                "GET /handoffs",
                200,
                r#"{"handoffs":[
                    {"task_id":"current@zed","goal":"把 handoff 并入 memora","state_excerpt":"实现中",
                     "client":"zed","session_id":"A1","created_at":1791993600,"acked":false,"acked_by":"","next_step_count":2}
                ]}"#,
            ),
        ]);
        let cfg = HandoffConfig {
            endpoint: ep,
            token: TOKEN.into(),
        };
        let out = run_list(&HandoffOpts::default(), &cfg).unwrap();
        assert_eq!(out, 0);
    }

    #[test]
    fn load_with_no_pending_exits_zero() {
        let ep = serve(vec![
            ("GET /api/v1/projects", 200, r#"[{"project_id":"solo"}]"#),
            ("GET /handoffs/pending", 204, ""),
        ]);
        let cfg = HandoffConfig {
            endpoint: ep,
            token: TOKEN.into(),
        };
        let out = run_load(&HandoffOpts::default(), &cfg).unwrap();
        assert_eq!(out, 0, "无待接手是正常状态");
    }

    #[test]
    fn ambiguous_load_exits_nonzero() {
        let ep = serve(vec![
            ("GET /api/v1/projects", 200, r#"[{"project_id":"solo"}]"#),
            (
                "GET /handoffs/pending",
                300,
                r#"{"status":"ambiguous","candidates":[{"task_id":"a"},{"task_id":"b"}]}"#,
            ),
        ]);
        let cfg = HandoffConfig {
            endpoint: ep,
            token: TOKEN.into(),
        };
        let out = run_load(&HandoffOpts::default(), &cfg).unwrap();
        assert_ne!(out, 0, "歧义必须非零退出（脚本需要人来选）");
    }

    #[test]
    fn lost_ack_race_exits_nonzero() {
        let ep = serve(vec![
            ("GET /api/v1/projects", 200, r#"[{"project_id":"solo"}]"#),
            (
                "POST /pending/ack",
                200,
                r#"{"status":"found","won":false,"handoff":{"acked_by":"zed"}}"#,
            ),
        ]);
        let cfg = HandoffConfig {
            endpoint: ep,
            token: TOKEN.into(),
        };
        let out = run_ack(
            &HandoffOpts {
                client: Some("cursor".into()),
                ..Default::default()
            },
            &cfg,
        )
        .unwrap();
        assert_ne!(out, 0, "won=false 必须让脚本知道没抢到");
    }

    #[test]
    fn save_reads_json_from_file() {
        let path =
            std::env::temp_dir().join(format!("memora_cli_save_{}.json", std::process::id()));
        std::fs::write(&path, r#"{"goal":"g","state":"s","client":"zed"}"#).unwrap();
        let ep = serve(vec![
            ("GET /api/v1/projects", 200, r#"[{"project_id":"solo"}]"#),
            (
                "POST /handoffs",
                201,
                r#"{"handoff":{"task_id":"current@zed","revision":1},"task_id":"current@zed","revision":1}"#,
            ),
        ]);
        let cfg = HandoffConfig {
            endpoint: ep,
            token: TOKEN.into(),
        };
        let out = run_save(
            &HandoffOpts {
                file: Some(path.clone()),
                ..Default::default()
            },
            &cfg,
        )
        .unwrap();
        assert_eq!(out, 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn invalid_save_json_is_rejected_without_a_request() {
        let path = std::env::temp_dir().join(format!("memora_cli_bad_{}.json", std::process::id()));
        std::fs::write(&path, "not json").unwrap();
        let ep = serve(vec![]); // 任何请求都会 404
        let cfg = HandoffConfig {
            endpoint: ep,
            token: TOKEN.into(),
        };
        let result = run_save(
            &HandoffOpts {
                file: Some(path.clone()),
                ..Default::default()
            },
            &cfg,
        );
        assert!(result.is_err(), "非法 JSON 必须在发请求前被拒");
        std::fs::remove_file(&path).ok();
    }

    /// rm 是破坏性操作：解析层强制 `--task`，执行层防御性再挡一次（run 可被直接调用）。
    #[test]
    fn rm_without_task_is_rejected_defensively() {
        let ep = serve(vec![]);
        let cfg = HandoffConfig {
            endpoint: ep,
            token: TOKEN.into(),
        };
        let err = run_rm(
            &HandoffOpts {
                project: Some("solo".into()),
                ..Default::default()
            },
            &cfg,
        )
        .unwrap_err();
        assert!(err.contains("--task"), "{err}");
    }

    #[test]
    fn rm_with_task_sends_delete() {
        let ep = serve(vec![(
            "DELETE /handoffs/gone",
            200,
            r#"{"task_id":"gone"}"#,
        )]);
        let cfg = HandoffConfig {
            endpoint: ep,
            token: TOKEN.into(),
        };
        let out = run_rm(
            &HandoffOpts {
                project: Some("solo".into()),
                task: Some("gone".into()),
                ..Default::default()
            },
            &cfg,
        )
        .unwrap();
        assert_eq!(out, 0);
    }

    #[test]
    fn explicit_project_skips_project_discovery() {
        let ep = serve(vec![("GET /handoffs/pending", 204, "")]);
        let cfg = HandoffConfig {
            endpoint: ep,
            token: TOKEN.into(),
        };
        let out = run_load(
            &HandoffOpts {
                project: Some("solo".into()),
                ..Default::default()
            },
            &cfg,
        )
        .unwrap();
        assert_eq!(out, 0);
    }

    /// list 无 --project 时按两级规则自动选定唯一项目（并因此走两条请求）。
    #[test]
    fn list_renders_human_readable_summary() {
        let ep = serve(vec![
            ("GET /api/v1/projects", 200, r#"[{"project_id":"solo"}]"#),
            (
                "GET /handoffs",
                200,
                r#"{"handoffs":[
                    {"task_id":"current@zed","goal":"把 handoff 并入 memora","state_excerpt":"实现中",
                     "client":"zed","session_id":"A1","created_at":1791993600,"acked":false,"acked_by":"","next_step_count":2}
                ]}"#,
            ),
        ]);
        let cfg = HandoffConfig {
            endpoint: ep,
            token: TOKEN.into(),
        };
        let out = run_list(&HandoffOpts::default(), &cfg).unwrap();
        assert_eq!(out, 0);
    }
}
