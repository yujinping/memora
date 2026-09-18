//! 配置加载：单实例 + 稳定绝对默认路径 + TOML。
//!
//! @author yujinping
//! @intent 配置来源必须**可预测**。此前默认配置取「`CWD/.env` → 可执行文件目录/.env →
//!         `DATA_DIR/.env`」中第一个命中者，而 `CWD` 既是相对路径又排第一，于是
//!         「你在跟哪个实例说话」取决于你在哪个目录敲命令：`status` 报「未运行」
//!         （假阴性），`stop` 打印「未运行」并返回 0 而进程照旧在跑（静默假成功）。
//!         故改为**单实例**模型：默认路径是基于 `HOME`/XDG 的**绝对路径**，与 `CWD`
//!         无关；`--config` 是唯一的配置指路开关，其余 flag 仅作临时覆盖。
//!
//! @intent 四层优先级不变：**命令行选项 > 进程环境变量 > 配置文件 > 内置默认值**。
//!         保留环境变量层以贴合 12-factor 与 systemd 的 `Environment=`；保留 flag 层
//!         以支持临时试验。文件层的键一律建模为 `Option`——`None` 表示「本文件未表态」
//!         而非「空值」，从而与下层不混淆。
//!
//! @intent 文件层启用 `deny_unknown_fields`：写错键名会硬失败并点名键名。这是把
//!         「改了配置却没生效」从「无从排查」变成「一句话报错」的关键，与
//!         [`resolve_port`] 拒绝非法端口而非静默回退的取向一致。
//!
//! @intent 默认路径**不做** `CWD` 兜底：一旦让 `./config.toml` 参与默认探测，就又回到
//!         「命中结果随执行目录漂移」。需要在项目目录里放配置时，请显式
//!         `--config ./config.toml`。
//!
//! @intent 合并逻辑收敛到 [`Config::load_with`]，环境变量取值经 [`EnvVars`] 注入。
//!         这样四层优先级能被单测**完整**钉住，而不必在测试里改动进程级环境变量
//!         （那是全局状态，并行用例下必然互相干扰）。

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::cli::Overrides;

/// 未显式配置时的默认监听端口。
pub const DEFAULT_PORT: u16 = 6789;
/// 配置文件名（默认路径的末段）。
pub const CONFIG_FILE_NAME: &str = "config.toml";
/// 应用目录名：XDG 基目录下的末段，也是数据 / 日志 / 实例记录所在目录名。
pub const APP_DIR_NAME: &str = "memora";
/// `HOME` 与 XDG 变量都不可用时的数据目录回退值。
pub const FALLBACK_DATA_DIR: &str = "./data";
/// `HOME` 与 XDG 变量都不可用时的配置文件回退值。
pub const FALLBACK_CONFIG_FILE: &str = "./config.toml";
/// 未显式配置时新项目采用的默认后端标识。
pub const DEFAULT_STORAGE_BACKEND: &str = "sqlite_file";
/// 未显式配置时允许的 Host：仅本机回环。
pub const DEFAULT_MCP_ALLOWED_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1"];
/// 未显式配置时的日志级别（`RUST_LOG` 语义）。
pub const DEFAULT_LOG: &str = "info,sqlx=warn,sea_orm=warn";

/// 显式指定的配置文件不存在时的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingPolicy {
    /// 硬失败。`run` / `start` / `stop` / `status` / `restart` 适用：`--config` 指向一个
    /// 不存在的文件是明确的笔误，静默按默认值运行会掩盖它。
    Reject,
    /// 容忍缺失。仅 `init` 适用——它的职责恰恰是生成那个文件。
    Tolerate,
}

/// 进程环境变量的取值快照（只含本模块关心的键）。
///
/// @intent 把环境读取收敛为一个可构造的值，使 [`Config::load_with`] 可被单测注入任意
///         环境，避免测试依赖（或篡改）进程级环境。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvVars {
    /// `PORT`
    pub port: Option<String>,
    /// `DATA_DIR`
    pub data_dir: Option<String>,
    /// `ADMIN_TOKEN`
    pub admin_token: Option<String>,
    /// `STORAGE_BACKEND`
    pub storage_backend: Option<String>,
    /// `MCP_ALLOWED_HOSTS`（逗号分隔）
    pub mcp_allowed_hosts: Option<String>,
    /// `RUST_LOG`
    pub rust_log: Option<String>,
    /// `XDG_CONFIG_HOME`
    pub xdg_config_home: Option<String>,
    /// `XDG_DATA_HOME`
    pub xdg_data_home: Option<String>,
    /// `HOME`
    pub home: Option<String>,
}

impl EnvVars {
    /// 读取当前进程环境。
    pub fn from_process() -> Self {
        let get = |key: &str| env::var(key).ok();
        Self {
            port: get("PORT"),
            data_dir: get("DATA_DIR"),
            admin_token: get("ADMIN_TOKEN"),
            storage_backend: get("STORAGE_BACKEND"),
            mcp_allowed_hosts: get("MCP_ALLOWED_HOSTS"),
            rust_log: get("RUST_LOG"),
            xdg_config_home: get("XDG_CONFIG_HOME"),
            xdg_data_home: get("XDG_DATA_HOME"),
            home: get("HOME"),
        }
    }
}

/// 配置来源结论（供启动日志与排障提示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// `--config` 显式指定并已成功加载
    Explicit(PathBuf),
    /// 默认绝对路径命中并已成功加载
    Default(PathBuf),
    /// 文件缺失（显式指定但被容忍，或默认路径本就为空）；使用环境变量与内置默认值
    Missing(PathBuf),
}

impl ConfigSource {
    /// 本次生效（或本应生效）的配置文件路径。
    pub fn path(&self) -> &Path {
        match self {
            Self::Explicit(path) | Self::Default(path) | Self::Missing(path) => path,
        }
    }

    /// 是否真的从文件读到了配置。
    pub fn loaded(&self) -> bool {
        matches!(self, Self::Explicit(_) | Self::Default(_))
    }

    /// 把来源结论写入日志。
    pub fn log(&self) {
        match self {
            Self::Explicit(path) => tracing::info!(path = %path.display(), "--config loaded"),
            Self::Default(path) => tracing::info!(path = %path.display(), "default config loaded"),
            Self::Missing(path) => tracing::info!(
                path = %path.display(),
                "no config file at the default path; using process environment and \
                 built-in defaults"
            ),
        }
    }
}

/// 生效配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// 监听端口
    pub port: u16,
    /// 数据目录（元库 + 各项目库 + 日志 + 实例记录）
    pub data_dir: String,
    /// 管理 API（`/api/v1`）凭据
    pub admin_token: String,
    /// 新项目默认存储后端
    pub storage_backend: String,
    /// MCP Host 白名单（空表示未配置，由 `effective_mcp_allowed_hosts` 回退）
    pub mcp_allowed_hosts: Vec<String>,
    /// 日志级别（`RUST_LOG` 语义）
    pub log: String,
    /// 本次生效的配置文件路径。即便文件缺失也保留该路径，供 `init` 写入与排障提示。
    pub config_path: PathBuf,
    /// 该路径上是否真的读到了配置文件。`false` 表示本次完全依赖环境变量与内置默认值。
    pub config_loaded: bool,
}

/// 配置文件的反序列化形态。
///
/// @intent 全部字段为 `Option`：`None` 表示「本文件未表态」而非「空值」。
///         `deny_unknown_fields` 让写错键名立刻失败并点名键名。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    port: Option<u16>,
    data_dir: Option<String>,
    admin_token: Option<String>,
    storage_backend: Option<String>,
    mcp_allowed_hosts: Option<Vec<String>>,
    log: Option<String>,
}

impl Config {
    /// 按四层优先级加载配置（读取当前进程环境）。
    pub fn load(overrides: &Overrides, explicit: Option<&Path>) -> Result<(Config, ConfigSource)> {
        Self::load_with(
            overrides,
            explicit,
            MissingPolicy::Reject,
            &EnvVars::from_process(),
        )
    }

    /// 供 `init` 使用：允许 `--config` 指向尚不存在的文件。
    ///
    /// @intent `init` 的职责就是生成配置文件，若在此对「显式指定但不存在」硬失败，
    ///         `memora init --config ~/.config/memora/config.toml` 将永远无法创建它。
    pub fn load_for_init(
        overrides: &Overrides,
        explicit: Option<&Path>,
    ) -> Result<(Config, ConfigSource)> {
        Self::load_with(
            overrides,
            explicit,
            MissingPolicy::Tolerate,
            &EnvVars::from_process(),
        )
    }

    /// 四层合并的本体：`flag` > `env` > 文件 > 内置默认值。
    ///
    /// @intent 顺序不可调换，且每层都只负责「把值交给纯函数」，合并语义全部集中在
    ///         [`resolve_port`] / [`resolve_text`] / [`resolve_log`] /
    ///         [`resolve_allowed_hosts`] 中，便于逐一断言。
    pub fn load_with(
        overrides: &Overrides,
        explicit: Option<&Path>,
        policy: MissingPolicy,
        env: &EnvVars,
    ) -> Result<(Config, ConfigSource)> {
        let default_path =
            default_config_path_from(env.xdg_config_home.as_deref(), env.home.as_deref());
        let (file, source) = load_file(explicit, &default_path, policy)?;

        let default_data_dir =
            default_data_dir_from(env.xdg_data_home.as_deref(), env.home.as_deref());

        let port = resolve_port(overrides.port, env.port.as_deref(), file.port)?;
        let data_dir = resolve_text(
            overrides.data_dir.as_deref(),
            env.data_dir.as_deref(),
            file.data_dir.as_deref(),
            &default_data_dir,
        );
        let admin_token = resolve_text(
            overrides.admin_token.as_deref(),
            env.admin_token.as_deref(),
            file.admin_token.as_deref(),
            "",
        );
        let storage_backend = resolve_text(
            overrides.storage_backend.as_deref(),
            env.storage_backend.as_deref(),
            file.storage_backend.as_deref(),
            DEFAULT_STORAGE_BACKEND,
        );
        let mcp_allowed_hosts = resolve_allowed_hosts(
            overrides.mcp_allowed_hosts.as_deref(),
            env.mcp_allowed_hosts.as_deref(),
            file.mcp_allowed_hosts.as_deref(),
        );
        let log = resolve_log(env.rust_log.as_deref(), file.log.as_deref());

        let config = Config {
            port,
            data_dir,
            admin_token,
            storage_backend,
            mcp_allowed_hosts,
            log,
            config_path: source.path().to_path_buf(),
            config_loaded: source.loaded(),
        };
        Ok((config, source))
    }

    /// 解析 `MCP_ALLOWED_HOSTS` 风格的白名单：逗号分隔，去除首尾空白并丢弃空项。
    ///
    /// @intent 未配置与「显式配置为空」在语义上都表示「未指定」，故此处不填默认值，
    ///         由 [`Config::effective_mcp_allowed_hosts`] 统一回退，保持单一真相源。
    pub fn parse_allowed_hosts(raw: &str) -> Vec<String> {
        raw.split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// 生效的 MCP 允许 Host：未配置时回退到默认回环列表。
    pub fn effective_mcp_allowed_hosts(&self) -> Vec<String> {
        if self.mcp_allowed_hosts.is_empty() {
            DEFAULT_MCP_ALLOWED_HOSTS
                .iter()
                .map(|host| host.to_string())
                .collect()
        } else {
            self.mcp_allowed_hosts.clone()
        }
    }

    /// 构造测试用配置（不读环境变量，不读文件）。
    #[cfg(test)]
    pub fn for_test(port: u16, data_dir: &str, admin_token: &str, storage_backend: &str) -> Config {
        Config {
            port,
            data_dir: data_dir.to_string(),
            admin_token: admin_token.to_string(),
            storage_backend: storage_backend.to_string(),
            mcp_allowed_hosts: Vec::new(),
            log: DEFAULT_LOG.to_string(),
            config_path: PathBuf::from(FALLBACK_CONFIG_FILE),
            config_loaded: false,
        }
    }
}

/// 默认数据目录：`$XDG_DATA_HOME/memora` → `$HOME/.local/share/memora` → `./data`。
///
/// @intent 默认值必须是**绝对路径**：默认配置若仍写作 `./data`，它本身就是一个随执行
///         目录漂移的移动靶，「单实例」也就无从谈起。
pub fn default_data_dir_from(xdg_data_home: Option<&str>, home: Option<&str>) -> String {
    xdg_base(xdg_data_home, home, ".local/share")
        .map(|base| base.join(APP_DIR_NAME).display().to_string())
        .unwrap_or_else(|| FALLBACK_DATA_DIR.to_string())
}

/// 默认配置文件：`$XDG_CONFIG_HOME/memora/config.toml` →
/// `$HOME/.config/memora/config.toml` → `./config.toml`。
pub fn default_config_path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> PathBuf {
    xdg_base(xdg_config_home, home, ".config")
        .map(|base| base.join(APP_DIR_NAME).join(CONFIG_FILE_NAME))
        .unwrap_or_else(|| PathBuf::from(FALLBACK_CONFIG_FILE))
}

/// XDG 基目录：优先环境变量，其次 `$HOME/<fallback_relative>`，两者皆无则 `None`。
///
/// @intent 只认 `HOME`/XDG 环境变量，**不查 passwd 数据库**：容器与 systemd 里 `HOME`
///         缺失是常态，此时回退到相对默认值并由调用方告警，比拿 passwd 中某个未必
///         适合写入的目录更可预测。
fn xdg_base(xdg: Option<&str>, home: Option<&str>, fallback_relative: &str) -> Option<PathBuf> {
    if let Some(raw) = xdg.filter(|value| !value.trim().is_empty()) {
        return Some(PathBuf::from(raw));
    }
    home.filter(|value| !value.trim().is_empty())
        .map(|home| Path::new(home).join(fallback_relative))
}

/// 读取并校验配置文件。
fn load_file(
    explicit: Option<&Path>,
    default_path: &Path,
    policy: MissingPolicy,
) -> Result<(FileConfig, ConfigSource)> {
    let (path, explicit_flag) = match explicit {
        Some(path) => (path.to_path_buf(), true),
        None => (default_path.to_path_buf(), false),
    };

    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if !explicit_flag || policy == MissingPolicy::Tolerate {
                return Ok((FileConfig::default(), ConfigSource::Missing(path)));
            }
            bail!(
                "config file {} does not exist; run `memora init` to generate one, or point \
                 --config at an existing file",
                path.display()
            );
        }
        Err(err) => {
            return Err(err).with_context(|| format!("cannot read config file {}", path.display()))
        }
    };

    let file: FileConfig = toml::from_str(&raw).with_context(|| {
        format!(
            "failed to parse config file {} (check TOML syntax and key names; \
             unknown keys are rejected on purpose)",
            path.display()
        )
    })?;

    // `port = 0` 必须在这里拦下：u16 反序列化拦不住 0，而内核会把 0 解释为「随机可用
    // 端口」，与固定端口的运维预期不符。
    if file.port == Some(0) {
        bail!(
            "config file {} sets port = 0: the kernel would bind a random port; use 1..=65535",
            path.display()
        );
    }

    let source = if explicit_flag {
        ConfigSource::Explicit(path)
    } else {
        ConfigSource::Default(path)
    };
    Ok((file, source))
}

/// 解析端口：flag > 环境变量 > 文件 > 默认值。
///
/// @intent 非法取值一律 `Err`，绝不静默回退：`PORT=70000` 或 `port = 0` 静默变成 6789
///         会让「改配置没反应」这类问题极难定位。
pub fn resolve_port(flag: Option<u16>, env_raw: Option<&str>, file: Option<u16>) -> Result<u16> {
    if let Some(port) = flag {
        if port == 0 {
            bail!("port 0 is not allowed: the kernel would bind a random port");
        }
        return Ok(port);
    }

    if let Some(raw) = env_raw.map(str::trim).filter(|s| !s.is_empty()) {
        return match raw.parse::<u16>() {
            Ok(port) if port > 0 => Ok(port),
            _ => bail!("PORT={raw:?} is invalid: expected an integer in 1..=65535"),
        };
    }

    match file {
        Some(0) => bail!("port 0 is not allowed: the kernel would bind a random port"),
        Some(port) => Ok(port),
        None => Ok(DEFAULT_PORT),
    }
}

/// 解析文本类配置：flag > 环境变量 > 文件 > 默认值；空白串视为「本层未表态」。
pub fn resolve_text(
    flag: Option<&str>,
    env_raw: Option<&str>,
    file: Option<&str>,
    default: &str,
) -> String {
    flag.filter(|value| !value.trim().is_empty())
        .or_else(|| env_raw.filter(|value| !value.trim().is_empty()))
        .or_else(|| file.filter(|value| !value.trim().is_empty()))
        .map(str::to_string)
        .unwrap_or_else(|| default.to_string())
}

/// 解析日志级别：`RUST_LOG` > 文件 `log` > 默认值。
///
/// @intent 保留 `RUST_LOG` 优先，是因为它是排查期的临时开关，运维预期它能就地压过
///         持久化配置（与大多数 Rust 服务一致）。
pub fn resolve_log(env_raw: Option<&str>, file: Option<&str>) -> String {
    env_raw
        .filter(|value| !value.trim().is_empty())
        .or_else(|| file.filter(|value| !value.trim().is_empty()))
        .unwrap_or(DEFAULT_LOG)
        .to_string()
}

/// 解析 Host 白名单：flag/env 层是逗号分隔串，文件层是数组，前者优先。
pub fn resolve_allowed_hosts(
    flag: Option<&str>,
    env_raw: Option<&str>,
    file: Option<&[String]>,
) -> Vec<String> {
    if let Some(raw) = [flag, env_raw]
        .into_iter()
        .flatten()
        .find(|value| !value.trim().is_empty())
    {
        return Config::parse_allowed_hosts(raw);
    }
    file.map(|list| {
        list.iter()
            .map(|item| item.trim())
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// 每个用例独立的临时目录。
    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("memora_cfg_{}_{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 写一份配置文件并返回其路径。
    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join(CONFIG_FILE_NAME);
        fs::write(&path, body).unwrap();
        path
    }

    /// 仅注入 `HOME` 的环境快照（不碰进程环境）。
    fn env_with_home(home: &str) -> EnvVars {
        EnvVars {
            home: Some(home.to_string()),
            ..EnvVars::default()
        }
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

    /// 默认路径必须是绝对路径且与 CWD 无关——这是「单实例」的地基。
    #[test]
    fn default_config_path_prefers_xdg_then_home_then_relative() {
        assert_eq!(
            default_config_path_from(Some("/xdg"), Some("/home/u")),
            PathBuf::from("/xdg/memora/config.toml")
        );
        assert_eq!(
            default_config_path_from(None, Some("/home/u")),
            PathBuf::from("/home/u/.config/memora/config.toml")
        );
        assert_eq!(
            default_config_path_from(Some("   "), Some("/home/u")),
            PathBuf::from("/home/u/.config/memora/config.toml"),
            "空白 XDG 视为未设置"
        );
        assert_eq!(
            default_config_path_from(None, None),
            PathBuf::from(FALLBACK_CONFIG_FILE),
            "无 HOME 时回退相对默认值"
        );
    }

    #[test]
    fn default_data_dir_prefers_xdg_then_home_then_relative() {
        assert_eq!(
            default_data_dir_from(Some("/xdgdata"), None),
            "/xdgdata/memora"
        );
        assert_eq!(
            default_data_dir_from(None, Some("/home/u")),
            "/home/u/.local/share/memora"
        );
        assert_eq!(default_data_dir_from(None, None), FALLBACK_DATA_DIR);
    }

    /// 文件层的值必须真的进得来（防止「解析了却没接线」）。
    #[test]
    fn file_layer_reaches_config() {
        let dir = temp_dir();
        let path = write_config(
            &dir,
            "port = 7000\n\
             data_dir = \"/srv/memora\"\n\
             admin_token = \"from-file\"\n\
             storage_backend = \"in_mem\"\n\
             mcp_allowed_hosts = [\"mem.example.com\", \" localhost \"]\n\
             log = \"debug\"\n",
        );

        let (config, source) = Config::load_with(
            &Overrides::default(),
            Some(&path),
            MissingPolicy::Reject,
            &env_with_home("/home/u"),
        )
        .unwrap();

        assert_eq!(config.port, 7000);
        assert_eq!(config.data_dir, "/srv/memora");
        assert_eq!(config.admin_token, "from-file");
        assert_eq!(config.storage_backend, "in_mem");
        assert_eq!(
            config.mcp_allowed_hosts,
            vec!["mem.example.com".to_string(), "localhost".to_string()]
        );
        assert_eq!(config.log, "debug");
        assert_eq!(source, ConfigSource::Explicit(path.clone()));
        assert_eq!(config.config_path, path);
        assert!(config.config_loaded);
    }

    #[test]
    fn env_layer_overrides_file_layer() {
        let dir = temp_dir();
        let path = write_config(&dir, "port = 7000\ndata_dir = \"/srv/memora\"\n");
        let env = EnvVars {
            port: Some("7100".to_string()),
            data_dir: Some("/env/data".to_string()),
            home: Some("/home/u".to_string()),
            ..EnvVars::default()
        };

        let (config, _) = Config::load_with(
            &Overrides::default(),
            Some(&path),
            MissingPolicy::Reject,
            &env,
        )
        .unwrap();
        assert_eq!(config.port, 7100);
        assert_eq!(config.data_dir, "/env/data");
    }

    #[test]
    fn flag_layer_overrides_env_layer() {
        let dir = temp_dir();
        let path = write_config(&dir, "port = 7000\n");
        let env = EnvVars {
            port: Some("7100".to_string()),
            home: Some("/home/u".to_string()),
            ..EnvVars::default()
        };
        let overrides = Overrides {
            port: Some(7200),
            ..Overrides::default()
        };

        let (config, _) =
            Config::load_with(&overrides, Some(&path), MissingPolicy::Reject, &env).unwrap();
        assert_eq!(config.port, 7200);
    }

    /// 写错键名必须硬失败并点名键名——「改了配置没生效」的主要成因就是静默忽略未知键。
    #[test]
    fn unknown_key_is_rejected() {
        let dir = temp_dir();
        let path = write_config(&dir, "port = 7000\nporrt = 7001\n");

        let err = Config::load_with(
            &Overrides::default(),
            Some(&path),
            MissingPolicy::Reject,
            &env_with_home("/home/u"),
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("porrt"), "错误须点名未知键: {message}");
        assert!(
            message.contains("config.toml"),
            "错误须包含文件路径: {message}"
        );
    }

    #[test]
    fn malformed_toml_is_rejected() {
        let dir = temp_dir();
        let path = write_config(&dir, "port = \n");

        let err = Config::load_with(
            &Overrides::default(),
            Some(&path),
            MissingPolicy::Reject,
            &env_with_home("/home/u"),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("config.toml"),
            "错误须包含具体路径: {err:#}"
        );
    }

    /// `port = 0` 会被内核解释为随机端口，必须拦在配置层。
    #[test]
    fn port_zero_in_file_is_rejected() {
        let dir = temp_dir();
        let path = write_config(&dir, "port = 0\n");

        let err = Config::load_with(
            &Overrides::default(),
            Some(&path),
            MissingPolicy::Reject,
            &env_with_home("/home/u"),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("port = 0"),
            "错误须指出 port = 0: {err:#}"
        );
    }

    /// 超出 u16 的端口同样必须硬失败，而不是被静默当作未配置。
    #[test]
    fn oversized_port_in_file_is_rejected() {
        let dir = temp_dir();
        let path = write_config(&dir, "port = 70000\n");

        assert!(Config::load_with(
            &Overrides::default(),
            Some(&path),
            MissingPolicy::Reject,
            &env_with_home("/home/u"),
        )
        .is_err());
    }

    /// 显式指定却不存在的文件是笔误，必须点名 `--config`。
    #[test]
    fn explicit_config_must_exist() {
        let dir = temp_dir();
        let missing = dir.join("nope.toml");

        let err = Config::load_with(
            &Overrides::default(),
            Some(&missing),
            MissingPolicy::Reject,
            &env_with_home("/home/u"),
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("--config"),
            "错误须指出是哪个选项: {message}"
        );
        assert!(
            message.contains("nope.toml"),
            "错误须包含具体路径: {message}"
        );
    }

    /// `init` 必须允许 `--config` 指向尚不存在的文件（它就是要创建它）。
    #[test]
    fn init_tolerates_missing_explicit_config() {
        let dir = temp_dir();
        let missing = dir.join("fresh.toml");

        let (config, source) = Config::load_with(
            &Overrides::default(),
            Some(&missing),
            MissingPolicy::Tolerate,
            &env_with_home("/home/u"),
        )
        .unwrap();
        assert_eq!(source, ConfigSource::Missing(missing.clone()));
        assert_eq!(config.config_path, missing, "仍须记住该目标路径");
        assert!(!config.config_loaded);
    }

    /// 默认路径缺失不是错误：首次运行本就没有配置文件。
    #[test]
    fn missing_default_config_is_not_an_error_but_is_reported() {
        let (config, source) = Config::load_with(
            &Overrides::default(),
            None,
            MissingPolicy::Reject,
            &env_with_home("/home/u"),
        )
        .unwrap();

        assert_eq!(
            source,
            ConfigSource::Missing(PathBuf::from("/home/u/.config/memora/config.toml"))
        );
        assert!(!config.config_loaded);
        assert!(!source.loaded());
    }

    /// 什么都没给时，默认值必须与 CWD 无关。
    #[test]
    fn defaults_are_used_when_nothing_is_provided() {
        let (config, _) = Config::load_with(
            &Overrides::default(),
            None,
            MissingPolicy::Reject,
            &env_with_home("/home/u"),
        )
        .unwrap();

        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.data_dir, "/home/u/.local/share/memora");
        assert_eq!(config.storage_backend, DEFAULT_STORAGE_BACKEND);
        assert_eq!(config.log, DEFAULT_LOG);
        assert!(config.admin_token.is_empty());
        assert!(config.mcp_allowed_hosts.is_empty());
    }

    #[test]
    fn port_precedence_is_flag_then_env_then_file_then_default() {
        assert_eq!(
            resolve_port(Some(7000), Some("7100"), Some(7200)).unwrap(),
            7000
        );
        assert_eq!(resolve_port(None, Some("7100"), Some(7200)).unwrap(), 7100);
        assert_eq!(
            resolve_port(None, Some("  7100  "), Some(7200)).unwrap(),
            7100
        );
        assert_eq!(resolve_port(None, None, Some(7200)).unwrap(), 7200);
        assert_eq!(resolve_port(None, None, None).unwrap(), DEFAULT_PORT);
        assert_eq!(resolve_port(None, Some(""), None).unwrap(), DEFAULT_PORT);
    }

    /// 非法端口必须报错——静默回退会让「改了配置没反应」变得无法排查。
    #[test]
    fn invalid_port_is_rejected_rather_than_defaulted() {
        for raw in ["0", "65536", "abc", "67 89", "-1"] {
            let err = resolve_port(None, Some(raw), None).unwrap_err();
            assert!(
                err.to_string().contains("PORT"),
                "错误须点名 PORT（实际: {err}）；输入 {raw:?}"
            );
        }
        assert!(resolve_port(Some(0), None, None).is_err());
        assert!(resolve_port(None, None, Some(0)).is_err());
    }

    #[test]
    fn text_precedence_is_flag_then_env_then_file_then_default() {
        assert_eq!(
            resolve_text(Some("/flag"), Some("/env"), Some("/file"), "/def"),
            "/flag"
        );
        assert_eq!(
            resolve_text(None, Some("/env"), Some("/file"), "/def"),
            "/env"
        );
        assert_eq!(resolve_text(None, None, Some("/file"), "/def"), "/file");
        assert_eq!(resolve_text(None, None, None, "/def"), "/def");
        // 空白串视为「本层未表态」，避免 DATA_DIR="" 这类配置把数据写到进程 CWD 之外
        assert_eq!(resolve_text(None, Some("   "), None, "/def"), "/def");
        assert_eq!(resolve_text(Some(""), Some("/env"), None, "/def"), "/env");
        assert_eq!(resolve_text(None, None, Some("  "), "/def"), "/def");
    }

    #[test]
    fn log_precedence_is_env_then_file_then_default() {
        assert_eq!(resolve_log(Some("debug"), Some("trace")), "debug");
        assert_eq!(resolve_log(None, Some("trace")), "trace");
        assert_eq!(resolve_log(Some("  "), Some("trace")), "trace");
        assert_eq!(resolve_log(None, None), DEFAULT_LOG);
    }

    #[test]
    fn allowed_hosts_precedence_is_flag_env_then_file_array() {
        let file = vec!["from-file.example".to_string(), " localhost ".to_string()];
        assert_eq!(
            resolve_allowed_hosts(Some("flag.example"), Some("env.example"), Some(&file)),
            vec!["flag.example".to_string()]
        );
        assert_eq!(
            resolve_allowed_hosts(None, Some("env.example,localhost"), Some(&file)),
            vec!["env.example".to_string(), "localhost".to_string()]
        );
        assert_eq!(
            resolve_allowed_hosts(None, None, Some(&file)),
            vec!["from-file.example".to_string(), "localhost".to_string()]
        );
        assert!(resolve_allowed_hosts(None, None, None).is_empty());
        assert!(resolve_allowed_hosts(None, None, Some(&[])).is_empty());
    }
}
