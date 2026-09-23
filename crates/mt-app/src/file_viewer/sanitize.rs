//! 不可信 Markdown 的清洗:远程文档预览与 AI 会话正文共用一套。按渲染器同一份
//! GFM AST 定位节点、按源码区间替换,反复重解析到不动点;收敛不了整篇退成可见
//! 代码块。[`sanitize_session_markdown`] 经 `file_viewer` 再导出给会话面板用。

use markdown::{ParseOptions, mdast::Node as MarkdownNode};

use super::markdown::{MarkdownReplacement, markdown_replacement};

fn remote_markdown_url_allowed(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
        || lower.starts_with('#')
}

const MAX_UNTRUSTED_MARKDOWN_SANITIZE_PASSES: usize = 4;

fn markdown_plain_text(node: &MarkdownNode) -> String {
    match node {
        MarkdownNode::Image(image) => image.alt.clone(),
        MarkdownNode::ImageReference(image) => image.alt.clone(),
        _ => node
            .children()
            .map(|children| {
                children.iter().fold(String::new(), |mut text, child| {
                    text.push_str(&markdown_plain_text(child));
                    text
                })
            })
            .unwrap_or_else(|| node.to_string()),
    }
}

fn markdown_safe_plain_label(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        return fallback.into();
    }
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Turn an untrusted raw-HTML AST node into visible Markdown text. Escape every
/// ASCII punctuation character so a second GFM parse cannot recreate either an
/// `mdast::Html` node or Markdown links/images hidden inside an attribute value.
/// Backslash escapes render as the original punctuation, preserving readable
/// source without giving the replacement any active Markdown syntax.
fn inert_markdown_html_source(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn collect_untrusted_markdown_replacements(
    node: &MarkdownNode,
    replacements: &mut Vec<MarkdownReplacement>,
) {
    match node {
        MarkdownNode::Link(link) if !remote_markdown_url_allowed(&link.url) => {
            let label = markdown_safe_plain_label(&markdown_plain_text(node), "link");
            if let Some(replacement) = markdown_replacement(node, label) {
                replacements.push(replacement);
            }
            return;
        }
        MarkdownNode::Image(image) => {
            let alt = markdown_safe_plain_label(&image.alt, "image");
            if let Some(replacement) = markdown_replacement(node, alt) {
                replacements.push(replacement);
            }
            return;
        }
        MarkdownNode::ImageReference(image) => {
            let alt = markdown_safe_plain_label(&image.alt, "image");
            if let Some(replacement) = markdown_replacement(node, alt) {
                replacements.push(replacement);
            }
            return;
        }
        MarkdownNode::Definition(definition) if !remote_markdown_url_allowed(&definition.url) => {
            if let Some(replacement) = markdown_replacement(node, String::new()) {
                replacements.push(replacement);
            }
            return;
        }
        MarkdownNode::Html(html) => {
            if let Some(replacement) =
                markdown_replacement(node, inert_markdown_html_source(&html.value))
            {
                replacements.push(replacement);
            }
            return;
        }
        _ => {}
    }

    if let Some(children) = node.children() {
        for child in children {
            collect_untrusted_markdown_replacements(child, replacements);
        }
    }
}

#[cfg(test)]
fn collect_remote_markdown_replacements(
    node: &MarkdownNode,
    replacements: &mut Vec<MarkdownReplacement>,
) {
    collect_untrusted_markdown_replacements(node, replacements);
}

fn markdown_as_indented_code(source: &str) -> String {
    source
        .split('\n')
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn apply_markdown_replacements(source: &str, mut replacements: Vec<MarkdownReplacement>) -> String {
    replacements.sort_unstable_by_key(|replacement| std::cmp::Reverse(replacement.start));

    let mut sanitized = source.to_string();
    let mut next_start = source.len();
    for replacement in replacements {
        if replacement.end > next_start
            || replacement.start > replacement.end
            || source.get(replacement.start..replacement.end).is_none()
        {
            continue;
        }
        sanitized.replace_range(replacement.start..replacement.end, &replacement.value);
        next_start = replacement.start;
    }
    sanitized
}

/// Keep reparsing transformed Markdown until the renderer's own GFM grammar
/// sees no disallowed nodes. Escaping an HTML block can change the following
/// indented block into active Markdown, so a single AST generation is not a
/// sufficient security boundary.
fn sanitize_untrusted_markdown_with_pass_limit(source: &str, pass_limit: usize) -> String {
    let mut sanitized = source.to_string();
    for _ in 0..pass_limit {
        let Ok(ast) = markdown::to_mdast(&sanitized, &ParseOptions::gfm()) else {
            return markdown_as_indented_code(source);
        };
        let mut replacements = Vec::new();
        collect_untrusted_markdown_replacements(&ast, &mut replacements);
        if replacements.is_empty() {
            return sanitized;
        }
        sanitized = apply_markdown_replacements(&sanitized, replacements);
    }

    // A transformed document is safe to render only after reparsing proves it
    // has no active replacements. If the bounded loop cannot establish that
    // fixed point, keep the original source visible as inert code.
    markdown_as_indented_code(source)
}

fn sanitize_untrusted_markdown(source: &str) -> String {
    sanitize_untrusted_markdown_with_pass_limit(source, MAX_UNTRUSTED_MARKDOWN_SANITIZE_PASSES)
}

/// Remote rich-text is untrusted input from another machine. Parse with the
/// same GFM AST used by `TextView::markdown`, then replace disallowed links,
/// images, and reference definitions by source byte range. Every real raw-HTML
/// node becomes visible inert source; AST positions keep fenced/indented/inline
/// code byte-for-byte out of scope.
pub(super) fn sanitize_remote_markdown(source: &str) -> String {
    sanitize_untrusted_markdown(source)
}

/// AI session logs are untrusted rich text and share the process-wide preview
/// HTTP client. Preserve Markdown formatting and explicit Markdown links, turn
/// every image into plain alt text, and make raw HTML visible but inert so
/// opening a history entry cannot read local files or issue background network
/// requests.
pub fn sanitize_session_markdown(source: &str) -> String {
    sanitize_untrusted_markdown(source)
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;
