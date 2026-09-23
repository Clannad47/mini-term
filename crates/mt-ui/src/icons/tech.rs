//! 技术栈徽标(对照 `src/components/TechIcon.tsx`)。
//!
//! # 与原版的关系
//!
//! 原版用 devicon 的 `*-original.svg` 裸资产;GPUI 侧搬的是**同一批资产**——
//! 官方 logo 的那条 `d` 经 `tools/gen_tech_icons.mjs` 烘焙进 [`super::tech_art`],
//! 渲染仍是自绘(为什么不能让 gpui 去读 SVG,判据在 [`super::vector`] 的模块注释)。
//! 也就是说几何与配色都是官方的,不再是改造前那套「品牌色片 + 简化字形」的
//! 认色不认形的骨架。种类也从 12 扩到 51,覆盖主流语言 / 前端 / 后端 / 移动 / 基础设施。
//!
//! 本模块只剩 Element;形状表在生成物 [`super::tech_art`] 里,按技术栈的**落盘字符串**
//! 查。`ProjectKind` 枚举连同字符串、展示名、菜单分组(`TechCategory`)住在
//! `mt_project::project_kind` —— 那是会落盘的领域数据;mt-ui 不依赖 mt-project
//! (它带 git2 等重库),枚举 → 字符串的映射由宿主做。
//!
//! # 已知偏差
//!
//! - **渐变**:`paint_path` 一次只吃一个纯色,渐变按 stop 的 offset 做梯形积分取均值。
//!   Angular 与 Kotlin 官方就是「红 → 紫」的整条渐变,均值必然落在洋红,与官方观感
//!   有出入(试过 devicon 的 plain 变体,那两枚要么没有配色、要么就是同一个洋红,
//!   没有更好的选择);Node.js 会从立体渐变变成扁平纯绿。形状一律不受影响;
//! - **深色底不可读的一批**:devicon 里 Rust / Apple / Express / Remix / Django 这些
//!   官方就是纯黑,画在深色面板上等于隐形。生成器按 WCAG 对比度检出后,单色的整枚改成
//!   跟随主题色(浅色主题下会自动变回深色)、多色的逐笔保色相提亮。
//!
//! 跑 `node tools/verify_icons.mjs tech` 可以逐枚比对官方原图与烘焙结果。
//!
//! # 宿主接线(mt-app)
//!
//! 项目列表 / 文件树根节点:
//!
//! ```ignore
//! use mt_project::project_kind::ProjectKind;
//! use mt_ui::icons::TechIcon;
//! if let Some(kind) = ProjectKind::from_str(&project.kind) {
//!     row = row.child(TechIcon::new(kind.as_str()).size(px(14.0)));
//! }
//! ```
//!
//! 「手动指定项目类型」的菜单按 `mt_project::project_kind::TechCategory` 分二级子菜单
//! —— 五十多项平铺成一条长龙没法用。`ALL_PROJECT_KINDS`(同在 mt-project)
//! 已按分组聚拢,直接顺序扫一遍即可分段。

use gpui::{App, Hsla, IntoElement, Pixels, RenderOnce, Window, px};

use super::tech_art::shapes_of;
use super::vector::{Shape, VectorIcon};

/// 所有形状表(单测遍历用)。
#[cfg(test)]
pub(super) fn shape_tables() -> Vec<&'static [Shape]> {
    super::tech_art::TECH_ART_KINDS
        .iter()
        .map(|k| shapes_of(k).expect("TECH_ART_KINDS 里的每一项都有形状表"))
        .collect()
}

/// 技术栈徽标。
///
/// ```ignore
/// TechIcon::new(ProjectKind::Rust.as_str()).size(px(14.0))
/// ```
#[derive(IntoElement)]
pub struct TechIcon {
    shapes: &'static [Shape],
    size: Pixels,
    color: Option<Hsla>,
}

impl TechIcon {
    /// `kind` 是技术栈的**落盘字符串**(`mt_project::project_kind::ProjectKind::as_str`)。
    /// 默认 14px —— 与 `TechIcon.tsx` 的 `size = 14` 一致。
    ///
    /// 宿主手上的 `ProjectKind` 一律有图:形状表与枚举出自生成器的同一张 CATALOG,
    /// mt-app 另有单测逐项对账。万一传了表里没有的字符串,debug 构建直接断言
    /// (与 mt-i18n 的 `t()` 查不到 key 同一口径),release 画成空白。
    pub fn new(kind: &str) -> Self {
        let shapes = shapes_of(kind);
        debug_assert!(shapes.is_some(), "技术栈徽标表里没有 {kind:?}");
        Self {
            shapes: shapes.unwrap_or(&[]),
            size: px(14.0),
            color: None,
        }
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    /// 把整枚徽标压成一个颜色,盖掉官方配色(置灰、禁用态)。
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }
}

impl RenderOnce for TechIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let icon = VectorIcon::new(self.shapes, self.size);
        match self.color {
            Some(color) => icon.force_ink(color),
            None => icon,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tech_art::TECH_ART_KINDS;
    use super::*;

    /// 原「每种都有图有名有分组」的「有图」一半(名字与分组随枚举去了 mt-project)。
    /// 表里每一项都得真有形状;表外的字符串查不到。
    #[test]
    fn 每种都有图() {
        for k in TECH_ART_KINDS {
            assert!(shapes_of(k).is_some_and(|s| !s.is_empty()), "{k} 没有形状");
        }
        assert!(shapes_of("cobol").is_none());
    }
}
