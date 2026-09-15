//! 自守护进程管理（caddy 式 `start` / `stop` / `restart` / `status` / `init`）。
//!
//! @author yujinping
//! @intent P4.2：为「无 systemd、无 root」的裸机自托管提供进程生命周期管理。
//!         之所以采用混合形态而非纯自守护：**自守护无法在进程崩溃后自动拉起**，
//!         这对长期记忆服务是不可接受的；保留前台 `run` 后，systemd 退化为可选组件，
//!         只负责开机自启与崩溃重启，两个场景都不妥协。
//!
//! @intent 全部实现为同步阻塞调用（含健康探测与退出等待）：这些都是一次性动作，
//!         同步代码换来「无 async 传染」且可直接单测，比引入 tokio I/O 组合子更划算。
//!         `run` 分支仍由 `main` 用 tokio 运行时承载。
//!
//! @intent 启动子进程时**显式注入已解析的配置**（而非让子进程重新读 `.env`）：
//!         否则子进程的 CWD 与父进程可能不同，`.env` 探测结果随之漂移，
//!         出现「`start` 时是 6789、实际监听 7000」这类难以理解的现象。

use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::config::{Config, DEFAULT_DATA_DIR, DEFAULT_PORT, ENV_FILE_NAME};
use crate::pidfile::{self, PidFile, EXIT_NOT_RUNNING};

/// 健康探测的轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// `start` 等待服务就绪的上限。
const HEALTH_TIMEOUT: Duration = Duration::from_secs(15);
/// 单次健康探测的连接与读写超时。
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// `stop` 等待优雅退出的上限，超时后升级为 SIGKILL。
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// SIGKILL 之后等待进程被回收的上限。
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// systemd unit 模板（`include_str!` 内嵌，故 `init` 不依赖部署目录）。
const SYSTEMD_UNIT: &str = include_str!("../deploy/memora.service");
/// Caddyfile 模板（同上）。
const CADDYFILE: &str = include_str!("../deploy/Caddyfile");
/// `.env` 模板（同上）。
const ENV_TEMPLATE: &str = include_str!("../deploy/env.template");

/// 后台子进程的启动规格。
///
/// @intent 把「怎么启动」抽成可断言的数据而非直接 spawn，使参数拼装与配置注入
///         能被单元测试钉住——这是最容易出错又最难在集成测试里观察的一环。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    /// 子进程可执行文件
    pub exe: PathBuf,
    /// 传给子进程的参数
    pub args: Vec<String>,
    /// 显式注入的环境变量（覆盖子进程自身可能探测到的 `.env`）
    pub envs: Vec<(String, String)>,
    /// 子进程 stdout/stderr 追加写入的日志文件
    pub log_path: PathBuf,
}

/// 由已解析配置构造后台子进程规格。
///
/// @intent 只注入非空项：`ADMIN_TOKEN` 为空时不注入，让子进程沿用自身环境（可能来自
///         它探测到的 `.env`），避免用空值覆盖掉有效配置。
pub fn child_spec(exe: &Path, config: &Config, log_path: &Path) -> ChildSpec {
    let mut envs = vec![
        ("PORT".to_string(), config.port.to_string()),
        ("DATA_DIR".to_string(), config.data_dir.clone()),
        (
            "STORAGE_BACKEND".to_string(),
            config.storage_backend.clone(),
        ),
    ];

    // 只注入非空项：空值会覆盖子进程自身探测到的 `.env` 配置，把有效设置抹成默认值。
    if !config.admin_token.trim().is_empty() {
        envs.push(("ADMIN_TOKEN".to_string(), config.admin_token.clone()));
    }
    if !config.mcp_allowed_hosts.is_empty() {
        envs.push((
            "MCP_ALLOWED_HOSTS".to_string(),
            config.mcp_allowed_hosts.join(","),
        ));
    }

    ChildSpec {
        exe: exe.to_path_buf(),
        args: vec!["run".to_string()],
        envs,
        log_path: log_path.to_path_buf(),
    }
}

/// 后台启动：写 PID 文件，健康自检通过才返回成功。
///
/// @intent 「命令返回 0 但服务没起来」是自守护脚本最恶劣的失败模式，故本函数以
///         `/health` 探测结果作为唯一成功判据；失败时回滚（终止子进程 + 删除 PID 文件），
///         并把日志路径写进错误信息，使排障一步到位。
pub fn start(config: &Config) -> Result<i32> {
    let data_dir = PathBuf::from(&config.data_dir);
    fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;

    let pid_file = PidFile::new(&data_dir);
    // live_pid 会顺手清理陈旧 PID 文件（上一个进程崩溃后留下的），故无需单独处理
    if let Some(pid) = pid_file.live_pid()? {
        bail!("memora is already running (pid {pid}); use `memora restart` or `memora stop` first");
    }

    let exe = std::env::current_exe().context("cannot determine the current executable path")?;
    let log_path = PidFile::log_path(&data_dir);
    let spec = child_spec(&exe, config, &log_path);

    let child = spawn_detached(&spec)?;
    let pid = child.id() as i32;

    // 先记 PID 再自检：自检期间进程已可被 `stop` 管理；若此处写失败则立刻回收子进程，
    // 避免留下「在跑但没人管」的孤儿服务。
    if let Err(err) = pid_file.write(child.id()) {
        let _ = pidfile::terminate(pid);
        return Err(err).context("failed to write pid file");
    }

    if !wait_for_health(config.port, HEALTH_TIMEOUT) {
        let _ = pidfile::terminate(pid);
        if !wait_for_exit(pid, STOP_TIMEOUT) {
            let _ = pidfile::force_kill(pid);
        }
        let _ = pid_file.remove();
        bail!(
            "memora did not become healthy on port {} within {}s; check the log at {}",
            config.port,
            HEALTH_TIMEOUT.as_secs(),
            log_path.display()
        );
    }

    println!("memora started (pid {pid}) on port {}", config.port);
    println!("  log:  {}", log_path.display());
    println!("  pid:  {}", pid_file.path().display());
    println!("  stop: memora stop");
    Ok(0)
}

/// 停止后台进程：SIGTERM 优雅退出，超时升级 SIGKILL。
///
/// @intent 未运行时返回成功（幂等），以便脚本无条件调用；`status` 才承担
///         「未运行即非零」的语义。
pub fn stop(config: &Config) -> Result<i32> {
    let pid_file = PidFile::new(Path::new(&config.data_dir));
    let Some(pid) = pid_file.live_pid()? else {
        println!("memora is not running");
        return Ok(0);
    };

    pidfile::terminate(pid).with_context(|| format!("failed to send SIGTERM to pid {pid}"))?;

    if wait_for_exit(pid, STOP_TIMEOUT) {
        pid_file.remove()?;
        println!("memora stopped (pid {pid})");
        return Ok(0);
    }

    println!(
        "pid {pid} did not exit within {}s, sending SIGKILL",
        STOP_TIMEOUT.as_secs()
    );
    pidfile::force_kill(pid).with_context(|| format!("failed to send SIGKILL to pid {pid}"))?;
    if !wait_for_exit(pid, KILL_TIMEOUT) {
        bail!("pid {pid} is still alive after SIGKILL; check for uninterruptible I/O");
    }
    pid_file.remove()?;
    println!("memora killed (pid {pid})");
    Ok(0)
}

/// 重启：等价于 `stop` + `start`（未运行时可直接拉起）。
pub fn restart(config: &Config) -> Result<i32> {
    stop(config)?;
    start(config)
}

/// 查看状态：未运行时返回退出码 `3`。
pub fn status(config: &Config) -> Result<i32> {
    let pid_file = PidFile::new(Path::new(&config.data_dir));
    let Some(pid) = pid_file.live_pid()? else {
        println!(
            "memora is not running (no live pid file at {})",
            pid_file.path().display()
        );
        return Ok(EXIT_NOT_RUNNING);
    };

    // 进程存活不等于服务可用（可能仍在初始化，或已卡死），故额外探一次 /health
    let healthy = probe_health(config.port);
    println!("memora is running (pid {pid})");
    println!(
        "  port: {}{}",
        config.port,
        if healthy { " (healthy)" } else { " (unreachable)" }
    );
    println!("  data: {}", config.data_dir);
    println!("  pid:  {}", pid_file.path().display());
    println!(
        "  log:  {}",
        PidFile::log_path(Path::new(&config.data_dir)).display()
    );
    Ok(if healthy { 0 } else { 1 })
}

/// 生成 `.env` 与部署模板。
pub fn init(out_dir: &Path, force: bool) -> Result<i32> {
    let env_path = out_dir.join(ENV_FILE_NAME);
    if env_path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            env_path.display()
        );
    }

    let exe = std::env::current_exe().context("cannot determine the current executable path")?;
    fs::write(&env_path, render_env_template(DEFAULT_DATA_DIR, DEFAULT_PORT))
        .with_context(|| format!("cannot write {}", env_path.display()))?;

    println!("wrote {}", env_path.display());
    println!();
    println!("下一步：");
    println!(
        "  1. 编辑 {}：至少设置 ADMIN_TOKEN（openssl rand -hex 32）",
        env_path.display()
    );
    println!("  2. 前台试跑：{} run", exe.display());
    println!("  3. 后台常驻：{} start    （或使用下方 systemd unit）", exe.display());
    println!();
    println!("{}", render_deploy_templates(&exe, DEFAULT_DATA_DIR, DEFAULT_PORT));
    Ok(0)
}

/// 渲染 `.env` 模板。
pub fn render_env_template(data_dir: &str, port: u16) -> String {
    ENV_TEMPLATE
        .replace("__PORT__", &port.to_string())
        .replace("__DATA_DIR__", data_dir)
}

/// 渲染 systemd unit 与 Caddyfile 两段部署模板（占位符已替换为本次运行的实际取值）。
pub fn render_deploy_templates(exe: &Path, data_dir: &str, port: u16) -> String {
    let substitute = |template: &str| -> String {
        template
            .replace("__EXE__", &exe.display().to_string())
            .replace("__DATA_DIR__", data_dir)
            .replace("__PORT__", &port.to_string())
    };

    format!(
        "# ===== systemd unit → /etc/systemd/system/memora.service =====\n{}\n\
         # ===== Caddyfile 片段 → 并入 /etc/caddy/Caddyfile =====\n{}\n",
        substitute(SYSTEMD_UNIT),
        substitute(CADDYFILE),
    )
}

/// 分离地启动子进程：新建会话（setsid）并把 I/O 重定向到日志文件。
fn spawn_detached(spec: &ChildSpec) -> Result<Child> {
    use std::os::unix::process::CommandExt;

    let stdout = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.log_path)
        .with_context(|| format!("cannot open log file {}", spec.log_path.display()))?;
    let stderr = stdout
        .try_clone()
        .context("cannot duplicate the log file handle")?;

    let mut command = Command::new(&spec.exe);
    command
        .args(&spec.args)
        // stdin 接到 /dev/null：服务不接受交互输入，留着终端会让「后台进程」仍持有 tty
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .envs(spec.envs.iter().cloned());

    // SAFETY: `pre_exec` 在 fork 之后、exec 之前的子进程中运行，只允许调用
    // async-signal-safe 的函数；`setsid(2)` 满足该约束，且不触碰任何 Rust 分配器状态。
    // 失败时返回错误，spawn 会随之失败，不会留下半初始化的子进程。
    unsafe {
        command.pre_exec(|| {
            // 新建会话并脱离控制终端：缺少这一步时，启动它的 shell 退出会把 SIGHUP
            // 传播给服务进程，表现为「SSH 一断开服务就没了」。
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command
        .spawn()
        .with_context(|| format!("failed to spawn {}", spec.exe.display()))
}

/// 轮询等待进程退出；超时返回 `false`。
pub fn wait_for_exit(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !pidfile::is_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 轮询等待服务健康；超时返回 `false`。
pub fn wait_for_health(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if probe_health(port) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 单次健康探测：`GET /health` 返回 200 且响应体含 `ok` 才算通过。
///
/// @intent 校验响应体而非仅状态码：端口被其他服务占用时，仅凭 200 会误判为本服务已就绪。
pub fn probe_health(port: u16) -> bool {
    match http_get(port, "/health", PROBE_TIMEOUT) {
        Ok((200, body)) => body.contains("ok"),
        _ => false,
    }
}

/// 发起一次最小化 HTTP/1.1 `GET`，返回 `(状态码, 响应体)`。
///
/// @intent 手写而非引入 HTTP 客户端：只需处理「本机、单请求、读到 EOF」这一种情形，
///         为此拉入 reqwest/hyper 客户端栈不划算。`Connection: close` 让服务端在
///         响应后关闭连接，故 `read_to_string` 能自然读到 EOF。
pub fn http_get(port: u16, path: &str, timeout: Duration) -> io::Result<(u16, String)> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let request =
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    stream.flush()?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    parse_http_response(&response)
}

/// 从原始响应中解析状态码与响应体。
fn parse_http_response(response: &str) -> io::Result<(u16, String)> {
    let (head, body) = match response.split_once("\r\n\r\n") {
        Some((head, body)) => (head, body),
        None => (response, ""),
    };

    let status_line = head.lines().next().unwrap_or_default();
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|raw| raw.parse::<u16>().ok())
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("malformed HTTP response: {status_line:?}"),
            )
        })?;

    Ok((code, body.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// 起一个只回一个固定响应的「假服务」，返回其端口。
    fn fake_http_server(status_line: &'static str, body: &'static str) -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            // 只服务一次探测即可
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    /// 取一个确定无人监听的端口。
    fn closed_port() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    #[test]
    fn http_get_reads_status_and_body() {
        let port = fake_http_server("HTTP/1.1 200 OK", "ok");
        let (status, body) = http_get(port, "/health", PROBE_TIMEOUT).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "ok");
    }

    #[test]
    fn http_get_reports_error_when_nothing_listens() {
        assert!(http_get(closed_port(), "/health", PROBE_TIMEOUT).is_err());
    }

    /// 端口被别的服务占用时不得误判为「本服务已就绪」。
    #[test]
    fn probe_health_requires_ok_body() {
        let ok = fake_http_server("HTTP/1.1 200 OK", "ok");
        assert!(probe_health(ok));

        let alien = fake_http_server("HTTP/1.1 200 OK", "{\"service\":\"something-else\"}");
        assert!(!probe_health(alien), "200 但响应体不是本服务的标识");

        let unhealthy = fake_http_server("HTTP/1.1 503 Service Unavailable", "ok");
        assert!(!probe_health(unhealthy));

        assert!(!probe_health(closed_port()));
    }

    #[test]
    fn wait_for_health_gives_up_on_timeout() {
        let started = Instant::now();
        assert!(!wait_for_health(closed_port(), Duration::from_millis(300)));
        assert!(started.elapsed() >= Duration::from_millis(250));
    }

    #[test]
    fn wait_for_health_succeeds_immediately_when_ready() {
        let port = fake_http_server("HTTP/1.1 200 OK", "ok");
        assert!(wait_for_health(port, Duration::from_secs(2)));
    }

    #[test]
    fn wait_for_exit_detects_a_live_process() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        assert!(
            !wait_for_exit(pid, Duration::from_millis(200)),
            "仍在运行的进程不应被判定为已退出"
        );
        pidfile::force_kill(pid).unwrap();
        let mut child = child;
        let _ = child.wait();
    }

    #[test]
    fn wait_for_exit_detects_an_exited_process() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert!(wait_for_exit(pid, Duration::from_millis(200)));
    }

    /// 子进程必须拿到与 `start` 时刻一致的配置，而不是重新探测 `.env`。
    #[test]
    fn child_spec_injects_resolved_config() {
        let mut config = Config::for_test(6790, "/var/lib/memora", "s3cret", "sqlite_file");
        config.mcp_allowed_hosts = vec!["mem.example.com".to_string(), "localhost".to_string()];
        let spec = child_spec(Path::new("/usr/local/bin/memora"), &config, Path::new("l.log"));

        assert_eq!(spec.exe, PathBuf::from("/usr/local/bin/memora"));
        assert_eq!(spec.args, vec!["run".to_string()]);
        assert!(spec.envs.contains(&("PORT".to_string(), "6790".to_string())));
        assert!(spec
            .envs
            .contains(&("DATA_DIR".to_string(), "/var/lib/memora".to_string())));
        assert!(spec
            .envs
            .contains(&("ADMIN_TOKEN".to_string(), "s3cret".to_string())));
        assert!(spec.envs.contains(&(
            "MCP_ALLOWED_HOSTS".to_string(),
            "mem.example.com,localhost".to_string()
        )));
    }

    /// 空值不得注入：否则会用空 ADMIN_TOKEN 覆盖子进程自身探测到的 `.env` 配置。
    #[test]
    fn child_spec_omits_empty_values() {
        let config = Config::for_test(6790, "/data", "", "sqlite_file");
        let spec = child_spec(Path::new("/bin/memora"), &config, Path::new("l.log"));

        assert!(!spec.envs.iter().any(|(k, _)| k == "ADMIN_TOKEN"));
        assert!(
            !spec.envs.iter().any(|(k, _)| k == "MCP_ALLOWED_HOSTS"),
            "未配置白名单时不得注入空串——空串会被解析为「未指定」但徒增歧义"
        );
    }

    #[test]
    fn status_is_not_running_for_a_fresh_data_dir() {
        let dir = std::env::temp_dir().join(format!("memora_p4_status_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let config = Config::for_test(
            closed_port(),
            dir.display().to_string(),
            "admin",
            "sqlite_file",
        );

        assert_eq!(status(&config).unwrap(), EXIT_NOT_RUNNING);
    }

    #[test]
    fn init_writes_env_template_and_refuses_to_overwrite() {
        let dir = std::env::temp_dir().join(format!("memora_p4_init_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let env_path = dir.join(ENV_FILE_NAME);
        let _ = fs::remove_file(&env_path);

        assert_eq!(init(&dir, false).unwrap(), 0);
        let written = fs::read_to_string(&env_path).unwrap();
        assert!(written.contains("PORT="), "模板应包含 PORT");

        // 已存在 → 默认拒绝覆盖，避免抹掉用户改过的配置
        assert!(init(&dir, false).is_err());
        assert!(init(&dir, true).is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rendered_templates_substitute_runtime_values() {
        let rendered =
            render_deploy_templates(Path::new("/opt/memora/memora"), "/var/lib/memora", 6790);
        assert!(rendered.contains("/opt/memora/memora"), "应替换可执行文件路径");
        assert!(rendered.contains("/var/lib/memora"), "应替换数据目录");
        assert!(rendered.contains("6790"), "应替换端口");
        assert!(
            !rendered.contains("__"),
            "不得残留未替换的占位符: {rendered}"
        );
        assert!(rendered.contains("systemd"), "应包含 systemd unit");
        assert!(rendered.contains("reverse_proxy"), "应包含 Caddy 反代配置");
    }
}
