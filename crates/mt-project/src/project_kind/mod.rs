//! 项目技术栈:[`ProjectKind`] 枚举(**会落盘**)与目录探测。
//!
//! # 为什么住在 mt-project
//!
//! `ProjectKind` 是领域数据 —— 用户手动指定的类型以 [`ProjectKind::as_str`] 的字符串
//! 存进配置的 `kindOverride`,自动探测([`detect_local`])读的是项目目录里的标记文件。
//! 此前枚举与徽标形状表一起生成在 mt-ui 的美术数据文件(`icons/tech_art.rs`)里,
//! 纯探测逻辑(当时在 `mt-app/src/project_kind.rs`)因此只能依赖 mt-ui。
//!
//! 现在的分工:
//!
//! | 位置 | 内容 |
//! |---|---|
//! | `catalog`(生成物) | 枚举、落盘字符串、展示名、菜单分组、[`ALL_PROJECT_KINDS`] |
//! | 本文件 | [`TechCategory`] 菜单分组(手写) |
//! | `detect`(自 mt-app 移入) | 标记文件表、[`classify_project`]、[`detect_local`] |
//! | mt-ui `icons::tech_art`(生成物) | 按落盘字符串查的徽标形状表 |
//!
//! mt-ui 不依赖本 crate(git2 等重库),徽标由宿主按字符串映射:
//! `mt_ui::icons::TechIcon::new(kind.as_str())`。枚举与形状表出自
//! `crates/mt-ui/tools/gen_tech_icons.mjs` 的同一张 CATALOG,mt-app 另有单测逐项对账。

mod catalog;
mod detect;

pub use catalog::{ALL_PROJECT_KINDS, ProjectKind};
pub use detect::{
    PROJECT_MARKER_FILES, ProjectProbe, classify_project, detect_local, is_marker_file,
    parse_package_deps,
};

/// 项目类型在「手动指定」菜单里的二级分组。
///
/// 分组本身要翻译,但**具体种类名不翻译**(Rust / React / Docker 都是专有名词)。
/// 文案 key 给宿主查 `mt-i18n`,本 crate 不依赖 i18n。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TechCategory {
    Language,
    Frontend,
    Backend,
    Mobile,
    Infra,
}

/// 菜单里的分组顺序。
pub const ALL_TECH_CATEGORIES: &[TechCategory] = &[
    TechCategory::Language,
    TechCategory::Frontend,
    TechCategory::Backend,
    TechCategory::Mobile,
    TechCategory::Infra,
];

impl TechCategory {
    /// `projectList` 命名空间下的文案 key。
    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::Language => "menu.kindCategory.language",
            Self::Frontend => "menu.kindCategory.frontend",
            Self::Backend => "menu.kindCategory.backend",
            Self::Mobile => "menu.kindCategory.mobile",
            Self::Infra => "menu.kindCategory.infra",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 存量配置里的十二个种类一个都不能少() {
        // 这些字符串**落在用户配置**里(项目的 kindOverride)。改一个字、少一项,
        // 存量配置就读不回来 —— 徽标会从用户设定的那个变回自动探测的结果
        for legacy in [
            "java", "rust", "go", "python", "nodejs", "react", "vuejs", "nextjs", "svelte", "vite",
            "flutter", "php",
        ] {
            assert!(
                ProjectKind::from_str(legacy).is_some(),
                "{legacy} 从表里消失了,存量配置会读不回来"
            );
        }
    }

    #[test]
    fn 字符串双向可逆且无重复() {
        let mut seen: Vec<&str> = Vec::new();
        for k in ALL_PROJECT_KINDS {
            let s = k.as_str();
            assert_eq!(ProjectKind::from_str(s), Some(*k), "{s} 转不回来");
            assert!(!seen.contains(&s), "{s} 重复了");
            seen.push(s);
        }
        assert_eq!(ProjectKind::from_str("cobol"), None);
    }

    /// 原「每种都有图有名有分组」的名字与分组两半;「有图」随形状表留在 mt-ui
    /// (`icons::tech` 的 `每种都有图`),两边的逐项对账在 mt-app。
    #[test]
    fn 每种都有名有分组() {
        for k in ALL_PROJECT_KINDS {
            assert!(!k.label().is_empty(), "{:?} 没有展示名", k);
            assert!(
                ALL_TECH_CATEGORIES.contains(&k.category()),
                "{:?} 的分组不在菜单顺序表里",
                k
            );
        }
    }

    #[test]
    fn 菜单顺序已按分组聚拢() {
        // 菜单是「一个分组一个子菜单」,顺序表要是交错的,分段就得先排序 ——
        // 生成器已经拢好了,这里钉住它
        let mut order: Vec<TechCategory> = Vec::new();
        for k in ALL_PROJECT_KINDS {
            let c = k.category();
            if order.last() != Some(&c) {
                assert!(!order.contains(&c), "{c:?} 分组被拆成了不连续的两段");
                order.push(c);
            }
        }
        assert_eq!(order, ALL_TECH_CATEGORIES, "分组顺序与菜单顺序表不一致");
    }

    #[test]
    fn 覆盖面够广() {
        // 这次改造的目的就是「支持市面上流行的开发语言」,种类数掉下来说明清单被误删
        assert!(
            ALL_PROJECT_KINDS.len() >= 50,
            "只剩 {} 种",
            ALL_PROJECT_KINDS.len()
        );
        for must in [
            "rust", "kotlin", "swift", "ruby", "csharp", "cpp", "django", "docker",
        ] {
            assert!(ProjectKind::from_str(must).is_some(), "缺了 {must}");
        }
    }

    /// 搬家护栏:枚举自 mt-ui 挪进本 crate,落盘形态(`kindOverride` 里的字符串)
    /// 必须**逐字**不变。这里把 51 个取值连同顺序整张钉死,再逐个 `as_str → from_str`
    /// 走一遍往返 —— 生成器改坏一个字、或者有人手改了生成物,这条先红。
    #[test]
    fn 落盘字符串逐字不变且能往返() {
        const PERSISTED: &[&str] = &[
            "rust",
            "go",
            "java",
            "kotlin",
            "python",
            "csharp",
            "cpp",
            "c",
            "swift",
            "ruby",
            "php",
            "dart",
            "elixir",
            "scala",
            "haskell",
            "zig",
            "lua",
            "perl",
            "react",
            "vuejs",
            "angular",
            "svelte",
            "nextjs",
            "nuxtjs",
            "astro",
            "solidjs",
            "remix",
            "vite",
            "nodejs",
            "django",
            "flask",
            "fastapi",
            "rails",
            "laravel",
            "spring",
            "dotnet",
            "nestjs",
            "express",
            "deno",
            "bun",
            "flutter",
            "android",
            "apple",
            "tauri",
            "electron",
            "unity",
            "godot",
            "docker",
            "kubernetes",
            "terraform",
            "ansible",
        ];
        let written: Vec<&str> = ALL_PROJECT_KINDS.iter().map(|k| k.as_str()).collect();
        assert_eq!(written, PERSISTED, "落盘字符串或顺序变了");
        for (kind, text) in ALL_PROJECT_KINDS.iter().zip(PERSISTED) {
            // 写出去是这个字符串,读回来还是同一个种类
            assert_eq!(kind.as_str(), *text);
            assert_eq!(ProjectKind::from_str(text), Some(*kind), "{text} 读不回来");
        }
        // 抽几个把「枚举变体 ↔ 字符串」钉到具体值,防整张表整体平移
        assert_eq!(ProjectKind::Vue.as_str(), "vuejs");
        assert_eq!(ProjectKind::Node.as_str(), "nodejs");
        assert_eq!(ProjectKind::Next.as_str(), "nextjs");
        assert_eq!(ProjectKind::Apple.as_str(), "apple");
    }
}
