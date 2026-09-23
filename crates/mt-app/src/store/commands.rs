//! 命令库(issue #81)的 `AppStore` 方法:命令与分组的 CRUD + 「跑一条」。
//!
//! 分组的口径与 SSH 连接那套完全一致(`store/ssh.rs`):**组名即键**,命令上的
//! `group` 字段是归属的单一来源,`groups` 列表只补充「还没有命令的空分组」。
//! 所有改动**立即落盘**(`save_config_now`)—— 命令库改动低频且每一条都是
//! 用户手敲的,不值得为它进 500ms 防抖窗口去赌一次崩溃。

use gpui::Context;
use mt_config::{CommandLibrary, SavedCommand};

use crate::command_library::{merge_groups_on_rename, normalize_group, run_payload};

use super::events::StoreChanged;
use super::{AppStore, ConfigSection, StoreEvent};

impl AppStore {
    /// 整个命令库(只读)。
    pub fn command_library(&self) -> &CommandLibrary {
        &self.config.command_library
    }

    /// 新增或更新一条命令(按 id 判定)。名称 / 命令两头 trim,分组归一化
    /// (空串 → `None`);trim 后任一为空则**拒收**,返回 `false` —— 表单层
    /// 已经拦过一道,这里是最后一道闸,防的是程序化调用。
    pub fn upsert_saved_command(&mut self, cmd: SavedCommand, cx: &mut Context<Self>) -> bool {
        let name = cmd.name.trim().to_string();
        let command = cmd.command.trim().to_string();
        if name.is_empty() || command.is_empty() {
            return false;
        }
        let next = SavedCommand {
            id: cmd.id,
            name,
            command,
            group: normalize_group(cmd.group.as_deref()).map(str::to_string),
        };
        let lib = &mut self.config.command_library;
        match lib.commands.iter_mut().find(|c| c.id == next.id) {
            Some(slot) => *slot = next,
            None => lib.commands.push(next),
        }
        self.save_config_now();
        cx.changed(StoreEvent::Config(ConfigSection::CommandLibrary));
        true
    }

    /// 删除一条命令。所属分组**不动**(空组照样留着,与 SSH 同款)。
    pub fn remove_saved_command(&mut self, id: &str, cx: &mut Context<Self>) {
        let lib = &mut self.config.command_library;
        let before = lib.commands.len();
        lib.commands.retain(|c| c.id != id);
        if lib.commands.len() == before {
            return;
        }
        self.save_config_now();
        cx.changed(StoreEvent::Config(ConfigSection::CommandLibrary));
    }

    /// 新建一个空分组。重名(显式列表里有 / 已有命令归在该组下)返回 `false`。
    pub fn create_command_group(&mut self, name: &str, cx: &mut Context<Self>) -> bool {
        let Some(name) = normalize_group(Some(name)) else {
            return false;
        };
        let lib = &self.config.command_library;
        let exists = lib.groups.iter().any(|g| g.trim() == name)
            || lib
                .commands
                .iter()
                .any(|c| normalize_group(c.group.as_deref()) == Some(name));
        if exists {
            return false;
        }
        self.config.command_library.groups.push(name.to_string());
        self.save_config_now();
        cx.changed(StoreEvent::Config(ConfigSection::CommandLibrary));
        true
    }

    /// 分组改名:命令归属逐条改 + 显式列表同步替换(重命名成已有组名时自然合并)。
    pub fn rename_command_group(&mut self, old_name: &str, new_name: &str, cx: &mut Context<Self>) {
        let Some(next) = normalize_group(Some(new_name)) else {
            return;
        };
        if next == old_name {
            return;
        }
        let lib = &mut self.config.command_library;
        lib.groups = merge_groups_on_rename(&lib.groups, old_name, next);
        for c in &mut lib.commands {
            if normalize_group(c.group.as_deref()) == Some(old_name) {
                c.group = Some(next.to_string());
            }
        }
        self.save_config_now();
        cx.changed(StoreEvent::Config(ConfigSection::CommandLibrary));
    }

    /// 解散分组:组里的命令回落「未分组」,组名从显式列表移除(命令不删)。
    pub fn dissolve_command_group(&mut self, name: &str, cx: &mut Context<Self>) {
        let lib = &mut self.config.command_library;
        lib.groups.retain(|g| g.trim() != name);
        for c in &mut lib.commands {
            if normalize_group(c.group.as_deref()) == Some(name) {
                c.group = None;
            }
        }
        self.save_config_now();
        cx.changed(StoreEvent::Config(ConfigSection::CommandLibrary));
    }

    /// 把一条命令写进某个 pane。`newline = true` 连回车一起(= 运行),
    /// `false` 只把命令敲进去、光标停在行尾等用户改参数(Ctrl+↵)。
    ///
    /// 走 [`write_to_pane`](Self::write_to_pane) 而不是裸 PTY 写 —— 与用户自己
    /// 敲这条命令同一条链路,AI 输入检测那一路看得见它。返回是否真写进去了
    /// (pane 没 pty / 已关时 `false`,调用方据此决定要不要收浮层)。
    pub fn run_saved_command(
        &mut self,
        project_id: &str,
        pane_id: &str,
        command: &str,
        newline: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let payload = run_payload(command, newline);
        if payload.is_empty() {
            return false;
        }
        self.write_to_pane(project_id, pane_id, &payload, cx)
    }
}
