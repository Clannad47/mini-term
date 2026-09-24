//! ssh_service —— SSH 工具的共享业务编排层。
//!
//! 自 `mt-ssh-mcp.rs` 整体迁入（ssh-cli-skill spec §3）：连接查找、池 acquire、
//! transport 错 evict+单次 retry、auth 失败 30s cooldown、审计日志、mini-term
//! 数据目录传输硬护栏，全部收敛在这里单点执行。MCP handler 与 CLI/daemon 都是薄传输层。
//!
//! exec 的输出走 `on_output(StreamKind, &[u8])` 流式回调：daemon 侧写 IPC 帧
//! 实时转发，MCP 侧收集进缓冲再 cap_output 打包 JSON —— 两端行为各自保持不变。
//!
//! 两条纪律（与 MCP 时代一致）：
//! - 错误信息**绝不含密码**（只回显用户给的标识符 / 透传库错误文本）；
//! - 连接视图**绝不含 password / identityFile**。

use std::io::Write;
use std::time::Duration;

use mt_ssh::pool::{
    run_sftp_download_on_session, run_sftp_upload_on_session, SftpTransferError, SshPool,
};
use mt_ssh::russh::ChannelMsg;
use serde::Serialize;

/// MCP 侧输出封顶:stdout / stderr 各自最多保留约 100 KB,超出截断并标记。
/// CLI/daemon 侧不封顶（流式透传），本常量仅供 MCP 适配层取用。
pub const OUTPUT_CAP_BYTES: usize = 100 * 1024;

/// exec 的默认超时秒数。
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// upload / download 的默认超时秒数。比 exec 宽松——文件传输天然更慢,
/// 大文件需要更长窗口。
pub const DEFAULT_TRANSFER_TIMEOUT_SECS: u64 = 300;

/// 审计日志文件名(与 `config.json` 同目录)。沿用 MCP 时代的文件与行格式。
const AUDIT_LOG_FILE: &str = "ssh-mcp-audit.log";

/// auth 连失败后的 gatetime cooldown 时长。
const UNHEALTHY_COOLDOWN: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// 连接视图 / 列表
// ---------------------------------------------------------------------------

/// 暴露给 agent 的 SSH 连接视图。
///
/// 安全要点:**绝不包含 `password` / `identityFile` 等敏感字段**——
/// `mt_core::SshConnection` 含明文密码,绝不能直接序列化给 agent。
/// 这里只挑选展示用的非敏感字段。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConnectionView {
    /// 连接稳定 id,后续 exec 用它指定连接。
    pub id: String,
    /// 连接展示名。
    pub name: String,
    /// 远程主机地址。
    pub host: String,
    /// 远程 SSH 端口。
    pub port: u16,
    /// 登录用户名。
    pub user: String,
    /// 连接所属分组(可选)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// 把连接列表投影成对 agent 可见的视图。
///
/// 安全核心:映射到不含 `password` / `identityFile` 的 `SshConnectionView`,
/// 绝不把明文密码序列化给 agent。传入的连接列表已由
/// `read_ssh_connections_for_project` 按项目关联范围过滤。抽成纯函数便于单测。
pub fn connection_views(conns: Vec<mt_core::SshConnection>) -> Vec<SshConnectionView> {
    conns
        .into_iter()
        .map(|c| SshConnectionView {
            id: c.id,
            name: c.name,
            host: c.host,
            port: c.port,
            user: c.user,
            group: c.group,
        })
        .collect()
}

/// 调用方的连接范围来源。MCP 保留存量 project-id 语义；CLI/daemon 必须使用
/// 随机能力令牌，未知令牌 fail closed。
#[derive(Debug, Clone, Copy)]
pub enum ConnectionScope<'a> {
    LegacyProject(Option<&'a str>),
    Capability(&'a str),
}

/// 按调用来源读取连接，并把能力令牌错误归一为可读的参数错误。
fn read_scoped_connections(
    scope: ConnectionScope<'_>,
) -> Result<Vec<mt_core::SshConnection>, ServiceError> {
    match scope {
        ConnectionScope::LegacyProject(project_id) => {
            Ok(mt_core::read_ssh_connections_for_project(project_id))
        }
        ConnectionScope::Capability(project_token) => {
            mt_core::read_ssh_connections_for_token(project_token)
                .map_err(ServiceError::InvalidParams)
        }
    }
}

/// 列出对指定调用范围可见的 SSH 连接(不含敏感字段)。
///
/// 每次调用重读 config.json —— 保证主程序里改「关联 SSH」范围即时生效。
pub fn list_connections(
    scope: ConnectionScope<'_>,
) -> Result<Vec<SshConnectionView>, ServiceError> {
    read_scoped_connections(scope).map(connection_views)
}

// ---------------------------------------------------------------------------
// 错误分类
// ---------------------------------------------------------------------------

/// service 层错误。MCP 时代的两类映射保留语义，另有 daemon 断连专用取消态:
///
/// - `InvalidParams`:参数/业务错(连接未找到、护栏拒绝、SFTP 业务错)——
///   MCP 侧映射 `invalid_params`,CLI 侧一样是 exit 2;
/// - `Internal`:连接/认证/传输层错(acquire 失败、cooldown、retry 后仍失败、
///   transfer 超时)—— MCP 侧映射 `internal_error`。
///
/// 错误文本**绝不含密码**(与 MCP 同一纪律)。
#[derive(Debug)]
pub enum ServiceError {
    InvalidParams(String),
    Internal(String),
    Cancelled,
}

impl ServiceError {
    /// 取人类可读的错误信息。
    pub fn message(&self) -> &str {
        match self {
            ServiceError::InvalidParams(m) | ServiceError::Internal(m) => m,
            ServiceError::Cancelled => "ssh exec cancelled",
        }
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

// ---------------------------------------------------------------------------
// 纯函数:连接查找 / 命令拼装 / 输出封顶
// ---------------------------------------------------------------------------

/// 在连接列表里按 name 或 id 查找连接。
///
/// 匹配规则(先 name 后 id,均大小写敏感,与 SshModal 行为一致):
/// - name 精确命中多条 → 歧义错误;
/// - name 无命中再按 id 精确命中;
/// - 均无命中 → 未找到错误。
///
/// 传入列表已由 `read_ssh_connections_for_project` 按项目关联范围过滤,
/// 因此本项目无权访问的连接天然「未找到」,无需再单独校验授权。
/// 所有错误信息**不含密码**(只回显用户给的标识符)。
pub fn find_connection(
    conns: &[mt_core::SshConnection],
    selector: &str,
) -> Result<mt_core::SshConnection, String> {
    let by_name: Vec<&mt_core::SshConnection> =
        conns.iter().filter(|c| c.name == selector).collect();
    match by_name.len() {
        1 => Ok(by_name[0].clone()),
        n if n > 1 => Err(format!(
            "SSH connection name '{selector}' is ambiguous: {n} connections share this name. \
            Use the connection id instead."
        )),
        // name 无命中 → 退而按 id 精确匹配
        _ => match conns.iter().find(|c| c.id == selector) {
            Some(c) => Ok(c.clone()),
            None => Err(format!(
                "No SSH connection found matching '{selector}'. \
                Call ssh_list_connections to see available connections."
            )),
        },
    }
}

/// 拼远程要执行的命令:`cwd` 非空时前缀 `cd <cwd> && `。
pub fn build_remote_command(command: &str, cwd: Option<&str>) -> String {
    match cwd.map(str::trim).filter(|s| !s.is_empty()) {
        Some(dir) => format!("cd {dir} && {command}"),
        None => command.to_string(),
    }
}

/// 把一段输出按字节封顶。返回 (截断后的文本, 是否发生截断)。
///
/// 按字节而非字符封顶以严格控制返回体积;在 UTF-8 字符边界处切割,
/// 避免产生非法 UTF-8。仅 MCP 适配层使用(CLI 不封顶)。
pub fn cap_output(s: &str, cap: usize) -> (String, bool) {
    if s.len() <= cap {
        return (s.to_string(), false);
    }
    // 从 cap 处向前回退到一个 UTF-8 字符边界
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n…[output truncated]");
    (out, true)
}

// ---------------------------------------------------------------------------
// 审计日志
// ---------------------------------------------------------------------------

/// 脱敏掩码。凭据被替换成这三个字符。
const MASK: &str = "***";

/// 已知会用 `-p<密码>` **紧贴**写法的客户端(mysql 家族)。
///
/// 只在命令里出现它们时才认这种形态 —— 见 `redact_command` 的取舍说明。
/// 按 basename 比对(去掉目录与 `.exe`),`sudo mysql -pX` / `/usr/bin/mysql -pX` /
/// `docker exec db mysql -pX` 都能命中。
const GLUED_PASSWORD_CLIENTS: &[&str] = &[
    "mysql",
    "mysqladmin",
    "mysqldump",
    "mysqlshow",
    "mysqlimport",
    "mariadb",
    "mariadb-admin",
    "mariadb-dump",
    "mariadb-show",
    "mariadb-import",
];

/// 审计日志脱敏:把命令行里**明确是凭据**的片段换成 `***`。
///
/// 审计日志记的是命令原文,`mysql -pSecret` / `curl -u user:pass` /
/// `https://user:pass@host` 这类写法会把明文凭据落进磁盘上的
/// `ssh-mcp-audit.log`(且该文件与 `config.json` 同目录、长期留存)。
///
/// 覆盖的四种形态:
///
/// | 形态 | 处理 |
/// |------|------|
/// | `-p<值>` 紧贴(mysql 风格) | → `-p***`,**且仅当命令里出现 mysql 家族客户端** |
/// | `--password=<值>` / `--password <值>` | 值整段 → `***` |
/// | `-u` / `--user` 的 `用户:密码` | 只掩冒号后半段 → `用户:***` |
/// | URL 的 `scheme://用户:密码@主机` | 只掩密码段 → `scheme://用户:***@主机` |
///
/// **宁漏勿误伤**:审计日志的可用性优先,拿不准的形态一律不动 ——
/// 独立的 `-p 3306`(端口)、`ssh://user@host`(无密码段)、`http://h:8080/x`
/// (那个冒号是端口)全部原样保留;不认识的自定义凭据开关(`--token=` 等)也不动。
///
/// 紧贴 `-p` 之所以要挂 mysql 家族这道闸:`tar -pxvf` / `cp -pr` / `mkdir -pv` /
/// `docker run -p8080:80` / `ssh -p2222` 都是同一种「`-p` 后紧贴非空」的形态,
/// 无差别掩会把审计日志掩成废纸,那比漏掩一条 `sudo -u x mysql5 -pX` 更糟。
///
/// 纯函数,不碰 IO —— 便于单测穷举形态。
pub fn redact_command(command: &str) -> String {
    let segments = split_keep_whitespace(command);
    let allow_glued_p = segments.iter().any(|s| is_glued_password_client(s));

    // 上一个非空白 token 若是「独立开关」,当前 token 就是它的值。
    enum Pending {
        Password,
        User,
    }
    let mut pending: Option<Pending> = None;
    let mut out = String::with_capacity(command.len());

    for seg in segments {
        // 空白段原样回填(审计行要尽量贴近用户实际敲下的那行)
        if seg
            .as_bytes()
            .first()
            .map(|b| b.is_ascii_whitespace())
            .unwrap_or(false)
        {
            out.push_str(seg);
            continue;
        }
        match pending.take() {
            Some(Pending::Password) => {
                out.push_str(MASK);
                continue;
            }
            Some(Pending::User) => {
                // 无冒号说明只是个用户名,不是凭据 —— 原样保留
                out.push_str(&mask_user_value(seg));
                continue;
            }
            None => {}
        }
        if seg == "--password" {
            pending = Some(Pending::Password);
            out.push_str(seg);
            continue;
        }
        if seg == "-u" || seg == "--user" {
            pending = Some(Pending::User);
            out.push_str(seg);
            continue;
        }
        out.push_str(&redact_token(seg, allow_glued_p));
    }
    out
}

/// 把字符串切成「空白段 / 非空白段」交替的切片序列,拼回去与原文逐字节相同。
///
/// 不用 `split_whitespace`:那会把连续空格、制表压成一个,审计行就不是原文了。
/// 按字节判空白是安全的 —— ASCII 空白绝不会出现在 UTF-8 多字节序列内部,
/// 切点必然落在字符边界上。
fn split_keep_whitespace(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let is_ws = bytes[i].is_ascii_whitespace();
        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() == is_ws {
            j += 1;
        }
        out.push(&s[i..j]);
        i = j;
    }
    out
}

/// token 是否是 mysql 家族客户端(去目录、去 `.exe`、忽略大小写)。
fn is_glued_password_client(token: &str) -> bool {
    let base = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .to_ascii_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    GLUED_PASSWORD_CLIENTS.contains(&base)
}

/// 单个 token 的脱敏:紧贴形态的开关 + URL userinfo。
fn redact_token(token: &str, allow_glued_p: bool) -> String {
    if let Some(value) = token.strip_prefix("--password=") {
        if !value.is_empty() {
            return format!("--password={MASK}");
        }
        return token.to_string();
    }
    if let Some(value) = token.strip_prefix("--user=") {
        return format!("--user={}", mask_user_value(value));
    }
    if !token.starts_with("--") {
        // `-uuser:pass`(curl 允许紧贴)。没有冒号就只是用户名,不动。
        if let Some(value) = token.strip_prefix("-u") {
            if value.contains(':') {
                return format!("-u{}", mask_user_value(value));
            }
        }
        // `-pSecret`(mysql 风格)。独立的 `-p` 不在此列 —— 它后面跟的通常是端口。
        if allow_glued_p {
            if let Some(value) = token.strip_prefix("-p") {
                if !value.is_empty() {
                    return format!("-p{MASK}");
                }
            }
        }
    }
    redact_url_userinfo(token)
}

/// `-u` / `--user` 取到的值:整段是 URL 时(`redis-cli -u redis://:pw@h`)按 URL 规则走,
/// 否则按 `用户:密码` 掩冒号后半段。
///
/// 少了这一岔,`-u redis://:pw@h` 会被当成 `用户=redis` 而把 `//:pw@h` 整段掩掉 ——
/// 密码是没了,主机名也一起没了。
fn mask_user_value(value: &str) -> String {
    if value.contains("://") {
        redact_url_userinfo(value)
    } else {
        mask_after_first_colon(value)
    }
}

/// `用户:密码` → `用户:***`。没有冒号、或冒号后为空则原样返回。
fn mask_after_first_colon(value: &str) -> String {
    match value.split_once(':') {
        Some((user, pass)) if !pass.is_empty() => format!("{user}:{MASK}"),
        _ => value.to_string(),
    }
}

/// 掩掉 token 里所有 URL 的 userinfo 密码段:`scheme://user:pass@host` →
/// `scheme://user:***@host`。
///
/// 边界按 RFC 3986 取:authority 到第一个 `/` `?` `#` 为止,userinfo 到 authority
/// 里**最后一个** `@` 为止,密码是 userinfo 里**第一个** `:` 之后的部分。
/// 没有 `@` 的一律不动 —— `http://host:8080/p` 那个冒号是端口,不是密码。
fn redact_url_userinfo(token: &str) -> String {
    if !token.contains("://") {
        return token.to_string();
    }
    let mut out = String::with_capacity(token.len());
    let mut rest = token;
    while let Some(idx) = rest.find("://") {
        let (head, after) = rest.split_at(idx + 3);
        out.push_str(head);
        let auth_end = after.find(['/', '?', '#']).unwrap_or(after.len());
        let authority = &after[..auth_end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str(&mask_after_first_colon(&authority[..at]));
                out.push('@');
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &after[auth_end..];
    }
    out.push_str(rest);
    out
}

/// 格式化一行审计日志。抽出便于单测。
///
/// 形如:`2026-05-18T12:34:56Z\tconn=prod\texit=0\tcmd=ls -la`
/// 命令先过 `redact_command` 脱敏(明文凭据不落盘),再把换行替换成空格,
/// 保证一次执行就是一行。
fn format_audit_line(timestamp: &str, conn_name: &str, command: &str, exit: Option<i32>) -> String {
    let exit_str = match exit {
        Some(code) => code.to_string(),
        None => "timeout".to_string(),
    };
    let one_line_cmd = redact_command(command).replace(['\n', '\r'], " ");
    format!("{timestamp}\tconn={conn_name}\texit={exit_str}\tcmd={one_line_cmd}\n")
}

/// 极简 UTC 时间戳(无需引入 chrono):基于 UNIX 秒数。
///
/// 仅用于审计日志,精确到秒、UTC。失败回退 "unknown"。
fn utc_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => return "unknown".to_string(),
    };
    // 把 UNIX 秒数拆成 YYYY-MM-DDTHH:MM:SSZ(标准公历换算)。
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // 从 1970-01-01 起按年累加,处理闰年。
    let mut year = 1970i64;
    let mut day_of_era = days as i64;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let year_len = if leap { 366 } else { 365 };
        if day_of_era < year_len {
            break;
        }
        day_of_era -= year_len;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_lens = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0usize;
    while month < 12 && day_of_era >= month_lens[month] {
        day_of_era -= month_lens[month];
        month += 1;
    }
    format!(
        "{year:04}-{:02}-{:02}T{hh:02}:{mm:02}:{ss:02}Z",
        month + 1,
        day_of_era + 1,
    )
}

/// 把一行(已格式化的)审计日志追加到 `{config.json 所在目录}/ssh-mcp-audit.log`。
///
/// exec 与 transfer 两类审计共用的落盘尾段;写日志失败绝不影响工具结果 ——
/// 只往 stderr 记一笔。
fn append_audit_line(line: &str) {
    let Some(cfg_path) = mt_core::config_json_path() else {
        eprintln!("[mt-ssh-svc] audit: cannot locate config.json dir, skipping audit log");
        return;
    };
    let Some(dir) = cfg_path.parent() else {
        eprintln!("[mt-ssh-svc] audit: config.json has no parent dir, skipping audit log");
        return;
    };
    let log_path = dir.join(AUDIT_LOG_FILE);
    let write_result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = write_result {
        eprintln!("[mt-ssh-svc] audit: failed to write {AUDIT_LOG_FILE}: {e}");
    }
}

/// exec 审计:格式化一行并落盘。
fn append_audit_log(conn_name: &str, command: &str, exit: Option<i32>) {
    append_audit_line(&format_audit_line(
        &utc_timestamp(),
        conn_name,
        command,
        exit,
    ));
}

/// 格式化一行传输审计日志。抽出便于单测。
///
/// 形如:`2026-06-09T12:34:56Z\tdir=upload\tconn=prod\tlocal=/a/b\tremote=/c/d\tresult=1234`
/// 路径里的换行/制表替换成空格,保证一次传输就是一行。`result` 成功时为字节数,失败为 `error`。
fn format_transfer_audit_line(
    timestamp: &str,
    direction: TransferDirection,
    conn_name: &str,
    local_path: &str,
    remote_path: &str,
    result: Option<u64>,
) -> String {
    let sanitize = |s: &str| s.replace(['\n', '\r', '\t'], " ");
    let result_str = match result {
        Some(bytes) => bytes.to_string(),
        None => "error".to_string(),
    };
    format!(
        "{timestamp}\tdir={}\tconn={}\tlocal={}\tremote={}\tresult={result_str}\n",
        direction.as_str(),
        sanitize(conn_name),
        sanitize(local_path),
        sanitize(remote_path),
    )
}

/// 传输审计:格式化一行并落盘。`result = Some(bytes)` 成功、`None` 失败。
fn append_transfer_audit_log(
    direction: TransferDirection,
    conn_name: &str,
    local_path: &str,
    remote_path: &str,
    result: Option<u64>,
) {
    append_audit_line(&format_transfer_audit_line(
        &utc_timestamp(),
        direction,
        conn_name,
        local_path,
        remote_path,
        result,
    ));
}

// ---------------------------------------------------------------------------
// 传输护栏
// ---------------------------------------------------------------------------

/// 安全硬护栏:本地路径是否落在 mini-term 自己的数据目录(含子目录)里。
///
/// 数据目录里没有一样东西该经 SSH 工具流出去,更不该被远端文件覆盖:`config.db`
/// 及其 `.bak` / `-wal` / `-shm`(全部配置,SSH 密码是 mt-secret 信封)、
/// `config.json` 投影与 `.pre-sqlite` 存档(同样带信封)、`credential.key`(解信封
/// 的主密钥,Unix 上就是裸密钥)、`layout.db` / `usage.db` / hook 端口文件 / 审计
/// 日志……agent 一句 upload 能把「信封 + 钥匙」一起带走,一句 download 能覆盖配置
/// 或密钥。所以不再逐个文件点名,**整棵目录树、上传下载两个方向一律拒绝**。其它
/// 本地文件按 PRD Decision 不做沙箱限制(仅审计),这是唯一一条硬拒绝。
///
/// 数据目录的定位见 [`protected_data_dirs`];一个都定位不到则放行(无从比对,
/// 不误伤普通文件)。
pub fn is_blocked_local_path(local_path: &str) -> bool {
    is_blocked_local_path_against(local_path, &protected_data_dirs())
}

/// 需要护住的 mini-term 数据目录。
///
/// - 装机版数据目录:`mt_core::config_json_path()` 的所在目录 —— sidecar 读 SSH
///   投影、懒加载 `credential.key`、写审计日志用的都是这同一个锚点(主程序
///   `mt_config::app_data_dir` 拼出的也是它);
/// - 环境里带着 `MT_APP_DATA_DIR` 时它指的目录一并护住:那是主程序
///   `mt_config::active_data_dir` 的覆盖口径(非空即生效,dev 实例的隔离数据目录,
///   里面同样有自己的 `config.db` 与 `credential.key`)。sidecar 自己不从那里读
///   任何东西,这里只多拦、不改变读取口径。
fn protected_data_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = mt_core::config_json_path()
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
        .into_iter()
        .collect();
    if let Some(dev_dir) = std::env::var_os("MT_APP_DATA_DIR").filter(|v| !v.is_empty()) {
        dirs.push(std::path::PathBuf::from(dev_dir));
    }
    dirs
}

/// `is_blocked_local_path` 的可测核心:`local_path` 规范化后是否落在任一 `roots`
/// 之内(含 root 本身)。
///
/// 两边都走 [`resolve_lenient`](存在的部分 canonicalize,不存在的尾段按字面折叠
/// —— 下载的目标文件往往还不存在,不能因此绕过护栏),再按路径**分量**判定前缀:
/// `com.mini-term.app-backup` 这类同名前缀的兄弟目录不会误伤。Windows 与默认
/// 大小写不敏感的 macOS 上统一小写后比较。
fn is_blocked_local_path_against(local_path: &str, roots: &[std::path::PathBuf]) -> bool {
    let local = comparable_path(&resolve_lenient(std::path::Path::new(local_path)));
    roots
        .iter()
        .any(|root| local.starts_with(comparable_path(&resolve_lenient(root))))
}

/// 把路径尽量解析成真实的绝对路径。
///
/// 相对路径先接到当前目录上;然后从整条路径起逐段缩短,找最长的、能 canonicalize
/// 的前缀 —— 它消解了 `..`、符号链接 / junction 与 8.3 短名;还不存在的尾段按字面
/// 接上(`.` 丢弃、`..` 弹一级)。一段都解析不了(盘符都不存在)时退回纯字面折叠。
fn resolve_lenient(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let components: Vec<Component> = absolute.components().collect();
    for split in (1..=components.len()).rev() {
        let prefix: PathBuf = components[..split].iter().collect();
        if let Ok(mut resolved) = std::fs::canonicalize(&prefix) {
            push_lexically(&mut resolved, &components[split..]);
            return resolved;
        }
    }
    let mut resolved = PathBuf::new();
    push_lexically(&mut resolved, &components);
    resolved
}

/// 按字面把路径分量接到 `base` 上:`.` 丢弃,`..` 弹掉上一段普通目录名(到根就停,
/// 与操作系统对 `/..` 的处理一致)。
fn push_lexically(base: &mut std::path::PathBuf, components: &[std::path::Component]) {
    use std::path::Component;

    for component in components {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(base.components().next_back(), Some(Component::Normal(_))) {
                    base.pop();
                } else if !base.has_root() {
                    // 解析不出当前目录时残留的相对路径:保留前导 `..`
                    base.push(component.as_os_str());
                }
            }
            other => base.push(other.as_os_str()),
        }
    }
}

/// 比较用的路径形态:Windows 与 macOS 的默认文件系统大小写不敏感,统一小写;
/// 其它平台原样。
fn comparable_path(path: &std::path::Path) -> std::path::PathBuf {
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        std::path::PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

/// 文件传输方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

impl TransferDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            TransferDirection::Upload => "upload",
            TransferDirection::Download => "download",
        }
    }
}

// ---------------------------------------------------------------------------
// exec —— 流式回调 + 完整编排
// ---------------------------------------------------------------------------

/// 状态型 exec 取消令牌。`cancel()` 后状态永久保持，既唤醒当前 waiter，也保证
/// 尚未进入 SSH channel 阶段的 future 稍后观察到取消。
#[derive(Debug, Clone)]
pub struct ExecCancellation {
    state: tokio::sync::watch::Sender<bool>,
}

impl ExecCancellation {
    pub fn new() -> Self {
        let (state, _) = tokio::sync::watch::channel(false);
        Self { state }
    }

    pub fn cancel(&self) {
        self.state.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.state.borrow()
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.state.subscribe();
        if *receiver.borrow_and_update() {
            return;
        }
        while receiver.changed().await.is_ok() {
            if *receiver.borrow_and_update() {
                return;
            }
        }
    }
}

impl Default for ExecCancellation {
    fn default() -> Self {
        Self::new()
    }
}

enum ControlledStep<T> {
    Ready(T),
    TimedOut,
    Cancelled,
}

async fn await_exec_step<F>(
    future: F,
    deadline: tokio::time::Instant,
    cancellation: &ExecCancellation,
) -> ControlledStep<F::Output>
where
    F: std::future::Future,
{
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => ControlledStep::Cancelled,
        _ = tokio::time::sleep_until(deadline) => ControlledStep::TimedOut,
        output = future => ControlledStep::Ready(output),
    }
}

/// exec 输出流的类别,对应远程命令的 stdout / stderr。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// 一次 exec 编排的最终结果。输出已经通过 `on_output` 回调流出,这里只带元数据。
///
/// 超时时 `exit_code` 恒为 `None`、`timed_out = true`;**回调已流出的部分输出
/// 不会撤回** —— CLI/daemon 侧这正是「超时打印已收到的部分输出」的契约;MCP 侧
/// 适配层按旧行为把缓冲清空(旧实现超时路径拿不到部分输出)。
#[derive(Debug)]
pub struct ExecOutcome {
    /// 远程命令退出码。超时、或远端未上报 exit-status 时为 None。
    pub exit_code: Option<i32>,
    /// 是否因超时被强制终止。
    pub timed_out: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionExecOutcome {
    Completed(Option<i32>),
    TimedOut,
    Cancelled,
}

/// 在已 acquire 到的 session 上开 channel、跑 exec,输出经回调流出。
///
/// 超时与取消在 channel 所在状态机内处理：一旦 channel 已建立，任何提前结束
/// 分支都先显式发送 SSH close，不能依赖 `Channel` 的 Drop。返回 `Result`:
/// `Err(String)` 代表 transport-level 失败(channel 开不了 / exec 发不出去),
/// caller 可用此信号触发"evict + 重连"。**transport 错只会发生在任何输出流出之前**
/// (进入收流循环后错误只表现为流自然结束),因此 caller 重试不会造成输出重复。
pub async fn run_exec_on_session(
    session: &mt_ssh::pool::CachedSession,
    remote_command: &str,
    timeout: Duration,
    cancellation: &ExecCancellation,
    on_output: &mut (impl FnMut(StreamKind, &[u8]) + Send),
) -> Result<SessionExecOutcome, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let handle_guard = match await_exec_step(session.lock(), deadline, cancellation).await {
        ControlledStep::Ready(guard) => guard,
        ControlledStep::TimedOut => return Ok(SessionExecOutcome::TimedOut),
        ControlledStep::Cancelled => return Ok(SessionExecOutcome::Cancelled),
    };
    let mut channel =
        match await_exec_step(handle_guard.channel_open_session(), deadline, cancellation).await {
            ControlledStep::Ready(Ok(channel)) => channel,
            ControlledStep::Ready(Err(e)) => {
                return Err(format!("channel_open_session failed: {e}"));
            }
            ControlledStep::TimedOut => return Ok(SessionExecOutcome::TimedOut),
            ControlledStep::Cancelled => return Ok(SessionExecOutcome::Cancelled),
        };

    match await_exec_step(channel.exec(true, remote_command), deadline, cancellation).await {
        ControlledStep::Ready(Ok(())) => {}
        ControlledStep::Ready(Err(e)) => {
            let _ = channel.close().await;
            return Err(format!("channel exec failed: {e}"));
        }
        ControlledStep::TimedOut => {
            let _ = channel.close().await;
            return Ok(SessionExecOutcome::TimedOut);
        }
        ControlledStep::Cancelled => {
            let _ = channel.close().await;
            return Ok(SessionExecOutcome::Cancelled);
        }
    }

    let mut exit_code: Option<i32> = None;

    // RFC 4254 §5.2: ExtendedData.ext == 1 表示 stderr。
    const SSH_EXTENDED_DATA_STDERR: u32 = 1;

    loop {
        match await_exec_step(channel.wait(), deadline, cancellation).await {
            ControlledStep::Ready(Some(msg)) => match msg {
                ChannelMsg::Data { data } => on_output(StreamKind::Stdout, &data),
                ChannelMsg::ExtendedData { data, ext } if ext == SSH_EXTENDED_DATA_STDERR => {
                    on_output(StreamKind::Stderr, &data);
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    exit_code = Some(exit_status as i32);
                    // 不能立刻 break:服务器可能在 ExitStatus 之后还会发 Close/Eof,
                    // 也可能还有最后一批 Data 待收。让循环走到 wait() 返回 None。
                }
                // 收到 Eof / Close 后,wait() 很快会返回 None 退出循环;不需要 break。
                _ => {}
            },
            ControlledStep::Ready(None) => break,
            ControlledStep::TimedOut => {
                let _ = channel.close().await;
                return Ok(SessionExecOutcome::TimedOut);
            }
            ControlledStep::Cancelled => {
                let _ = channel.close().await;
                return Ok(SessionExecOutcome::Cancelled);
            }
        }
    }
    // 主动关闭 channel(server-side 可能已经关了,这里幂等 best-effort)。
    let _ = channel.close().await;
    drop(handle_guard);

    Ok(SessionExecOutcome::Completed(exit_code))
}

/// 通过已保存的 SSH 连接在远程主机上执行一条命令 —— 完整编排。
///
/// 编排步骤(与 MCP 时代逐项一致):
/// 1. 每次重读 config.json，按 MCP project-id 或 CLI capability token 解析范围；
/// 2. 拼远程命令(可选 cwd 前缀);
/// 3. 池 acquire(lazy 建/复用 session);cooldown 中直接返错;
/// 4. channel 内状态机强制超时/取消并显式 close；transport 错 → evict + 单次 retry;
///    retry 仍失败 → mark_unhealthy 30s;
/// 5. 审计:除 acquire/cooldown 错之外,每次执行(含超时与 retry 失败)记一行。
///
/// 输出经 `on_output` 实时流出;超时路径不撤回已流出的部分输出(见 [`ExecOutcome`])。
#[derive(Debug, Clone, Copy)]
pub struct ExecRequest<'a> {
    pub scope: ConnectionScope<'a>,
    pub connection: &'a str,
    pub command: &'a str,
    pub cwd: Option<&'a str>,
    pub timeout_secs: Option<u64>,
}

pub async fn exec(
    pool: &SshPool,
    request: ExecRequest<'_>,
    cancellation: &ExecCancellation,
    mut on_output: impl FnMut(StreamKind, &[u8]) + Send,
) -> Result<ExecOutcome, ServiceError> {
    let ExecRequest {
        scope,
        connection,
        command,
        cwd,
        timeout_secs,
    } = request;
    // 1. 每次入口都重读配置，保证范围或连接变更立即生效。
    let conn = find_connection(&read_scoped_connections(scope)?, connection)
        .map_err(ServiceError::InvalidParams)?;

    // 2. 拼远程命令(可选 cwd 前缀)。
    let remote_command = build_remote_command(command, cwd);
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1));
    let conn_name_for_audit = conn.name.clone();
    let conn_id = conn.id.clone();

    // 3. 走池:lazy 建/复用 session。acquire 失败(transport / auth 层错)直接返
    //    (不进 retry,auth 错重试只会徒增暴力)。
    let session = pool.acquire(&conn).await.map_err(ServiceError::Internal)?;
    if session.is_unhealthy_now() {
        return Err(ServiceError::Internal(
            "session is in cooldown after a previous auth failure; retry shortly".into(),
        ));
    }

    // 4. 在 session 上跑 exec。超时/取消由 channel 内状态机处理，保证显式 close。
    //    第一次失败 → evict + 重新 acquire → 再试一次 → 仍失败 → 标 unhealthy + 返错。
    let first = run_exec_on_session(
        &session,
        &remote_command,
        timeout,
        cancellation,
        &mut on_output,
    )
    .await;

    let outcome = match first {
        Ok(SessionExecOutcome::Completed(exit_code)) => {
            session.touch();
            ExecOutcome {
                exit_code,
                timed_out: false,
            }
        }
        Err(first_err) => {
            // transport-level 错(channel 开不了 / exec 发不出),可能是死链 race。
            // 移除并重建,再试一次。
            eprintln!("[mt-ssh-svc] exec on cached session failed, retrying: {first_err}");
            pool.evict(&conn_id).await;
            let session2 = pool
                .acquire(&conn)
                .await
                .map_err(|e| ServiceError::Internal(format!("reconnect failed: {e}")))?;
            if session2.is_unhealthy_now() {
                return Err(ServiceError::Internal(
                    "session is in cooldown after a previous auth failure; retry shortly".into(),
                ));
            }
            match run_exec_on_session(
                &session2,
                &remote_command,
                timeout,
                cancellation,
                &mut on_output,
            )
            .await
            {
                Ok(SessionExecOutcome::Completed(exit_code)) => {
                    session2.touch();
                    ExecOutcome {
                        exit_code,
                        timed_out: false,
                    }
                }
                Err(second_err) => {
                    // 两次都失败 —— 进 30s gatetime cooldown,避免连发把服务器打死。
                    session2.mark_unhealthy(UNHEALTHY_COOLDOWN);
                    append_audit_log(&conn_name_for_audit, command, None);
                    return Err(ServiceError::Internal(format!(
                        "ssh exec failed after retry: {second_err}"
                    )));
                }
                Ok(SessionExecOutcome::TimedOut) => {
                    // 第二次:超时。强制中止 + timedOut=true。
                    eprintln!(
                        "[mt-ssh-svc] exec timed out on retry after {}s",
                        timeout.as_secs()
                    );
                    ExecOutcome {
                        exit_code: None,
                        timed_out: true,
                    }
                }
                Ok(SessionExecOutcome::Cancelled) => return Err(ServiceError::Cancelled),
            }
        }
        Ok(SessionExecOutcome::TimedOut) => {
            // 第一次超时。不 evict、不 disconnect session —— 单 channel 超时
            // 不代表整个 session 死了。
            eprintln!(
                "[mt-ssh-svc] exec timed out after {}s on connection '{}'",
                timeout.as_secs(),
                conn_name_for_audit
            );
            ExecOutcome {
                exit_code: None,
                timed_out: true,
            }
        }
        Ok(SessionExecOutcome::Cancelled) => return Err(ServiceError::Cancelled),
    };

    // 5. 审计日志:每次执行记一行(失败不影响结果)。超时记 exit=timeout。
    append_audit_log(&conn_name_for_audit, command, outcome.exit_code);

    Ok(outcome)
}

// ---------------------------------------------------------------------------
// transfer —— SFTP 上传/下载完整编排
// ---------------------------------------------------------------------------

/// 根据方向在一条 session 上跑一次 SFTP 传输。把 upload/download 两个 pool 函数
/// 统一成同一签名,供重试编排复用。
async fn run_one_transfer(
    direction: TransferDirection,
    session: &mt_ssh::pool::CachedSession,
    local_path: &str,
    remote_path: &str,
    timeout: Duration,
) -> Result<u64, SftpTransferError> {
    match direction {
        TransferDirection::Upload => {
            run_sftp_upload_on_session(session, local_path, remote_path, timeout).await
        }
        TransferDirection::Download => {
            run_sftp_download_on_session(session, remote_path, local_path, timeout).await
        }
    }
}

/// 通过已保存的 SSH 连接做 SFTP 上传/下载 —— 完整编排。
///
/// 编排:护栏 → 查连接 → acquire → 超时 + transport 错的 evict+单次 retry →
/// 审计 → 返回字节数。逻辑与 exec 同构;SFTP 业务错(路径不存在/无权限/本地 IO)
/// 不 evict、不 retry,按 `InvalidParams` 返回。
pub async fn transfer(
    pool: &SshPool,
    direction: TransferDirection,
    scope: ConnectionScope<'_>,
    connection: &str,
    local_path: &str,
    remote_path: &str,
    timeout_secs: Option<u64>,
) -> Result<u64, ServiceError> {
    // 0. 安全硬护栏:本地路径落在 mini-term 自己的数据目录里一律拒绝(配置库及其
    //    备份 / WAL、带密码信封的投影与存档、解信封的 credential.key 都在那里)。
    //    upload 与 download 都拦 —— 上传外泄、下载覆盖配置或密钥均不可接受。
    if is_blocked_local_path(local_path) {
        return Err(ServiceError::InvalidParams(
            "refusing to transfer files inside mini-term's own data directory: it holds the \
            config database (with its backups), the saved SSH credentials and the key that \
            decrypts them, which must never be uploaded or overwritten via this tool."
                .to_string(),
        ));
    }

    // 1. 查连接(列表已按本项目关联范围过滤,越权连接天然「未找到」)。错误不含密码。
    let conn = find_connection(&read_scoped_connections(scope)?, connection)
        .map_err(ServiceError::InvalidParams)?;

    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TRANSFER_TIMEOUT_SECS).max(1));
    let conn_name_for_audit = conn.name.clone();
    let conn_id = conn.id.clone();

    // 2. 走池:lazy 建/复用 session。auth/transport 错直接返(不进 retry)。
    let session = pool.acquire(&conn).await.map_err(ServiceError::Internal)?;
    if session.is_unhealthy_now() {
        return Err(ServiceError::Internal(
            "session is in cooldown after a previous auth failure; retry shortly".into(),
        ));
    }

    // 3. 跑传输,整段套 tokio::time::timeout。第一次 transport 错 → evict + 重连 → 再试一次。
    let first = tokio::time::timeout(
        timeout,
        run_one_transfer(direction, &session, local_path, remote_path, timeout),
    )
    .await;

    let bytes = match first {
        Ok(Ok(n)) => {
            session.touch();
            n
        }
        Ok(Err(e)) if e.is_transport() => {
            // transport-level:可能死链 race,evict + 重建再试一次。
            eprintln!(
                "[mt-ssh-svc] sftp {} on cached session failed (transport), retrying: {e}",
                direction.as_str()
            );
            pool.evict(&conn_id).await;
            let session2 = pool
                .acquire(&conn)
                .await
                .map_err(|e| ServiceError::Internal(format!("reconnect failed: {e}")))?;
            if session2.is_unhealthy_now() {
                return Err(ServiceError::Internal(
                    "session is in cooldown after a previous auth failure; retry shortly".into(),
                ));
            }
            match tokio::time::timeout(
                timeout,
                run_one_transfer(direction, &session2, local_path, remote_path, timeout),
            )
            .await
            {
                Ok(Ok(n)) => {
                    session2.touch();
                    n
                }
                Ok(Err(second_err)) => {
                    if second_err.is_transport() {
                        session2.mark_unhealthy(UNHEALTHY_COOLDOWN);
                    }
                    append_transfer_audit_log(
                        direction,
                        &conn_name_for_audit,
                        local_path,
                        remote_path,
                        None,
                    );
                    return Err(ServiceError::Internal(format!(
                        "ssh {} failed after retry: {second_err}",
                        direction.as_str()
                    )));
                }
                Err(_) => {
                    append_transfer_audit_log(
                        direction,
                        &conn_name_for_audit,
                        local_path,
                        remote_path,
                        None,
                    );
                    return Err(ServiceError::Internal(format!(
                        "ssh {} timed out after {}s on retry",
                        direction.as_str(),
                        timeout.as_secs()
                    )));
                }
            }
        }
        Ok(Err(e)) => {
            // SFTP 业务错(路径不存在/无权限/本地 IO):不 evict、不 retry,直接返。
            append_transfer_audit_log(
                direction,
                &conn_name_for_audit,
                local_path,
                remote_path,
                None,
            );
            return Err(ServiceError::InvalidParams(format!(
                "ssh {} failed: {e}",
                direction.as_str()
            )));
        }
        Err(_) => {
            // 超时(第一次)。
            append_transfer_audit_log(
                direction,
                &conn_name_for_audit,
                local_path,
                remote_path,
                None,
            );
            return Err(ServiceError::Internal(format!(
                "ssh {} timed out after {}s",
                direction.as_str(),
                timeout.as_secs()
            )));
        }
    };

    // 4. 审计:成功记字节数。
    append_transfer_audit_log(
        direction,
        &conn_name_for_audit,
        local_path,
        remote_path,
        Some(bytes),
    );

    Ok(bytes)
}

// ============================================================================
// tests —— 随代码自 mt-ssh-mcp.rs 迁入,行为断言不变
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn conn(id: &str, password: Option<&str>) -> mt_core::SshConnection {
        mt_core::SshConnection {
            id: id.into(),
            name: format!("conn-{id}"),
            host: "10.0.0.5".into(),
            port: 22,
            user: "root".into(),
            password: password.map(|s| s.into()),
            identity_file: Some("/home/u/.ssh/id_rsa".into()),
            group: Some("内网".into()),
            extra: Default::default(),
        }
    }

    // --- connection_views ---

    #[test]
    fn connection_views_projects_all_connections() {
        // connection_views 本身不再过滤,原样投影传入的连接列表
        let conns = vec![conn("1", Some("secret1")), conn("2", None)];
        let views = connection_views(conns);
        let ids: Vec<&str> = views.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["1", "2"]);
    }

    #[test]
    fn connection_views_empty_input_yields_empty() {
        assert!(connection_views(vec![]).is_empty());
    }

    #[test]
    fn serialized_view_never_leaks_password_or_identity_file() {
        // 安全验收:即便源连接有明文密码与私钥路径,序列化结果也绝不能含它们。
        let conns = vec![conn("1", Some("super-secret-password"))];
        let views = connection_views(conns);
        let json = serde_json::to_string(&views).unwrap();
        assert!(!json.contains("super-secret-password"));
        assert!(!json.to_lowercase().contains("password"));
        assert!(!json.contains("identityFile"));
        assert!(!json.contains("id_rsa"));
        // 但应保留展示字段
        assert!(json.contains("\"host\":\"10.0.0.5\""));
    }

    // --- find_connection ---

    #[test]
    fn find_connection_matches_by_name() {
        let conns = vec![conn("1", None)];
        let found = find_connection(&conns, "conn-1").unwrap();
        assert_eq!(found.id, "1");
    }

    #[test]
    fn find_connection_matches_by_id_when_name_misses() {
        let conns = vec![conn("abc", None)];
        let found = find_connection(&conns, "abc").unwrap();
        assert_eq!(found.id, "abc");
    }

    #[test]
    fn find_connection_errors_when_not_found() {
        let conns = vec![conn("1", None)];
        let err = find_connection(&conns, "does-not-exist").unwrap_err();
        assert!(err.contains("No SSH connection found"));
    }

    #[test]
    fn find_connection_errors_on_ambiguous_name() {
        let mut a = conn("1", None);
        let mut b = conn("2", None);
        a.name = "dup".into();
        b.name = "dup".into();
        let err = find_connection(&[a, b], "dup").unwrap_err();
        assert!(err.contains("ambiguous"));
    }

    #[test]
    fn find_connection_not_found_error_never_contains_password() {
        // 安全:未找到时错误信息只回显 selector,绝不泄漏任何连接的明文密码。
        let conns = vec![conn("1", Some("topsecretpw"))];
        let err = find_connection(&conns, "does-not-exist").unwrap_err();
        assert!(!err.contains("topsecretpw"));
    }

    // --- build_remote_command ---

    #[test]
    fn build_remote_command_without_cwd() {
        assert_eq!(build_remote_command("ls -la", None), "ls -la");
    }

    #[test]
    fn build_remote_command_with_cwd_prefixes_cd() {
        assert_eq!(
            build_remote_command("ls -la", Some("/var/log")),
            "cd /var/log && ls -la"
        );
    }

    #[test]
    fn build_remote_command_ignores_blank_cwd() {
        assert_eq!(build_remote_command("pwd", Some("   ")), "pwd");
        assert_eq!(build_remote_command("pwd", Some("")), "pwd");
    }

    // --- cap_output ---

    #[test]
    fn cap_output_short_string_unchanged() {
        let (out, trunc) = cap_output("hello", 100);
        assert_eq!(out, "hello");
        assert!(!trunc);
    }

    #[test]
    fn cap_output_truncates_long_string() {
        let big = "x".repeat(500);
        let (out, trunc) = cap_output(&big, 100);
        assert!(trunc);
        assert!(out.starts_with(&"x".repeat(100)));
        assert!(out.contains("output truncated"));
    }

    #[test]
    fn cap_output_exact_cap_not_truncated() {
        let s = "y".repeat(100);
        let (out, trunc) = cap_output(&s, 100);
        assert!(!trunc);
        assert_eq!(out, s);
    }

    #[test]
    fn cap_output_respects_utf8_boundary() {
        // 多字节字符:cap 落在字符中间时,回退到边界,结果仍是合法 UTF-8。
        let s = "中".repeat(100); // 每个 '中' 占 3 字节
        let (out, trunc) = cap_output(&s, 100); // 100 不是 3 的倍数
        assert!(trunc);
        // 结果可被正常当作 &str 使用即证明是合法 UTF-8
        assert!(out.chars().take_while(|&c| c == '中').count() <= 34);
    }

    // --- format_audit_line ---

    #[test]
    fn format_audit_line_basic() {
        let line = format_audit_line("2026-05-18T12:00:00Z", "prod", "ls -la", Some(0));
        assert!(line.starts_with("2026-05-18T12:00:00Z\t"));
        assert!(line.contains("conn=prod"));
        assert!(line.contains("exit=0"));
        assert!(line.contains("cmd=ls -la"));
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn format_audit_line_timeout_has_no_exit_code() {
        let line = format_audit_line("2026-05-18T12:00:00Z", "prod", "sleep 999", None);
        assert!(line.contains("exit=timeout"));
    }

    #[test]
    fn format_audit_line_collapses_multiline_command() {
        let line = format_audit_line("t", "c", "echo a\necho b\r\necho c", Some(0));
        // 命令里的换行被替成空格 —— 一次执行只占一行
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn format_audit_line_redacts_credentials() {
        // 安全验收:审计行里绝不出现明文密码
        let line = format_audit_line(
            "t",
            "prod",
            "mysql -h db -uroot -pS3cr3tPw -e 'select 1'",
            Some(0),
        );
        assert!(!line.contains("S3cr3tPw"), "实际: {line}");
        assert!(line.contains("-p***"), "实际: {line}");
        // 非凭据部分照旧可读
        assert!(line.contains("-h db"));
        assert!(line.contains("select 1"));
    }

    // --- redact_command ---

    #[test]
    fn redact_glued_mysql_password() {
        assert_eq!(redact_command("mysql -pSecret"), "mysql -p***");
        assert_eq!(
            redact_command("mysqldump -uroot -pP@ss db > d.sql"),
            "mysqldump -uroot -p*** db > d.sql"
        );
        // 带路径 / .exe / sudo 前缀也认得出客户端
        assert_eq!(
            redact_command("sudo /usr/bin/mysql -pSecret"),
            "sudo /usr/bin/mysql -p***"
        );
        assert_eq!(redact_command("mysql.exe -pSecret"), "mysql.exe -p***");
        assert_eq!(
            redact_command("docker exec db mariadb -pSecret"),
            "docker exec db mariadb -p***"
        );
    }

    #[test]
    fn redact_leaves_standalone_dash_p_and_its_value_alone() {
        // 反例:独立 `-p` 后面跟的是端口,不是密码
        assert_eq!(redact_command("mysql -p 3306"), "mysql -p 3306");
        assert_eq!(redact_command("psql -p 5432 -h db"), "psql -p 5432 -h db");
        assert_eq!(redact_command("ps -p 1234"), "ps -p 1234");
    }

    #[test]
    fn redact_leaves_non_credential_glued_dash_p_alone() {
        // 反例:命令里没有 mysql 家族客户端时,紧贴 `-p` 一律不动 ——
        // tar/cp/mkdir/docker/ssh 的 `-p` 都不是密码
        assert_eq!(redact_command("tar -pxvf a.tar"), "tar -pxvf a.tar");
        assert_eq!(redact_command("cp -pr a b"), "cp -pr a b");
        assert_eq!(redact_command("mkdir -pv /a/b"), "mkdir -pv /a/b");
        assert_eq!(
            redact_command("docker run -p8080:80 nginx"),
            "docker run -p8080:80 nginx"
        );
        assert_eq!(redact_command("ssh -p2222 h"), "ssh -p2222 h");
    }

    #[test]
    fn redact_long_password_flag_both_shapes() {
        assert_eq!(
            redact_command("pg_dump --password=Secret db"),
            "pg_dump --password=*** db"
        );
        assert_eq!(
            redact_command("wget --password Secret http://h/f"),
            "wget --password *** http://h/f"
        );
        // 值在下一段,中间隔多少空白都认
        assert_eq!(
            redact_command("wget --password   Secret"),
            "wget --password   ***"
        );
        // 光秃秃的 `--password=`(没有值)不动
        assert_eq!(redact_command("x --password="), "x --password=");
    }

    #[test]
    fn redact_user_colon_password() {
        assert_eq!(
            redact_command("curl -u alice:hunter2 https://api/x"),
            "curl -u alice:*** https://api/x"
        );
        assert_eq!(
            redact_command("curl -ualice:hunter2 https://api/x"),
            "curl -ualice:*** https://api/x"
        );
        assert_eq!(
            redact_command("curl --user=alice:hunter2 https://api/x"),
            "curl --user=alice:*** https://api/x"
        );
        assert_eq!(
            redact_command("curl --user alice:hunter2 https://api/x"),
            "curl --user alice:*** https://api/x"
        );
    }

    #[test]
    fn redact_leaves_user_without_colon_alone() {
        // 反例:没有冒号就只是用户名,不是凭据
        assert_eq!(redact_command("curl -u alice https://api"), "curl -u alice https://api");
        assert_eq!(redact_command("sudo -u www-data ls"), "sudo -u www-data ls");
        assert_eq!(redact_command("id -u"), "id -u");
    }

    #[test]
    fn redact_url_userinfo_password() {
        assert_eq!(
            redact_command("curl https://alice:hunter2@api.example.com/v1?a=1"),
            "curl https://alice:***@api.example.com/v1?a=1"
        );
        assert_eq!(
            redact_command("git clone https://u:tok@github.com/o/r.git"),
            "git clone https://u:***@github.com/o/r.git"
        );
        // 无用户名、只有密码的形态(redis / amqp 常见)
        assert_eq!(
            redact_command("redis-cli -u redis://:pw@127.0.0.1:6379"),
            "redis-cli -u redis://:***@127.0.0.1:6379"
        );
    }

    #[test]
    fn redact_leaves_non_credential_urls_alone() {
        // 反例:端口的冒号、无密码的 userinfo、scp 风格的 host:path 都不是凭据
        assert_eq!(
            redact_command("curl http://example.com:8080/a?b=1"),
            "curl http://example.com:8080/a?b=1"
        );
        assert_eq!(
            redact_command("ssh ssh://user@host:22/x"),
            "ssh ssh://user@host:22/x"
        );
        assert_eq!(
            redact_command("git clone git@github.com:owner/repo.git"),
            "git clone git@github.com:owner/repo.git"
        );
    }

    #[test]
    fn redact_leaves_plain_commands_byte_identical() {
        // 反例:没有凭据形态的命令必须逐字节原样返回(含连续空格与缩进)
        for cmd in [
            "ls -la",
            "systemctl restart nginx",
            "tail -n 100 /var/log/syslog | grep -i error",
            "  echo   'a   b'  ",
            "find / -name '*.log' -mtime +7 -delete",
            "",
        ] {
            assert_eq!(redact_command(cmd), cmd, "不该改动: {cmd}");
        }
    }

    #[test]
    fn redact_preserves_newlines_and_multiple_hits() {
        // 多段命令里出现多处凭据时全都掩掉,换行等空白原样保留
        let src = "mysql -pA\ncurl -u u:B https://x:C@h/p";
        let out = redact_command(src);
        assert_eq!(out, "mysql -p***\ncurl -u u:*** https://x:***@h/p");
        assert!(!out.contains("-pA"));
        assert!(!out.contains(":B"));
        assert!(!out.contains(":C@"));
    }

    // --- utc_timestamp ---

    #[test]
    fn utc_timestamp_has_expected_shape() {
        let ts = utc_timestamp();
        // 形如 YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(ts.len(), 20, "got: {ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        // 年份在合理范围
        let year: i64 = ts[..4].parse().unwrap();
        assert!((2025..2100).contains(&year));
    }

    // --- 传输护栏 is_blocked_local_path ---------------------------------

    /// 造一个「数据目录」夹具:`<tmp>/mt-ssh-svc-guard-<pid>-<name>/{data,outside}`,
    /// 数据目录里放几个真实存在的文件与一个子目录。返回 (夹具根, 数据目录, 外部目录)。
    fn guard_fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("mt-ssh-svc-guard-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("com.mini-term.app");
        let outside = base.join("outside");
        std::fs::create_dir_all(data.join("themes")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        for file in ["config.db", "config.json", "credential.key"] {
            std::fs::write(data.join(file), b"x").unwrap();
        }
        (base, data, outside)
    }

    fn blocked(local: &std::path::Path, roots: &[PathBuf]) -> bool {
        is_blocked_local_path_against(local.to_str().unwrap(), roots)
    }

    #[test]
    fn guard_blocks_every_file_under_the_data_dir() {
        // 数据库迁移 + 密码封存之后,要护的远不止 config.json:库本体、备份、WAL、
        // 存档、主密钥,以及子目录里的东西 —— 存在的与还不存在的(下载目标)都拦。
        let (base, data, _) = guard_fixture("under");
        let roots = [data.clone()];
        for rel in [
            "config.db",
            "config.json",
            "credential.key",
            "config.db.bak",
            "config.db-wal",
            "config.db-shm",
            "config.json.pre-sqlite",
            "themes/pack.json",
            "not-yet/created/deep.bin",
        ] {
            let path = data.join(rel);
            assert!(blocked(&path, &roots), "应拦截 {}", path.display());
        }
        assert!(blocked(&data, &roots), "数据目录本身也算在内");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn guard_allows_paths_outside_the_data_dir() {
        let (base, data, outside) = guard_fixture("outside");
        let roots = [data.clone()];
        std::fs::write(outside.join("notes.txt"), b"hi").unwrap();
        assert!(!blocked(&outside.join("notes.txt"), &roots));
        assert!(!blocked(&outside.join("new-download.bin"), &roots));
        // 同名前缀的兄弟目录按路径分量比较,不误伤
        let sibling = base.join("com.mini-term.app-backup").join("config.db");
        assert!(!blocked(&sibling, &roots));
        // 没有可护的目录(定位失败)时放行
        assert!(!blocked(&data.join("config.db"), &[]));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn guard_resolves_dotdot_before_comparing() {
        let (base, data, outside) = guard_fixture("dotdot");
        let roots = [data.clone()];
        // 从外部目录绕回数据目录:存在的文件、以及经由不存在的中间目录
        let sneaky = outside
            .join("..")
            .join("com.mini-term.app")
            .join("credential.key");
        assert!(blocked(&sneaky, &roots));
        let via_missing = outside
            .join("missing")
            .join("..")
            .join("..")
            .join("com.mini-term.app")
            .join("config.db-wal");
        assert!(blocked(&via_missing, &roots));
        // 反过来从数据目录里 `..` 出去的路径不拦
        let escaped = data
            .join("themes")
            .join("..")
            .join("..")
            .join("outside")
            .join("a.txt");
        assert!(!blocked(&escaped, &roots));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn guard_works_before_the_data_dir_exists() {
        // 全新机器上数据目录还没建:两边都按字面折叠到同一个存在的祖先,照样命中。
        let (base, _, _) = guard_fixture("fresh");
        let data = base.join("not-created").join("com.mini-term.app");
        let roots = [data.clone()];
        assert!(blocked(&data.join("credential.key"), &roots));
        assert!(!blocked(
            &base.join("not-created").join("other.txt"),
            &roots
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn guard_checks_every_protected_root() {
        // 装机版目录 + MT_APP_DATA_DIR 指的 dev 目录:落在任一个里都拦。
        let (base, data, outside) = guard_fixture("roots");
        let dev = outside.join("dev-data");
        std::fs::create_dir_all(&dev).unwrap();
        let roots = [data.clone(), dev.clone()];
        assert!(blocked(&dev.join("config.db"), &roots));
        assert!(blocked(&data.join("config.db"), &roots));
        assert!(!blocked(&outside.join("x.txt"), &roots));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn guard_is_case_insensitive_on_windows() {
        let (base, data, _) = guard_fixture("case");
        let roots = [data.clone()];
        let upper = PathBuf::from(data.to_string_lossy().to_uppercase());
        assert!(blocked(&upper.join("CONFIG.DB"), &roots));
        assert!(blocked(&upper.join("NEW-FILE.BIN"), &roots));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn guard_follows_symlinks_into_the_data_dir() {
        let (base, data, outside) = guard_fixture("symlink");
        let roots = [data.clone()];
        let link = outside.join("innocent");
        std::os::unix::fs::symlink(&data, &link).unwrap();
        assert!(blocked(&link.join("credential.key"), &roots));
        assert!(blocked(&link.join("not-yet.bin"), &roots));
        let _ = std::fs::remove_dir_all(&base);
    }

    // --- format_transfer_audit_line ---------------------------------------

    #[test]
    fn format_transfer_audit_line_upload_basic() {
        let line = format_transfer_audit_line(
            "2026-06-09T12:00:00Z",
            TransferDirection::Upload,
            "prod",
            "/local/a.bin",
            "/remote/b.bin",
            Some(1234),
        );
        assert!(line.starts_with("2026-06-09T12:00:00Z\t"));
        assert!(line.contains("dir=upload"));
        assert!(line.contains("conn=prod"));
        assert!(line.contains("local=/local/a.bin"));
        assert!(line.contains("remote=/remote/b.bin"));
        assert!(line.contains("result=1234"));
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn format_transfer_audit_line_download_error_result() {
        let line =
            format_transfer_audit_line("t", TransferDirection::Download, "c", "/l", "/r", None);
        assert!(line.contains("dir=download"));
        assert!(line.contains("result=error"));
    }

    #[test]
    fn format_transfer_audit_line_sanitizes_newlines_and_tabs_to_one_line() {
        // 路径里的换行/制表被替成空格 —— 一次传输只占一行。
        let line = format_transfer_audit_line(
            "t",
            TransferDirection::Upload,
            "weird\nconn",
            "/a\tb\nc",
            "/r\rd",
            Some(0),
        );
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
    }

    // --- SftpTransferError: 分类 + 错误不含密码 ----------------------------

    #[test]
    fn sftp_transfer_error_classification() {
        assert!(SftpTransferError::Transport("x".into()).is_transport());
        assert!(!SftpTransferError::Sftp("x".into()).is_transport());
    }

    #[test]
    fn sftp_transfer_error_message_never_contains_password() {
        // 安全:错误文本只透传库错误(不携带凭据)。模拟一个含路径的业务错,
        // 验证它绝不会回带连接密码(密码从不进入这些错误的构造)。
        let err =
            SftpTransferError::Sftp("sftp open '/etc/shadow' failed: permission denied".into());
        let msg = err.to_string();
        assert!(!msg.to_lowercase().contains("password"));
        assert!(msg.contains("permission denied"));
    }

    // --- ServiceError 分类与文本 -------------------------------------------

    #[test]
    fn service_error_message_passthrough() {
        assert_eq!(
            ServiceError::InvalidParams("no such connection".into()).message(),
            "no such connection"
        );
        assert_eq!(
            ServiceError::Internal("reconnect failed: x".into()).to_string(),
            "reconnect failed: x"
        );
    }

    #[tokio::test]
    async fn exec_cancellation_wakes_current_and_future_waiters() {
        let cancellation = ExecCancellation::new();
        let waiting = cancellation.clone();
        let waiter = tokio::spawn(async move { waiting.cancelled().await });

        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("当前 waiter 应被唤醒")
            .expect("waiter task");

        tokio::time::timeout(Duration::from_millis(50), cancellation.cancelled())
            .await
            .expect("取消状态必须对未来 waiter 保持可见");
    }

    // --- 回调收集器:MCP 侧收集行为等价于旧 run_exec_on_session 返回值 -----

    #[test]
    fn callback_collector_accumulates_streams_separately() {
        // MCP 适配层用同一个闭包把 stdout/stderr 分别收集进两个缓冲。
        // 这里验证该收集模式:分片乱序到达也能按流归位、字节序保持。
        let mut stdout_buf: Vec<u8> = Vec::new();
        let mut stderr_buf: Vec<u8> = Vec::new();
        {
            let mut collect = |kind: StreamKind, data: &[u8]| match kind {
                StreamKind::Stdout => stdout_buf.extend_from_slice(data),
                StreamKind::Stderr => stderr_buf.extend_from_slice(data),
            };
            collect(StreamKind::Stdout, b"hel");
            collect(StreamKind::Stderr, b"warn:");
            collect(StreamKind::Stdout, b"lo\n");
            collect(StreamKind::Stderr, b" disk\n");
        }
        assert_eq!(stdout_buf, b"hello\n");
        assert_eq!(stderr_buf, b"warn: disk\n");
    }
}
