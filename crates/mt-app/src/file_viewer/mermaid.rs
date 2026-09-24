//! Mermaid 图表(issue #80):图表文本 → SVG → 位图,挂在 gpui 的资源系统上后台
//! 渲染;页签关闭 / 重切分块时连缓存带图集纹理一起放掉。预览里的画法见
//! [`super::FileViewer::render_md_mermaid`]。

use std::future::Future;
use std::sync::Arc;

use gpui::{App, Window};
use gpui_component::ActiveTheme as _;

use crate::i18n::t;
use crate::ui;

// ─── mermaid 图表:后台渲染成位图,挂在 gpui 资源系统上 ─────────────

/// [`MermaidAsset`] 的键。配色进 key 是因为亮 / 暗主题切换后同一份图表要
/// 重画(底色、线色、字色全跟着主题走),而 gpui 的资源缓存只认 key。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct MermaidKey {
    pub(super) code: Arc<str>,
    dark: bool,
    /// 画布底色 `0xRRGGBB`:取文档页的底色,让图表与正文融为一体(mermaid 自带
    /// 的暗色底 #333 在本应用的暗色页面上是一块突兀的灰板)。取的是 `bg_base`
    /// 而不是页面实际刷的 `bg_document` —— 后者在背景图皮肤下带透明度,而图表
    /// 是一张不透明的位图,只能贴不透明的那个底。
    background: u32,
}

impl MermaidKey {
    pub(super) fn new(code: &Arc<str>, cx: &App) -> Self {
        Self {
            code: code.clone(),
            dark: cx.theme().mode.is_dark(),
            background: rgb_u32(ui::bg_base()),
        }
    }
}

/// `Hsla` → `0xRRGGBB`(alpha 丢掉:画布底色必须不透明,否则去预乘那步会把
/// 整张图的颜色算歪)。
fn rgb_u32(color: gpui::Hsla) -> u32 {
    let rgba = gpui::Rgba::from(color);
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (byte(rgba.r) << 16) | (byte(rgba.g) << 8) | byte(rgba.b)
}

/// 渲染失败的原因,画在退回的代码块下面(见 [`super::FileViewer::render_md_mermaid`])。
#[derive(Clone, Debug)]
pub(super) struct MermaidError(Arc<str>);

impl std::fmt::Display for MermaidError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Mermaid 图表 → 位图,挂在 gpui 的资源系统上(`window.use_asset`):同一 key
/// 只渲染一次、后台线程跑、完成后自动重画当前视图 —— 与本地图片那条路
/// (`ImageAssetLoader`)同一机制,预览里的图表因此也是「先占位、好了换图」。
///
/// 两步:`mermaid-rs-renderer` 把图表文本排成 SVG(纯 Rust,用系统字体量字宽),
/// 再交给 **gpui 自带的 [`gpui::SvgRenderer`]** 栅格化(`render_single_frame`)
/// —— 2× 栅格(`SMOOTH_SVG_SCALE_FACTOR`,150% / 200% 缩放下不糊)、去预乘 +
/// RGBA→BGRA、回填 `scale_factor` 全在它里面,与 gpui 画本地 svg 图片是同一条路。
/// 它的字体库也比自己搭的一份全:系统字体 + 打包字体 + `system-ui` 这类 CSS
/// 关键字回退 + **emoji 回退**(标签里的 emoji 不再丢字)。
/// **不能**走 `Image::from_bytes(ImageFormat::Svg)`:那条路 1× 栅格化且漏了
/// 通道交换(见 [`super::FileViewer::render_image`] 的注释)。
pub(super) enum MermaidAsset {}

impl gpui::Asset for MermaidAsset {
    type Source = MermaidKey;
    type Output = Result<Arc<gpui::RenderImage>, MermaidError>;

    // 不能写成 `async fn`:trait 要求返回的 future 是 `'static`,而 `async fn`
    // 会把 `cx` 的借用捕获进 future 里,过不了 'static 检查。
    //
    // 栅格器**在这里**取(`App::svg_renderer` 返回的是 clone,内部两个 Arc:
    // `dyn AssetSource`(`Send + Sync`)与 `usvg::Options<'static>`(两个解析
    // 闭包都是 `Send + Sync`)),因此满足 future 的 `Send + 'static`;上游自己
    // 的 `ImageAssetLoader::load`(`elements/img.rs:630`)就是这么写的。
    // 拿的是同一份 `Options`,那份懒建的字体库因此与画图标共用,不再各付一次
    // 加载系统字体的几十到几百毫秒。
    #[allow(clippy::manual_async_fn)]
    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move { render_mermaid_image(&source, &renderer) }
    }
}

/// 排版出来的画布不超过这个尺寸就当空图:解析器对不少残缺写法(括号没闭合、
/// 箭头写错)**不报错**,只排出一张 2×边距(8px)的空白画布 —— 这种要按失败
/// 处理,退回代码块让用户看得见原文,而不是一块什么都没有的空白。
const MERMAID_EMPTY_CANVAS: f32 = 16.0;

fn render_mermaid_image(
    key: &MermaidKey,
    renderer: &gpui::SvgRenderer,
) -> Result<Arc<gpui::RenderImage>, MermaidError> {
    let svg = render_mermaid_svg(key)?;
    // 倍率传 1.0:`render_parsed` 自己再乘一道 `SMOOTH_SVG_SCALE_FACTOR`,
    // 位图尺寸因此与本地 svg 图片那条路一致,[`image_display_width`] 按同一个
    // 常量除回去。
    renderer
        .render_single_frame(svg.as_bytes(), 1.0)
        .map_err(|err| MermaidError(err.to_string().into()))
}

/// Mermaid 文本 → SVG 字符串。走三段式管线而不是一把梭的 `render`,是为了
/// 在排版结果上判空(见 [`MERMAID_EMPTY_CANVAS`])与截住它自己的「语法错误」
/// 炸弹图(`DiagramData::Error`,mermaid.js 那张 bomb 图的复刻)—— 两种都退回
/// 代码块,比画一张看不出所以然的图强;解析与排版之间还垫了一层
/// [`crate::mermaid_compat`],把渲染器与 mermaid.js 不一致的几处掰回来。
fn render_mermaid_svg(key: &MermaidKey) -> Result<String, MermaidError> {
    use mermaid_rs_renderer::layout::DiagramData;
    use mermaid_rs_renderer::{
        LayoutConfig, Theme, compute_layout, parse_mermaid_strict, render_svg,
    };

    let mut theme = if key.dark {
        Theme::dark()
    } else {
        Theme::modern()
    };
    theme.background = format!("#{:06X}", key.background);
    let config = LayoutConfig::default();
    let mut parsed =
        parse_mermaid_strict(&key.code).map_err(|err| MermaidError(err.to_string().into()))?;
    let compat = crate::mermaid_compat::Compat::apply(&mut parsed.graph);
    let mut layout = compute_layout(&parsed.graph, &theme, &config);
    if let DiagramData::Error(error) = &layout.diagram {
        return Err(MermaidError(error.message.clone().into()));
    }
    if layout.width <= MERMAID_EMPTY_CANVAS && layout.height <= MERMAID_EMPTY_CANVAS {
        return Err(MermaidError(t("fileViewer", "mermaidEmptyDiagram").into()));
    }
    compat.restore(&mut layout);
    Ok(render_svg(&layout, &theme, &config))
}

/// 把这些图表从 gpui 的资源缓存与图集里放掉。渲染途中调用必须把当前窗口
/// 递进来:那一刻它被从 `App.windows` 里摘出去了,`App::drop_image` 只遍历
/// 得到**其它**窗口(`on_release` 不在任何窗口的更新里,传 `None` 即可)。
///
/// 只对要过的 key 调用(见 [`super::FileViewer::mermaid_requested`]):`fetch_asset`
/// 对没见过的 key 会**发起**一次渲染,放东西反倒先做了一遍活。
/// gpui-pre 的 `fetch_asset` 直接给出已完成的结果(还在渲染的返回 `None`),
/// 不再是 `(Task, bool)`。
pub(super) fn release_mermaid_assets(
    keys: &[MermaidKey],
    cx: &mut App,
    mut window: Option<&mut Window>,
) {
    for key in keys {
        if let Some(Ok(image)) = cx.fetch_asset::<MermaidAsset>(key) {
            cx.drop_image(image, window.as_deref_mut());
        }
        cx.remove_asset::<MermaidAsset>(key);
    }
}

#[cfg(test)]
#[path = "mermaid_tests.rs"]
mod tests;
