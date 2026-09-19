//! 全仓统一的 tooltip:比 gpui-component 默认款**字更小、要停得更久才弹**。
//!
//! # 字号:自绘气泡(仍然必要)
//!
//! gpui-component 的 `Tooltip::render` 把 `.text_sm()`(0.875rem)写在
//! `.refine_style(&self.style)` **之前** —— 0.6.2 起调用点确实能用 `.text_size()`
//! 盖过去,但那要求每个调用点都记得挂一次样式:全仓 50 多处,漏一处就花。
//! 于是这里把那段气泡样式整份抄过来改成 [`TOOLTIP_FONT_SIZE`],
//! **内容仍用它的 [`Text`]**,换行/富文本行为与上游逐字节一致。
//!
//! # 停留时长:交给上游的 `tooltip_show_delay`
//!
//! gpui-pre 0.3.5 把这个旋钮开成了公开面(`StatefulInteractiveElement::tooltip_show_delay`
//! / `Interactivity::tooltip_show_delay`)。生效路径是:写进
//! `Interactivity.tooltip_show_delay`(`Option<Duration>`)→ prepaint 里随
//! `register_tooltip_mouse_handlers` 传给鼠标处理器 → `None` 才回落上游的
//! `DEFAULT_TOOLTIP_SHOW_DELAY`(500ms)。计时本身是「鼠标进入排一个 timer,
//! 到点才 `show_tooltip` 建视图」,中途离开当场取消,期间移动**不重排**计时。
//!
//! 所以气泡自己不必再懂延迟:用 [`TooltipExt`] 的方法一次挂上两件事
//! (延迟 + 气泡闭包),别只挂裸的 `.tooltip()` —— 那是 500ms 档。
//!
//! 个别调用点就是要「快弹」(用量统计弹窗工具栏的纯图标按钮 —— 不弹提示
//! 就认不出是什么键),**故意**只挂裸 `.tooltip()` 回落到 500ms。
//! 默认仍是 [`SHOW_DELAY`],别顺手全仓铺开快弹。
//!
//! # 退役记档:曾经的「二段延迟」
//!
//! gpui 0.2.2 的 500ms 是 `elements/div.rs` 里的私有常量,当时只能在它后面
//! 再接一段:视图先渲染成空 div,等 700ms 的 `Task` 到点再画,靠
//! `cx.refresh_windows()` 唤醒重绘。两条代价随着换用上游 API 一并消失:
//!
//! - 每弹一次提示要**绕缓存全窗重绘一次**;
//! - 气泡锚点取的是 gpui 建视图那一刻(第 500ms)的鼠标位置,后面 700ms 里
//!   鼠标在同一元素内移动不会重新贴位。现在建视图发生在整个 [`SHOW_DELAY`]
//!   之后,`show_tooltip` 当场取 `window.mouse_position()`,锚点即最新位置。

use std::time::Duration;

use gpui::{
    AnyElement, AnyView, App, AppContext, Context, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, px, rems,
};
use gpui_component::{ActiveTheme, h_flex, text::Text};

/// 鼠标要在元素上停多久才弹提示。
///
/// **调这一个数就等于调全仓的停留时长**:它整个交给上游的
/// `tooltip_show_delay`,不再有「上游 500ms + 自己再等一段」的叠加。
/// 1200ms = 退役前那套二段延迟的总时长(gpui 的 500 + 自己的 700),手感不变。
pub const SHOW_DELAY: Duration = Duration::from_millis(1200);

/// 气泡字号。上游是 `text_sm`(0.875rem);这里降一档到 0.75rem。
///
/// 跟 `text_xs()` 同值,写成 `rems` 是为了「想再调时有个明确的旋钮」。
/// 用 rem 而非 px:随 `gpui_component` 主题的 `font_size`(= 窗口 rem 基准)
/// 缩放,与上游同一套相对关系。
const TOOLTIP_FONT_SIZE: f32 = 0.75;

/// 给可交互元素挂全仓统一手感的 tooltip:[`SHOW_DELAY`] + 本模块的小字气泡。
///
/// ```ignore
/// div().id("x").tip(t("app", "titleBar.settings"))          // 文本
/// div().id("y").tip_with(move |window, cx| ...)             // 自造气泡视图
/// ```
///
/// 两个方法都会把延迟挂在同一个 `Interactivity` 上,顺序无关。
/// 想要 500ms 快弹的调用点**不要**用这里的方法,直接挂裸 `.tooltip()`。
pub trait TooltipExt: StatefulInteractiveElement + Sized {
    /// 纯文本提示。
    fn tip(self, text: impl Into<SharedString>) -> Self {
        let text: SharedString = text.into();
        self.tip_with(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
    }

    /// 自造气泡视图(富文本 / 自定义元素走这条,见 [`Tooltip::element`])。
    fn tip_with(self, build: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.tooltip_show_delay(SHOW_DELAY).tooltip(build)
    }
}

impl<T: StatefulInteractiveElement> TooltipExt for T {}

enum TooltipContent {
    Text(Text),
    Element(Box<dyn Fn(&mut Window, &mut App) -> AnyElement>),
}

/// 一条 tooltip 气泡。用法与 `gpui_component::tooltip::Tooltip` 完全一致:
///
/// ```ignore
/// .tip_with(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
/// ```
///
/// 停留时长不归它管 —— 那是元素侧的 [`TooltipExt`] / `tooltip_show_delay`。
pub struct Tooltip {
    content: TooltipContent,
}

impl Tooltip {
    /// 纯文本气泡。
    pub fn new(text: impl Into<Text>) -> Self {
        Self {
            content: TooltipContent::Text(text.into()),
        }
    }

    /// 自定义元素气泡(用量面板的趋势图六行详情就是这条)。
    pub fn element<E, F>(builder: F) -> Self
    where
        E: IntoElement,
        F: Fn(&mut Window, &mut App) -> E + 'static,
    {
        Self {
            content: TooltipContent::Element(Box::new(move |window, cx| {
                builder(window, cx).into_any_element()
            })),
        }
    }

    /// 建成 `AnyView` 交给 gpui。上游到这一刻已经等满了延迟,直接画。
    pub fn build(self, _: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|_| self).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.content {
            TooltipContent::Text(text) => div().child(text.clone()),
            TooltipContent::Element(builder) => div().child(builder(window, cx)),
        };

        // 气泡本体。样式抄自 gpui-component 的 Tooltip,只改字号 ——
        // 外面那层 div 是上游留的:m_3 是相对鼠标的偏移,写在子级才生效。
        div()
            .child(
                h_flex()
                    .font_family(cx.theme().font_family.clone())
                    .m_3()
                    .bg(cx.theme().popover)
                    .text_color(cx.theme().popover_foreground)
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_md()
                    .rounded(px(6.))
                    .justify_between()
                    .py_0p5()
                    .px_2()
                    .text_size(rems(TOOLTIP_FONT_SIZE))
                    .gap_3()
                    .child(content),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 停留时长必须明显长于 gpui 自己的 `DEFAULT_TOOLTIP_SHOW_DELAY`(500ms),
    /// 否则挂这套扩展就没意义了(退化成上游默认手感)。
    #[test]
    fn 停留时长长于上游默认档() {
        assert!(SHOW_DELAY > Duration::from_millis(500));
    }

    /// 退役二段延迟时的换算:老口径是 gpui 的 500ms + 自己的 700ms。
    /// 手感不该因为换实现而变,这里把那个总数钉住。
    #[test]
    fn 停留时长沿用二段延迟时代的总时长() {
        assert_eq!(SHOW_DELAY, Duration::from_millis(1200));
    }

    /// 字号必须比上游的 0.875rem 小,否则这个模块就白抄了。
    #[test]
    fn 字号小于上游默认档() {
        assert!(TOOLTIP_FONT_SIZE < 0.875);
    }
}
