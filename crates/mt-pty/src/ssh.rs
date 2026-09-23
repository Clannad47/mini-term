//! SSH 相关的 PTY 侧逻辑:密码自动填充状态机 + 远程启动器 argv 拼装。
//!
//! 为什么归本 crate:两件事都只跟「往 PTY 里写什么字节 / 用什么 argv 起子进程」
//! 有关,不需要知道连接是怎么配置的。**查连接、解密码、准备私钥临时副本**属于
//! 上层(配置层),本模块只接收已经取到的明文密码与主机参数。

use std::time::{Duration, Instant};

use mt_core::{SshPromptScan, scan_ssh_prompt, strip_ansi_codes};

/// 跨缓冲块匹配密码提示时保留的输出尾部长度(字符)。
const RESIDUAL_KEEP: usize = 256;

/// 每块 PTY 输出只取尾部这么多字节去 strip(见 [`stripped_tail`])。
///
/// reader 一次最多读 64KB,而判定只看 residual 末尾 [`RESIDUAL_KEEP`] 个字符 ——
/// 刷屏时整块 strip + 拼接 + 数字符纯属浪费。
const FEED_TAIL_BYTES: usize = 4 * 1024;

/// 自注册起的有效期。过期后即便看到密码提示也不再填充,只能用户手输。
///
/// 兜的是「注册之后迟迟没有用户输入」这一种情况:公钥 / agent 先认证成功、
/// 用户又一直没敲键盘时,解除逻辑([`SshAutofill::disarm_on_input`])还没机会
/// 生效,autofill 不能因此无限期待命。SSH 的密码提示在连接建立的头几秒内出现,
/// 60 秒足够覆盖慢链路。
pub const AUTOFILL_TTL: Duration = Duration::from_secs(60);

/// OpenSSH 首连未知主机时确认提示里不变的那一段(按小写比较)。旧版提示是
/// `(yes/no)? `,7.6 起是 `(yes/no/[fingerprint])? `,两种都以它开头。
const HOST_KEY_CONFIRM_MARK: &str = "continue connecting (yes/no";

/// 确认提示的 `?` 之后、同一行上最多允许的字符数 —— 那是用户正在输入的回答的
/// 回显:`yes` / `no`,或 `SHA256:` 加 43 位 base64 的指纹,再加点退格之类的杂音。
const HOST_KEY_ANSWER_MAX: usize = 96;

/// 一次注册里最多认几轮主机密钥确认:ProxyJump 时跳板机与目标机各问一次。
const HOST_KEY_CONFIRMS_MAX: u8 = 2;

/// 一个 PTY 会话的 SSH 密码自动填充状态。
///
/// 生命周期:`new` 注册 → 每段 PTY 输出喂给 [`feed`](Self::feed) → 命中密码提示
/// 回写一次密码后自解除(`done`);命中 "Permission denied, please try again."
/// 则永久禁用,避免连灌错误密码把账号锁掉;自注册起超过 [`AUTOFILL_TTL`] 同样
/// 永久禁用。三种收场都会丢掉手里的明文密码。
pub struct SshAutofill {
    password: String,
    /// 累加的输出尾部,用于跨缓冲块匹配密码提示
    residual: String,
    /// 已填充、已禁用(命中错误密码)或已过期后置位,后续输出不再处理
    done: bool,
    /// 用户首次向 PTY 真实输入时是否解除本 autofill。两条生产路径都传 `true`:
    /// - 远程项目 pane:直接 spawn ssh,注册之后不再写任何命令,首个 write 即用户输入;
    /// - 「SSH 连接」菜单:先把 `ssh ...\r` 写进 PTY **再**注册
    ///   ([`PtySession::write_then_arm_ssh_autofill`](crate::PtySession::write_then_arm_ssh_autofill)),
    ///   那条命令写在注册之前,解除不到它。
    ///
    /// 用户一打字即解除:公钥 / agent 先认证成功时全程没有 SSH 密码提示,不解除
    /// 就会一直待命,把 SSH 密码灌进之后 sudo / su / mysql -p / 再 ssh 跳板乃至远端
    /// 恶意打印的任何 "password:" 提示里。
    ///
    /// `false` 时只剩「命中提示 / 认证失败 / 过期」三条自解除路径(生产代码已不用)。
    ///
    /// 例外见 `awaiting_host_key_confirm`:首连回答主机密钥确认的那一行不解除。
    disarm_on_input: bool,
    /// 输出正停在 OpenSSH 首连的主机密钥确认提示上(判定见 [`host_key_confirm_pending`])。
    ///
    /// 这时用户敲的 `yes` / 指纹是在回答 ssh,解除了首连就总得手输密码(旧的
    /// `false` 语义下这条路是通的)。状态**只由输出决定、每次 feed 重算**:尾巴是
    /// 确认提示(后面至多跟着同一行上回答的回显)才成立,回车后的换行或任何别的
    /// 输出一到就失效,此后用户再输入照常解除。
    awaiting_host_key_confirm: bool,
    /// 本次注册里进入过几轮确认状态,封顶 [`HOST_KEY_CONFIRMS_MAX`] —— 远端哪怕
    /// 反复伪造这行提示,也只能让用户的一两行输入不解除,60 秒有效期照常兜底。
    host_key_confirms: u8,
    /// 注册时刻,有效期从这里起算(见 [`AUTOFILL_TTL`])。
    armed_at: Instant,
}

impl SshAutofill {
    pub fn new(password: String, disarm_on_input: bool) -> Self {
        Self::armed_at(password, disarm_on_input, Instant::now())
    }

    /// 指定注册时刻构造 —— 有效期判定的时间源注入口,单测用它免去真等 60 秒。
    fn armed_at(password: String, disarm_on_input: bool, armed_at: Instant) -> Self {
        Self {
            password,
            residual: String::new(),
            done: false,
            disarm_on_input,
            awaiting_host_key_confirm: false,
            host_key_confirms: 0,
            armed_at,
        }
    }

    /// 用户**此刻**的真实输入是否应当解除本 autofill:注册时要求了(见
    /// `disarm_on_input` 字段),且眼下不是在回答主机密钥确认(见
    /// `awaiting_host_key_confirm` 字段)。
    pub fn disarm_on_input(&self) -> bool {
        self.disarm_on_input && !self.awaiting_host_key_confirm
    }

    /// 已完成(填过密码、命中认证失败或已过期)。
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// 喂一段 PTY 输出(原始字节,不要求是完整的 UTF-8)。命中密码提示时返回
    /// **应回写到 PTY 的密码**(不含回车,调用方负责补 `\r`),每个会话只返回一次。
    pub fn feed(&mut self, data: impl AsRef<[u8]>) -> Option<String> {
        self.feed_at(data.as_ref(), Instant::now())
    }

    /// [`feed`](Self::feed) 的可测核心:`now` 由调用方给定。
    fn feed_at(&mut self, data: &[u8], now: Instant) -> Option<String> {
        if self.done {
            return None;
        }
        if now.saturating_duration_since(self.armed_at) >= AUTOFILL_TTL {
            self.finish();
            return None;
        }
        let (tail, truncated) = stripped_tail(data);
        if truncated {
            // 块的前段被丢掉了:旧 residual 与本块尾巴之间已有断层,不能再拼接
            // (否则可能把两段拼成一个本不存在的提示)。本块尾巴本身已比
            // RESIDUAL_KEEP 长得多,整块处理时旧 residual 也会被截掉,不丢判定信息。
            self.residual = tail;
        } else {
            self.residual.push_str(&tail);
        }
        // 仅保留尾部,解决提示被分块切断的情况;按 char 边界截断
        keep_last_chars(&mut self.residual, RESIDUAL_KEEP);
        self.track_host_key_confirm();
        match scan_ssh_prompt(&self.residual) {
            SshPromptScan::AuthFailed => {
                self.finish();
                None
            }
            SshPromptScan::Password => {
                let password = std::mem::take(&mut self.password);
                self.finish();
                Some(password)
            }
            SshPromptScan::None => None,
        }
    }

    /// 按刚更新过的输出尾巴重算「正在等主机密钥确认」(见 `awaiting_host_key_confirm`)。
    fn track_host_key_confirm(&mut self) {
        if !host_key_confirm_pending(&self.residual) {
            self.awaiting_host_key_confirm = false;
        } else if !self.awaiting_host_key_confirm && self.host_key_confirms < HOST_KEY_CONFIRMS_MAX
        {
            self.host_key_confirms += 1;
            self.awaiting_host_key_confirm = true;
        }
    }

    /// 收场:置 `done` 并丢掉明文密码与输出尾巴。
    fn finish(&mut self) {
        self.done = true;
        self.awaiting_host_key_confirm = false;
        self.password = String::new();
        self.residual = String::new();
    }
}

/// 输出尾巴是否停在 OpenSSH 的主机密钥确认提示上:最后一处
/// [`HOST_KEY_CONFIRM_MARK`] 之后,提示本身在同一行里以 `?` 收尾,`?` 之后只剩
/// 同一行上不超过 [`HOST_KEY_ANSWER_MAX`] 个字符(用户回答的回显)。
///
/// 回答后按回车,回显的换行一到即不再成立;提示没到齐(还没有 `?`)也不成立。
fn host_key_confirm_pending(residual: &str) -> bool {
    // 只转 ASCII 大小写,字节下标与原串一致
    let lower = residual.to_ascii_lowercase();
    let Some(at) = lower.rfind(HOST_KEY_CONFIRM_MARK) else {
        return false;
    };
    let rest = &lower[at + HOST_KEY_CONFIRM_MARK.len()..];
    // 提示剩下的只有 `)?` 或 `/[fingerprint])?` 这么一小段
    let Some(question) = rest.find('?') else {
        return false;
    };
    if question > 24 || rest[..question].contains('\n') {
        return false;
    }
    let answer = &rest[question + 1..];
    !answer.contains('\n') && answer.chars().count() <= HOST_KEY_ANSWER_MAX
}

/// 把一块 PTY 输出剥成判定够用的纯文本尾巴,并返回是否丢弃了块的前段。
///
/// 先按 [`FEED_TAIL_BYTES`] 取尾部(起点挪到 UTF-8 字符开头)再 strip;剥完不足
/// `2 × RESIDUAL_KEEP` 个字符(尾部几乎全是转义序列)就把窗口放大 4 倍重来,
/// 直到够长或覆盖整块。留出一倍余量是因为窗口起点可能切在一个转义序列中间,
/// 残下的参数字符会以可见文本出现在最前面 —— 转义序列的参数段远短于这份余量,
/// 落不进末尾 `RESIDUAL_KEEP` 的判定窗口,判定结果与整块处理一致。
fn stripped_tail(data: &[u8]) -> (String, bool) {
    let mut window = FEED_TAIL_BYTES;
    loop {
        let start = tail_start(data, window);
        let text = strip_ansi_codes(&String::from_utf8_lossy(&data[start..]));
        if start == 0 || text.chars().count() >= RESIDUAL_KEEP * 2 {
            return (text, start > 0);
        }
        window = window.saturating_mul(4);
    }
}

/// 取末尾约 `window` 字节时的起点下标:落在多字节字符中间(续字节 `10xxxxxx`)
/// 就往后挪到下一个字符开头,最多挪 3 字节。
fn tail_start(data: &[u8], window: usize) -> usize {
    let mut start = data.len().saturating_sub(window);
    while start < data.len() && start > 0 && data[start] & 0xC0 == 0x80 {
        start += 1;
    }
    start
}

/// 只保留 `text` 的最后 `keep` 个字符(按 char 边界,从尾部数,不整串计数)。
fn keep_last_chars(text: &mut String, keep: usize) {
    if keep == 0 {
        text.clear();
        return;
    }
    if let Some((cut, _)) = text.char_indices().rev().nth(keep - 1) {
        text.drain(..cut);
    }
}

// ---------------------------------------------------------------------------
// 远程启动器 argv(把 ssh 本身当成 PTY 的子进程起起来)
// ---------------------------------------------------------------------------

/// POSIX shell 单引号安全包裹:`'` → `'\''`。
/// 远程路径来自用户输入,拼进 `cd <path>` 前必须做引号安全处理,
/// 防止 `;`、`$()`、空格等在远程 shell 里被解释。
pub fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 拼 ssh 的远端命令:`cd '<path>' 2>/dev/null; exec $SHELL -l`。
/// `$SHELL` 保持字面量 —— 本地不经过 shell(portable-pty 直接 spawn ssh,
/// 参数按 argv 传递),它由远程 sshd 用登录 shell 执行时才展开,
/// 从而落在用户自己的默认 shell 上。路径失效时忽略 `cd` 错误并从登录目录启动。
pub fn build_remote_login_command(remote_path: &str) -> String {
    format!(
        "cd {} 2>/dev/null; exec $SHELL -l",
        shell_single_quote(remote_path)
    )
}

/// 拼直接 spawn `ssh` 作 PTY 子进程的参数列表(不经本地 shell,
/// 对齐 WSL 分支 spawn wsl.exe 的启动器重写模式)。
///
/// 形如:`-t [-p <port>] [-i <identity>] user@host "cd '<path>' 2>/dev/null; exec $SHELL -l"`。
/// **绝不能加 `-o BatchMode=yes`**:它会连带禁用密码认证,
/// 而密码连接依赖 PTY autofill 灌密码([`SshAutofill`])。
pub fn build_ssh_launcher_args(
    host: &str,
    port: u16,
    user: &str,
    identity: Option<&str>,
    remote_path: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-t".to_string()];
    if port != 0 && port != 22 {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    if let Some(key) = identity {
        args.push("-i".to_string());
        args.push(key.to_string());
    }
    args.push(format!("{user}@{host}"));
    args.push(build_remote_login_command(remote_path));
    args
}

/// 在 PATH 里找可执行文件(本机 OpenSSH 客户端探测用)。
fn find_in_path(program: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// 定位本机 ssh 客户端。Windows 10+ 自带 OpenSSH 客户端(System32\\OpenSSH),
/// 缺失时返回 None 由调用方给出明确安装提示。
pub fn find_ssh_client() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        find_in_path("ssh.exe")
    } else {
        find_in_path("ssh")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // === 密码自动填充状态机 ===

    #[test]
    fn autofill_fills_once_on_password_prompt() {
        let mut autofill = SshAutofill::new("secret".into(), true);
        assert_eq!(
            autofill.feed("root@10.0.0.5's password: ").as_deref(),
            Some("secret")
        );
        // 每个会话只填一次:后续再出现提示不再回写
        assert!(autofill.is_done());
        assert!(autofill.feed("root@10.0.0.5's password: ").is_none());
    }

    #[test]
    fn autofill_matches_prompt_split_across_chunks() {
        // 提示被 PTY 读缓冲切成两段,靠 residual 拼回来。
        let mut autofill = SshAutofill::new("secret".into(), false);
        assert!(autofill.feed("root@host's pass").is_none());
        assert_eq!(autofill.feed("word: ").as_deref(), Some("secret"));
    }

    #[test]
    fn autofill_strips_ansi_before_matching() {
        let mut autofill = SshAutofill::new("secret".into(), false);
        assert_eq!(
            autofill
                .feed("\x1b[1;32mroot@host's password: \x1b[0m")
                .as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn autofill_disabled_after_permission_denied() {
        // 认证失败后永久禁用:不能对着下一个提示继续灌错误密码。
        let mut autofill = SshAutofill::new("wrong".into(), false);
        assert!(
            autofill
                .feed("Permission denied, please try again.\r\nroot@host's password: ")
                .is_none()
        );
        assert!(autofill.is_done());
        assert!(autofill.feed("root@host's password: ").is_none());
    }

    #[test]
    fn autofill_ignores_hostkey_and_passphrase_prompts() {
        let mut autofill = SshAutofill::new("secret".into(), false);
        assert!(
            autofill
                .feed("Are you sure you want to continue connecting (yes/no/[fingerprint])? ")
                .is_none()
        );
        assert!(
            autofill
                .feed("Enter passphrase for key '/home/u/.ssh/id_rsa': ")
                .is_none()
        );
    }

    #[test]
    fn autofill_residual_is_bounded() {
        // 长时间刷屏不会把 residual 撑成常驻内存;尾部保留量足够跨块匹配提示。
        let mut autofill = SshAutofill::new("secret".into(), false);
        for _ in 0..50 {
            assert!(autofill.feed("x".repeat(1024)).is_none());
        }
        assert!(autofill.residual.chars().count() <= RESIDUAL_KEEP);
        // 截断后仍能正常命中提示
        assert_eq!(
            autofill.feed("root@host's password: ").as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn autofill_disarm_flag_is_reported_verbatim() {
        // 两条生产路径都传 true(用户一打字即解除);false 只剩自解除语义。
        assert!(SshAutofill::new("s".into(), true).disarm_on_input());
        assert!(!SshAutofill::new("s".into(), false).disarm_on_input());
    }

    // === 首连主机密钥确认 ===

    const HOST_KEY_PROMPT: &str = "The authenticity of host 'h (10.0.0.5)' can't be established.\r\n\
         ED25519 key fingerprint is SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG.\r\n\
         This key is not known by any other names.\r\n\
         Are you sure you want to continue connecting (yes/no/[fingerprint])? ";

    #[test]
    fn host_key_confirm_answer_keeps_autofill_then_fills_password() {
        let mut autofill = SshAutofill::new("secret".into(), true);
        assert!(autofill.disarm_on_input(), "刚注册时用户输入照常解除");
        assert!(autofill.feed(HOST_KEY_PROMPT).is_none());
        assert!(
            !autofill.disarm_on_input(),
            "停在确认提示上:敲 yes 不该解除"
        );
        // 用户逐字敲 yes,回显落在同一行上,状态保持
        assert!(autofill.feed("y").is_none());
        assert!(autofill.feed("es").is_none());
        assert!(!autofill.disarm_on_input());
        // 回车的回显换行一到,回到正常语义
        assert!(autofill.feed("\r\n").is_none());
        assert!(autofill.disarm_on_input(), "确认之后再有输入照常解除");
        assert_eq!(
            autofill
                .feed(
                    "Warning: Permanently added 'h' (ED25519) to the list of known hosts.\r\n\
                     root@h's password: "
                )
                .as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn host_key_confirm_matches_old_wording_and_fingerprint_answer() {
        // OpenSSH 7.6 之前没有 `/[fingerprint]`;回答也可以是整串指纹
        let mut autofill = SshAutofill::new("secret".into(), true);
        autofill.feed("Are you sure you want to continue connecting (yes/no)? ");
        assert!(!autofill.disarm_on_input());
        autofill.feed("SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG");
        assert!(!autofill.disarm_on_input(), "指纹回显仍在同一行上");
    }

    #[test]
    fn host_key_confirm_state_ends_on_other_output() {
        let mut autofill = SshAutofill::new("secret".into(), true);
        autofill.feed(HOST_KEY_PROMPT);
        assert!(!autofill.disarm_on_input());
        // 提示之后在同一行上出现一大段别的输出:不再当成回答的回显
        autofill.feed("x".repeat(HOST_KEY_ANSWER_MAX + 1));
        assert!(autofill.disarm_on_input());
    }

    #[test]
    fn host_key_confirm_requires_the_prompt_at_the_tail() {
        // 提示只出现在中间(之后已换行出了别的内容)→ 不成立
        let mut autofill = SshAutofill::new("secret".into(), true);
        autofill.feed(format!("{HOST_KEY_PROMPT}yes\r\nlast login ...\r\n$ "));
        assert!(autofill.disarm_on_input());
        // 提示还没到齐(没有 `?`)也不成立
        let mut partial = SshAutofill::new("secret".into(), true);
        partial.feed("Are you sure you want to continue connecting (yes/no/[finger");
        assert!(partial.disarm_on_input());
    }

    #[test]
    fn host_key_confirm_state_is_capped_per_registration() {
        // 远端反复伪造确认提示:只认前两轮(跳板机 + 目标机),第三轮起输入照常解除。
        let mut autofill = SshAutofill::new("secret".into(), true);
        for round in 1..=HOST_KEY_CONFIRMS_MAX {
            autofill.feed(HOST_KEY_PROMPT);
            assert!(!autofill.disarm_on_input(), "第 {round} 轮应当认");
            autofill.feed("yes\r\n");
            assert!(autofill.disarm_on_input());
        }
        autofill.feed(HOST_KEY_PROMPT);
        assert!(autofill.disarm_on_input(), "超过封顶后不再豁免");
    }

    // === 有效期 ===

    #[test]
    fn autofill_expires_after_ttl() {
        // 注册后 60 秒没等到提示(公钥先认证成功、用户又没敲键盘),之后出现的
        // "password:" 一律不填,并且永久禁用、丢掉明文密码。
        let t0 = Instant::now();
        let mut autofill = SshAutofill::armed_at("secret".into(), true, t0);
        assert!(
            autofill
                .feed_at(b"[sudo] password for root: ", t0 + AUTOFILL_TTL)
                .is_none()
        );
        assert!(autofill.is_done());
        assert!(autofill.password.is_empty(), "过期后不应再持有明文密码");
        // 哪怕时间源倒回有效期内,已禁用的也不再复活
        assert!(autofill.feed_at(b"root@host's password: ", t0).is_none());
    }

    #[test]
    fn autofill_fills_right_before_ttl() {
        let t0 = Instant::now();
        let mut autofill = SshAutofill::armed_at("secret".into(), false, t0);
        let almost = t0 + AUTOFILL_TTL - Duration::from_millis(1);
        assert!(autofill.feed_at(b"Last login: Mon\r\n", almost).is_none());
        assert_eq!(
            autofill
                .feed_at(b"root@host's password: ", almost)
                .as_deref(),
            Some("secret")
        );
        assert!(autofill.password.is_empty(), "填完不应再持有明文密码");
    }

    // === 大块输出只处理尾部 ===

    #[test]
    fn autofill_matches_prompt_at_end_of_large_chunk() {
        // reader 一次最多读 64KB:提示在一大块输出的末尾时仍要命中。
        let mut autofill = SshAutofill::new("secret".into(), true);
        let chunk = format!("{}root@host's password: ", "motd line\r\n".repeat(6000));
        assert!(chunk.len() > FEED_TAIL_BYTES * 4);
        assert_eq!(autofill.feed(&chunk).as_deref(), Some("secret"));
    }

    #[test]
    fn autofill_matches_prompt_after_escape_heavy_chunk() {
        // 尾部窗口里几乎全是转义序列(剥完不够长)时窗口要放大,不能漏判。
        let mut autofill = SshAutofill::new("secret".into(), true);
        let chunk = format!("{}root@host's password: ", "\x1b[0m\x1b[K".repeat(8000));
        assert_eq!(autofill.feed(&chunk).as_deref(), Some("secret"));
    }

    #[test]
    fn autofill_auth_failure_survives_escape_heavy_gap() {
        // "Permission denied" 与新提示之间隔着 8KB 转义序列:放大窗口后仍要看到
        // 认证失败并停手,而不是对着新提示再灌一次错误密码。
        let mut autofill = SshAutofill::new("wrong".into(), true);
        let chunk = format!(
            "Permission denied, please try again.\r\n{}root@host's password: ",
            "\x1b[0m".repeat(2000)
        );
        assert!(autofill.feed(&chunk).is_none());
        assert!(autofill.is_done());
    }

    #[test]
    fn autofill_matches_prompt_split_after_large_chunk() {
        // 大块被截尾处理后,跨块匹配照常:前一块结尾的半截提示 + 下一块的后半截。
        let mut autofill = SshAutofill::new("secret".into(), true);
        let head = format!("{}root@host's pass", "x".repeat(20 * 1024));
        assert!(autofill.feed(&head).is_none());
        assert_eq!(autofill.feed("word: ").as_deref(), Some("secret"));
    }

    #[test]
    fn autofill_does_not_splice_across_dropped_middle() {
        // 大块的前段被丢弃后,不能再与之前的 residual 拼接成一个不存在的提示。
        // (全是转义序列的大块会被整块处理 —— 那种情况下两段之间确实没有可见字符,
        // 拼起来就是真提示,见 autofill_matches_prompt_after_escape_heavy_chunk。)
        let mut autofill = SshAutofill::new("secret".into(), true);
        assert!(autofill.feed("root@host's pass").is_none());
        let chunk = format!("{}word: ", "x".repeat(20 * 1024));
        assert!(autofill.feed(&chunk).is_none());
        assert!(!autofill.is_done());
    }

    #[test]
    fn stripped_tail_cuts_on_char_boundary() {
        // 窗口起点落在多字节字符中间时要挪到字符开头,不产生替换字符。
        let data = format!("{}尾巴", "中".repeat(3000));
        let (text, truncated) = stripped_tail(data.as_bytes());
        assert!(truncated);
        assert!(!text.contains('\u{FFFD}'), "切出了半个字符");
        assert!(text.ends_with("尾巴"));
    }

    #[test]
    fn keep_last_chars_counts_from_the_end() {
        let mut text = "ab中文cd".to_string();
        keep_last_chars(&mut text, 4);
        assert_eq!(text, "中文cd");
        keep_last_chars(&mut text, 10);
        assert_eq!(text, "中文cd");
    }

    // === 远程启动器 argv ===

    #[test]
    fn shell_single_quote_wraps_plain_path() {
        assert_eq!(shell_single_quote("/home/u/proj"), "'/home/u/proj'");
    }

    #[test]
    fn shell_single_quote_escapes_embedded_single_quotes() {
        // it's → 'it'\''s':单引号闭合 + 转义字面量 + 重新开引号
        assert_eq!(shell_single_quote("/a/it's"), r"'/a/it'\''s'");
    }

    #[test]
    fn shell_single_quote_neutralizes_shell_metacharacters() {
        // `;`、`$()`、空格等在单引号内均为字面量,不会被远程 shell 解释
        let quoted = shell_single_quote("/tmp/x; rm -rf $HOME `id`");
        assert_eq!(quoted, "'/tmp/x; rm -rf $HOME `id`'");
    }

    #[test]
    fn build_remote_login_command_quotes_path_and_keeps_shell_literal() {
        let cmd = build_remote_login_command("/home/u/my proj");
        assert_eq!(cmd, "cd '/home/u/my proj' 2>/dev/null; exec $SHELL -l");
        // $SHELL 必须保持字面量,由远程登录 shell 展开
        assert!(cmd.contains("$SHELL"));
    }

    #[test]
    fn build_ssh_launcher_args_default_port_no_identity() {
        let args = build_ssh_launcher_args("h.example.com", 22, "root", None, "/srv/app");
        assert_eq!(
            args,
            vec![
                "-t".to_string(),
                "root@h.example.com".to_string(),
                "cd '/srv/app' 2>/dev/null; exec $SHELL -l".to_string(),
            ]
        );
    }

    #[test]
    fn build_ssh_launcher_args_port_zero_treated_as_default() {
        let args = build_ssh_launcher_args("h", 0, "u", None, "/p");
        assert!(!args.contains(&"-p".to_string()));
    }

    #[test]
    fn build_ssh_launcher_args_custom_port_and_identity() {
        let args = build_ssh_launcher_args(
            "10.0.0.5",
            2222,
            "deploy",
            Some(r"C:\Temp\mini-term-ssh-keys\abc.key"),
            "/home/deploy",
        );
        assert_eq!(
            args,
            vec![
                "-t".to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "-i".to_string(),
                r"C:\Temp\mini-term-ssh-keys\abc.key".to_string(),
                "deploy@10.0.0.5".to_string(),
                "cd '/home/deploy' 2>/dev/null; exec $SHELL -l".to_string(),
            ]
        );
    }

    #[test]
    fn build_ssh_launcher_args_never_uses_batchmode() {
        // BatchMode=yes 会连带禁用密码认证,破坏 PTY autofill 灌密码链路。
        // 任何组合下都不允许出现。
        for (port, identity) in [(22u16, None), (2222, Some("/k")), (0, None)] {
            let args = build_ssh_launcher_args("h", port, "u", identity, "/p");
            assert!(
                !args.iter().any(|a| a.contains("BatchMode")),
                "args 不得包含 BatchMode: {args:?}"
            );
        }
    }

    #[test]
    fn build_ssh_launcher_args_hostile_remote_path_is_contained() {
        // 恶意路径整体落在单引号内,`;` 与 `$()` 不会成为独立命令
        let args = build_ssh_launcher_args("h", 22, "u", None, "/tmp'; rm -rf /; echo '");
        let remote_cmd = args.last().unwrap();
        assert_eq!(
            remote_cmd,
            r"cd '/tmp'\''; rm -rf /; echo '\''' 2>/dev/null; exec $SHELL -l"
        );
    }
}
