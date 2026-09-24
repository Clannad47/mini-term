use super::*;

#[test]
fn html_的本地资源改写成_file_url() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("site");
    let image_url = to_file_url(&base.join("img/a.png")).expect("测试基准路径应为绝对路径");
    let out = rewrite_html_urls(r#"<img src="img/a.png" alt="a">"#, &base);
    assert_eq!(out, format!(r#"<img src="{image_url}" alt="a">"#));
    // 单引号 / 大写属性名 / 等号旁的空白都认
    let image_url = to_file_url(&base.join("a.png")).expect("测试基准路径应为绝对路径");
    let out = rewrite_html_urls("<img SRC = 'a.png'>", &base);
    assert_eq!(out, format!("<img SRC = '{image_url}'>"));
    // href / poster 同样处理
    let poster_url = to_file_url(&base.join("p.jpg")).expect("测试基准路径应为绝对路径");
    let out = rewrite_html_urls(r#"<video poster="p.jpg"></video>"#, &base);
    assert!(out.contains(&poster_url), "{out}");

    // 排除清单(原版正则那一串)一律原样
    for keep in [
        r#"<a href="https://x.dev">x</a>"#,
        r#"<img src="data:image/png;base64,AAA">"#,
        // 井号锚点:`"#` 会提前结束 `r#"…"#`,这条必须用 `r##"…"##`
        r##"<a href="#anchor">锚</a>"##,
        r#"<a href="mailto:a@b.c">mail</a>"#,
        r#"<a href="javascript:void(0)">js</a>"#,
        r#"<img src="file:///D:/site/a.png">"#,
    ] {
        assert_eq!(rewrite_html_urls(keep, &base), keep, "不该改:{keep}");
    }
    // `data-src` 不是 src
    let keep = r#"<img data-src="a.png">"#;
    assert_eq!(rewrite_html_urls(keep, &base), keep);

    // 远程清洗器在见过 svg/math 后会保守扫描后续 raw-text，防 HTML5
    // namespace 恢复漏掉活动图片；可信本地 HTML 不得复用这条 fail-closed
    // 策略，否则 textarea 里的示例文本会被误改成 file:// URL。
    let keep = r#"<svg></svg><textarea><img src="literal.png"></textarea>"#;
    assert_eq!(rewrite_html_urls(keep, &base), keep);
}
