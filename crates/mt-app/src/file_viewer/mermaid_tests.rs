use super::*;

fn mermaid_key(code: &str, dark: bool) -> MermaidKey {
    MermaidKey {
        code: code.into(),
        dark,
        background: 0x1E1E2E,
    }
}

#[test]
fn mermaid_渲染_合法图表出_svg_且底色跟主题() {
    let key = mermaid_key("graph TD\n    A[开始] --> B[结束]", false);
    let svg = render_mermaid_svg(&key).expect("issue #80 的样例必须渲染成功");
    assert!(svg.starts_with("<svg"), "{}", &svg[..svg.len().min(80)]);
    assert!(
        svg.contains("开始") && svg.contains("结束"),
        "中文标签要原样进 SVG"
    );
    // 画布底色是 key 里的主题底色,不是 mermaid 自带的白 / #333
    assert!(
        svg.contains("fill=\"#1E1E2E\""),
        "{}",
        &svg[..svg.len().min(400)]
    );
    let dark = render_mermaid_svg(&mermaid_key("sequenceDiagram\n    A->>B: hi", true)).unwrap();
    assert!(dark.contains("fill=\"#1E1E2E\""));

    // 同一份图表经 gpui 的 SvgRenderer 栅格化后,尺寸 = SVG 标称尺寸 ×
    // SMOOTH_SVG_SCALE_FACTOR(与 gpui 的 svg 图片同倍率,image_display_width
    // 按 is_svg 除回去才是逻辑宽)。栅格器在这里自己建一个:它只要一个
    // `AssetSource`,`()` 就是一个(打包字体取不到时只记一行日志,系统字体照旧),
    // 因此无窗口的单测环境也能跑。
    let renderer = gpui::SvgRenderer::new(std::sync::Arc::new(()));
    let image = render_mermaid_image(&key, &renderer).unwrap();
    let size = image.size(0);
    assert!(size.width.0 > 32 && size.height.0 > 32, "{size:?}");
    let nominal =
        mermaid_rs_renderer::measure(&key.code, mermaid_rs_renderer::RenderOptions::default())
            .unwrap();
    assert_eq!(
        size.width.0,
        (nominal.width * gpui::SMOOTH_SVG_SCALE_FACTOR) as i32
    );
    assert_eq!(
        size.height.0,
        (nominal.height * gpui::SMOOTH_SVG_SCALE_FACTOR) as i32
    );
    // 底色像素:直通 BGRA(不透明底不受去预乘影响),B 在前 —— 去预乘 + 换通道
    // 这一步现在由 gpui 的 `swap_rgba_pa_to_bgra` 做,靶子不变
    let bytes = image.as_bytes(0).unwrap();
    assert_eq!(&bytes[..4], &[0x2E, 0x1E, 0x1E, 0xFF]);
    // 画布不是空的:底色之外还得有线条/文字的像素
    assert!(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| *p != [0x2E, 0x1E, 0x1E, 0xFF]),
        "整张图不能只有底色"
    );
}

/// 2026-09-21 用户拿 hop-ai 网关设计方案的架构图对比 Typora:实体原样、子图
/// 竖排、虚线节点跑到最左。三条修在 [`crate::mermaid_compat`],这里验整条链路。
#[test]
fn mermaid_渲染_实体解码_子图方向跟父图_虚线节点不掉层() {
    let src = "flowchart LR
    subgraph Hosts[\"接入方\"]
        H1[\"系统 A 页面<br/>&lt;script&gt; + &lt;hop-ai-chat&gt;\"]
    end
    subgraph GW[\"网关\"]
        direction TB
        EDGE[\"Chat API\"]
        IDP[\"身份适配层\"]
        LOOP[\"Agent Loop\"]
        LLM[\"LLM Provider\"]
    end
    P1[\"DeepSeek 云\"]
    P2[\"私有化 vLLM\"]
    H1 --> EDGE
    EDGE --> IDP --> LOOP
    LOOP <--> LLM
    LLM --> P1
    LLM -.-> P2
";
    let svg = render_mermaid_svg(&mermaid_key(src, true)).unwrap();
    // 实体解码后再按 SVG 转义:`<script>` → `&lt;script&gt;`;不解码会是 `&amp;lt;`
    assert!(svg.contains("&lt;script&gt;"), "{svg}");
    assert!(!svg.contains("&amp;lt;"), "{svg}");

    // SVG 里节点没有 id,按标签文字找它前面最近的节点框 `<rect x="…" y="…"`
    // (同一层的框左对齐,比文字中心好比)
    let pos = |label: &str| -> (f32, f32) {
        let at = svg
            .find(&format!(">{label}<"))
            .unwrap_or_else(|| panic!("{label} 不在 SVG 里:{svg}"));
        let rect = svg[..at].rfind("<rect x=\"").unwrap() + "<rect x=\"".len();
        let mut nums = svg[rect..]
            .split('"')
            .step_by(2)
            .take(2)
            .map(|s| s.parse::<f32>().unwrap());
        (nums.next().unwrap(), nums.next().unwrap())
    };
    // GW 有节点连到外面 → direction TB 被忽略,EDGE → IDP → LOOP 横着排
    let (edge, idp, lp) = (pos("Chat API"), pos("身份适配层"), pos("Agent Loop"));
    assert!(edge.0 < idp.0 && idp.0 < lp.0, "{edge:?} {idp:?} {lp:?}");
    assert!((edge.1 - lp.1).abs() < 1.0, "{edge:?} {lp:?}");
    // 只靠虚线挂着的 P2 排在 LLM 右边、与 P1 同层,而不是第 0 层
    let (llm, p1, p2) = (pos("LLM Provider"), pos("DeepSeek 云"), pos("私有化 vLLM"));
    assert!(p2.0 > llm.0, "{p2:?} {llm:?}");
    assert!((p2.0 - p1.0).abs() < 1.0, "{p2:?} {p1:?}");
    // 那条边画出来仍是虚线
    assert!(svg.contains("stroke-dasharray"), "{svg}");
}

#[test]
fn mermaid_渲染_残缺与空图退回失败() {
    // 解析器对没闭合的括号不报错、只排出 16×16 的空画布 —— 必须按失败处理
    let err = render_mermaid_svg(&mermaid_key("graph TD\n    A[开始 --> B", false))
        .expect_err("空画布必须报失败,否则预览里是一块什么都没有的空白");
    assert_eq!(err.to_string(), t("fileViewer", "mermaidEmptyDiagram"));
    // 校验器认得的硬错误(subgraph 没 end)走 ParseError
    assert!(
        render_mermaid_svg(&mermaid_key("graph TD\n    subgraph x\n    A-->B", false)).is_err()
    );
    // 空文本
    assert!(render_mermaid_svg(&mermaid_key("", false)).is_err());
    assert!(render_mermaid_svg(&mermaid_key("   \n", false)).is_err());
}

#[test]
fn mermaid_画布底色_丢掉_alpha() {
    // 去预乘 + RGBA→BGRA 那一步已交还给 gpui 的 `swap_rgba_pa_to_bgra`
    // (`SvgRenderer::render_parsed` 内部做,上游自带单测),这里只剩本仓
    // 自己那段「主题色 → 画布底色」的换算
    assert_eq!(rgb_u32(gpui::rgb(0x1E1E2E).into()), 0x1E1E2E);
    assert_eq!(
        rgb_u32(gpui::rgba(0xFFFFFF80).into()),
        0xFFFFFF,
        "alpha 丢掉"
    );
}
