//! Mermaid 图表交给 `mermaid-rs-renderer` 排版前的「mermaid.js 口径」对齐。
//!
//! 文档里的 Mermaid 图基本都是对着 mermaid.js(VS Code / Typora / GitHub 的预览)
//! 写、调到满意的,而 `mermaid-rs-renderer` 0.3.1 有几处与它的**实际行为**不一致,
//! 同一份图在本程序里就画歪了(2026-09-21 用 hop-ai 网关设计方案那张架构图
//! 对出来的,`mmdr` 命令行复现一致,不是本程序用法的问题):
//!
//! 1. **标签里的 HTML 实体原样画出来**:`&lt;script&gt;` 就显示 `&lt;script&gt;`。
//!    mermaid.js 的标签是 HTML,实体由浏览器解码;它另有一套自家的
//!    `#quot;` / `#35;` 实体写法(`encodeEntities` 把 `#\w+;` 转成 `&…;`)。
//! 2. **子图里的 `direction TB` 一律生效**。mermaid.js 的文档明说(flowchart
//!    "Limitation" 一节):子图只要有节点连到外面,`direction` 就被忽略、跟父图
//!    走(实现上是 `adjustClustersAndEdges` 标 `externalConnections`,带标的簇不再
//!    单独抽出来按自己的方向排)。照字面生效的结果是整个子图竖排、几十条出入边
//!    全从子图底部绕出去。
//! 3. **虚线边不参与分层**(`rank_edges_for_manual_layout` 的启发式:实线覆盖
//!    过半节点时虚线只画不排)。只靠一条虚线挂着的节点因此没有层级,被扔到
//!    第 0 层 —— 画面最左,一条虚线横穿整张图去够它。
//!
//! 三条都在解析结果([`Graph`],字段全公开)上改,不动源码文本、不 fork 渲染器。
//! 三条已合成一条提给上游:<https://github.com/1jehuang/mermaid-rs-renderer/issues/152>;
//! 升级渲染器时逐条核对,上游修了哪条就撤哪条。
//! 第 4 条差异**没做**:子图标题按节点标签的宽度上限折行(mermaid.js 不折),
//! 上限是全图共用的 `max_label_width_chars`,分不开;标题折成两行不影响读图。
use std::borrow::Cow;
use std::collections::HashSet;

use mermaid_rs_renderer::{DiagramKind, EdgeStyle, Graph, Layout};

/// 排版前在解析结果上做的对齐,以及排版后要还原的东西。
pub(crate) struct Compat {
    /// 为了参与分层临时改成实线的虚线边(`graph.edges` 下标)。
    promoted_dotted: Vec<usize>,
}

impl Compat {
    pub(crate) fn apply(graph: &mut Graph) -> Self {
        decode_label_entities(graph);
        inherit_direction_when_linked_outside(graph);
        let promoted_dotted = promote_dotted_only_edges(graph);
        Self { promoted_dotted }
    }

    /// 排版完成后把临时改成实线的边改回虚线。流程图的 `layout.edges` 与
    /// `graph.edges` 同序一一对应(上游 `build_edge_layouts` 按下标遍历)。
    pub(crate) fn restore(&self, layout: &mut Layout) {
        for &idx in &self.promoted_dotted {
            if let Some(edge) = layout.edges.get_mut(idx) {
                edge.style = EdgeStyle::Dotted;
            }
        }
    }
}

// ─── 1. 标签实体 ─────────────────────────────────────────────

fn decode_label_entities(graph: &mut Graph) {
    for node in graph.nodes.values_mut() {
        decode_in_place(&mut node.label);
    }
    for sub in &mut graph.subgraphs {
        decode_in_place(&mut sub.label);
    }
    for edge in &mut graph.edges {
        for label in [&mut edge.label, &mut edge.start_label, &mut edge.end_label]
            .into_iter()
            .flatten()
        {
            decode_in_place(label);
        }
    }
}

fn decode_in_place(text: &mut String) {
    if let Cow::Owned(decoded) = decode_entities(text) {
        *text = decoded;
    }
}

/// 解码标签里的字符引用:HTML 的 `&name;` / `&#123;` / `&#x1F;`,以及 mermaid
/// 自家的 `#name;` / `#123;`。认不得的(`&bogus;`、`C# 8;`)原样保留。
/// 命名引用查的是 markdown crate 的 HTML5 全表(已是依赖,不另引库)。
pub(crate) fn decode_entities(text: &str) -> Cow<'_, str> {
    if !text.contains(['&', '#']) {
        return Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if !matches!(bytes[i], b'&' | b'#') {
            i += 1;
            continue;
        }
        match decode_reference(bytes, i) {
            Some((decoded, end)) => {
                out.push_str(&text[copied..i]);
                out.push_str(&decoded);
                copied = end;
                i = end;
            }
            None => i += 1,
        }
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// `bytes[start]` 是 `&` 或 `#`。返回 (解码结果, 引用结束后的下标)。
fn decode_reference(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    // `&#…;` 与 `#数字;` 是数值引用,`&name;` 与 `#name;` 是命名引用;
    // 十六进制只有 `&#x…;` 一种写法(mermaid 的 `#x41;` 会被当成命名引用查表)
    let (numeric, radix, body_start) = match bytes[start] {
        b'&' if bytes.get(start + 1) == Some(&b'#') => match bytes.get(start + 2) {
            Some(b'x' | b'X') => (true, 16, start + 3),
            _ => (true, 10, start + 2),
        },
        b'&' => (false, 10, start + 1),
        _ => (
            bytes.get(start + 1).is_some_and(u8::is_ascii_digit),
            10,
            start + 1,
        ),
    };
    let (accept, max_len): (fn(&u8) -> bool, usize) = match (numeric, radix) {
        (true, 16) => (u8::is_ascii_hexdigit, 8),
        (true, _) => (u8::is_ascii_digit, 8),
        (false, _) => (u8::is_ascii_alphanumeric, 32),
    };
    let mut end = body_start;
    while end < bytes.len() && end - body_start < max_len && accept(&bytes[end]) {
        end += 1;
    }
    if end == body_start || bytes.get(end) != Some(&b';') {
        return None;
    }
    let body = std::str::from_utf8(&bytes[body_start..end]).ok()?;
    let decoded = if numeric {
        let ch = char::from_u32(u32::from_str_radix(body, radix).ok()?)?;
        if ch.is_control() {
            return None;
        }
        ch.to_string()
    } else {
        markdown::decode_named(body, true)?
    };
    Some((decoded, end + 1))
}

// ─── 2. 连到外面的子图不认自己的 direction ───────────────────

fn inherit_direction_when_linked_outside(graph: &mut Graph) {
    if graph.kind != DiagramKind::Flowchart {
        return;
    }
    let edges = &graph.edges;
    for sub in &mut graph.subgraphs {
        if sub.direction.is_none() {
            continue;
        }
        // `sub.nodes` 已含嵌套子图的节点(解析器把节点记进整条祖先链),与
        // mermaid.js 按 descendants 判「连到外面」同口径;指向子图本身的边
        // (`X --> GW`)两端都不在集合里,不算
        let members: HashSet<&str> = sub.nodes.iter().map(String::as_str).collect();
        let linked_outside = edges
            .iter()
            .any(|edge| members.contains(edge.from.as_str()) != members.contains(edge.to.as_str()));
        if linked_outside {
            sub.direction = None;
        }
    }
}

// ─── 3. 只靠虚线挂着的节点也要分层 ────────────────────────────

/// 某个端点没有任何实线边的虚线边,临时改成实线让它参与分层;返回改过的下标,
/// 排版后由 [`Compat::restore`] 改回。两端都有实线边的虚线不动:上游把它们当
/// 次要边(不排层、路由让路)的取舍本身没问题。
fn promote_dotted_only_edges(graph: &mut Graph) -> Vec<usize> {
    if graph.kind != DiagramKind::Flowchart {
        return Vec::new();
    }
    let mut on_solid: HashSet<&str> = HashSet::new();
    for edge in &graph.edges {
        if edge.style != EdgeStyle::Dotted {
            on_solid.insert(edge.from.as_str());
            on_solid.insert(edge.to.as_str());
        }
    }
    let promoted: Vec<usize> = graph
        .edges
        .iter()
        .enumerate()
        .filter(|(_, edge)| {
            edge.style == EdgeStyle::Dotted
                && !(on_solid.contains(edge.from.as_str()) && on_solid.contains(edge.to.as_str()))
        })
        .map(|(idx, _)| idx)
        .collect();
    for &idx in &promoted {
        graph.edges[idx].style = EdgeStyle::Solid;
    }
    promoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_rs_renderer::{LayoutConfig, Theme, compute_layout, parse_mermaid_strict};

    #[test]
    fn 实体解码_html_与_mermaid_两种写法() {
        assert_eq!(
            decode_entities("&lt;script&gt; + &lt;hop-ai-chat&gt;"),
            "<script> + <hop-ai-chat>"
        );
        assert_eq!(decode_entities("A #quot;B#quot; &amp; C"), "A \"B\" & C");
        assert_eq!(decode_entities("&#35;&#x41;#9829;"), "#A♥");
        assert_eq!(decode_entities("&nbsp;"), "\u{a0}");
        // 认不得的原样保留:未知名字、`#` 后面不是引用体、没有分号、控制字符
        for kept in [
            "&bogus;", "C# 8; ok", "a & b", "&lt", "#x41;", "&#0;", "&#9;",
        ] {
            assert_eq!(decode_entities(kept), kept, "{kept}");
            assert!(
                matches!(decode_entities(kept), Cow::Borrowed(_)),
                "没解出东西就不该分配:{kept}"
            );
        }
        assert!(matches!(
            decode_entities("纯文本 no refs"),
            Cow::Borrowed(_)
        ));
    }

    /// hop-ai 网关设计方案那张图的骨架:GW 子图写了 `direction TB`,但 EDGE 被
    /// 外面的 H1 连着 —— mermaid.js 会忽略 TB 按父图的 LR 排;ISO 子图没连到
    /// 外面,TB 保留。
    const LINKED_OUTSIDE: &str = "flowchart LR
    H1[host] --> EDGE
    subgraph GW[gw]
        direction TB
        EDGE --> IDP --> LOOP
    end
    subgraph ISO[iso]
        direction TB
        X --> Y
    end
";

    #[test]
    fn 连到外面的子图_direction_被忽略_孤立子图保留() {
        let mut graph = parse_mermaid_strict(LINKED_OUTSIDE).unwrap().graph;
        let _ = Compat::apply(&mut graph);
        let dir = |id: &str| {
            graph
                .subgraphs
                .iter()
                .find(|sub| sub.id.as_deref() == Some(id))
                .unwrap()
                .direction
        };
        assert_eq!(dir("GW"), None);
        assert_eq!(dir("ISO"), Some(mermaid_rs_renderer::Direction::TopDown));

        // 落到排版上:GW 里的链按 LR 横着走(x 递增、y 相同)
        let layout = compute_layout(&graph, &Theme::modern(), &LayoutConfig::default());
        let at = |id: &str| (layout.nodes[id].x, layout.nodes[id].y);
        assert!(
            at("EDGE").0 < at("IDP").0 && at("IDP").0 < at("LOOP").0,
            "{layout:?}"
        );
        assert!((at("EDGE").1 - at("LOOP").1).abs() < 1.0, "{layout:?}");
    }

    #[test]
    fn 只靠虚线挂着的节点_排在虚线来源的下一层_画出来仍是虚线() {
        // 实线覆盖过半节点,上游分层时会把虚线整个扔掉:P2 没有层级 → 第 0 层
        let src = "flowchart LR
    A --> B --> C
    C --> P1
    C -.-> P2
";
        let mut graph = parse_mermaid_strict(src).unwrap().graph;
        let compat = Compat::apply(&mut graph);
        assert_eq!(compat.promoted_dotted, vec![3]);
        assert!(
            graph
                .edges
                .iter()
                .all(|edge| edge.style == EdgeStyle::Solid)
        );

        let mut layout = compute_layout(&graph, &Theme::modern(), &LayoutConfig::default());
        compat.restore(&mut layout);
        assert_eq!(layout.edges[3].style, EdgeStyle::Dotted);
        assert_eq!(
            (&layout.edges[3].from[..], &layout.edges[3].to[..]),
            ("C", "P2")
        );
        let x = |id: &str| layout.nodes[id].x;
        assert!(x("P2") > x("C"), "P2 该排在 C 右边:{layout:?}");
        assert!((x("P2") - x("P1")).abs() < 1.0, "P2 与 P1 同层:{layout:?}");

        // 两端都有实线边的虚线不动(上游的次要边取舍保留)
        let src = "flowchart LR
    A --> B --> C
    A -.-> C
";
        let mut graph = parse_mermaid_strict(src).unwrap().graph;
        let compat = Compat::apply(&mut graph);
        assert!(compat.promoted_dotted.is_empty());
        assert_eq!(graph.edges[2].style, EdgeStyle::Dotted);
    }

    #[test]
    fn 边标签与子图标题也解实体() {
        // 状态图 / 序列图的解析器拿 `;` 切语句,实体到不了标签,只有流程图能验
        let src = "flowchart LR
    subgraph S[\"&lt;S&gt;\"]
        A -- \"a &lt; b\" --> B
    end
";
        let mut graph = parse_mermaid_strict(src).unwrap().graph;
        let _ = Compat::apply(&mut graph);
        assert!(
            graph
                .edges
                .iter()
                .any(|edge| edge.label.as_deref() == Some("a < b")),
            "{:?}",
            graph.edges
        );
        assert_eq!(graph.subgraphs[0].label, "<S>");
    }
}
