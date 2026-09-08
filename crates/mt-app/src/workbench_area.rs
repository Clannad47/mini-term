//! 主内容工作区：常驻终端页 + 项目级文件页签。
//!
//! 文件页签是纯运行时状态，不进入 `SplitNode`、PTY 映射或布局数据库。切到文件页
//! 只是不渲染 [`TerminalArea`]，终端实体及其全部后台会话仍由 `AppStore` 保活。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use gpui::{
    App, AppContext, Context, Entity, Global, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::WindowExt as _;
use mt_ui::icons::FileIcon;
use mt_ui::tooltip::Tooltip;

use crate::file_viewer::{DocumentSource, FileViewer};
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::prompt::{Confirm, show_alert};
use crate::store::AppStore;
use crate::terminal_area::TerminalArea;
use crate::ui;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DocumentBackendKey {
    Local,
    Remote {
        connection_id: String,
        connection_fingerprint: u64,
    },
}

/// 一个打开文件的稳定身份。项目、后端连接身份和路径共同参与去重。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DocumentKey {
    project_id: String,
    backend: DocumentBackendKey,
    normalized_path: String,
}

impl DocumentKey {
    fn from_source(source: &DocumentSource) -> Self {
        let (backend, normalized_path) = match source {
            DocumentSource::Local { path, .. } => (
                DocumentBackendKey::Local,
                normalize_local_document_path(path),
            ),
            DocumentSource::Remote {
                connection, path, ..
            } => (
                DocumentBackendKey::Remote {
                    connection_id: connection.id.clone(),
                    connection_fingerprint: crate::remote_ssh::connection_fingerprint(connection),
                },
                normalize_remote_document_path(path),
            ),
        };
        Self {
            project_id: source.project_id().to_string(),
            backend,
            normalized_path,
        }
    }
}

fn normalize_local_document_path(path: &Path) -> String {
    let normalized = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        normalized.replace('\\', "/").to_lowercase()
    } else {
        normalized
    }
}

fn normalize_remote_document_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WorkbenchPage {
    Terminal,
    Document(DocumentKey),
}

struct DocumentTab {
    key: DocumentKey,
    title: String,
    document: Entity<FileViewer>,
}

type RenderDocumentTab = (DocumentKey, String, Entity<FileViewer>);
type ActiveWorkbenchSnapshot = (String, WorkbenchPage, Vec<RenderDocumentTab>);

struct ProjectDocuments {
    tabs: Vec<DocumentTab>,
    active: WorkbenchPage,
}

impl Default for ProjectDocuments {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active: WorkbenchPage::Terminal,
        }
    }
}

impl ProjectDocuments {
    fn index_of(&self, key: &DocumentKey) -> Option<usize> {
        self.tabs.iter().position(|tab| &tab.key == key)
    }

    fn close(&mut self, key: &DocumentKey) -> Option<Entity<FileViewer>> {
        let index = self.index_of(key)?;
        let removed = self.tabs.remove(index).document;
        if self.active == WorkbenchPage::Document(key.clone()) {
            let remaining = self
                .tabs
                .iter()
                .map(|tab| tab.key.clone())
                .collect::<Vec<_>>();
            self.active = next_document_after_close(&remaining, index)
                .map(WorkbenchPage::Document)
                .unwrap_or(WorkbenchPage::Terminal);
        }
        Some(removed)
    }
}

fn next_document_after_close(
    remaining: &[DocumentKey],
    removed_index: usize,
) -> Option<DocumentKey> {
    remaining
        .get(removed_index)
        .or_else(|| {
            removed_index
                .checked_sub(1)
                .and_then(|index| remaining.get(index))
        })
        .cloned()
}

/// 页签右键菜单里的批量关闭范围，锚点是被右键的那一页。
///
/// 三档都**只覆盖文档页签**：常驻的终端页不是文档，既不在 `tabs` 里，也就
/// 永远不会被「关闭其他」带走 —— 它是关不掉的（见 [`WorkbenchPage`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseScope {
    Others,
    ToRight,
    ToLeft,
}

/// 按范围挑出要关的页签（不含锚点自己）。顺序与页签条一致，便于逐个 `close`。
fn documents_in_scope(
    keys: &[DocumentKey],
    anchor_index: usize,
    scope: CloseScope,
) -> Vec<DocumentKey> {
    keys.iter()
        .enumerate()
        .filter(|(index, _)| match scope {
            CloseScope::Others => *index != anchor_index,
            CloseScope::ToRight => *index > anchor_index,
            CloseScope::ToLeft => *index < anchor_index,
        })
        .map(|(_, key)| key.clone())
        .collect()
}

struct GlobalWorkbench(Entity<WorkbenchArea>);
impl Global for GlobalWorkbench {}

/// 安装统一文件打开入口。文件树和全局搜索都只调用这里。
pub fn install(area: Entity<WorkbenchArea>, cx: &mut App) {
    cx.set_global(GlobalWorkbench(area));
}

fn global(cx: &App) -> Option<Entity<WorkbenchArea>> {
    cx.try_global::<GlobalWorkbench>()
        .map(|global| global.0.clone())
}

fn project_documents_are_dirty(project: &ProjectDocuments, cx: &App) -> bool {
    project
        .tabs
        .iter()
        .any(|tab| tab.document.read(cx).is_dirty())
}

/// 项目移除、worktree 清理等生命周期操作的统一防丢失闸。
pub fn project_has_dirty_documents(project_id: &str, cx: &App) -> bool {
    global(cx).is_some_and(|area| {
        area.read(cx)
            .projects
            .get(project_id)
            .is_some_and(|project| project_documents_are_dirty(project, cx))
    })
}

/// 关窗确认使用的未保存文档列表。项目名与页签名一起展示，避免不同项目中的
/// 同名文件让用户无法判断哪些草稿会被丢弃。
pub fn dirty_document_names(cx: &App) -> Vec<String> {
    let Some(area) = global(cx) else {
        return Vec::new();
    };
    let area = area.read(cx);
    let store = area.store.read(cx);
    let mut names = Vec::new();
    for (project_id, documents) in &area.projects {
        let project_name = store
            .project(project_id)
            .map(|project| project.name.as_str())
            .unwrap_or(project_id);
        for tab in &documents.tabs {
            if tab.document.read(cx).is_dirty() {
                names.push(format!("{project_name}: {}", tab.title));
            }
        }
    }
    names.sort();
    names
}

/// 按当前项目快照打开文件。远程项目会同时快照连接身份，断链时明确报错。
pub fn open_active_file(
    store: Entity<AppStore>,
    path: PathBuf,
    highlight_line: Option<u32>,
    window: &mut Window,
    cx: &mut App,
) {
    let snapshot = {
        let store = store.read(cx);
        let Some(project) = store.active_project() else {
            return;
        };
        (
            project.clone(),
            store.is_remote_project(&project.id),
            store.remote_connection_of(&project.id),
        )
    };
    let (project, remote, connection) = snapshot;
    let source = if remote {
        let Some(connection) = connection else {
            show_alert(
                t("terminalArea", "remoteConnectFailedTitle"),
                t("fileTree", "remote.broken"),
                window,
                cx,
            );
            return;
        };
        DocumentSource::Remote {
            project_id: project.id.clone(),
            connection,
            project_root: project.path.clone(),
            path,
        }
    } else {
        DocumentSource::Local {
            project_id: project.id.clone(),
            project_root: PathBuf::from(&project.path),
            path,
        }
    };

    let Some(area) = global(cx) else {
        return;
    };
    area.update(cx, |area, cx| {
        area.open_document(source, highlight_line, window, cx)
    });
}

/// 文件页内部的 Ctrl/Cmd+W 入口。延迟执行前先快照来源身份，避免用户在
/// `window.defer` 落地前切换页签后误关新的活动页。
pub fn close_document_source(source: DocumentSource, window: &mut Window, cx: &mut App) {
    let Some(area) = global(cx) else {
        return;
    };
    let project_id = source.project_id().to_string();
    let key = DocumentKey::from_source(&source);
    area.update(cx, |area, cx| {
        area.request_close_document(project_id, key, window, cx)
    });
}

/// Unified entry for existing navigation surfaces that explicitly jump to a
/// terminal pane (toast, session list, title bar, tray). Activating a hidden
/// pane without switching the workbench page would leave focus in an invisible
/// PTY while the document page remains on screen.
pub fn activate_terminal_page(window: &mut Window, cx: &mut App) {
    let Some(area) = global(cx) else {
        return;
    };
    area.update(cx, |area, cx| area.activate_terminal(window, cx));
}

/// 文档异步读盘完成时用来判断是否可以接管焦点。后台页签或其它项目的迟到结果
/// 只能更新自身内容，不能把键盘焦点从当前终端/文档页抢走。
pub fn is_document_active(source: &DocumentSource, cx: &App) -> bool {
    let Some(area) = global(cx) else {
        return false;
    };
    let key = DocumentKey::from_source(source);
    let area = area.read(cx);
    let Some(active_project_id) = area.store.read(cx).active_project_id.clone() else {
        return false;
    };
    active_project_id == key.project_id
        && area
            .projects
            .get(&active_project_id)
            .is_some_and(|project| project.active == WorkbenchPage::Document(key))
}

/// Restore focus after a modal overlay closes without capturing the document
/// that happened to be active before the close. The active project and page
/// are resolved at handoff time, and `FileViewer::on_activated` performs the
/// final identity/overlay checks again in the deferred callback.
pub fn reactivate_active_document(expected_project_id: &str, window: &mut Window, cx: &mut App) {
    let Some(area) = global(cx) else {
        return;
    };
    area.update(cx, |area, cx| {
        area.reactivate_active_document(expected_project_id, window, cx)
    });
}

/// 文件页签宿主。
pub struct WorkbenchArea {
    store: Entity<AppStore>,
    terminal_area: Entity<TerminalArea>,
    projects: HashMap<String, ProjectDocuments>,
    last_rendered_project: Option<String>,
    last_rendered_page: Option<WorkbenchPage>,
    /// 页签条的横向滚动位置。**全项目共用一份**：切项目时页签整条重建，
    /// 上一个项目的偏移留着也无意义，切完那一帧的 `scroll_to_item` 会把活动页
    /// 拉回视野。
    tab_scroll: ScrollHandle,
}

impl WorkbenchArea {
    pub fn new(
        store: Entity<AppStore>,
        terminal_area: Entity<TerminalArea>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&store, |this, store, cx| {
            let project_ids = store
                .read(cx)
                .projects()
                .iter()
                .map(|project| project.id.clone())
                .collect::<std::collections::HashSet<_>>();
            // 正常移除入口会在 AppStore 层拒绝丢弃脏页签；这里再留一道兜底，
            // 防止配置被其它路径直接改写时观察者静默销毁内存草稿。
            this.projects.retain(|project_id, project| {
                project_ids.contains(project_id) || project_documents_are_dirty(project, cx)
            });
            for project in this.projects.values() {
                for tab in &project.tabs {
                    tab.document
                        .update(cx, |document, cx| document.validate_remote_source(cx));
                }
            }
            cx.notify();
        })
        .detach();
        Self {
            store,
            terminal_area,
            projects: HashMap::new(),
            last_rendered_project: None,
            last_rendered_page: None,
            tab_scroll: ScrollHandle::new(),
        }
    }

    pub fn is_terminal_active(&self, cx: &App) -> bool {
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return true;
        };
        self.is_terminal_page_for_project(&project_id, cx)
    }

    fn is_terminal_page_for_project(&self, project_id: &str, cx: &App) -> bool {
        if self.store.read(cx).active_project_id.as_deref() != Some(project_id) {
            return false;
        }
        self.projects
            .get(project_id)
            .is_none_or(|project| project.active == WorkbenchPage::Terminal)
    }

    fn open_document(
        &mut self,
        source: DocumentSource,
        highlight_line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project_id = source.project_id().to_string();
        let key = DocumentKey::from_source(&source);
        let project = self.projects.entry(project_id).or_default();
        if let Some(index) = project.index_of(&key) {
            let document = project.tabs[index].document.clone();
            project.active = WorkbenchPage::Document(key);
            window.defer(cx, move |window, cx| {
                document.update(cx, |document, cx| {
                    document.reveal_line(highlight_line, window, cx);
                    document.on_activated(window, cx);
                });
            });
            cx.notify();
            return;
        }

        let title = source.file_name();
        let document = cx.new(|cx| FileViewer::new_document(source, highlight_line, window, cx));
        cx.observe(&document, |_this, _document, cx| cx.notify())
            .detach();
        project.tabs.push(DocumentTab {
            key: key.clone(),
            title,
            document: document.clone(),
        });
        project.active = WorkbenchPage::Document(key);
        window.defer(cx, move |window, cx| {
            document.update(cx, |document, cx| document.on_activated(window, cx));
        });
        cx.notify();
    }

    pub fn activate_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return;
        };
        self.projects.entry(project_id.clone()).or_default().active = WorkbenchPage::Terminal;
        let pane_id = self.store.read(cx).active_pane_id(&project_id);
        if let Some(pane_id) = pane_id {
            self.store.update(cx, |store, cx| {
                store.focus_pane(&project_id, &pane_id, window, cx)
            });
        }
        cx.notify();
    }

    fn activate_document(
        &mut self,
        project_id: &str,
        key: &DocumentKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(project_id) else {
            return;
        };
        let Some(index) = project.index_of(key) else {
            return;
        };
        let document = project.tabs[index].document.clone();
        project.active = WorkbenchPage::Document(key.clone());
        window.defer(cx, move |window, cx| {
            document.update(cx, |document, cx| {
                document.on_activated(window, cx);
            });
        });
        cx.notify();
    }

    fn reactivate_active_document(
        &mut self,
        expected_project_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.store.read(cx).active_project_id.as_deref() != Some(expected_project_id) {
            return;
        }
        let Some(WorkbenchPage::Document(key)) = self
            .projects
            .get(expected_project_id)
            .map(|project| project.active.clone())
        else {
            return;
        };
        self.activate_document(expected_project_id, &key, window, cx);
    }

    pub fn search_active_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return;
        };
        let Some(WorkbenchPage::Document(key)) = self
            .projects
            .get(&project_id)
            .map(|project| project.active.clone())
        else {
            return;
        };
        let Some(document) = self.projects.get(&project_id).and_then(|project| {
            project
                .index_of(&key)
                .map(|index| project.tabs[index].document.clone())
        }) else {
            return;
        };
        document.update(cx, |document, cx| document.open_search(window, cx));
    }

    fn request_close_document(
        &mut self,
        project_id: String,
        key: DocumentKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dirty = self
            .projects
            .get(&project_id)
            .and_then(|project| project.index_of(&key).map(|index| &project.tabs[index]))
            .is_some_and(|tab| tab.document.read(cx).is_dirty());
        if !dirty {
            self.close_document(&project_id, &key, window, cx);
            return;
        }

        let this = cx.entity();
        Confirm::new(
            t("fileViewer", "unsavedTitle"),
            t("fileViewer", "unsavedMessage"),
        )
        .open(
            move |window, cx| {
                let this = this.clone();
                let project_id = project_id.clone();
                let key = key.clone();
                window.defer(cx, move |window, cx| {
                    this.update(cx, |area, cx| {
                        area.close_document(&project_id, &key, window, cx)
                    });
                });
            },
            window,
            cx,
        );
    }

    fn close_document(
        &mut self,
        project_id: &str,
        key: &DocumentKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_documents(project_id, std::slice::from_ref(key), None, window, cx);
    }

    /// 一次关掉一批页签。`fallback` 是右键的那一页（批量关闭时必然留下），
    /// 活动页被这一批带走时直接落到它上面 —— 逐个 `close` 的左右邻居收敛
    /// 在批量语境下会把活动页停在一个马上又要被关掉的页签上。
    fn close_documents(
        &mut self,
        project_id: &str,
        keys: &[DocumentKey],
        fallback: Option<&DocumentKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(project_id) else {
            return;
        };
        let mut closed_active = false;
        for key in keys {
            closed_active |= project.active == WorkbenchPage::Document(key.clone());
            let _ = project.close(key);
        }
        if closed_active
            && let Some(anchor) = fallback
            && project.index_of(anchor).is_some()
        {
            project.active = WorkbenchPage::Document(anchor.clone());
        }
        // 批量关闭后剩下的页签会左移，滚动偏移原地不动就会停在一段空白上；
        // 把留下的锚点重新拉进视野（活动页没被动过时也一样成立）
        if let Some(anchor) = fallback
            && let Some(index) = project.index_of(anchor)
        {
            self.tab_scroll.scroll_to_item(index + 1);
        }
        let project_is_visible =
            self.store.read(cx).active_project_id.as_deref() == Some(project_id);
        if !closed_active || !project_is_visible {
            cx.notify();
            return;
        }
        match project.active.clone() {
            WorkbenchPage::Terminal => self.activate_terminal(window, cx),
            WorkbenchPage::Document(next) => self.activate_document(project_id, &next, window, cx),
        }
        cx.notify();
    }

    /// 右键菜单的三档批量关闭要关掉哪些页签。菜单同时用它判断该不该置灰。
    fn scope_targets(
        &self,
        project_id: &str,
        anchor: &DocumentKey,
        scope: CloseScope,
    ) -> Vec<DocumentKey> {
        let Some(project) = self.projects.get(project_id) else {
            return Vec::new();
        };
        let Some(anchor_index) = project.index_of(anchor) else {
            return Vec::new();
        };
        let keys = project
            .tabs
            .iter()
            .map(|tab| tab.key.clone())
            .collect::<Vec<_>>();
        documents_in_scope(&keys, anchor_index, scope)
    }

    /// 批量关闭入口。脏页签只弹**一次**确认（列出文件名），取消则一个都不关。
    fn request_close_scope(
        &mut self,
        project_id: String,
        anchor: DocumentKey,
        scope: CloseScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self.scope_targets(&project_id, &anchor, scope);
        if targets.is_empty() {
            return;
        }
        let dirty = self
            .projects
            .get(&project_id)
            .map(|project| {
                project
                    .tabs
                    .iter()
                    .filter(|tab| targets.contains(&tab.key) && tab.document.read(cx).is_dirty())
                    .map(|tab| tab.title.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if dirty.is_empty() {
            self.close_documents(&project_id, &targets, Some(&anchor), window, cx);
            return;
        }

        let this = cx.entity();
        Confirm::new(
            t("fileViewer", "unsavedTitle"),
            tr!("fileViewer", "unsavedBatchMessage", count = dirty.len()),
        )
        .detail(dirty_preview(&dirty))
        .open(
            move |window, cx| {
                let this = this.clone();
                let project_id = project_id.clone();
                let anchor = anchor.clone();
                let targets = targets.clone();
                window.defer(cx, move |window, cx| {
                    this.update(cx, |area, cx| {
                        area.close_documents(&project_id, &targets, Some(&anchor), window, cx)
                    });
                });
            },
            window,
            cx,
        );
    }

    /// 文档页签的右键菜单。
    ///
    /// 终端页**没有**这份菜单，也不出现在任何一档的关闭范围里 —— 它是常驻页，
    /// 关不掉（[`CloseScope`]）。
    fn document_tab_menu(
        &self,
        project_id: &str,
        key: &DocumentKey,
        cx: &Context<Self>,
    ) -> Vec<MenuEntry> {
        let area = cx.entity();
        let close = {
            let (area, project_id, key) = (area.clone(), project_id.to_string(), key.clone());
            MenuItem::new(t("fileViewer", "closeTab"))
                // 键位见 file_viewer.rs 的 on_key_down(Ctrl/Cmd+W)
                .shortcut(menu::hotkey_label(true, false, false, "W"))
                .on_click(move |window, cx| {
                    let (project_id, key) = (project_id.clone(), key.clone());
                    area.update(cx, |area, cx| {
                        area.request_close_document(project_id, key, window, cx)
                    });
                })
        };
        let mut entries = vec![close.into(), menu::separator()];
        for (scope, label) in [
            (CloseScope::Others, t("fileViewer", "closeOthers")),
            (CloseScope::ToRight, t("fileViewer", "closeToRight")),
            (CloseScope::ToLeft, t("fileViewer", "closeToLeft")),
        ] {
            // 没得可关就置灰(而不是抽掉那一项):菜单条目固定,肌肉记忆才稳
            let empty = self.scope_targets(project_id, key, scope).is_empty();
            let (area, project_id, key) = (area.clone(), project_id.to_string(), key.clone());
            entries.push(
                MenuItem::new(label)
                    .disabled(empty)
                    .on_click(move |window, cx| {
                        let (project_id, key) = (project_id.clone(), key.clone());
                        area.update(cx, |area, cx| {
                            area.request_close_scope(project_id, key, scope, window, cx)
                        });
                    })
                    .into(),
            );
        }
        entries
    }

    fn active_snapshot(&self, cx: &App) -> Option<ActiveWorkbenchSnapshot> {
        let project_id = self.store.read(cx).active_project_id.clone()?;
        let project = self.projects.get(&project_id);
        let active = project
            .map(|project| project.active.clone())
            .unwrap_or(WorkbenchPage::Terminal);
        let tabs = project
            .map(|project| {
                project
                    .tabs
                    .iter()
                    .map(|tab| (tab.key.clone(), tab.title.clone(), tab.document.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Some((project_id, active, tabs))
    }
}

impl Render for WorkbenchArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((project_id, active, tabs)) = self.active_snapshot(cx) else {
            self.last_rendered_project = None;
            self.last_rendered_page = None;
            return div().size_full().child(self.terminal_area.clone());
        };

        let page_changed = self.last_rendered_project.as_deref() != Some(project_id.as_str())
            || self.last_rendered_page.as_ref() != Some(&active);
        if page_changed {
            self.last_rendered_project = Some(project_id.clone());
            self.last_rendered_page = Some(active.clone());
            match &active {
                WorkbenchPage::Terminal => {
                    let area = cx.entity();
                    let store = self.store.clone();
                    let project_id = project_id.clone();
                    window.defer(cx, move |window, cx| {
                        if !area.read(cx).is_terminal_page_for_project(&project_id, cx)
                            || window.has_active_dialog(cx)
                            || !crate::overlay::allows(crate::overlay::Yield::ToOverlay)
                        {
                            return;
                        }
                        let pane_id = store.read(cx).active_pane_id(&project_id);
                        if let Some(pane_id) = pane_id {
                            store.update(cx, |store, cx| {
                                store.focus_pane(&project_id, &pane_id, window, cx)
                            });
                        }
                    });
                }
                WorkbenchPage::Document(key) => {
                    if let Some((_, _, document)) =
                        tabs.iter().find(|(candidate, _, _)| candidate == key)
                    {
                        let document = document.clone();
                        window.defer(cx, move |window, cx| {
                            document.update(cx, |document, cx| {
                                document.on_activated(window, cx);
                            });
                        });
                    }
                }
            }
            // 页签多到溢出时，刚激活的那一页可能整个在视野之外(尤其是新打开的
            // 文件排在最右)。终端页固定是第 0 个子元素，文档页顺次排在它后面。
            self.tab_scroll.scroll_to_item(match &active {
                WorkbenchPage::Terminal => 0,
                WorkbenchPage::Document(key) => tabs
                    .iter()
                    .position(|(candidate, _, _)| candidate == key)
                    .map_or(0, |index| index + 1),
            });
        }

        // 尚未打开文件时保持原终端区的尺寸与结构，一旦有文档才出现工作区页签条。
        if tabs.is_empty() {
            return div().size_full().child(self.terminal_area.clone());
        }

        let terminal_active = active == WorkbenchPage::Terminal;
        // 页签条横向滚动(与终端 tab 栏同一套):页签**不压缩**,溢出即可横向滚。
        //
        // 垂直滚轮不必自己映射 —— gpui 只在 `overflow.x == Scroll && overflow.y
        // != Scroll` 且 `restrict_scroll_to_axis == false`(默认)时把 `delta.y`
        // 记到 x 上(gpui-0.2.2 `elements/div.rs`)。`track_scroll` 是为了能主动
        // 把活动页拉进视野。
        let mut tab_bar = div()
            .id("workbench-tabs")
            .h(px(34.0))
            .flex_none()
            .flex()
            .items_center()
            .overflow_x_scroll()
            .track_scroll(&self.tab_scroll)
            .bg(ui::bg_elevated())
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(
                div()
                    .id("workbench-tab-terminal")
                    .h_full()
                    .flex_none()
                    .min_w(px(110.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_size(ui::font_px(12.0))
                    .when(terminal_active, |el| {
                        el.bg(ui::bg_terminal())
                            .text_color(ui::text_primary())
                            .border_t_2()
                            .border_color(ui::accent())
                    })
                    .when(!terminal_active, |el| {
                        el.text_color(ui::text_muted())
                            .border_t_2()
                            .border_color(gpui::Hsla {
                                a: 0.0,
                                ..ui::accent()
                            })
                    })
                    .child(t("terminalArea", "terminal"))
                    .on_click(
                        cx.listener(|this, _event, window, cx| this.activate_terminal(window, cx)),
                    ),
            );

        for (key, title, document) in &tabs {
            let selected = active == WorkbenchPage::Document(key.clone());
            let dirty = document.read(cx).is_dirty();
            let tab_key = key.clone();
            let close_key = key.clone();
            let menu_key = key.clone();
            let click_project = project_id.clone();
            let close_project = project_id.clone();
            let menu_project = project_id.clone();
            tab_bar = tab_bar.child(
                div()
                    .id(SharedString::from(format!(
                        "workbench-tab-{:016x}",
                        stable_hash(key)
                    )))
                    .h_full()
                    .flex_none()
                    .min_w(px(120.0))
                    .max_w(px(220.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(12.0))
                    .when(selected, |el| {
                        // 与文档页容器同色:背景图皮肤下随内容区一起半透明,
                        // 不透明的 bg_base 会在半透明页签条上凸成一块实色
                        el.bg(ui::bg_document())
                            .text_color(ui::text_primary())
                            .border_t_2()
                            .border_color(ui::accent())
                    })
                    .when(!selected, |el| {
                        el.text_color(ui::text_muted())
                            .border_t_2()
                            .border_color(gpui::Hsla {
                                a: 0.0,
                                ..ui::accent()
                            })
                    })
                    .child(FileIcon::new(title, false, false).size(px(14.0)))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .child(title.clone()),
                    )
                    .when(dirty, |el| {
                        el.child(
                            div()
                                .id(SharedString::from(format!(
                                    "workbench-tab-dirty-{:016x}",
                                    stable_hash(key)
                                )))
                                .w(px(6.0))
                                .h(px(6.0))
                                .flex_none()
                                .rounded_full()
                                .bg(ui::accent())
                                .tooltip(|window, cx| {
                                    Tooltip::new(t("fileViewer", "unsaved")).build(window, cx)
                                }),
                        )
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "workbench-tab-close-{:016x}",
                                stable_hash(key)
                            )))
                            .w(px(18.0))
                            .h(px(18.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.0))
                            .text_color(ui::text_muted())
                            .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
                            .child("×")
                            .on_click(cx.listener(move |this, _event, window, cx| {
                                cx.stop_propagation();
                                this.request_close_document(
                                    close_project.clone(),
                                    close_key.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.activate_document(&click_project, &tab_key, window, cx)
                    }))
                    // 页签右键菜单:关闭 / 关闭其他 / 关闭右边 / 关闭左边。
                    // 常驻的终端页没有这份菜单 —— 它关不掉
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            let entries = this.document_tab_menu(&menu_project, &menu_key, cx);
                            menu::show(event.position, entries, window, cx);
                        }),
                    ),
            );
        }

        let body = match &active {
            WorkbenchPage::Terminal => self.terminal_area.clone().into_any_element(),
            WorkbenchPage::Document(key) => tabs
                .iter()
                .find(|(candidate, _, _)| candidate == key)
                .map(|(_, _, document)| document.clone().into_any_element())
                .unwrap_or_else(|| self.terminal_area.clone().into_any_element()),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(tab_bar)
            .child(div().flex_1().min_h(px(0.0)).overflow_hidden().child(body))
    }
}

/// 批量关闭确认框里列出的未保存文件名。与关窗确认同一套口径：只列前几条，
/// 其余折成一行「另有 N 项」，免得一次关几十个页签时确认框长到出屏。
const DIRTY_PREVIEW_LIMIT: usize = 5;

fn dirty_preview(names: &[String]) -> Vec<String> {
    let mut lines = names
        .iter()
        .take(DIRTY_PREVIEW_LIMIT)
        .cloned()
        .collect::<Vec<_>>();
    let remaining = names.len().saturating_sub(lines.len());
    if remaining > 0 {
        lines.push(tr!("app", "closeConfirm.remaining", count = remaining));
    }
    lines
}

fn stable_hash(key: &DocumentKey) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(path: &str) -> DocumentKey {
        DocumentKey {
            project_id: "p".into(),
            backend: DocumentBackendKey::Local,
            normalized_path: path.into(),
        }
    }

    #[test]
    fn document_key_separates_remote_connection_identity() {
        let a = DocumentKey {
            project_id: "p".into(),
            backend: DocumentBackendKey::Remote {
                connection_id: "ssh".into(),
                connection_fingerprint: 1,
            },
            normalized_path: "/work/a.rs".into(),
        };
        let mut b = a.clone();
        b.backend = DocumentBackendKey::Remote {
            connection_id: "ssh".into(),
            connection_fingerprint: 2,
        };
        assert_ne!(a, b);
    }

    #[cfg(windows)]
    #[test]
    fn normalized_paths_deduplicate_separator_variants_on_windows() {
        let normalized = normalize_local_document_path(Path::new("C:\\Work\\src\\main.rs"));
        assert!(!normalized.contains('\\'));
    }

    #[cfg(not(windows))]
    #[test]
    fn local_posix_paths_preserve_backslash_file_names() {
        assert_ne!(
            normalize_local_document_path(Path::new("/work/a\\b.rs")),
            normalize_local_document_path(Path::new("/work/a/b.rs"))
        );
    }

    #[test]
    fn remote_paths_remain_case_sensitive_on_windows_hosts() {
        assert_ne!(
            normalize_remote_document_path(Path::new("/work/A.rs")),
            normalize_remote_document_path(Path::new("/work/a.rs"))
        );
        assert_ne!(
            normalize_remote_document_path(Path::new("/work/a\\b.rs")),
            normalize_remote_document_path(Path::new("/work/a/b.rs"))
        );
    }

    #[test]
    fn close_scope_never_includes_the_anchor_itself() {
        let keys = [key("a"), key("b"), key("c")];
        for scope in [CloseScope::Others, CloseScope::ToRight, CloseScope::ToLeft] {
            assert!(
                !documents_in_scope(&keys, 1, scope).contains(&key("b")),
                "{scope:?} 不该关掉被右键的那一页"
            );
        }
    }

    #[test]
    fn close_scope_splits_by_position() {
        let keys = [key("a"), key("b"), key("c")];
        assert_eq!(
            documents_in_scope(&keys, 1, CloseScope::Others),
            vec![key("a"), key("c")]
        );
        assert_eq!(
            documents_in_scope(&keys, 1, CloseScope::ToRight),
            vec![key("c")]
        );
        assert_eq!(
            documents_in_scope(&keys, 1, CloseScope::ToLeft),
            vec![key("a")]
        );
    }

    /// 两端的页签各有一档「没得可关」——菜单据此置灰，而不是抽掉那一项。
    #[test]
    fn close_scope_is_empty_at_the_edges() {
        let keys = [key("a"), key("b")];
        assert!(documents_in_scope(&keys, 0, CloseScope::ToLeft).is_empty());
        assert!(documents_in_scope(&keys, 1, CloseScope::ToRight).is_empty());
        assert!(documents_in_scope(&[key("a")], 0, CloseScope::Others).is_empty());
    }

    #[test]
    fn dirty_preview_folds_the_long_tail() {
        let names = (0..8).map(|i| format!("f{i}.rs")).collect::<Vec<_>>();
        let lines = dirty_preview(&names);
        assert_eq!(lines.len(), DIRTY_PREVIEW_LIMIT + 1);
        assert_eq!(lines[..DIRTY_PREVIEW_LIMIT], names[..DIRTY_PREVIEW_LIMIT]);
        assert!(lines[DIRTY_PREVIEW_LIMIT].contains('3'), "剩余 3 项要算准");
        assert_eq!(dirty_preview(&names[..2]), names[..2].to_vec());
    }

    #[test]
    fn closing_active_document_prefers_right_neighbor_then_left() {
        assert_eq!(
            next_document_after_close(&[key("a"), key("c")], 1),
            Some(key("c"))
        );
        assert_eq!(
            next_document_after_close(&[key("a"), key("b")], 2),
            Some(key("b"))
        );
        assert_eq!(next_document_after_close(&[], 0), None);
    }
}
