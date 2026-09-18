//! 命令行接口：子命令与全局选项解析。
//!
//! @author yujinping
//! @intent P4.2：让单个二进制自带进程管理（caddy 式 `run` / `start` / `stop` /
//!         `restart` / `status` / `init`），使「无 systemd、无 root」的裸机自托管
//!         同样可运维；同时保留「无子命令即前台运行」的向后兼容，
//!         既有 systemd / 容器部署零改动。
//!
//! @intent 解析刻意手写而非引入 clap：本服务的选项面仅 6 个，而 clap 会带来可观的
//!         编译时间与二进制约 300KB 的体积增长，与「2GB 服务器上的单二进制」定位不符。
//!         代价是 `--help` 文本需自行维护（见 [`usage`]），但选项稳定后这属一次性成本。

use std::fmt;
use std::path::PathBuf;

/// 子命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// 前台运行（默认）：日志走 stdout/stderr，供 systemd 或容器托管
    Run,
    /// 后台启动：脱离控制终端、日志落盘，健康自检通过后才算成功
    Start,
    /// 停止后台进程：先 SIGTERM 优雅退出，超时再 SIGKILL
    Stop,
    /// 重启后台进程（等价于 stop + start，未运行时可直接启动）
    Restart,
    /// 查看运行状态；未运行时退出码为 3
    Status,
    /// 生成配置文件与部署模板
    Init {
        /// 已存在配置文件时是否覆盖
        force: bool,
    },
    /// 打印用法
    Help,
    /// 打印版本
    Version,
}

/// flag 层覆盖项（配置的四层优先级中最高的一层）。
///
/// @intent 字段与 [`crate::config::Config`] 一一对应，但全部为 `Option`：
///         `None` 表示「本层未表态」，交由下层（进程环境变量 / 配置文件 / 内置默认值）决定。
///         这样「覆盖」与「未指定」在类型上即不可混淆，无需哨兵字符串。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// 监听端口
    pub port: Option<u16>,
    /// 数据目录
    pub data_dir: Option<String>,
    /// 管理员令牌
    pub admin_token: Option<String>,
    /// 新项目默认存储后端标识
    pub storage_backend: Option<String>,
    /// MCP Host 白名单原始串（逗号分隔）；解析交由 `Config::parse_allowed_hosts`
    pub mcp_allowed_hosts: Option<String>,
}

/// 命令行解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// 要执行的子命令
    pub command: Command,
    /// flag 层覆盖项
    pub overrides: Overrides,
    /// `--config` 显式指定的配置文件路径；`None` 表示使用 XDG 默认绝对路径
    pub config_path: Option<PathBuf>,
}

/// 解析错误。
///
/// @intent 逐项建模而非拼接字符串，使调用方（与测试）能精确断言失败原因；
///         错误信息本身面向运维，故一律给出「怎么改」而非仅「哪里错」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// 未知子命令
    UnknownCommand(String),
    /// 未知选项
    UnknownFlag(String),
    /// 选项缺少取值
    MissingValue(String),
    /// 选项取值非法
    InvalidValue {
        /// 出错的选项名
        flag: String,
        /// 原始取值
        value: String,
        /// 期望的取值形态
        expected: String,
    },
    /// 出现多余的参数
    UnexpectedArgument(String),
    /// `--force` 只对 `init` 有意义
    ForceRequiresInit,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::UnknownCommand(name) => write!(
                f,
                "unknown command `{name}`; expected one of: run, start, stop, restart, status, init"
            ),
            CliError::UnknownFlag(flag) => {
                write!(f, "unknown option `{flag}`; see `memora --help`")
            }
            CliError::MissingValue(flag) => write!(f, "option `{flag}` requires a value"),
            CliError::InvalidValue {
                flag,
                value,
                expected,
            } => write!(
                f,
                "invalid value `{value}` for `{flag}`: expected {expected}"
            ),
            CliError::UnexpectedArgument(arg) => {
                write!(f, "unexpected argument `{arg}`; see `memora --help`")
            }
            CliError::ForceRequiresInit => write!(f, "`--force` is only valid with `init`"),
        }
    }
}

impl std::error::Error for CliError {}

/// 用法文本（`memora --help` 与 `memora help` 共用）。
pub fn usage() -> String {
    format!(
        "Memora (忆庐) {version} — 轻量自托管 AI 长期记忆服务\n\
         \n\
         用法：\n\
         \x20 memora [run] [选项]           前台运行（默认；供 systemd / 容器托管）\n\
         \x20 memora start [选项]           后台启动（自守护，写入实例记录）\n\
         \x20 memora stop [选项]            停止后台进程\n\
         \x20 memora restart [选项]         重启后台进程\n\
         \x20 memora status [选项]          查看运行状态（未运行退出码 3）\n\
         \x20 memora init [--force]        生成 config.toml 与部署模板\n\
         \n\
         选项：\n\
         \x20 -p, --port <PORT>            监听端口\n\
         \x20     --data-dir <PATH>        数据目录\n\
         \x20     --admin-token <TOKEN>    管理员令牌（/api/v1 鉴权）\n\
         \x20     --storage-backend <NAME> 新项目默认存储后端\n\
         \x20     --mcp-allowed-hosts <CSV> MCP 允许的 Host 白名单（逗号分隔）\n\
         \x20     --config <PATH>          指定配置文件（默认 $XDG_CONFIG_HOME/memora/config.toml）\n\
         \x20 -h, --help                   显示本帮助\n\
         \x20 -V, --version                显示版本\n\
         \n\
         配置优先级：命令行选项 > 进程环境变量 > 配置文件 > 内置默认值\n\
         默认路径是绝对路径，与当前目录无关：任何目录下的 `memora status` 都指向同一实例，\n\
         故日常操作无需带参数。为某个实例单独指定配置时才需要 `--config`。\n",
        version = env!("CARGO_PKG_VERSION")
    )
}

/// 解析命令行参数（不含 argv[0]）。
///
/// @intent 三条规则需特别注意：
///         1. `-h/--help`、`-V/--version` 在任意位置出现即短路生效，便于
///            `memora start --help` 也能拿到用法；
///         2. 首个位置参数为已知子命令，其余位置参数一律拒绝（选项值已由 `--k v` 消费）；
///         3. 无位置参数（或仅有选项）时默认 `Run`——这是既有部署方式的兼容入口。
pub fn parse<I, S>(args: I) -> Result<Cli, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let argv: Vec<String> = args.into_iter().map(Into::into).collect();

    // 帮助 / 版本短路：先于一切校验，保证任何写法都能拿到用法
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(Cli {
            command: Command::Help,
            overrides: Overrides::default(),
            config_path: None,
        });
    }
    if argv.iter().any(|a| a == "-V" || a == "--version") {
        return Ok(Cli {
            command: Command::Version,
            overrides: Overrides::default(),
            config_path: None,
        });
    }

    let mut overrides = Overrides::default();
    let mut config_path: Option<PathBuf> = None;
    let mut command: Option<Command> = None;
    let mut force = false;

    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        if arg == "-p" {
            let value = next_value(&mut it, "-p")?;
            overrides.port = Some(parse_port(&value)?);
            continue;
        }

        if let Some(rest) = arg.strip_prefix("--") {
            let (key, inline) = match rest.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (rest, None),
            };
            match key {
                "port" => {
                    let value = value_of(inline, &mut it, "--port")?;
                    overrides.port = Some(parse_port(&value)?);
                }
                "data-dir" => {
                    let value = value_of(inline, &mut it, "--data-dir")?;
                    overrides.data_dir = Some(value);
                }
                "admin-token" => {
                    let value = value_of(inline, &mut it, "--admin-token")?;
                    overrides.admin_token = Some(value);
                }
                "storage-backend" => {
                    let value = value_of(inline, &mut it, "--storage-backend")?;
                    overrides.storage_backend = Some(value);
                }
                "mcp-allowed-hosts" => {
                    let value = value_of(inline, &mut it, "--mcp-allowed-hosts")?;
                    overrides.mcp_allowed_hosts = Some(value);
                }
                "config" => {
                    let value = value_of(inline, &mut it, "--config")?;
                    config_path = Some(PathBuf::from(value));
                }
                "force" => force = true,
                other => return Err(CliError::UnknownFlag(format!("--{other}"))),
            }
            continue;
        }

        if arg.starts_with('-') {
            return Err(CliError::UnknownFlag(arg));
        }

        match command {
            None => command = Some(parse_command(&arg)?),
            Some(_) => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    let command = match command {
        Some(Command::Init { .. }) => Command::Init { force },
        Some(other) => {
            if force {
                return Err(CliError::ForceRequiresInit);
            }
            other
        }
        None => {
            if force {
                return Err(CliError::ForceRequiresInit);
            }
            Command::Run
        }
    };

    Ok(Cli {
        command,
        overrides,
        config_path,
    })
}

/// 子命令名 → 枚举；未知名称给出可枚举的候选清单。
fn parse_command(name: &str) -> Result<Command, CliError> {
    match name {
        "run" => Ok(Command::Run),
        "start" => Ok(Command::Start),
        "stop" => Ok(Command::Stop),
        "restart" => Ok(Command::Restart),
        "status" => Ok(Command::Status),
        "init" => Ok(Command::Init { force: false }),
        "help" => Ok(Command::Help),
        "version" => Ok(Command::Version),
        other => Err(CliError::UnknownCommand(other.to_string())),
    }
}

/// 解析端口：`1..=65535`。0 会被内核解释为「随机可用端口」，与运维预期不符，故拒绝。
fn parse_port(raw: &str) -> Result<u16, CliError> {
    match raw.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(CliError::InvalidValue {
            flag: "--port".to_string(),
            value: raw.to_string(),
            expected: "an integer in 1..=65535".to_string(),
        }),
    }
}

/// 取 `--flag=value` 的内联值，缺失时消费下一个参数。
fn value_of(
    inline: Option<&str>,
    it: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, CliError> {
    match inline {
        Some(value) => Ok(value.to_string()),
        None => next_value(it, flag),
    }
}

/// 消费下一个参数作为选项值；缺失或「看起来像另一个选项」时报错。
///
/// @intent 拒绝以 `--` 开头的取值，避免 `--data-dir --port` 这类漏写把选项名吞成路径，
///         从而在更远处引发费解的故障。
fn next_value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, CliError> {
    match it.next() {
        Some(value) if value.starts_with("--") => Err(CliError::MissingValue(flag.to_string())),
        Some(value) => Ok(value),
        None => Err(CliError::MissingValue(flag.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析一组参数（测试内省略 argv[0]）。
    fn parse_args(args: &[&str]) -> Result<Cli, CliError> {
        parse(args.iter().copied())
    }

    #[test]
    fn no_args_defaults_to_run() {
        let cli = parse_args(&[]).unwrap();
        assert_eq!(cli.command, Command::Run);
        assert_eq!(cli.overrides, Overrides::default());
        assert_eq!(cli.config_path, None);
    }

    /// 向后兼容：既有部署方式 `PORT=6790 ./memora` 不带子命令，等价于 `run`。
    #[test]
    fn options_only_still_default_to_run() {
        let cli = parse_args(&["--port", "6790", "--data-dir", "/var/lib/memora"]).unwrap();
        assert_eq!(cli.command, Command::Run);
        assert_eq!(cli.overrides.port, Some(6790));
        assert_eq!(cli.overrides.data_dir.as_deref(), Some("/var/lib/memora"));
    }

    #[test]
    fn every_documented_subcommand_is_accepted() {
        let cases = [
            ("run", Command::Run),
            ("start", Command::Start),
            ("stop", Command::Stop),
            ("restart", Command::Restart),
            ("status", Command::Status),
            ("init", Command::Init { force: false }),
            ("help", Command::Help),
            ("version", Command::Version),
        ];
        for (name, expected) in cases {
            let cli = parse_args(&[name]).unwrap_or_else(|e| panic!("`{name}` 应被接受: {e}"));
            assert_eq!(cli.command, expected, "子命令 {name}");
        }
    }

    #[test]
    fn subcommand_accepts_options_after_it() {
        let cli = parse_args(&["start", "--port=6791", "--config", "/etc/memora.toml"]).unwrap();
        assert_eq!(cli.command, Command::Start);
        assert_eq!(cli.overrides.port, Some(6791));
        assert_eq!(cli.config_path, Some(PathBuf::from("/etc/memora.toml")));
    }

    /// `--config` 可写作内联形式，与 `--port=6791` 保持一致。
    #[test]
    fn config_accepts_inline_value() {
        let cli = parse_args(&["status", "--config=/tmp/a.toml"]).unwrap();
        assert_eq!(cli.config_path, Some(PathBuf::from("/tmp/a.toml")));
    }

    /// 旧开关已被移除，不得留下「两套心智长期并存」的残留：
    /// 写成 `--env-file` 必须直接报未知选项，而不是被默默忽略。
    #[test]
    fn env_file_is_no_longer_accepted() {
        assert_eq!(
            parse_args(&["start", "--env-file", "/etc/memora.env"]).unwrap_err(),
            CliError::UnknownFlag("--env-file".to_string())
        );
    }

    #[test]
    fn short_p_is_an_alias_for_port() {
        let cli = parse_args(&["-p", "7000"]).unwrap();
        assert_eq!(cli.overrides.port, Some(7000));
    }

    #[test]
    fn unknown_subcommand_is_rejected_with_candidates() {
        let err = parse_args(&["reload"]).unwrap_err();
        assert_eq!(err, CliError::UnknownCommand("reload".to_string()));
        assert!(err.to_string().contains("start"), "应列出可用子命令");
    }

    #[test]
    fn unknown_option_is_rejected() {
        let err = parse_args(&["--verbose"]).unwrap_err();
        assert_eq!(err, CliError::UnknownFlag("--verbose".to_string()));
        assert_eq!(
            parse_args(&["-x"]).unwrap_err(),
            CliError::UnknownFlag("-x".to_string())
        );
    }

    /// 0 会被内核当作「随机端口」，静默接受会让运维以为服务在 0 端口；
    /// 非数字同理必须报错，而不是回退到默认端口。
    #[test]
    fn invalid_port_is_rejected() {
        for raw in ["0", "65536", "abcd", "", "-1"] {
            match parse_args(&["--port", raw]) {
                Err(CliError::InvalidValue { flag, expected, .. }) => {
                    assert_eq!(flag, "--port");
                    assert!(expected.contains("65535"));
                }
                other => panic!("`--port {raw}` 应被拒绝，实际 {other:?}"),
            }
        }
    }

    #[test]
    fn missing_option_value_is_rejected() {
        assert_eq!(
            parse_args(&["--data-dir"]).unwrap_err(),
            CliError::MissingValue("--data-dir".to_string())
        );
        // 漏写取值时不得把下一个选项名吞作取值
        assert_eq!(
            parse_args(&["--data-dir", "--port"]).unwrap_err(),
            CliError::MissingValue("--data-dir".to_string())
        );
    }

    #[test]
    fn help_and_version_win_from_any_position() {
        assert_eq!(
            parse_args(&["start", "--help"]).unwrap().command,
            Command::Help
        );
        assert_eq!(parse_args(&["--help"]).unwrap().command, Command::Help);
        assert_eq!(parse_args(&["-h"]).unwrap().command, Command::Help);
        assert_eq!(
            parse_args(&["status", "--version"]).unwrap().command,
            Command::Version
        );
        assert_eq!(parse_args(&["-V"]).unwrap().command, Command::Version);
    }

    #[test]
    fn help_text_documents_every_subcommand_and_the_precedence_rule() {
        let text = usage();
        for name in ["run", "start", "stop", "restart", "status", "init"] {
            assert!(text.contains(name), "帮助文本缺少子命令 {name}");
        }
        assert!(
            text.contains("命令行选项 > 进程环境变量 > 配置文件 > 内置默认值"),
            "帮助文本必须写明配置优先级"
        );
    }

    /// 用户要的是「不带参数也能问到同一个实例」，故默认配置路径必须写进帮助文本。
    #[test]
    fn help_text_documents_the_default_config_path_and_config_flag() {
        let text = usage();
        assert!(text.contains("--config"), "帮助文本必须写明 --config");
        assert!(
            text.contains("$XDG_CONFIG_HOME/memora/config.toml"),
            "帮助文本必须写明默认配置路径"
        );
        assert!(
            !text.contains("--env-file"),
            "已移除的开关不得残留在帮助文本中"
        );
    }

    #[test]
    fn positional_after_subcommand_is_rejected() {
        assert_eq!(
            parse_args(&["start", "extra"]).unwrap_err(),
            CliError::UnexpectedArgument("extra".to_string())
        );
    }

    #[test]
    fn force_only_applies_to_init() {
        assert_eq!(
            parse_args(&["init", "--force"]).unwrap().command,
            Command::Init { force: true }
        );
        assert_eq!(
            parse_args(&["start", "--force"]).unwrap_err(),
            CliError::ForceRequiresInit
        );
        assert_eq!(
            parse_args(&["--force"]).unwrap_err(),
            CliError::ForceRequiresInit
        );
    }

    #[test]
    fn all_override_flags_are_captured() {
        let cli = parse_args(&[
            "--port",
            "6800",
            "--data-dir",
            "/data",
            "--admin-token",
            "s3cret",
            "--storage-backend",
            "in_mem",
            "--mcp-allowed-hosts",
            "mem.example.com,localhost",
        ])
        .unwrap();
        assert_eq!(
            cli.overrides,
            Overrides {
                port: Some(6800),
                data_dir: Some("/data".to_string()),
                admin_token: Some("s3cret".to_string()),
                storage_backend: Some("in_mem".to_string()),
                mcp_allowed_hosts: Some("mem.example.com,localhost".to_string()),
            }
        );
    }
}
