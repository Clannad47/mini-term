//! 单行省略截断的文本元素([`TruncatedText`])。
//!
//! # 为什么不能用 `div().truncate()`
//!
//! gpui 的 `truncate()` = `overflow_hidden + whitespace_nowrap + text_ellipsis`,
//! 截断发生在文本的**测量闭包**里,而这个闭包有一个按 `wrap_width` 命中的缓存
//! (`gpui/src/elements/text.rs` `TextLayout::layout`):
//!
//! - nowrap 时 `wrap_width` 恒为 `None`,缓存条件 `wrap_width.is_none()` 永远为真,
//!   于是**第一次测量的结果一锤定音**。flex 项的第一次测量是 MaxContent(算
//!   flex-basis),量出整段全宽;收缩后再按定宽量一次直接吃缓存,`truncate_line`
//!   根本没跑 —— 结果是只裁剪、没有「…」(分支名尾巴上剩半个字母那种)。
//! - 不 nowrap、改 `line_clamp(1)` 也不行:taffy 多趟布局里有一趟按 `flex_1`
//!   容器的基准宽 **0** 去量子树(算 hypothetical cross size),`truncate_line(…, 0)`
//!   得到的「…」被缓存,后面 MaxContent 那趟又因为 `wrap_width.is_none()` 命中
//!   它,元素就只剩一个「…」或干脆尺寸为 0。
//!
//! 只有 `flex_1`(basis 0,第一次测量就是定宽)的项碰巧能出省略号,宽度随内容走
//! 的项(胶囊、徽章、标题)全中招。
//!
//! # 这里的做法
//!
//! 测量只报**自然宽度**(整段整形一次),flex 想怎么收缩随它;`prepaint` 拿到
//! 最终 bounds 后,放不下才按 bounds 宽度 `truncate_line` + 重新整形;`paint`
//! 画整形后的那一行。整形不跨帧缓存 —— 每帧一次 `shape_line`,这类文本都是
//! 一行几十个字,便宜。
//!
//! 字号 / 字体 / 颜色一律继承 `window.text_style()`,与普通文本子节点一致,
//! 所以照旧在外层 div 上 `text_size` / `text_color`。

use gpui::{
    App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, ShapedLine, SharedString, Size, Style, TextRun, Window, px,
    size,
};

/// 单行文本,放不下时尾部换成「…」。
pub struct TruncatedText {
    text: SharedString,
}

impl TruncatedText {
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self { text: text.into() }
    }
}

/// 省略号。与 gpui `text_ellipsis()` 用的同一个字符。
const ELLIPSIS: &str = "…";

/// `request_layout` 里算好、给后两个阶段用的东西。
pub struct Measured {
    font_size: Pixels,
    line_height: Pixels,
    runs: Vec<TextRun>,
    /// 整段文本的整形结果(自然宽度)。
    full: ShapedLine,
}

impl IntoElement for TruncatedText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TruncatedText {
    type RequestLayoutState = Measured;
    /// 放不下时按最终宽度截断后的整形结果;`None` = 放得下,画 `full`。
    type PrepaintState = Option<ShapedLine>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Measured) {
        let text_style = window.text_style();
        let rem = window.rem_size();
        let font_size = text_style.font_size.to_pixels(rem);
        let line_height = text_style.line_height.to_pixels(font_size.into(), rem);
        let runs = vec![text_style.to_run(self.text.len())];
        let full = window
            .text_system()
            .shape_line(self.text.clone(), font_size, &runs, None);
        let natural: Size<Pixels> = size(full.width, line_height);

        let mut style = Style::default();
        // 能在 flex 里收缩到 0:截断的前提就是允许被压
        style.min_size.width = px(0.0).into();
        style.flex_shrink = 1.0;
        let layout_id = window.request_measured_layout(style, move |known, available, _, _| {
            // 已知宽度照收;定宽 available(块级父节点)按它封顶;
            // Min/MaxContent 一律报自然宽 —— 收不收缩交给 flex,不在这里猜
            let width = known.width.unwrap_or(match available.width {
                AvailableSpace::Definite(w) => natural.width.min(w),
                _ => natural.width,
            });
            size(width, known.height.unwrap_or(natural.height))
        });
        (
            layout_id,
            Measured {
                font_size,
                line_height,
                runs,
                full,
            },
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        measured: &mut Measured,
        window: &mut Window,
        _cx: &mut App,
    ) -> Option<ShapedLine> {
        let width = bounds.size.width;
        if measured.full.width <= width {
            return None;
        }
        let font = window.text_style().font();
        let mut wrapper = window.text_system().line_wrapper(font, measured.font_size);
        let mut runs = measured.runs.clone();
        let truncated = wrapper.truncate_line(self.text.clone(), width, ELLIPSIS, &mut runs);
        Some(
            window
                .text_system()
                .shape_line(truncated, measured.font_size, &runs, None),
        )
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        measured: &mut Measured,
        truncated: &mut Option<ShapedLine>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let line = truncated.as_ref().unwrap_or(&measured.full);
        // 整形失败只会少画一行字,不值得 panic
        let _ = line.paint(bounds.origin, measured.line_height, window, cx);
    }
}
