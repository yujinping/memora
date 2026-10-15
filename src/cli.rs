//! 命令行接口：子命令与全局选项解析。
//!
//! @author yujinping
//! @intent P4.2：让单个二进制自带进程管理（caddy 式 `run` / `start` / `stop` /
//!         `restart` / `status` / `init`），使「无 systemd、无 root」的裸机自托管
//!         同样可运维；同时保留「无子命令即前台运行」的向后兼容，
//!         既有 systemd / 容器部署零改动。
//!
//! @intent 解析刻意手写而非引入 clap：服务的选项面不大，而 clap 会带来可观的
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
    /// 任务交接（P4.3）：唯一的两级命令
    Handoff {
        /// 动作（save / load / list / ack / rm），缺失时解析期即报错
        action: HandoffAction,
    },
    /// 打印用法
    Help,
    /// 打印版本
    Version,
}

/// `memora handoff` 的动作（设计 §5.3：5 个显式子命令，方向推断交给会话内的代理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffAction {
    /// 存一次交接（stdin 或 `--file` 的 JSON）
    Save,
    /// 取一条交接（缺 `--task` 走零 ID 解析）
    Load,
    /// 列待接手（缺省即「待接手 + 倒序 + 上限 10」，等价于设想中的 latest）
    List,
    /// 标记接手（缺 `--task` 接当前唯一待接手）
    Ack,
    /// 删除整个任务（唯一强制要求 `--task` 的动作）
    Remove,
}

/// `memora handoff` 的选项集（全部可省，见设计 §5.3 的缺省表）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandoffOpts {
    /// 项目标识；省略时按「MEMORA_PROJECT → 服务端唯一项目」两级解析
    pub project: Option<String>,
    /// 任务标识；save / list 完全可省，load / ack 可省（零 ID），仅 rm 必填
    pub task: Option<String>,
    /// list 的条数上限；省略 = 10
    pub limit: Option<usize>,
    /// list 含已接手（等价于 REST 的 `unacked=0`；刻意不提供 `--unacked`）
    pub all: bool,
    /// list 只列 task_id 以该前缀开头的条目
    pub prefix: Option<String>,
    /// 写入方 / 接手方标识（zed / workbuddy ...）；save 时建议填（分片键）
    pub client: Option<String>,
    /// 服务端地址（http/https，可带路径前缀）；省略 = `http://127.0.0.1:<PORT>`
    pub server: Option<String>,
    /// 连接与整体读超时（秒）；省略 = 10
    pub timeout: Option<u64>,
    /// save 从文件读 JSON；省略读 stdin
    pub file: Option<PathBuf>,
    /// 输出原始 JSON（供脚本消费）
    pub json: bool,
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
    /// 网页控制台登录用户名
    pub admin_user: Option<String>,
    /// 网页控制台登录口令（**明文**，加载时即哈希，不落盘）
    pub admin_password: Option<String>,
    /// 网页控制台登录口令的 Argon2id PHC 串（与 `admin_password` 二选一，本项优先）
    pub admin_password_hash: Option<String>,
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
    /// `handoff` 子命令的选项（其他子命令必须保持默认值，见解析末尾的校验）
    pub handoff: HandoffOpts,
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
    /// `handoff` 缺动作（`memora handoff` 后必须跟 save/load/list/ack/rm）
    MissingHandoffAction,
    /// 交接选项（`--task` 等）只对 `handoff` 有意义
    HandoffFlagOutsideHandoff(String),
    /// `rm` 必须显式指认 `--task`（删除是破坏性操作，不能靠「猜当前待接手的」）
    TaskRequiredForRemove,
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
            CliError::MissingHandoffAction => write!(
                f,
                "`handoff` requires an action; expected one of: save, load, list, ack, rm"
            ),
            CliError::HandoffFlagOutsideHandoff(flag) => {
                write!(f, "`{flag}` is only valid with `handoff`")
            }
            CliError::TaskRequiredForRemove => {
                write!(
                    f,
                    "`handoff rm` requires `--task <id>`: deletion must be explicit"
                )
            }
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
         \x20 memora handoff save|load|list|ack|rm [选项]
         \x20                              任务交接（跨客户端 / 跨机器接力，见下）\n\
         \n\
         交接选项（仅 handoff）：\n\
         \x20     --project <ID>           项目标识（可省：MEMORA_PROJECT → 服务端唯一项目）\n\
         \x20     --task <ID>              任务标识（save/list 可省；load/ack 可省；rm 必填）\n\
         \x20     --limit <N>              list 条数上限（默认 10）\n\
         \x20     --all                    list 含已接手\n\
         \x20     --prefix <P>             list 只列 task_id 前缀匹配的条目\n\
         \x20     --client <NAME>          写入方 / 接手方标识（save 时建议填，如 zed）\n\
         \x20     --server <URL>           服务端地址（默认 http://127.0.0.1:<PORT>；支持 https 与路径前缀）\n\
         \x20     --timeout <SEC>          连接与读超时秒数（默认 10）\n\
         \x20     --file <PATH>            save 从文件读 JSON（默认 stdin）\n\
         \x20     --json                   输出原始 JSON（供脚本消费）\n\
         \n\
         选项：\n\
         \x20 -p, --port <PORT>            监听端口\n\
         \x20     --data-dir <PATH>        数据目录\n\
         \x20     --admin-token <TOKEN>    管理员令牌（/api/v1 鉴权）\n\
         \x20     --admin-user <NAME>      网页控制台（/admin）登录用户名，默认 admin\n\
         \x20     --admin-password <PWD>   网页控制台登录口令（明文；加载时即哈希，不落盘）\n\
         \x20     --admin-password-hash <PHC>  口令的 Argon2id 哈希串（优先于上一项）\n\
         \x20     --storage-backend <NAME> 新项目默认存储后端\n\
         \x20     --mcp-allowed-hosts <CSV> MCP 允许的 Host 白名单（逗号分隔）\n\
         \x20     --config <PATH>          指定配置文件（默认 $XDG_CONFIG_HOME/memora/config.toml）\n\
         \x20 -h, --help                   显示本帮助\n\
         \x20 -V, --version                显示版本\n\
         \n\
         配置优先级：命令行选项 > 进程环境变量 > 配置文件 > 内置默认值\n\
         默认路径是绝对路径，与当前目录无关：任何目录下的 `memora status` 都指向同一实例，\n\
         故日常操作无需带参数。为某个实例单独指定配置时才需要 `--config`。\n\
         网页控制台：服务启动后访问 http://localhost:<PORT>/admin\n",
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
            handoff: HandoffOpts::default(),
        });
    }
    if argv.iter().any(|a| a == "-V" || a == "--version") {
        return Ok(Cli {
            command: Command::Version,
            overrides: Overrides::default(),
            config_path: None,
            handoff: HandoffOpts::default(),
        });
    }

    let mut overrides = Overrides::default();
    let mut config_path: Option<PathBuf> = None;
    let mut command: Option<Command> = None;
    let mut force = false;
    let mut handoff = HandoffOpts::default();
    // `handoff` 是唯一的两级命令：首个位置参数是父命令，第二个才是动作。
    // 动作是解析局部量（不进 HandoffOpts）：选项集只装「用户显式给的」，
    // `handoff_flags_default_to_none` 这类断言才不依赖动作缺省值。
    let mut handoff_action: Option<HandoffAction> = None;

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
                "admin-user" => {
                    let value = value_of(inline, &mut it, "--admin-user")?;
                    overrides.admin_user = Some(value);
                }
                "admin-password" => {
                    let value = value_of(inline, &mut it, "--admin-password")?;
                    overrides.admin_password = Some(value);
                }
                "admin-password-hash" => {
                    let value = value_of(inline, &mut it, "--admin-password-hash")?;
                    overrides.admin_password_hash = Some(value);
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
                // ---- 交接选项（P4.3）：只对 `handoff` 有意义，末尾统一校验 ----
                "project" => {
                    handoff.project = Some(value_of(inline, &mut it, "--project")?);
                }
                "task" => {
                    handoff.task = Some(value_of(inline, &mut it, "--task")?);
                }
                "limit" => {
                    let value = value_of(inline, &mut it, "--limit")?;
                    handoff.limit = Some(parse_limit(&value)?);
                }
                "all" => handoff.all = true,
                "prefix" => {
                    handoff.prefix = Some(value_of(inline, &mut it, "--prefix")?);
                }
                "client" => {
                    handoff.client = Some(value_of(inline, &mut it, "--client")?);
                }
                "server" => {
                    handoff.server = Some(value_of(inline, &mut it, "--server")?);
                }
                "timeout" => {
                    let value = value_of(inline, &mut it, "--timeout")?;
                    handoff.timeout = Some(parse_timeout(&value)?);
                }
                "file" => {
                    handoff.file = Some(PathBuf::from(value_of(inline, &mut it, "--file")?));
                }
                "json" => handoff.json = true,
                other => return Err(CliError::UnknownFlag(format!("--{other}"))),
            }
            continue;
        }

        if arg.starts_with('-') {
            return Err(CliError::UnknownFlag(arg));
        }

        match command {
            None => command = Some(parse_command(&arg)?),
            // 仅 `handoff` 允许第二个位置参数（动作）；其余子命令维持单级
            Some(Command::Handoff { .. }) if handoff_action.is_none() => {
                handoff_action = Some(parse_handoff_action(&arg)?);
            }
            Some(_) => return Err(CliError::UnexpectedArgument(arg)),
        }
    }

    let command = match command {
        Some(Command::Init { .. }) => Command::Init { force },
        Some(Command::Handoff { .. }) => {
            let action = handoff_action.ok_or(CliError::MissingHandoffAction)?;
            if action == HandoffAction::Remove && handoff.task.is_none() {
                return Err(CliError::TaskRequiredForRemove);
            }
            Command::Handoff { action }
        }
        Some(other) => {
            if force {
                return Err(CliError::ForceRequiresInit);
            }
            reject_stray_handoff_flags(&handoff)?;
            other
        }
        None => {
            if force {
                return Err(CliError::ForceRequiresInit);
            }
            reject_stray_handoff_flags(&handoff)?;
            Command::Run
        }
    };

    Ok(Cli {
        command,
        overrides,
        config_path,
        handoff,
    })
}

/// `handoff` 专属选项出现在其他子命令上时必须报错，并点名是哪个选项。
///
/// @intent 静默忽略会让人以为选项生效了（比如 `memora start --limit 5`）；
///         逐字段检查保证错误信息指向**第一个**被误用的选项。
fn reject_stray_handoff_flags(handoff: &HandoffOpts) -> Result<(), CliError> {
    let strays = [
        ("--project", handoff.project.is_some()),
        ("--task", handoff.task.is_some()),
        ("--limit", handoff.limit.is_some()),
        ("--all", handoff.all),
        ("--prefix", handoff.prefix.is_some()),
        ("--client", handoff.client.is_some()),
        ("--server", handoff.server.is_some()),
        ("--timeout", handoff.timeout.is_some()),
        ("--file", handoff.file.is_some()),
        ("--json", handoff.json),
    ];
    for (flag, set) in strays {
        if set {
            return Err(CliError::HandoffFlagOutsideHandoff(flag.to_string()));
        }
    }
    Ok(())
}

/// `handoff` 动作名 → 枚举。
fn parse_handoff_action(name: &str) -> Result<HandoffAction, CliError> {
    match name {
        "save" => Ok(HandoffAction::Save),
        "load" => Ok(HandoffAction::Load),
        "list" => Ok(HandoffAction::List),
        "ack" => Ok(HandoffAction::Ack),
        "rm" => Ok(HandoffAction::Remove),
        other => Err(CliError::UnexpectedArgument(other.to_string())),
    }
}

/// 解析 `--limit`：非负整数。
fn parse_limit(raw: &str) -> Result<usize, CliError> {
    raw.parse::<usize>().map_err(|_| CliError::InvalidValue {
        flag: "--limit".to_string(),
        value: raw.to_string(),
        expected: "a non-negative integer".to_string(),
    })
}

/// 解析 `--timeout`：正整数秒（0 秒等于立刻超时，与运维预期不符，故拒绝）。
fn parse_timeout(raw: &str) -> Result<u64, CliError> {
    match raw.parse::<u64>() {
        Ok(secs) if secs > 0 => Ok(secs),
        _ => Err(CliError::InvalidValue {
            flag: "--timeout".to_string(),
            value: raw.to_string(),
            expected: "a positive integer (seconds)".to_string(),
        }),
    }
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
        // 占位动作在 parse 末尾以 `handoff.action` 收口（缺失 → MissingHandoffAction）
        "handoff" => Ok(Command::Handoff {
            action: HandoffAction::List,
        }),
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
            "--admin-user",
            "root",
            "--admin-password",
            "p4ssw0rd",
            "--admin-password-hash",
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA",
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
                admin_user: Some("root".to_string()),
                admin_password: Some("p4ssw0rd".to_string()),
                admin_password_hash: Some(
                    "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".to_string()
                ),
                storage_backend: Some("in_mem".to_string()),
                mcp_allowed_hosts: Some("mem.example.com,localhost".to_string()),
            }
        );
    }

    /// 三个凭据选项彼此独立，不得互相污染：
    /// `--admin-token`（脚本用）、`--admin-password`（明文）、`--admin-password-hash`（哈希）。
    #[test]
    fn credential_flags_do_not_leak_into_each_other() {
        let only_token = parse_args(&["--admin-token", "tok"]).unwrap();
        assert_eq!(only_token.overrides.admin_token.as_deref(), Some("tok"));
        assert_eq!(only_token.overrides.admin_password, None);
        assert_eq!(only_token.overrides.admin_password_hash, None);

        let only_password = parse_args(&["--admin-password", "pw"]).unwrap();
        assert_eq!(only_password.overrides.admin_token, None);
        assert_eq!(
            only_password.overrides.admin_password.as_deref(),
            Some("pw")
        );
        assert_eq!(only_password.overrides.admin_password_hash, None);

        // `--admin-password-hash` 的取值必须原样保留：它本身含有 `$` 与 `,`，
        // 若被当作分隔符或需要转义，PHC 串就会在解析阶段被悄悄改坏。
        let phc = "$argon2id$v=19$m=19456,t=2,p=1$YWJjZGVmZ2hpamtsbW5vcA$aGFzaA";
        let only_hash = parse_args(&["--admin-password-hash", phc]).unwrap();
        assert_eq!(
            only_hash.overrides.admin_password_hash.as_deref(),
            Some(phc)
        );
    }

    /// 帮助文本必须列出控制台相关选项与入口地址，否则用户无从知道它的存在。
    #[test]
    fn help_text_documents_the_console_and_its_credentials() {
        let text = usage();
        for expected in [
            "--admin-user",
            "--admin-password",
            "--admin-password-hash",
            "/admin",
        ] {
            assert!(text.contains(expected), "帮助文本缺少 {expected}");
        }
    }

    // ------------------------------------------------------ handoff（P4.3）

    #[test]
    fn handoff_accepts_all_five_actions() {
        let cases = [
            ("save", HandoffAction::Save),
            ("load", HandoffAction::Load),
            ("list", HandoffAction::List),
            ("ack", HandoffAction::Ack),
            ("rm", HandoffAction::Remove),
        ];
        for (name, expected) in cases {
            // rm 强制要求 --task（删除必须显式指认），单独带参验证
            let args: &[&str] = if name == "rm" {
                &["handoff", "rm", "--task", "gone"]
            } else {
                &["handoff", name]
            };
            let cli = parse_args(args).unwrap_or_else(|e| panic!("`handoff {name}` 应被接受: {e}"));
            assert_eq!(cli.command, Command::Handoff { action: expected });
        }
    }

    #[test]
    fn handoff_without_action_lists_candidates() {
        let err = parse_args(&["handoff"]).unwrap_err();
        assert_eq!(err, CliError::MissingHandoffAction);
        assert!(
            err.to_string().contains("save, load, list, ack, rm"),
            "错误信息应列出候选动作：{err}"
        );
    }

    #[test]
    fn handoff_unknown_action_is_rejected() {
        let err = parse_args(&["handoff", "sync"]).unwrap_err();
        assert_eq!(err, CliError::UnexpectedArgument("sync".to_string()));
    }

    #[test]
    fn handoff_third_positional_is_rejected() {
        assert_eq!(
            parse_args(&["handoff", "list", "extra"]).unwrap_err(),
            CliError::UnexpectedArgument("extra".to_string())
        );
    }

    /// 既有子命令是单级的：第二个位置参数仍然被拒绝（回归 #2）。
    #[test]
    fn other_subcommands_still_reject_second_positional() {
        assert_eq!(
            parse_args(&["start", "extra"]).unwrap_err(),
            CliError::UnexpectedArgument("extra".to_string())
        );
    }

    #[test]
    fn handoff_flags_are_captured() {
        let cli = parse_args(&[
            "handoff",
            "list",
            "--project",
            "memora",
            "--task",
            "current@zed",
            "--limit=5",
            "--all",
            "--prefix",
            "current@",
            "--client",
            "zed",
            "--server=https://mem.example.com/memora",
            "--timeout",
            "30",
            "--file",
            "/tmp/handoff.json",
            "--json",
        ])
        .unwrap();
        assert_eq!(cli.handoff.project.as_deref(), Some("memora"));
        assert_eq!(cli.handoff.task.as_deref(), Some("current@zed"));
        assert_eq!(cli.handoff.limit, Some(5));
        assert!(cli.handoff.all);
        assert_eq!(cli.handoff.prefix.as_deref(), Some("current@"));
        assert_eq!(cli.handoff.client.as_deref(), Some("zed"));
        assert_eq!(
            cli.handoff.server.as_deref(),
            Some("https://mem.example.com/memora")
        );
        assert_eq!(cli.handoff.timeout, Some(30));
        assert_eq!(cli.handoff.file, Some(PathBuf::from("/tmp/handoff.json")));
        assert!(cli.handoff.json);
    }

    #[test]
    fn handoff_flags_default_to_none() {
        let cli = parse_args(&["handoff", "load"]).unwrap();
        assert_eq!(cli.handoff, HandoffOpts::default());
    }

    /// 交接选项出现在其他子命令上必须报错并点名（不得静默忽略）。
    #[test]
    fn handoff_flags_outside_handoff_are_rejected_by_name() {
        assert_eq!(
            parse_args(&["start", "--task", "t"]).unwrap_err(),
            CliError::HandoffFlagOutsideHandoff("--task".to_string())
        );
        assert_eq!(
            parse_args(&["--json"]).unwrap_err(),
            CliError::HandoffFlagOutsideHandoff("--json".to_string())
        );
        assert_eq!(
            parse_args(&["status", "--server", "http://x"]).unwrap_err(),
            CliError::HandoffFlagOutsideHandoff("--server".to_string())
        );
    }

    /// `rm` 是唯一强制 `--task` 的动作：删除必须显式指认，不能「猜当前待接手的」。
    #[test]
    fn rm_requires_explicit_task() {
        assert_eq!(
            parse_args(&["handoff", "rm"]).unwrap_err(),
            CliError::TaskRequiredForRemove
        );
        let cli = parse_args(&["handoff", "rm", "--task", "gone"]).unwrap();
        assert_eq!(cli.handoff.task.as_deref(), Some("gone"));
    }

    #[test]
    fn invalid_limit_and_timeout_are_rejected() {
        assert!(matches!(
            parse_args(&["handoff", "list", "--limit", "abc"]),
            Err(CliError::InvalidValue { flag, .. }) if flag == "--limit"
        ));
        assert!(matches!(
            parse_args(&["handoff", "list", "--timeout", "0"]),
            Err(CliError::InvalidValue { flag, .. }) if flag == "--timeout"
        ));
        // `-1` 会作为取值被消费，落入数值校验（InvalidValue），而非未知选项
        assert!(matches!(
            parse_args(&["handoff", "list", "--limit", "-1"]),
            Err(CliError::InvalidValue { flag, .. }) if flag == "--limit"
        ));
    }

    #[test]
    fn handoff_documented_in_help_text() {
        let text = usage();
        for expected in [
            "handoff save|load|list|ack|rm",
            "--project",
            "--task",
            "--server",
        ] {
            assert!(text.contains(expected), "帮助文本缺少 {expected}");
        }
    }
}
