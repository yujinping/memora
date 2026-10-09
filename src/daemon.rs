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
//! @intent 启动子进程时**显式注入已解析的配置**（而非让子进程重新推导配置）：
//!         否则子进程的 CWD 与父进程可能不同，相对路径配置的解析结果随之漂移，
//!         出现「`start` 时是 6789、实际监听 7000」这类难以理解的现象。

use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::instance::{self, InstanceFile, InstanceRecord, EXIT_NOT_RUNNING};

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
/// 配置文件模板（同上）。
const CONFIG_TEMPLATE: &str = include_str!("../deploy/config.toml.template");

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
    /// 显式注入的环境变量（临时命令行覆盖不在配置文件里，只能靠环境变量传递）
    pub envs: Vec<(String, String)>,
    /// 子进程 stdout/stderr 追加写入的日志文件
    pub log_path: PathBuf,
}

/// 由已解析配置构造后台子进程规格。
///
/// @intent 只注入非空项：`ADMIN_TOKEN` 为空时不注入，让子进程沿用自身环境（可能来自
///         它自己读到的配置），避免用空值覆盖掉有效配置。
///
/// @intent 环境变量注入与 `--config` 透传并存，两者各管一段：
///         `start --port 6799` 这类**临时覆盖不在文件里**，只能靠环境变量传给子进程；
///         而文件里那些不可被环境变量表达的键，则靠子进程自己按同一路径重读。
///         仅当父进程确实读到了文件时才传 `--config`：显式指向不存在的文件是硬错误，
///         在「无配置文件」的情形下照传会把子进程直接顶死。
pub fn child_spec(exe: &Path, config: &Config, log_path: &Path) -> ChildSpec {
    let mut envs = vec![
        ("PORT".to_string(), config.port.to_string()),
        ("DATA_DIR".to_string(), config.data_dir.clone()),
        (
            "STORAGE_BACKEND".to_string(),
            config.storage_backend.clone(),
        ),
    ];

    // 只注入非空项：空值会覆盖子进程自身读到的配置，把有效设置抹成默认值。
    if !config.admin_token.trim().is_empty() {
        envs.push(("ADMIN_TOKEN".to_string(), config.admin_token.clone()));
    }
    if !config.mcp_allowed_hosts.is_empty() {
        envs.push((
            "MCP_ALLOWED_HOSTS".to_string(),
            config.mcp_allowed_hosts.join(","),
        ));
    }

    let mut args = vec!["run".to_string()];
    if config.config_loaded {
        args.push("--config".to_string());
        args.push(config.config_path.display().to_string());
    }

    ChildSpec {
        exe: exe.to_path_buf(),
        args,
        envs,
        log_path: log_path.to_path_buf(),
    }
}

/// 后台启动：写实例记录，健康自检通过才返回成功。
///
/// @intent 「命令返回 0 但服务没起来」是自守护脚本最恶劣的失败模式，故本函数以
///         `/health` 探测结果作为唯一成功判据；失败时回滚（终止子进程 + 删除记录），
///         并把日志路径写进错误信息，使排障一步到位。
pub fn start(config: &Config) -> Result<i32> {
    let data_dir = PathBuf::from(&config.data_dir);
    fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;

    let instance_file = InstanceFile::new(&data_dir);
    // live() 会顺手清理陈旧记录（上一个进程崩溃后留下的），故无需单独处理
    if let Some(record) = instance_file.live()? {
        bail!(
            "memora is already running (pid {}); use `memora restart` or `memora stop` first",
            record.pid
        );
    }

    let exe = std::env::current_exe().context("cannot determine the current executable path")?;
    let log_path = InstanceFile::log_path(&data_dir);
    let spec = child_spec(&exe, config, &log_path);

    let child = spawn_detached(&spec)?;
    let pid = child.id() as i32;

    // 记录「实际要跑成什么样」：端口取已解析值（含 `--port` 这类临时覆盖），
    // 而不是等子进程自报——子进程若因配置不一致而监听别的端口，健康自检就会失败。
    let record = InstanceRecord {
        pid,
        port: Some(config.port),
        data_dir: config.data_dir.clone(),
        started_at: instance::now_rfc3339(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    // 先记记录再自检：自检期间进程已可被 `stop` 管理；若此处写失败则立刻回收子进程，
    // 避免留下「在跑但没人管」的孤儿服务。
    if let Err(err) = instance_file.write(&record) {
        let _ = instance::terminate(pid);
        return Err(err).context("failed to write the instance record");
    }

    if !wait_for_health(config.port, HEALTH_TIMEOUT) {
        let _ = instance::terminate(pid);
        if !wait_for_exit(pid, STOP_TIMEOUT) {
            let _ = instance::force_kill(pid);
        }
        let _ = instance_file.remove();
        bail!(
            "memora did not become healthy on port {} within {}s; check the log at {}",
            config.port,
            HEALTH_TIMEOUT.as_secs(),
            log_path.display()
        );
    }

    println!("memora started (pid {pid}) on port {}", config.port);
    println!("  log:  {}", log_path.display());
    println!("  pid:  {}", instance_file.path().display());
    println!("  stop: memora stop");
    Ok(0)
}

/// 停止后台进程：SIGTERM 优雅退出，超时升级 SIGKILL。
///
/// @intent 未运行时返回成功（幂等），以便脚本无条件调用；`status` 才承担
///         「未运行即非零」的语义。
pub fn stop(config: &Config) -> Result<i32> {
    let instance_file = InstanceFile::new(Path::new(&config.data_dir));
    let Some(record) = instance_file.live()? else {
        println!("memora is not running");
        return Ok(0);
    };
    let pid = record.pid;

    instance::terminate(pid).with_context(|| format!("failed to send SIGTERM to pid {pid}"))?;

    if wait_for_exit(pid, STOP_TIMEOUT) {
        instance_file.remove()?;
        println!("memora stopped (pid {pid})");
        return Ok(0);
    }

    println!(
        "pid {pid} did not exit within {}s, sending SIGKILL",
        STOP_TIMEOUT.as_secs()
    );
    instance::force_kill(pid).with_context(|| format!("failed to send SIGKILL to pid {pid}"))?;
    if !wait_for_exit(pid, KILL_TIMEOUT) {
        bail!("pid {pid} is still alive after SIGKILL; check for uninterruptible I/O");
    }
    instance_file.remove()?;
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
    let instance_file = InstanceFile::new(Path::new(&config.data_dir));
    let Some(record) = instance_file.live()? else {
        println!(
            "memora is not running (no live instance record at {})",
            instance_file.path().display()
        );
        return Ok(EXIT_NOT_RUNNING);
    };

    // 进程存活不等于服务可用（可能仍在初始化，或已卡死），故额外探一次 /health。
    // 端口以**记录**为准：配置可能已被改过而进程尚未重启，那时探配置端口只会得到
    // 「unreachable」这种指向错误方向的结论。
    let port = record.port.unwrap_or(config.port);
    let healthy = probe_health(port);

    println!("memora is running (pid {})", record.pid);
    println!(
        "  port: {}{}",
        port,
        if healthy {
            " (healthy)"
        } else {
            " (unreachable)"
        }
    );
    match record.port {
        Some(recorded) if recorded != config.port => println!(
            "  note: the config asks for port {}; `memora restart` to apply it",
            config.port
        ),
        None => println!(
            "  note: this instance record predates port tracking; the port comes from the config"
        ),
        Some(_) => {}
    }
    println!("  data: {}", record.data_dir);
    println!("  pid:  {}", instance_file.path().display());
    println!(
        "  log:  {}",
        InstanceFile::log_path(Path::new(&config.data_dir)).display()
    );
    if !record.started_at.is_empty() {
        println!("  started: {}", record.started_at);
    }
    if !record.version.is_empty() {
        println!("  version: {}", record.version);
    }
    Ok(if healthy { 0 } else { 1 })
}

/// 生成配置文件与部署模板。
///
/// @intent 渲染的是**已解析的配置**（含命令行覆盖项），而不是模板里的内置默认值：
///         否则 `memora init --port 6799` 会生成一份写着 6789 的文件，
///         「我明明指定了端口」这类困惑随即发生。
pub fn init(config: &Config, force: bool) -> Result<i32> {
    let config_path = &config.config_path;
    if config_path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            config_path.display()
        );
    }
    if let Some(parent) = config_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create config dir {}", parent.display()))?;
    }

    let exe = std::env::current_exe().context("cannot determine the current executable path")?;
    write_private(config_path, &render_config_template(config))?;

    println!("wrote {}", config_path.display());
    println!();
    println!("下一步：");
    println!(
        "  1. 编辑 {}：至少设置 admin_token（openssl rand -hex 32）",
        config_path.display()
    );
    println!("  2. 前台试跑：{} run", exe.display());
    println!(
        "  3. 后台常驻：{} start    （或使用下方 systemd unit）",
        exe.display()
    );
    println!();
    println!("{}", render_deploy_templates(&exe, config));
    Ok(0)
}

/// 以 `0600` 写入文本文件（配置文件含 admin_token，不应被同组用户或他人读到）。
#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("cannot flush {}", path.display()))?;
    // `mode()` 只作用于新建文件；`--force` 覆盖已存在的 0644 文件时须显式收紧。
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot tighten permissions of {}", path.display()))
}

/// Windows 版 `write_private`：无 POSIX 权限位，私有性由用户目录 ACL 承担。
#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("cannot flush {}", path.display()))
}

/// 渲染 `config.toml` 模板（占位符替换为本次解析出的实际取值）。
///
/// @intent 取值一律经 `toml::Value` 转义后写入：数据目录里出现引号或反斜杠时，
///         手拼字符串会生成**语法非法**的文件，而报错会出现在下一次启动，
///         与真正的起因隔得很远。
pub fn render_config_template(config: &Config) -> String {
    let hosts = config.effective_mcp_allowed_hosts();
    let hosts_block = if hosts.is_empty() {
        "[]".to_string()
    } else {
        format!(
            "[\n{}\n]",
            hosts
                .iter()
                .map(|host| format!("    {}", toml_string(host)))
                .collect::<Vec<_>>()
                .join(",\n")
        )
    };
    // 空 token 写成占位值：`admin_token = ""` 会让所有 /api/v1 请求 401，
    // 而用户看到的是一份“看起来已经配好”的文件。
    let admin_token = if config.admin_token.trim().is_empty() {
        "change-me"
    } else {
        config.admin_token.as_str()
    };

    CONFIG_TEMPLATE
        .replace("__PORT__", &config.port.to_string())
        .replace("__DATA_DIR__", &toml_string(&config.data_dir))
        .replace("__ADMIN_TOKEN__", &toml_string(admin_token))
        .replace("__STORAGE_BACKEND__", &toml_string(&config.storage_backend))
        .replace("__MCP_ALLOWED_HOSTS__", &hosts_block)
        .replace("__LOG__", &toml_string(&config.log))
}

/// 把一个字符串渲染成合法的 TOML 字符串字面量（含引号与转义）。
fn toml_string(raw: &str) -> String {
    toml::Value::String(raw.to_string()).to_string()
}

/// 渲染 systemd unit 与 Caddyfile 两段部署模板（占位符已替换为本次运行的实际取值）。
pub fn render_deploy_templates(exe: &Path, config: &Config) -> String {
    let substitute = |template: &str| -> String {
        template
            .replace("__EXE__", &exe.display().to_string())
            .replace("__CONFIG__", &config.config_path.display().to_string())
            .replace("__DATA_DIR__", &config.data_dir)
            .replace("__PORT__", &config.port.to_string())
    };

    format!(
        "# ===== systemd unit → /etc/systemd/system/memora.service =====\n{}\n\
         # ===== Caddyfile 片段 → 并入 /etc/caddy/Caddyfile =====\n{}\n",
        substitute(SYSTEMD_UNIT),
        substitute(CADDYFILE),
    )
}

/// 分离地启动子进程：新建会话（setsid）并把 I/O 重定向到日志文件。
#[cfg(unix)]
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

/// Windows 版分离启动：新进程组 + 脱离控制台 + 不弹窗口（对应 POSIX setsid）。
///
/// @intent 无 `pre_exec` 钩子，改为 `creation_flags` 在创建进程时完成分离；
///         CREATE_NEW_PROCESS_GROUP 让子进程与父进程的控制台 Ctrl+C 隔离，
///         DETACHED_PROCESS 脱离父进程控制台，CREATE_NO_WINDOW 无窗口弹出。
#[cfg(not(unix))]
fn spawn_detached(spec: &ChildSpec) -> Result<Child> {
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
        // stdin 接到空设备：服务不接受交互输入
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .envs(spec.envs.iter().cloned())
        .creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW);

    command
        .spawn()
        .with_context(|| format!("failed to spawn {}", spec.exe.display()))
}

/// 轮询等待进程退出；超时返回 `false`。
pub fn wait_for_exit(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !instance::is_alive(pid) {
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

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
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
    #[cfg(unix)]
    fn wait_for_exit_detects_a_live_process() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        assert!(
            !wait_for_exit(pid, Duration::from_millis(200)),
            "仍在运行的进程不应被判定为已退出"
        );
        instance::force_kill(pid).unwrap();
        let mut child = child;
        let _ = child.wait();
    }

    #[test]
    #[cfg(unix)]
    fn wait_for_exit_detects_an_exited_process() {
        let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert!(wait_for_exit(pid, Duration::from_millis(200)));
    }

    /// 子进程必须拿到与 `start` 时刻一致的配置，而不是重新探测 `.env`。
    #[test]
    fn child_spec_injects_resolved_config() {
        let mut config = Config::for_test(6790, "/var/lib/memora", "s3cret", "sqlite_file");
        config.mcp_allowed_hosts = vec!["mem.example.com".to_string(), "localhost".to_string()];
        let spec = child_spec(
            Path::new("/usr/local/bin/memora"),
            &config,
            Path::new("l.log"),
        );

        assert_eq!(spec.exe, PathBuf::from("/usr/local/bin/memora"));
        assert_eq!(spec.args, vec!["run".to_string()]);
        assert!(spec
            .envs
            .contains(&("PORT".to_string(), "6790".to_string())));
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

    /// 数据目录下没有任何记录时：`status` 报「未运行」，`stop` 幂等地成功。
    #[test]
    fn status_is_not_running_for_a_fresh_data_dir() {
        let dir = std::env::temp_dir().join(format!(
            "memora_p4_status_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let config = Config::for_test(
            closed_port(),
            &dir.display().to_string(),
            "admin",
            "sqlite_file",
        );

        assert_eq!(status(&config).unwrap(), EXIT_NOT_RUNNING);
        assert_eq!(stop(&config).unwrap(), 0, "未运行时 stop 必须幂等成功");
    }

    /// 整改的核心断言：探活必须用**记录里的端口**。
    ///
    /// 场景：配置已被改到另一个端口而进程尚未重启。此时若拿配置端口去探，
    /// 会得到「unreachable」并误导向「服务挂了」；按记录探才能得出真实结论。
    #[test]
    fn status_uses_recorded_port() {
        let dir = std::env::temp_dir().join(format!(
            "memora_p4_recorded_port_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        // 实际服务在一个真实可探的端口上运行
        let live_port = fake_http_server("HTTP/1.1 200 OK", "ok");
        let file = InstanceFile::new(&dir);
        file.write(&InstanceRecord {
            // 用当前进程当「活着的实例」，这样 live() 不会把记录当作陈旧数据清掉
            pid: std::process::id() as i32,
            port: Some(live_port),
            data_dir: dir.display().to_string(),
            started_at: "2026-09-17T08:30:00Z".to_string(),
            version: "0.1.0".to_string(),
        })
        .unwrap();

        // 配置指向一个无人监听的端口，与记录不一致
        let stale_port = closed_port();
        let config = Config::for_test(
            stale_port,
            &dir.display().to_string(),
            "admin",
            "sqlite_file",
        );
        assert_ne!(stale_port, live_port);

        assert_eq!(
            status(&config).unwrap(),
            0,
            "应按记录中的端口 {} 探活成功，而不是按配置中的 {}",
            live_port,
            stale_port
        );
    }

    /// `stop` 必须只依赖记录：即便配置里的端口/目录与实例无关，
    /// 也要能把记录中的进程真正停下来（整改前会打印「未运行」并返回 0，进程照旧在跑）。
    #[test]
    #[cfg(unix)]
    fn stop_terminates_the_recorded_process() {
        let dir = std::env::temp_dir().join(format!(
            "memora_p4_stop_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        // 记录指向一个真实的 sleep 进程（模拟 memora 后台进程）。
        // 必须同时起一个「收尸」线程：`kill(pid, 0)` 对**僵尸进程**同样返回成功，
        // 而僵尸只有被父进程 wait 之后才消失。真实场景里 `start` 进程早已退出，
        // 后台进程被 init/launchd 收走，故不存在僵尸；测试里需要显式充当这个角色。
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });
        let file = InstanceFile::new(&dir);
        file.write(&InstanceRecord {
            pid,
            port: None,
            data_dir: dir.display().to_string(),
            started_at: String::new(),
            version: String::new(),
        })
        .unwrap();

        let config = Config::for_test(
            closed_port(),
            &dir.display().to_string(),
            "admin",
            "sqlite_file",
        );
        assert_eq!(stop(&config).unwrap(), 0);
        reaper.join().unwrap();

        assert!(!instance::is_alive(pid), "记录中的进程应已被停止");
        assert!(!file.path().exists(), "停止后应清掉实例记录");
    }

    /// `init` 渲染的必须是**已解析的配置**（尊重命令行覆盖项）。
    ///
    /// 关键在于渲染出的文本能被 `Config::load` 原样读回：只用字符串包含断言，
    /// 一份语法非法或键名写错的模板也能「通过」。
    #[test]
    fn init_renders_resolved_values_and_refuses_to_overwrite() {
        use crate::cli::Overrides;

        let dir = std::env::temp_dir().join(format!(
            "memora_p4_init_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");

        let mut config = Config::for_test(
            6799,
            &dir.join("data").display().to_string(),
            "s3cret",
            "in_mem",
        );
        config.config_path = config_path.clone();
        config.log = "debug".to_string();
        config.mcp_allowed_hosts = vec!["mem.example.com".to_string()];

        assert_eq!(init(&config, false).unwrap(), 0);

        // 直接回读：模板必须是合法 TOML，且键名被 `deny_unknown_fields` 接受
        let (loaded, source) = Config::load(&Overrides::default(), Some(&config_path)).unwrap();
        assert!(matches!(source, crate::config::ConfigSource::Explicit(_)));
        assert_eq!(loaded.port, 6799, "init 必须尊重命令行传入的端口");
        assert_eq!(loaded.data_dir, config.data_dir);
        assert_eq!(loaded.admin_token, "s3cret");
        assert_eq!(loaded.storage_backend, "in_mem");
        assert_eq!(loaded.log, "debug");
        assert_eq!(
            loaded.mcp_allowed_hosts,
            vec!["mem.example.com".to_string()]
        );

        // 已存在 → 默认拒绝覆盖，避免抹掉用户改过的配置
        assert!(init(&config, false).is_err());
        assert!(init(&config, true).is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    /// 配置文件含 admin_token，必须是 0600。
    #[test]
    #[cfg(unix)]
    fn init_writes_config_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "memora_p4_init_perm_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        let mut config =
            Config::for_test(6799, &dir.display().to_string(), "s3cret", "sqlite_file");
        config.config_path = dir.join("config.toml");
        init(&config, false).unwrap();

        let mode = fs::metadata(&config.config_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "配置文件权限应为 0600，实际 {mode:o}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 未设置 admin_token 时不得写成空串：那会让所有 /api/v1 请求 401，
    /// 而用户看到的是一份「看起来已经配好」的文件。
    #[test]
    fn init_writes_a_placeholder_admin_token() {
        let mut config = Config::for_test(6789, "/data", "", "sqlite_file");
        let rendered = render_config_template(&config);
        assert!(
            rendered.contains("admin_token = \"change-me\""),
            "{rendered}"
        );
        assert!(!rendered.contains("admin_token = \"\""));

        // 显式传入的值则原样落盘
        config.admin_token = "s3cret".to_string();
        assert!(render_config_template(&config).contains("admin_token = \"s3cret\""));
    }

    /// 数据目录里出现引号这类字符时，手拼字符串会生成语法非法的 TOML。
    #[test]
    fn rendered_config_escapes_special_characters() {
        let mut config =
            Config::for_test(6789, "/data/with \"quote\" and \\slash", "t", "sqlite_file");
        config.config_path = PathBuf::from("/tmp/config.toml");
        let rendered = render_config_template(&config);

        // 断言「渲染结果能被解析回原值」，而不是断言某种具体转义写法：
        // 后者会把实现细节钉成契约（`toml` 在此例中选择了单引号字面量串）。
        let parsed: toml::Value = toml::from_str(&rendered).expect("渲染结果必须是合法 TOML");
        assert_eq!(
            parsed.get("data_dir").and_then(toml::Value::as_str),
            Some("/data/with \"quote\" and \\slash"),
            "{rendered}"
        );
    }

    #[test]
    fn rendered_templates_substitute_runtime_values() {
        let mut config = Config::for_test(6790, "/var/lib/memora", "t", "sqlite_file");
        config.config_path = PathBuf::from("/etc/memora/config.toml");

        let rendered = render_deploy_templates(Path::new("/opt/memora/memora"), &config);
        assert!(
            rendered.contains("/opt/memora/memora"),
            "应替换可执行文件路径"
        );
        assert!(rendered.contains("/var/lib/memora"), "应替换数据目录");
        assert!(rendered.contains("6790"), "应替换端口");
        assert!(
            rendered.contains("/etc/memora/config.toml"),
            "systemd unit 应把配置文件路径传给 run：{rendered}"
        );
        assert!(
            !rendered.contains("__"),
            "不得残留未替换的占位符: {rendered}"
        );
        assert!(rendered.contains("systemd"), "应包含 systemd unit");
        assert!(rendered.contains("reverse_proxy"), "应包含 Caddy 反代配置");
        assert!(
            !rendered.contains("EnvironmentFile="),
            "配置已集中在 TOML 文件中，不再用 EnvironmentFile= 注入"
        );
    }
}
