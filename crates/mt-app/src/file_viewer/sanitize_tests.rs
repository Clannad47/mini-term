use super::*;
use crate::file_viewer::html_urls::sanitize_remote_html_urls;

fn contains_raw_markdown_html(node: &MarkdownNode) -> bool {
    matches!(node, MarkdownNode::Html(_))
        || node
            .children()
            .is_some_and(|children| children.iter().any(contains_raw_markdown_html))
}

fn contains_network_loading_markdown_construct(node: &MarkdownNode) -> bool {
    matches!(
        node,
        MarkdownNode::Html(_) | MarkdownNode::Image(_) | MarkdownNode::ImageReference(_)
    ) || node.children().is_some_and(|children| {
        children
            .iter()
            .any(contains_network_loading_markdown_construct)
    })
}

fn contains_active_markdown_construct(node: &MarkdownNode) -> bool {
    matches!(
        node,
        MarkdownNode::Html(_)
            | MarkdownNode::Link(_)
            | MarkdownNode::LinkReference(_)
            | MarkdownNode::Image(_)
            | MarkdownNode::ImageReference(_)
    ) || node
        .children()
        .is_some_and(|children| children.iter().any(contains_active_markdown_construct))
}

fn visible_backslash_escaped_source(value: &str) -> String {
    let mut chars = value.chars().peekable();
    let mut visible = String::with_capacity(value.len());
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek().is_some_and(|next| next.is_ascii_punctuation()) {
            visible.push(chars.next().expect("peeked punctuation must remain"));
        } else {
            visible.push(ch);
        }
    }
    visible
}

#[test]
fn 远程富文本禁用自动资源但保留显式网络链接() {
    let markdown = concat!(
        "- ![secret](file:///home/user/secret.png)\n",
        "![tracker](http://127.0.0.1:8080/a.png)\n",
        "`![code](file:///tmp/code.png)`\n",
        "```md\n![fenced](file:///tmp/fenced.png)\n```",
    );
    let sanitized = sanitize_remote_markdown(markdown);
    assert!(sanitized.contains("- secret"), "{sanitized}");
    assert!(sanitized.contains("tracker"), "{sanitized}");
    assert!(!sanitized.contains("![tracker]"), "{sanitized}");
    assert!(!sanitized.contains("file:///home/user/secret.png"));
    assert!(sanitized.contains("`![code](file:///tmp/code.png)`"));
    assert!(sanitized.contains("![fenced](file:///tmp/fenced.png)"));

    let references = sanitize_remote_markdown(concat!(
        "![secret][local]\n",
        "[local]: <file:///home/user/secret.png> \"title\"\n",
        "![web][remote]\n",
        "[remote]: https://example.com/image.png\n",
    ));
    assert!(!references.contains("file:///"), "{references}");
    assert!(
        references.contains("[remote]: https://example.com/image.png"),
        "{references}"
    );
    // Unresolved reference syntax may remain as literal text. The reparsed
    // AST below is the security boundary: no active image/reference node
    // may survive sanitization.
    let references_ast = markdown::to_mdast(&references, &ParseOptions::gfm())
        .expect("sanitized references must remain parseable");
    let mut unsafe_reference_nodes = Vec::new();
    collect_remote_markdown_replacements(&references_ast, &mut unsafe_reference_nodes);
    assert!(unsafe_reference_nodes.is_empty(), "{references}");

    let links = sanitize_remote_markdown(concat!(
        "[local](file:///etc/passwd)\n",
        "[relative](../secret.txt)\n",
        "[web](https://example.com/docs)\n",
        "[<file:///etc/shadow>](file:///tmp/outer)\n",
        "<file:///etc/group>\n",
        "`[code](file:///tmp/code)`\n",
        "``[code](file:///tmp/double)``\n",
        "` unmatched [unsafe](file:///tmp/unmatched)\n",
        "```md\n[code](file:///tmp/fenced)\n```",
    ));
    assert!(!links.contains("file:///etc/passwd"), "{links}");
    assert!(!links.contains("../secret.txt"), "{links}");
    assert!(!links.contains("file:///etc/group"), "{links}");
    assert!(!links.contains("file:///etc/shadow"), "{links}");
    assert!(!links.contains("file:///tmp/outer"), "{links}");
    assert!(!links.contains("file:///tmp/unmatched"), "{links}");
    assert!(links.contains("local\nrelative\n"), "{links}");
    assert!(links.contains("[web](https://example.com/docs)"), "{links}");
    assert!(links.contains("`[code](file:///tmp/code)`"), "{links}");
    assert!(links.contains("``[code](file:///tmp/double)``"), "{links}");
    assert!(links.contains("` unmatched unsafe"), "{links}");
    assert!(links.contains("[code](file:///tmp/fenced)"), "{links}");

    let multiline = sanitize_remote_markdown(concat!(
        "![secret](\nfile:///home/user/secret.png\n)\n",
        "[open](\nfile:///etc/passwd\n)\n",
    ));
    assert!(!multiline.contains("file:///"), "{multiline}");
    assert!(multiline.contains("secret"), "{multiline}");
    assert!(multiline.contains("open"), "{multiline}");

    let decoded_label_injection = sanitize_remote_markdown(concat!(
        "[&#91;open&#93;&#40;file:///etc/passwd&#41;](file:///outer)\n",
        "![&#91;image&#93;&#40;file:///tmp/a.png&#41;](file:///image)\n",
        "[&#91;ref&#93;]: file:///definition\n",
    ));
    assert!(
        !decoded_label_injection.contains("file:///outer"),
        "{decoded_label_injection}"
    );
    assert!(
        !decoded_label_injection.contains("file:///image"),
        "{decoded_label_injection}"
    );
    // 定义不能中断前面的段落；这一行从首次解析起就是普通文本，不会生成链接。
    assert!(
        decoded_label_injection.contains("[&#91;ref&#93;]: file:///definition"),
        "{decoded_label_injection}"
    );
    let ast = markdown::to_mdast(&decoded_label_injection, &ParseOptions::gfm())
        .expect("sanitized markdown must remain parseable");
    let mut unsafe_nodes = Vec::new();
    collect_remote_markdown_replacements(&ast, &mut unsafe_nodes);
    assert!(unsafe_nodes.is_empty(), "{decoded_label_injection}");

    let fence_edges = sanitize_remote_markdown(concat!(
        "    ```\n",
        "[after-indent](file:///tmp/after-indent)\n",
        "```md\n",
        "~~~\n",
        "[inside](file:///tmp/inside)\n",
        "```\n",
        "[outside](file:///tmp/outside)\n",
    ));
    assert!(
        !fence_edges.contains("file:///tmp/after-indent"),
        "{fence_edges}"
    );
    assert!(fence_edges.contains("file:///tmp/inside"), "{fence_edges}");
    assert!(
        !fence_edges.contains("file:///tmp/outside"),
        "{fence_edges}"
    );

    let html = concat!(
        r#"<img src="file:///home/user/secret.png">"#,
        r#"<img src="http://127.0.0.1:8080/a.png">"#,
        r#"<a href="file:///etc/passwd">local</a>"#,
        r#"<a href="https://example.com/docs">web</a>"#,
        r##"<a href="#section">anchor</a>"##,
    );
    let sanitized = sanitize_remote_html_urls(html);
    assert!(!sanitized.contains("file:///"), "{sanitized}");
    assert_eq!(sanitized.matches(r#"src="about:blank""#).count(), 2);
    assert!(sanitized.contains(r##"href="#""##), "{sanitized}");
    assert!(
        sanitized.contains("https://example.com/docs"),
        "{sanitized}"
    );
    assert!(sanitized.contains(r##"href="#section""##), "{sanitized}");

    let unquoted = sanitize_remote_html_urls(concat!(
        r#"<img src=file:///etc/passwd>"#,
        r#"<img/src=file:///etc/group>"#,
        r#"<img alt="x"src=file:///etc/hosts>"#,
        r#"<img src=https://example.com/image.png>"#,
        r#"<a href=../secret.txt>local</a>"#,
    ));
    assert!(!unquoted.contains("file:///"), "{unquoted}");
    assert!(!unquoted.contains("src=https://example.com/image.png"));
    assert!(unquoted.contains("src=about:blank"), "{unquoted}");
    assert!(unquoted.contains("href=#"), "{unquoted}");

    let stray_text = sanitize_remote_html_urls(
        "plain href=\" without a closing quote\n<img src=file:///etc/shadow>",
    );
    assert!(!stray_text.contains("file:///"), "{stray_text}");

    for source in [
        r#"<!-- normal --><img src="https://evil.test/normal.png">"#,
        r#"<!--x--!><img src="https://evil.test/bang.png">"#,
        r#"<!--><img src="https://evil.test/abrupt.png">"#,
        r#"<!--><img src="https://evil.test/abrupt-with-tail.png">-->"#,
        r#"<!---><img src="https://evil.test/short.png">"#,
        r#"</div "><img src="https://evil.test/end-tag.png">"#,
        r#"<script>x</script "><img src="https://evil.test/raw-end-tag.png">"#,
        r#"<svg><script><img src="https://evil.test/foreign.png"></script></svg>"#,
        r#"<svg><p><math></svg><script><img src="https://evil.test/foreign-recovery-a.png"></script>"#,
        r#"<svg></math><p><math></svg><script><img src="https://evil.test/foreign-recovery-b.png"></script>"#,
    ] {
        let html = sanitize_remote_html_urls(source);
        assert!(html.contains(r#"src="about:blank""#), "{html}");
        assert!(!html.contains("src=\"https://evil.test"), "{html}");

        let markdown = sanitize_remote_markdown(source);
        let ast = markdown::to_mdast(&markdown, &ParseOptions::gfm())
            .expect("sanitized Markdown must remain parseable");
        assert!(!contains_raw_markdown_html(&ast), "{markdown}");
        assert!(
            !contains_network_loading_markdown_construct(&ast),
            "{markdown}"
        );
        assert_eq!(visible_backslash_escaped_source(&markdown), source);
    }

    let raw_text = concat!(
        r#"<textarea /><img src="https://example.com/text-example.png"></textarea>"#,
        r#"<img src="https://evil.test/after-textarea.png">"#,
    );
    let scanned = sanitize_remote_html_urls(raw_text);
    assert!(
        scanned.contains("https://example.com/text-example.png"),
        "{scanned}"
    );
    assert!(scanned.contains(r#"src="about:blank""#), "{scanned}");
    assert!(
        !scanned.contains("https://evil.test/after-textarea.png"),
        "{scanned}"
    );

    let markdown = sanitize_remote_markdown(raw_text);
    assert_eq!(visible_backslash_escaped_source(&markdown), raw_text);
    let ast = markdown::to_mdast(&markdown, &ParseOptions::gfm())
        .expect("sanitized Markdown must remain parseable");
    assert!(!contains_raw_markdown_html(&ast), "{markdown}");
    assert!(
        !contains_network_loading_markdown_construct(&ast),
        "{markdown}"
    );
}

#[test]
fn markdown_html_只降级真实_ast_节点并保留代码原文() {
    let source = concat!(
        "`<img src=\"https://example.com/inline.png\">`\n\n",
        "`<Widget src=\"file:///tmp/widget\" />`\n\n",
        "```html\n<a href=\"file:///tmp/example\">example</a>\n```\n\n",
        "```jsx\n<Component href=\"file:///tmp/component\" />\n```\n\n",
        "<pre>\n&lt;img src=\"https://example.com/pre-example.png\"&gt;\n</pre>\n\n",
        "<!-- <img src=\"https://example.com/comment-example.png\"> -->\n\n",
        "<script>const demo = '<img src=\"https://example.com/script-example.png\">';</script>\n\n",
        r#"<img src="https://example.com/active.png">"#,
        "\n",
        r#"<a href="file:///etc/passwd">local</a>"#,
        "\n",
        r#"<a href="https://example.com/docs">web</a>"#,
    );
    let sanitized = sanitize_remote_markdown(source);
    assert!(
        sanitized.contains("`<img src=\"https://example.com/inline.png\">`"),
        "{sanitized}"
    );
    assert!(
        sanitized.contains("`<Widget src=\"file:///tmp/widget\" />`"),
        "{sanitized}"
    );
    assert!(
        sanitized.contains("```html\n<a href=\"file:///tmp/example\">example</a>\n```"),
        "{sanitized}"
    );
    assert!(
        sanitized.contains("```jsx\n<Component href=\"file:///tmp/component\" />\n```"),
        "{sanitized}"
    );
    assert_eq!(visible_backslash_escaped_source(&sanitized), source);

    let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
        .expect("sanitized Markdown must remain parseable");
    assert!(!contains_raw_markdown_html(&ast), "{sanitized}");
    assert!(
        !contains_network_loading_markdown_construct(&ast),
        "{sanitized}"
    );
}

#[test]
fn 审核载荷在远程与会话_markdown中都不能形成活动_html() {
    for payload in [
        r#"<div><select><title></select><img src="https://attacker.example/beacon.png"></title></div>"#,
        r#"<select><plaintext></select><img src="https://attacker.example/b2.png"><a href="file:///C:/Windows/notepad.exe">open</a>"#,
        r#"<template><col><title></template><img src="https://attacker.example/b3.png"></title>"#,
        r#"<div data-example="![beacon](https://attacker.example/b4.png)"></div>"#,
    ] {
        for sanitized in [
            sanitize_remote_markdown(payload),
            sanitize_session_markdown(payload),
        ] {
            assert_eq!(visible_backslash_escaped_source(&sanitized), payload);
            let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
                .expect("sanitized Markdown must remain parseable");
            assert!(!contains_raw_markdown_html(&ast), "{sanitized}");
            assert!(
                !contains_network_loading_markdown_construct(&ast),
                "{sanitized}"
            );
            assert!(!contains_active_markdown_construct(&ast), "{sanitized}");
            let mut unsafe_nodes = Vec::new();
            collect_untrusted_markdown_replacements(&ast, &mut unsafe_nodes);
            assert!(unsafe_nodes.is_empty(), "{sanitized}");
        }
    }
}

#[test]
fn html_block_type_1到5后的缩进活动载荷会清洗到不动点() {
    let html_blocks = [
        "<pre></pre>",
        "<style></style>",
        "<!-- comment -->",
        "<?php ?>",
        "<!DOCTYPE html>",
        "<![CDATA[value]]>",
    ];
    let indented_payloads = [
        "![network](https://attacker.example/image.png)",
        "[local](file:///etc/passwd)",
        "![local](file:///etc/passwd)",
        r#"<img src="https://attacker.example/raw.png">"#,
        r#"<a href="file:///etc/passwd">open</a>"#,
    ];

    for html_block in html_blocks {
        for payload in indented_payloads {
            let source = format!("{html_block}\n    {payload}\n");
            for sanitized in [
                sanitize_remote_markdown(&source),
                sanitize_session_markdown(&source),
            ] {
                assert_ne!(
                    sanitized,
                    markdown_as_indented_code(&source),
                    "正常审核载荷应在轮次上限内收敛:{source}"
                );
                let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
                    .expect("fixed-point Markdown must remain parseable");
                let mut replacements = Vec::new();
                collect_untrusted_markdown_replacements(&ast, &mut replacements);
                assert!(replacements.is_empty(), "{source}\n---\n{sanitized}");
                assert!(
                    !contains_active_markdown_construct(&ast),
                    "{source}\n---\n{sanitized}"
                );
            }
        }
    }
}

#[test]
fn markdown清洗超出轮次时整篇降级为可见代码块() {
    let source = concat!(
        "<!-- comment -->\n",
        "    ![network](https://attacker.example/image.png)\n",
    );
    let sanitized = sanitize_untrusted_markdown_with_pass_limit(source, 1);
    assert_eq!(sanitized, markdown_as_indented_code(source));

    let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
        .expect("fallback Markdown must remain parseable");
    let mut replacements = Vec::new();
    collect_untrusted_markdown_replacements(&ast, &mut replacements);
    assert!(replacements.is_empty(), "{sanitized}");
    assert!(!contains_active_markdown_construct(&ast), "{sanitized}");
}

#[test]
fn 已安全markdown在首轮不动点保持原文() {
    let source = concat!(
        "# 标题\n\n",
        "正文 [docs](https://example.com/docs)\n\n",
        "`<img src=\"https://example.com/code.png\">`\n",
    );
    assert_eq!(sanitize_remote_markdown(source), source);
    assert_eq!(sanitize_session_markdown(source), source);
}

#[test]
fn 不安全目标降级时保留带标点的标签() {
    let sanitized = sanitize_remote_markdown(concat!(
        "[main.rs](src/main.rs)\n",
        "![截图(1).png](./a.png)\n",
    ));
    assert!(sanitized.contains(r"main\.rs"), "{sanitized}");
    assert!(sanitized.contains(r"截图\(1\)\.png"), "{sanitized}");
    assert!(!sanitized.contains("link"), "{sanitized}");
    assert!(!sanitized.contains("image"), "{sanitized}");

    let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
        .expect("sanitized labels must remain parseable");
    let mut unsafe_nodes = Vec::new();
    collect_remote_markdown_replacements(&ast, &mut unsafe_nodes);
    assert!(unsafe_nodes.is_empty(), "{sanitized}");
}

#[test]
fn 会话富文本不触发任何图片或外部_html_资源() {
    let source = concat!(
        "![web](https://example.com/pixel)\n",
        "![local](file:///etc/passwd)\n",
        "![reference][image]\n",
        "[image]: https://example.com/reference.png\n",
        "[docs](https://example.com/docs)\n",
        "`<img src=\"https://example.com/code-inline.png\">`\n",
        "```html\n<img src=\"https://example.com/code-fenced.png\">\n```\n",
        r##"<img src="https://example.com/html.png"><img src="file:///etc/group"><a href="https://example.com/html">html</a><a href="#section">anchor</a>"##,
    );
    let sanitized = sanitize_session_markdown(source);
    let visible = visible_backslash_escaped_source(&sanitized);
    assert!(
        visible.contains(
            r##"<img src="https://example.com/html.png"><img src="file:///etc/group"><a href="https://example.com/html">html</a><a href="#section">anchor</a>"##,
        ),
        "raw HTML 源码应保持可见:{sanitized}"
    );
    assert!(
        sanitized.contains("`<img src=\"https://example.com/code-inline.png\">`"),
        "{sanitized}"
    );
    assert!(
        sanitized.contains("```html\n<img src=\"https://example.com/code-fenced.png\">\n```"),
        "{sanitized}"
    );
    assert!(
        sanitized.contains("[docs](https://example.com/docs)"),
        "{sanitized}"
    );

    let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
        .expect("sanitized session markdown must remain parseable");
    assert!(!contains_raw_markdown_html(&ast), "{sanitized}");
    let mut unsafe_nodes = Vec::new();
    collect_untrusted_markdown_replacements(&ast, &mut unsafe_nodes);
    assert!(unsafe_nodes.is_empty(), "{sanitized}");
}
