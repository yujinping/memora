//! 运行配置：四层来源叠加（命令行选项 > 进程环境变量 > `.env` > 内置默认值）。
//!
//! @author yujinping
//! @intent P1：从环境变量读取端口与数据目录。
//! @intent P3：新增 MCP 传输侧配置。rmcp 的 Streamable HTTP 传输默认只接受回环 Host
//!         （防 DNS 重绑定），而本服务定位是「经 Caddy 反代的公网端点」，反代会透传
//!         原始 Host，故必须把允许的 Host 开放为配置项，否则公网部署会被协议层拒绝。
//! @intent P4.2：把「来源」显式化。原先 `.env` 由 `dotenvy::dotenv().ok()` 加载——查找
//!         依赖 CWD 且失败被静默吞掉，症状是「`.env` 明明填了却没生效」而日志毫无线索。
//!         现在的契约是：
//!         1. 探测路径写入日志；未命中时列出**全部**候选路径；
//!         2. `--env-file` 显式指定 → 文件缺失或语法错误一律硬失败；
//!         3. 自动探测命中但语法错误 → 同样硬失败（不再 `.ok()` 吞掉）；
//!         4. 数值型配置（`PORT`）取值非法 → 硬失败，不回退默认值。
//!
//! @intent 解析逻辑（[`resolve_port`] / [`resolve_text`] / [`env_candidates`]）一律做成
//!         不读全局环境变量的纯函数，由 [`Config::load`] 注入实际取值。这使得大部分
//!         配置语义可以被单元测试穷尽覆盖，而不必在测试中改进程环境（并发测试下不安全）。

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::cli::Overrides;

/// 未显式配置时的默认监听端口。
pub const DEFAULT_PORT: u16 = 6789;
/// 未显式配置时的默认数据目录。
pub const DEFAULT_DATA_DIR: &str = "./data";
/// 未显式配置时新项目采用的默认后端标识。
pub const DEFAULT_STORAGE_BACKEND: &str = "sqlite_file";
/// 配置文件名（自动探测时使用）。
pub const ENV_FILE_NAME: &str = ".env";

/// 未显式配置时允许的 Host：仅本机回环。
///
/// @intent 默认取最小权限。公网部署须设 `MCP_ALLOWED_HOSTS`（含对外域名），
///         否则协议层会拒绝请求；启动日志会在沿用默认值时给出显式提示。
pub const DEFAULT_MCP_ALLOWED_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// `.env` 的来源判定结果。
///
/// @intent 把「从哪来的」变成可返回值而非日志副作用，启动时既能打日志也能被测试断言；
///         尤其是 [`EnvSource::NotFound`] 携带完整候选列表——这正是修复「`.env` 没生效」
///         这类故障的关键信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvSource {
    /// `--env-file` 显式指定并已成功加载
    Explicit(PathBuf),
    /// 按探测顺序自动命中
    Discovered(PathBuf),
    /// 自动探测未命中；附带探测过的全部候选路径
    NotFound(Vec<PathBuf>),
}

impl EnvSource {
    /// 把来源结论写入日志。
    pub fn log(&self) {
        match self {
            EnvSource::Explicit(path) => {
                tracing::info!(path = %path.display(), "--env-file loaded")
            }
            EnvSource::Discovered(path) => {
                tracing::info!(path = %path.display(), "discovered .env loaded")
            }
            EnvSource::NotFound(searched) => {
                let list: Vec<String> = searched.iter().map(|p| p.display().to_string()).collect();
                tracing::info!(
                    searched = ?list,
                    "no .env file found; using process environment and built-in defaults"
                );
            }
        }
    }
}

/// 服务运行配置。
#[derive(Clone, Debug)]
pub struct Config {
    /// 监听端口
    pub port: u16,
    /// 数据存储目录（每项目独立 SQLite 文件存放于此）
    pub data_dir: String,
    /// 管理员令牌：REST 管理层（/api/v1）鉴权
    pub admin_token: String,
    /// 新项目的默认存储后端标识（sqlite_file / in_mem）；既有项目以其登记值优先
    pub storage_backend: String,
    /// MCP 允许的 Host 白名单；空表示沿用 [`DEFAULT_MCP_ALLOWED_HOSTS`]
    pub mcp_allowed_hosts: Vec<String>,
}

impl Config {
    /// 按四层优先级加载配置，并返回 `.env` 的来源结论（供启动日志说明）。
    ///
    /// @intent 顺序不可调换：先确定并加载 `.env`，再逐项按 flag → 环境 → 默认取值。
    ///         由于 `dotenvy` **不覆盖**已存在的进程环境变量，两层的优先级天然成立；
    ///         而 flag 层在最后显式覆盖，故一定赢。
    pub fn load(overrides: &Overrides, env_file: Option<&Path>) -> Result<(Config, EnvSource)> {
        let source = load_env_file(overrides, env_file)?;

        let port = resolve_port(overrides.port, env::var("PORT").ok().as_deref())?;
        let data_dir = resolve_text(
            overrides.data_dir.as_deref(),
            env::var("DATA_DIR").ok().as_deref(),
            DEFAULT_DATA_DIR,
        );
        let admin_token = resolve_text(
            overrides.admin_token.as_deref(),
            env::var("ADMIN_TOKEN").ok().as_deref(),
            "",
        );
        let storage_backend = resolve_text(
            overrides.storage_backend.as_deref(),
            env::var("STORAGE_BACKEND").ok().as_deref(),
            DEFAULT_STORAGE_BACKEND,
        );
        let hosts_raw = resolve_text(
            overrides.mcp_allowed_hosts.as_deref(),
            env::var("MCP_ALLOWED_HOSTS").ok().as_deref(),
            "",
        );

        let config = Config {
            port,
            data_dir,
            admin_token,
            storage_backend,
            mcp_allowed_hosts: Self::parse_allowed_hosts(&hosts_raw),
        };
        Ok((config, source))
    }

    /// 解析 `MCP_ALLOWED_HOSTS`：逗号分隔，去除首尾空白并丢弃空项。
    ///
    /// @intent 未配置与「显式配置为空」在语义上都表示「未指定」，故此处不填默认值，
    ///         由 [`Config::effective_mcp_allowed_hosts`] 统一回退，保持单一真相源。
    pub fn parse_allowed_hosts(raw: &str) -> Vec<String> {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// 生效的 MCP 允许 Host：未配置时回退到默认回环列表。
    pub fn effective_mcp_allowed_hosts(&self) -> Vec<String> {
        if self.mcp_allowed_hosts.is_empty() {
            DEFAULT_MCP_ALLOWED_HOSTS
                .iter()
                .map(|h| h.to_string())
                .collect()
        } else {
            self.mcp_allowed_hosts.clone()
        }
    }

    /// 测试用构造：仅指定与用例相关的字段，其余取稳定默认值。
    ///
    /// @intent 给 `Config` 增字段会波及所有测试里的字面量构造，收敛为单一入口后，
    ///         后续新增配置只需改这一处。
    #[cfg(test)]
    pub fn for_test(
        port: u16,
        data_dir: impl Into<String>,
        admin_token: &str,
        storage_backend: &str,
    ) -> Self {
        Config {
            port,
            data_dir: data_dir.into(),
            admin_token: admin_token.to_string(),
            storage_backend: storage_backend.to_string(),
            mcp_allowed_hosts: Vec::new(),
        }
    }
}

/// 加载 `.env`：显式路径优先，否则按候选顺序探测。
///
/// @intent 「显式指定」与「自动探测」的失败语义刻意不同：前者是用户的明确意图，
///         路径写错必须立刻报错；后者未命中属正常情形（配置全走环境变量也合法），
///         只在日志中列出探测路径。
fn load_env_file(overrides: &Overrides, explicit: Option<&Path>) -> Result<EnvSource> {
    if let Some(path) = explicit {
        dotenvy::from_path(path).with_context(|| {
            format!(
                "failed to load --env-file {}: check that the file exists and is valid dotenv \
                 syntax (KEY=VALUE per line)",
                path.display()
            )
        })?;
        return Ok(EnvSource::Explicit(path.to_path_buf()));
    }

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // 探测顺序中的第三项依赖 DATA_DIR，但此刻 .env 尚未加载；
    // 故只用「flag 层 + 进程环境 + 默认值」推导，避免循环依赖。
    let data_dir = resolve_text(
        overrides.data_dir.as_deref(),
        env::var("DATA_DIR").ok().as_deref(),
        DEFAULT_DATA_DIR,
    );
    let candidates = env_candidates(&cwd, executable_dir().as_deref(), Path::new(&data_dir));

    match candidates.iter().find(|path| path.is_file()) {
        Some(path) => {
            dotenvy::from_path(path).with_context(|| {
                format!(
                    "failed to parse {} (found by auto-discovery): fix the file or pass \
                     --env-file explicitly",
                    path.display()
                )
            })?;
            Ok(EnvSource::Discovered(path.clone()))
        }
        None => Ok(EnvSource::NotFound(candidates)),
    }
}

/// `.env` 的候选路径，按优先级排列：CWD → 可执行文件所在目录 → `DATA_DIR`。
///
/// @intent 三个位置各有其场景：CWD 覆盖「就地运行」；可执行文件目录覆盖「从任意目录
///         以绝对路径启动」（部署后最常见的形态）；`DATA_DIR` 覆盖「配置随数据走」。
///         去重以避免同一路径被报告两次，让日志里的探测清单可信。
pub fn env_candidates(cwd: &Path, exe_dir: Option<&Path>, data_dir: &Path) -> Vec<PathBuf> {
    let mut raw = vec![cwd.join(ENV_FILE_NAME)];
    if let Some(dir) = exe_dir {
        raw.push(dir.join(ENV_FILE_NAME));
    }
    raw.push(data_dir.join(ENV_FILE_NAME));

    let mut out: Vec<PathBuf> = Vec::with_capacity(raw.len());
    for path in raw {
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// 可执行文件所在目录（用于探测随二进制一起分发的 `.env`）。
fn executable_dir() -> Option<PathBuf> {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

/// 解析端口：flag > 环境变量 > 默认值。
///
/// @intent 非法取值一律 `Err`，绝不静默回退：`PORT=70000` 静默变成 6789 会让「改配置
///         没反应」这类问题极难定位；同理拒绝 0——内核会把 0 解释为「随机可用端口」，
///         与运维对固定端口的预期不符。
pub fn resolve_port(flag: Option<u16>, env_raw: Option<&str>) -> Result<u16> {
    if let Some(port) = flag {
        if port == 0 {
            bail!("port 0 is not allowed: the kernel would bind a random port");
        }
        return Ok(port);
    }

    let Some(raw) = env_raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_PORT);
    };

    match raw.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => bail!("PORT={raw:?} is invalid: expected an integer in 1..=65535"),
    }
}

/// 解析文本类配置：flag > 环境变量 > 默认值；空白串视为「本层未表态」。
pub fn resolve_text(flag: Option<&str>, env_raw: Option<&str>, default: &str) -> String {
    let picked = flag
        .filter(|value| !value.trim().is_empty())
        .or_else(|| env_raw.filter(|value| !value.trim().is_empty()));
    match picked {
        Some(value) => value.to_string(),
        None => default.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// 每个用例独立的临时目录。
    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("memora_p4_env_{}_{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_comma_separated_hosts_and_trims() {
        let hosts = Config::parse_allowed_hosts(" mem.example.com , localhost ,mem.local:8443 ");
        assert_eq!(
            hosts,
            vec![
                "mem.example.com".to_string(),
                "localhost".to_string(),
                "mem.local:8443".to_string()
            ]
        );
    }

    #[test]
    fn drops_empty_entries() {
        assert_eq!(
            Config::parse_allowed_hosts("a,,b,   ,c"),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(Config::parse_allowed_hosts("").is_empty());
        assert!(Config::parse_allowed_hosts("  ,  ").is_empty());
    }

    /// 未配置 → 回环兜底；显式配置 → 以配置为准。
    #[test]
    fn effective_hosts_fall_back_to_loopback_then_yield_to_config() {
        let mut config = Config::for_test(0, ".", "admin", "sqlite_file");
        assert!(
            config.mcp_allowed_hosts.is_empty(),
            "for_test 不预设 Host 白名单"
        );
        assert_eq!(
            config.effective_mcp_allowed_hosts(),
            DEFAULT_MCP_ALLOWED_HOSTS
                .iter()
                .map(|h| h.to_string())
                .collect::<Vec<_>>()
        );

        config.mcp_allowed_hosts = vec!["mem.example.com".to_string()];
        assert_eq!(
            config.effective_mcp_allowed_hosts(),
            vec!["mem.example.com".to_string()]
        );
    }

    #[test]
    fn env_candidates_follow_cwd_exe_then_data_dir() {
        let candidates = env_candidates(
            Path::new("/srv/app"),
            Some(Path::new("/usr/local/bin")),
            Path::new("/var/lib/memora"),
        );
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/srv/app/.env"),
                PathBuf::from("/usr/local/bin/.env"),
                PathBuf::from("/var/lib/memora/.env"),
            ]
        );
    }

    /// 重复路径必须去重，否则「探测过哪些路径」这份日志会误导排查。
    #[test]
    fn env_candidates_deduplicate() {
        let candidates = env_candidates(
            Path::new("/srv/app"),
            Some(Path::new("/srv/app")),
            Path::new("/srv/app"),
        );
        assert_eq!(candidates, vec![PathBuf::from("/srv/app/.env")]);

        // 无 exe 目录时只保留两项
        let candidates = env_candidates(
            Path::new("/srv/app"),
            None,
            Path::new("/var/lib/memora"),
        );
        assert_eq!(candidates.len(), 2);
    }

    /// 显式指定的路径不存在 → 硬失败（此前会被静默忽略）。
    #[test]
    fn explicit_env_file_must_exist() {
        let missing = temp_dir().join("nope.env");
        let overrides = Overrides::default();
        let err = Config::load(&overrides, Some(&missing)).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("--env-file"), "错误须指出是哪个选项: {message}");
        assert!(message.contains("nope.env"), "错误须包含具体路径: {message}");
    }

    /// 语法错误 → 硬失败，且提示可操作的修复方向。
    #[test]
    fn malformed_env_file_is_rejected() {
        let dir = temp_dir();
        let path = dir.join("broken.env");
        fs::write(&path, "THIS_IS_NOT_DOTENV_SYNTAX\n").unwrap();

        let err = Config::load(&Overrides::default(), Some(&path)).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("broken.env"), "错误须包含具体路径: {message}");
    }

    #[test]
    fn auto_discovery_picks_the_first_existing_candidate() {
        let cwd = temp_dir();
        let exe = temp_dir();
        let data = temp_dir();
        fs::write(exe.join(ENV_FILE_NAME), "PORT=7100\n").unwrap();
        fs::write(data.join(ENV_FILE_NAME), "PORT=7200\n").unwrap();

        let candidates = env_candidates(&cwd, Some(&exe), &data);
        assert_eq!(
            candidates.iter().find(|p| p.is_file()),
            Some(&exe.join(ENV_FILE_NAME)),
            "应命中优先级更高的可执行文件目录"
        );
    }

    /// 未命中任何候选时不报错，但必须能报告探测过哪些路径。
    #[test]
    fn missing_env_file_is_not_an_error_but_reports_candidates() {
        let source = EnvSource::NotFound(vec![PathBuf::from("/a/.env"), PathBuf::from("/b/.env")]);
        match source {
            EnvSource::NotFound(paths) => assert_eq!(paths.len(), 2),
            other => panic!("构造错误: {other:?}"),
        }
    }

    #[test]
    fn port_precedence_is_flag_then_env_then_default() {
        assert_eq!(resolve_port(Some(7000), Some("7100")).unwrap(), 7000);
        assert_eq!(resolve_port(None, Some("7100")).unwrap(), 7100);
        assert_eq!(resolve_port(None, Some("  7100  ")).unwrap(), 7100);
        assert_eq!(resolve_port(None, None).unwrap(), DEFAULT_PORT);
        assert_eq!(resolve_port(None, Some("")).unwrap(), DEFAULT_PORT);
    }

    /// 非法端口必须报错——静默回退会让「改了配置没反应」变得无法排查。
    #[test]
    fn invalid_port_is_rejected_rather_than_defaulted() {
        for raw in ["0", "65536", "abc", "67 89", "-1"] {
            let err = resolve_port(None, Some(raw)).unwrap_err();
            assert!(
                err.to_string().contains("PORT"),
                "错误须点名 PORT（实际: {err}）；输入 {raw:?}"
            );
        }
        assert!(resolve_port(Some(0), None).is_err());
    }

    #[test]
    fn oversized_port_is_rejected() {
        // 70000 超出 u16 范围：此前会被静默当作未配置
        assert!(resolve_port(None, Some("70000")).is_err());
    }

    #[test]
    fn text_precedence_is_flag_then_env_then_default() {
        assert_eq!(resolve_text(Some("/flag"), Some("/env"), "/def"), "/flag");
        assert_eq!(resolve_text(None, Some("/env"), "/def"), "/env");
        assert_eq!(resolve_text(None, None, "/def"), "/def");
        // 空白串视为「本层未表态」，避免 DATA_DIR="" 这类配置把数据写到进程 CWD 之外
        assert_eq!(resolve_text(None, Some("   "), "/def"), "/def");
        assert_eq!(resolve_text(Some(""), Some("/env"), "/def"), "/env");
    }

    #[test]
    fn defaults_are_used_when_nothing_is_provided() {
        assert_eq!(resolve_text(None, None, DEFAULT_DATA_DIR), "./data");
        assert_eq!(
            resolve_text(None, None, DEFAULT_STORAGE_BACKEND),
            "sqlite_file"
        );
    }
}
