//! `~/.claude/settings.json` 的共用读改写助手。
//!
//! 本 crate 里有两个写者动这同一份用户全局配置:[`crate::hook_registry`] 写/摘
//! `hooks` 段,[`crate::ssh_registry`] 清理历史 MCP 白名单 `enabledMcpjsonServers`。
//! 此前两边各抄了一份 `claude_settings_path` 与读改写流程,容错口径还不一致
//! (空文件一边当 `{}`、一边报解析失败;顶层不是对象一边报错、一边会在
//! `settings["hooks"] = …` 处 panic)。现在统一走这里,口径取两者中更严的那份:
//!
//! - **原子写**:[`crate::util::atomic_write`](同目录临时文件 + rename),两边
//!   原本就都是它。写一半崩掉不会留下截断的 settings.json —— 那是用户的全局配置,
//!   还连着 Claude Code 自己与别的工具。
//! - **进程内串行**:整段读改写持 [`LOCK`]。两个写者都跑在 background executor
//!   的线程上(设置页「注册 hooks」与项目「启用 / 停用 SSH 工具」可以前后脚点),
//!   不串行就可能后写的一方拿着旧快照覆盖掉先写的一方刚加的条目。改造前两边都没有
//!   这把锁,这是合并后顺手补上的。**跨进程不设防**:Claude Code 自己也会改这个文件,
//!   那条竞争靠原子 rename 保证不出半截文件,丢更新的窗口只有毫秒级,与改造前一致。
//! - **读**:文件不在或内容全空白 → 当 `{}`;JSON 解析失败或顶层不是对象 → 报错且
//!   **不写** —— 绝不拿一份读不懂的配置去覆盖用户的文件。
//!
//! 只读的扫描(`hook_registry` 数已注册事件)不走这里、也不拿锁:原子 rename 保证
//! 读到的要么是旧文件要么是新文件,读失败按「没注册」处理。

use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde_json::Value;

/// 串行化本进程内对 settings.json 的读改写(见模块注释)。
static LOCK: Mutex<()> = Mutex::new(());

/// Claude Code 用户级配置文件:`~/.claude/settings.json`。
pub(crate) fn settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// 读改写一份 Claude settings.json:读出顶层对象 → `edit` 就地修改 → 原子写回。
///
/// 文件不在时从 `{}` 起并建好父目录;要「文件不在就什么都不做」的调用方自己先判
/// `path.exists()`。`edit` 返回 `Err` 时不落盘,错误原样透传。
pub(crate) fn update<T>(
    path: &Path,
    edit: impl FnOnce(&mut Value) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = LOCK.lock();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 .claude 目录失败: {}", e))?;
    }

    let mut settings = read_object(path)?;
    let out = edit(&mut settings)?;

    let json_str = serde_json::to_string_pretty(&settings)
        .map_err(|e| format!("序列化 Claude settings.json 失败: {}", e))?;
    crate::util::atomic_write(path, json_str.as_bytes())
        .map_err(|e| format!("写入 Claude settings.json 失败: {}", e))?;
    Ok(out)
}

/// 读出顶层对象。文件不在 / 全空白 → `{}`;读不懂 → `Err`(调用方据此不写)。
fn read_object(path: &Path) -> Result<Value, String> {
    let settings: Value = if path.exists() {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("读取 Claude settings.json 失败: {}", e))?;
        if content.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&content)
                .map_err(|e| format!("解析 Claude settings.json 失败: {}", e))?
        }
    } else {
        serde_json::json!({})
    };
    if !settings.is_object() {
        return Err("Claude settings.json 顶层不是 JSON 对象".to_string());
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例一个独立临时目录,不碰真实 home。
    fn unique_test_dir(label: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mt-claude-settings-test-{label}-{ts}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn 文件不在时从空对象起并建好父目录() {
        let dir = unique_test_dir("missing");
        let path = dir.join(".claude").join("settings.json");

        let got = update(&path, |s| {
            s["theme"] = serde_json::json!("dark");
            Ok(7)
        })
        .unwrap();

        assert_eq!(got, 7, "edit 的返回值原样透传");
        assert_eq!(read_json(&path), serde_json::json!({ "theme": "dark" }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 全空白文件当空对象() {
        let dir = unique_test_dir("blank");
        let path = dir.join("settings.json");
        std::fs::write(&path, " \n\t").unwrap();

        update(&path, |s| {
            s["k"] = serde_json::json!(1);
            Ok(())
        })
        .unwrap();

        assert_eq!(read_json(&path), serde_json::json!({ "k": 1 }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 读不懂的文件(坏 JSON / 顶层不是对象)报错,且一个字节都不改。
    #[test]
    fn 读不懂的文件报错且不落盘() {
        let dir = unique_test_dir("unreadable");
        let path = dir.join("settings.json");
        for (raw, needle) in [("[1, 2]", "顶层不是 JSON 对象"), ("{bad", "解析")] {
            std::fs::write(&path, raw).unwrap();
            let err = update(&path, |_| Ok(())).unwrap_err();
            assert!(err.contains(needle), "{raw:?} → {err}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 编辑闭包报错时不落盘() {
        let dir = unique_test_dir("edit-err");
        let path = dir.join("settings.json");
        let raw = "{\"hooks\": 3}";
        std::fs::write(&path, raw).unwrap();

        let err = update(&path, |s| {
            s["other"] = serde_json::json!(true);
            Err::<(), _>("hooks 字段不是对象".to_string())
        })
        .unwrap_err();

        assert_eq!(err, "hooks 字段不是对象");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 两个写者并发读改写同一个文件:串行化之后谁的改动都不丢。
    #[test]
    fn 并发读改写不丢更新() {
        let dir = unique_test_dir("concurrent");
        let path = dir.join("settings.json");

        std::thread::scope(|scope| {
            for i in 0..8 {
                let path = &path;
                scope.spawn(move || {
                    update(path, |s| {
                        s[format!("k{i}")] = serde_json::json!(i);
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });

        let settings = read_json(&path);
        for i in 0..8 {
            assert_eq!(settings[format!("k{i}")], i, "k{i} 被别的写者覆盖掉了");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
