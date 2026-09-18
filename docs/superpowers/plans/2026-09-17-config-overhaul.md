# 配置体系整改（单实例 + XDG 绝对路径 + TOML）实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 把「配置靠当前目录偶然命中 `.env`」改成「单实例 + 稳定的绝对默认路径 + TOML 配置」，并让 `status` / `stop` 依据**运行中实例实际记录的事实**（而非重新推导的配置）作答，同时修掉 `init` 忽略命令行选项的缺陷。

**架构：** 配置改为单文件 TOML，默认路径固定为 XDG 绝对路径（`$XDG_CONFIG_HOME/memora/config.toml`，回退 `$HOME/.config/...`），数据目录默认同为 XDG 绝对路径（`$XDG_DATA_HOME/memora`，回退 `$HOME/.local/share/...`）。`--config` 是唯一的配置指路开关，`--port` / `--data-dir` 等保留为临时覆盖。PID 文件升级为 JSON **实例记录**（pid + port + data_dir + started_at + version），`status` / `stop` 优先信记录中的端口，从而消除「探错端口」与「配置改过但进程没重启」两类误诊。

**技术栈：** Rust 2021 · `toml` 1（替换 `dotenvy`）· `serde` / `serde_json`（记录序列化）· `anyhow` · 现有 `libc`（信号与存活探测不变）

**偏离说明（相对 writing-plans 默认格式）：** 本计划在**同一会话内联执行**，代码直接落入源码，故计划不重复内联每步完整代码，只固定接口契约、文件清单、验证命令与测试清单。按系统约定，本计划**不包含 commit 步骤**（未经用户要求不提交）。

---

## 一、决策记录

| 决策 | 选择 | 被否决的选项与理由 |
|---|---|---|
| 实例模型 | **单实例**：一个众所周知的默认配置路径 | 多实例（身份随数据目录走）：无稳定默认锚点，退化成“CWD 决定一切”，即本次要根除的问题。多实例需求由 `--config` 显式指定满足 |
| 配置格式 | **TOML** | `.env`：无类型/无嵌套/未知键静默忽略，正是“改了配置没生效”的温床 |
| 未知键 | **拒绝**（`deny_unknown_fields`） | 忽略：与项目既有“非法取值一律 Err，绝不静默回退”的取向冲突 |
| 默认路径 | **绝对路径（基于 `HOME`/XDG）** | CWD 相对：命中结果随执行目录漂移，是上一轮三类误报的根因 |
| `HOME` 不可用时 | 回退相对默认值并告警 | 硬失败：容器/systemd 无 `HOME` 属常见环境，硬失败不可接受 |
| 命令覆盖项 | **保留 flag 层**（优先级不变） | 只留 `--config`：临时/测试场景仍需覆盖，且会破坏既有已文档化的 CLI 与用例 |
| 实例记录格式 | JSON 对象，**兼容旧版裸整数 PID 文件** | 直接拒绝旧格式：会让升级后残留的 PID 文件把 `start` 顶成硬错误 |
| 记录中的 `port` | `Option<u16>` | 给旧格式填默认 6789：会静默撒谎，宁可显式未知并回退到配置端口 |

---

## 二、接口契约

```rust
// ---- config.rs ----
pub const DEFAULT_PORT: u16 = 6789;
pub const CONFIG_FILE_NAME: &str = "config.toml";
pub const DEFAULT_LOG: &str = "info,sqlx=warn,sea_orm=warn";
pub const DEFAULT_MCP_ALLOWED_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1"];

/// $XDG_DATA_HOME/memora → $HOME/.local/share/memora → "./data"
pub fn default_data_dir_from(xdg_data_home: Option<&str>, home: Option<&str>) -> String;
/// $XDG_CONFIG_HOME/memora/config.toml → $HOME/.config/memora/config.toml → "./config.toml"
pub fn default_config_path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> PathBuf;

pub enum ConfigSource { Explicit(PathBuf), Default(PathBuf), Missing(PathBuf) }
impl ConfigSource { pub fn log(&self); pub fn path(&self) -> &Path; }

pub struct Config {
    pub port: u16,
    pub data_dir: String,
    pub admin_token: String,
    pub storage_backend: String,
    pub mcp_allowed_hosts: Vec<String>,
    pub log: String,
    /// 本次生效的配置文件路径（显式指定或默认路径；供 init 写入与子进程透传）
    pub config_path: PathBuf,
}
impl Config {
    pub fn load(overrides: &Overrides, config_path: Option<&Path>) -> Result<(Config, ConfigSource)>;
    pub fn effective_mcp_allowed_hosts(&self) -> Vec<String>; // 语义不变
    pub fn for_test(port: u16, data_dir: &str, admin_token: &str, storage_backend: &str) -> Config;
}
/// 纯函数，便于单测：四层取值
pub fn resolve_port(flag: Option<u16>, env_raw: Option<&str>, file: Option<u16>) -> Result<u16>;
pub fn resolve_text(flag: Option<&str>, env_raw: Option<&str>, file: Option<&str>, default: &str) -> String;
pub fn resolve_log(env_raw: Option<&str>, file: Option<&str>) -> String;

// ---- instance.rs（由 pidfile.rs 改名）----
pub const INSTANCE_FILE_NAME: &str = "memora.pid";
pub const LOG_FILE_NAME: &str = "memora.log";
pub const EXIT_NOT_RUNNING: i32 = 3;

#[derive(Serialize, Deserialize)]
pub struct InstanceRecord { pub pid: i32, pub port: Option<u16>, pub data_dir: String,
                            pub started_at: String, pub version: String }

pub struct InstanceFile { /* path */ }
impl InstanceFile {
    pub fn new(data_dir: &Path) -> Self;
    pub fn log_path(data_dir: &Path) -> PathBuf;
    pub fn path(&self) -> &Path;
    pub fn read(&self) -> io::Result<Option<InstanceRecord>>;   // 兼容旧版裸整数
    pub fn write(&self, record: &InstanceRecord) -> io::Result<()>;  // 原子替换，权限 0600
    pub fn remove(&self) -> io::Result<()>;
    pub fn live(&self) -> io::Result<Option<InstanceRecord>>;   // 读 + 判活 + 清陈旧
}
pub fn is_alive(pid: i32) -> bool;                 // 语义不变（EPERM 视为存活）
pub fn terminate(pid: i32) -> io::Result<()>;
pub fn force_kill(pid: i32) -> io::Result<()>;

// ---- cli.rs ----
pub struct Cli { pub command: Command, pub overrides: Overrides, pub config_path: Option<PathBuf> }
// --env-file 被 --config 取代

// ---- daemon.rs ----
pub fn start(config: &Config) -> Result<i32>;   // 写入含 port 的实例记录
pub fn stop(config: &Config) -> Result<i32>;    // 依记录中的 pid
pub fn restart(config: &Config) -> Result<i32>;
pub fn status(config: &Config) -> Result<i32>;  // 依记录中的 port 探活，并报告配置/实例分歧
pub fn init(config: &Config, force: bool) -> Result<i32>;  // 依解析后的配置渲染，尊重 flag
pub fn child_spec(exe: &Path, config: &Config, log_path: &Path) -> ChildSpec; // args: ["run","--config",path]
```

---

## 三、文件清单

| 文件 | 动作 | 职责 |
|---|---|---|
| `Cargo.toml` | 修改 | 加 `toml = "1"`，删 `dotenvy` |
| `src/config.rs` | 重写 | TOML 加载、默认路径、四层优先级 |
| `src/pidfile.rs` → `src/instance.rs` | 改名 + 重写 | 实例记录（读写/存活/信号） |
| `src/cli.rs` | 修改 | `--config` 取代 `--env-file`，更新用法文本与用例 |
| `src/daemon.rs` | 修改 | `status`/`stop` 读记录；`init` 尊重配置；`child_spec` 透传配置路径 |
| `src/main.rs` | 修改 | `init` 前置加载配置；`init_tracing` 用解析后的日志级别；`mod instance` |
| `deploy/config.toml.template` | 新建 | 取代 `deploy/env.template` |
| `deploy/env.template` | 删除 | 同上 |
| `deploy/memora.service` | 修改 | `ExecStart=__EXE__ run --config __CONFIG__`，去掉 `EnvironmentFile` |
| `deploy/Caddyfile` | 修改 | 注释中的配置项名改为 TOML 键 |
| `.gitignore` | 修改 | 忽略 `config.toml`（避免把含 token 的本地配置提交） |
| `README.md` | 修改 | 快速开始、配置表、部署段、文档表 |

---

## 四、任务清单

### 任务 1：依赖切换
- [ ] `Cargo.toml`：加 `toml = "1"`（默认 features 已含 `serde`/`parse`），删 `dotenvy`
- [ ] 运行 `cargo build`，预期：仅报 `dotenvy` 相关调用点的编译错误（下一步修）

### 任务 2：`config.rs` 重写（TOML + 默认绝对路径）
- [ ] 写失败的测试：默认路径解析（XDG 优先、`HOME` 回退、无 `HOME` 回退相对）、TOML 解析、`deny_unknown_fields` 拒绝未知键、`port = 0` 拒绝、显式 `--config` 缺失硬失败、四层优先级
- [ ] 运行 `cargo test config::`，预期：FAIL（函数未定义）
- [ ] 实现 `FileConfig`（serde，全 `Option`，`deny_unknown_fields`）、`default_data_dir`、`default_config_path`、`resolve_port/resolve_text/resolve_log`、`Config::load`、`ConfigSource`
- [ ] 运行 `cargo test config::`，预期：PASS

### 任务 3：`pidfile.rs` → `instance.rs`
- [ ] 写失败的测试：记录往返、旧版裸整数兼容、损坏内容报错、陈旧记录清理、信号语义
- [ ] 运行 `cargo test instance::`，预期：FAIL
- [ ] 实现 `InstanceRecord` + `InstanceFile`（原子写、0600、`live()`）
- [ ] 运行 `cargo test instance::`，预期：PASS

### 任务 4：`cli.rs` 换开关
- [ ] 写失败的测试：`--config` 被接受、`--env-file` 报未知选项、用法文本包含 `--config` 与默认路径说明
- [ ] 运行 `cargo test cli::`，预期：FAIL
- [ ] 实现：`Cli.config_path`、`--config`、更新 `usage()`
- [ ] 运行 `cargo test cli::`，预期：PASS

### 任务 5：`daemon.rs` 与 `main.rs`
- [ ] 写失败的测试：`init` 渲染出的 TOML 含传入的 port/data_dir；`child_spec` 的 args 含 `--config`；`status` 在「记录端口 ≠ 配置端口」时采用记录端口
- [ ] 运行 `cargo test daemon::`，预期：FAIL
- [ ] 实现 `start`/`stop`/`status`/`init`/`child_spec` 与 `main.rs` 的 init 前置加载、`init_tracing`
- [ ] 运行 `cargo test`，预期：全部 PASS

### 任务 6：模板与文档
- [ ] 新建 `deploy/config.toml.template`，删除 `deploy/env.template`
- [ ] 更新 `deploy/memora.service`、`deploy/Caddyfile`、`.gitignore`
- [ ] 更新 `README.md`（快速开始、配置表、部署段、文档表，并修掉指向不存在 `.env.example` 的死链）

### 任务 7：整体验证
- [ ] `cargo test` — 预期全部 PASS
- [ ] `cargo clippy --all-targets` — 预期零告警
- [ ] `cargo build --release`
- [ ] 端到端复现验证（见第六节）

---

## 五、测试清单（新增/改写）

| 用例 | 断言 |
|---|---|
| `default_config_path_prefers_xdg` | `XDG_CONFIG_HOME` 存在时用它，否则用 `$HOME/.config` |
| `default_data_dir_prefers_xdg` | 同上，数据目录 |
| `defaults_fall_back_when_home_missing` | 无 `HOME`/`XDG` 时回退 `./data` 与 `./config.toml` |
| `toml_config_is_parsed` | 各键正确进入 `Config` |
| `unknown_key_is_rejected` | 多写一个键 → 硬失败且错误含键名（防“改了没生效”） |
| `port_zero_in_file_is_rejected` | `port = 0` → 硬失败 |
| `explicit_config_must_exist` | `--config` 指向不存在文件 → 硬失败且点名 `--config` |
| `missing_default_config_is_not_an_error` | 默认路径不存在 → `ConfigSource::Missing`，走默认值 |
| `precedence_is_flag_env_file_default` | 四层依次压制 |
| `record_round_trips` | `write` → `read` 字段一致 |
| `legacy_bare_pid_is_accepted` | 旧格式 `4321` → `pid = 4321`，`port = None` |
| `malformed_record_is_an_error` | 垃圾内容 → 报错（不得当作“未运行”） |
| `stale_record_is_cleaned_up` | 不存在的 pid → 清理并返回 `None` |
| `init_renders_resolved_values` | `init` 写出的文本含传入 port/data_dir |
| `child_spec_passes_config_path` | args = `["run", "--config", <path>]` |
| `status_uses_recorded_port` | 记录端口与配置端口不同时，探活采用记录端口 |

---

## 六、端到端验收（必须实际执行并保留输出）

1. 用非默认 flag 启动：`memora start --port 6799 --data-dir <tmp> --admin-token t --config <tmp>/config.toml`
2. **无参数** 执行 `memora status`：预期命中同一实例、`6799 (healthy)`、退出码 0（整改前为 `not running` / 退出码 3）
3. 探测 `/health` 返回 200
4. `memora stop`（无参数）真实停止进程（整改前为静默 `not running` / 退出码 0 且进程存活）
5. 手动改写记录中的 pid 为不存在值 → `status` 报未运行并清理陈旧文件
6. 清理临时目录，确认工作树仅含预期改动

---

## 七、明确不做（YAGNI / 非目标）

- **不做**多实例注册表或固定路径的实例发现：会破坏「身份随数据目录走」，且单实例决策下无用例。
- **不做**配置热重载：修改配置仍需 `restart`，但 `status` 会显式提示分歧。
- **不保留** `--env-file` 别名：`v0.1.0` 无存量部署，留别名只会让两套心智长期并存。
- **不迁移** `EMBEDDING_API_KEY`：该项在代码中从未被读取（仅 README 提及“规划中”），随本次整改从文档移除，避免与 `deny_unknown_fields` 冲突。
- **不引入** `dirs` 等依赖解析 XDG：手写约十行即可，符合本项目「能不加依赖就不加」的取向。

---

## 八、自检

**规格覆盖度：** 单实例 → 任务 2/4；默认绝对路径 → 任务 2；TOML → 任务 1/2/6；`status`/`stop` 依记录 → 任务 3/5；`init` 尊重 flag → 任务 5；文档与模板 → 任务 6；验收 → 任务 7 与第六节。无遗漏。

**占位符扫描：** 无「待定/TODO/类似任务 N」；每个任务均给出可执行命令与预期结果。代码不内联的理由已在开头声明。

**类型一致性：** `Config.config_path`、`InstanceRecord.port: Option<u16>`、`InstanceFile::live()`、`ConfigSource::Missing` 在第二、三、四、五节中命名一致；`status` 采用记录端口的约定在契约与测试清单中一致。