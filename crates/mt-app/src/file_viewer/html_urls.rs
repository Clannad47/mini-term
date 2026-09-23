//! 本地 HTML 预览的资源 URL 改写:`src` / `href` / `poster` 的本地目标转成
//! `file://`,交给 [`super::http::PreviewHttpClient`] 读盘。词法扫描跳过注释、
//! raw-text 元素与普通文本,不把示例代码当属性。

use std::path::Path;

use super::markdown::{MdImageSrc, resolve_image_src, to_file_url};

/// 把 HTML 源里 `src` / `href` / `poster` 的**本地**目标改写成 `file:///…`。
///
/// 逐条对照原版 `htmlSrcDoc`(`FileViewerModal.tsx:134-143`)那条正则,排除清单
/// 也一样(http(s) / data / blob / mailto / tel / `#` / javascript)。原版靠
/// `convertFileSrc` 转 asset 协议,这里转 `file://` 交给 [`super::PreviewHttpClient`]。
pub(super) fn rewrite_html_urls(source: &str, base_dir: &Path) -> String {
    // 大小写不敏感的定位副本。`to_ascii_lowercase` 只动 ASCII,**字节长度不变**,
    // 索引因此能直接拿回原文切片(`to_lowercase` 就不行,有字符会变长)
    let lower = source.to_ascii_lowercase();
    let mut out = String::with_capacity(source.len());
    let mut pos = 0usize;
    for attr in html_url_attributes(&lower, false) {
        out.push_str(&source[pos..attr.value_start]);
        out.push_str(&rewrite_html_value(
            &source[attr.value_start..attr.value_end],
            base_dir,
        ));
        pos = attr.value_end;
    }
    out.push_str(&source[pos..]);
    out
}

#[derive(Clone, Copy)]
struct HtmlUrlAttribute {
    value_start: usize,
    value_end: usize,
    #[cfg(test)]
    name: &'static str,
}

fn skip_html_tag(lower: &str, cursor: usize) -> usize {
    let bytes = lower.as_bytes();
    bytes[cursor..]
        .iter()
        .position(|byte| *byte == b'>')
        .map(|relative| cursor + relative + 1)
        .unwrap_or(bytes.len())
}

fn skip_html_end_tag(lower: &str, mut cursor: usize) -> usize {
    let bytes = lower.as_bytes();
    while cursor < bytes.len() {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            break;
        }
        if bytes[cursor] == b'>' {
            return cursor + 1;
        }
        if bytes[cursor] == b'/' {
            cursor += 1;
            continue;
        }

        let name_start = cursor;
        while cursor < bytes.len()
            && !bytes[cursor].is_ascii_whitespace()
            && !matches!(bytes[cursor], b'=' | b'/' | b'>' | b'"' | b'\'' | b'<')
        {
            cursor += 1;
        }
        if cursor == name_start {
            // 关闭标签里的孤立引号只是解析错误，不会让后面的 `>` 失去结束作用。
            cursor += 1;
            continue;
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        match bytes.get(cursor).copied() {
            Some(quote @ (b'"' | b'\'')) => {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor] != quote {
                    cursor += 1;
                }
                if cursor < bytes.len() {
                    cursor += 1;
                }
            }
            Some(_) => {
                while cursor < bytes.len()
                    && !bytes[cursor].is_ascii_whitespace()
                    && bytes[cursor] != b'>'
                {
                    cursor += 1;
                }
            }
            None => break,
        }
    }
    bytes.len()
}

fn skip_html_comment(lower: &str, mut cursor: usize) -> usize {
    let bytes = lower.as_bytes();
    // HTML5 的 abrupt-closing empty comment：`<!-->` / `<!--->`。
    if bytes.get(cursor) == Some(&b'>') {
        return cursor + 1;
    }
    if bytes[cursor..].starts_with(b"->") {
        return cursor + 2;
    }
    while cursor < bytes.len() {
        if bytes[cursor..].starts_with(b"-->") {
            return cursor + 3;
        }
        if bytes[cursor..].starts_with(b"--!>") {
            return cursor + 4;
        }
        cursor += 1;
    }
    bytes.len()
}

fn is_raw_text_tag(tag: &str) -> bool {
    matches!(
        tag,
        "script"
            | "style"
            | "textarea"
            | "title"
            | "xmp"
            | "iframe"
            | "noembed"
            | "noframes"
            | "plaintext"
    )
}

fn skip_raw_text_element(lower: &str, mut cursor: usize, tag: &str) -> usize {
    if tag == "plaintext" {
        return lower.len();
    }
    let needle = format!("</{tag}");
    while let Some(relative) = lower[cursor..].find(&needle) {
        let close_start = cursor + relative;
        let name_end = close_start + needle.len();
        let boundary = lower.as_bytes().get(name_end).copied();
        if boundary.is_none_or(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>')) {
            return skip_html_end_tag(lower, name_end);
        }
        cursor = name_end;
    }
    lower.len()
}

/// 收集真实开始标签里的 `src=` / `href=` / `poster=` 值区间。HTML 允许属性值
/// 不加引号，也会恢复 `<img/src=x>` 与 `alt="x"src=y` 这类错误写法；同时必须
/// 跳过普通文本、注释和 HTML namespace 的 raw-text 内容，避免把示例代码当成
/// 属性；svg/math foreign content 则保守继续扫描，防止 namespace 恢复产生
/// 活动图片。
fn html_url_attributes(
    lower: &str,
    fail_closed_after_foreign_content: bool,
) -> Vec<HtmlUrlAttribute> {
    let bytes = lower.as_bytes();
    let mut attrs = Vec::new();
    let mut pos = 0usize;
    // foreign-content 的 tree-builder 恢复规则无法只靠词法标签栈精确复刻。
    // 不可信清洗启用 fail-closed 时，一旦见过非自闭合 svg/math，后续都不再
    // 跳过 raw-text，宁可多清洗也不能漏活动图片；可信本地改写不启用这条
    // 策略。
    let mut saw_foreign_content = false;

    while pos < bytes.len() {
        let Some(relative) = lower[pos..].find('<') else {
            break;
        };
        let open = pos + relative;
        let mut cursor = open + 1;
        if lower[cursor..].starts_with("!--") {
            pos = skip_html_comment(lower, cursor + 3);
            continue;
        }
        let Some(first) = bytes.get(cursor).copied() else {
            break;
        };
        if first == b'/' {
            let mut name_cursor = cursor + 1;
            while name_cursor < bytes.len()
                && !bytes[name_cursor].is_ascii_whitespace()
                && !matches!(bytes[name_cursor], b'/' | b'>')
            {
                name_cursor += 1;
            }
            pos = skip_html_end_tag(lower, name_cursor);
            continue;
        }
        if matches!(first, b'!' | b'?') {
            pos = skip_html_tag(lower, cursor + 1);
            continue;
        }
        if !first.is_ascii_alphabetic() {
            pos = cursor;
            continue;
        }

        let tag_start = cursor;
        while cursor < bytes.len()
            && !bytes[cursor].is_ascii_whitespace()
            && !matches!(bytes[cursor], b'/' | b'>')
        {
            cursor += 1;
        }
        let tag = &lower[tag_start..cursor];
        let mut self_closing = false;

        while cursor < bytes.len() {
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor >= bytes.len() {
                break;
            }
            if bytes[cursor] == b'>' {
                cursor += 1;
                break;
            }
            if bytes[cursor] == b'/' {
                if bytes.get(cursor + 1) == Some(&b'>') {
                    self_closing = true;
                    cursor += 2;
                    break;
                }
                // html5ever 接受 `<img/src=x>`；单独的 `/` 当属性分隔符跳过。
                cursor += 1;
                continue;
            }

            let name_start = cursor;
            while cursor < bytes.len()
                && !bytes[cursor].is_ascii_whitespace()
                && !matches!(bytes[cursor], b'=' | b'/' | b'>' | b'"' | b'\'' | b'<')
            {
                cursor += 1;
            }
            if cursor == name_start {
                cursor += 1;
                continue;
            }
            let name = match &lower[name_start..cursor] {
                "src" => Some("src"),
                "href" => Some("href"),
                "poster" => Some("poster"),
                _ => None,
            };

            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if bytes.get(cursor) != Some(&b'=') {
                continue;
            }
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }

            let (value_start, value_end) = match bytes.get(cursor).copied() {
                Some(quote @ (b'"' | b'\'')) => {
                    cursor += 1;
                    let value_start = cursor;
                    while cursor < bytes.len() && bytes[cursor] != quote {
                        cursor += 1;
                    }
                    let value_end = cursor;
                    if cursor < bytes.len() {
                        cursor += 1;
                    }
                    (value_start, value_end)
                }
                Some(_) => {
                    let value_start = cursor;
                    while cursor < bytes.len()
                        && !bytes[cursor].is_ascii_whitespace()
                        && bytes[cursor] != b'>'
                    {
                        cursor += 1;
                    }
                    (value_start, cursor)
                }
                None => (cursor, cursor),
            };
            if let Some(_name) = name {
                attrs.push(HtmlUrlAttribute {
                    value_start,
                    value_end,
                    #[cfg(test)]
                    name: _name,
                });
            }
        }

        pos = if (!fail_closed_after_foreign_content || !saw_foreign_content)
            && is_raw_text_tag(tag)
        {
            skip_raw_text_element(lower, cursor, tag)
        } else {
            cursor.max(open + 1)
        };
        if fail_closed_after_foreign_content && !self_closing && matches!(tag, "svg" | "math") {
            saw_foreign_content = true;
        }
    }

    attrs
}

/// Historical lexical sanitizer retained for focused regression tests. It is
/// not a security boundary: untrusted Markdown raw HTML is made inert by AST
/// replacement, and standalone remote HTML never enters the rich renderer.
#[cfg(test)]
fn sanitize_untrusted_html_urls(
    source: &str,
    allow_external_links: bool,
    allow_external_resources: bool,
) -> String {
    let lower = source.to_ascii_lowercase();
    let mut out = String::with_capacity(source.len());
    let mut pos = 0usize;
    for attr in html_url_attributes(&lower, true) {
        let value = source[attr.value_start..attr.value_end].trim();
        let value_lower = value.to_ascii_lowercase();
        let is_web = ["http:", "https:"]
            .iter()
            .any(|prefix| value_lower.starts_with(prefix));
        let replacement = match attr.name {
            "href" if allow_external_links && value.starts_with('#') => {
                &source[attr.value_start..attr.value_end]
            }
            "href"
                if allow_external_links
                    && (is_web
                        || ["mailto:", "tel:"]
                            .iter()
                            .any(|prefix| value_lower.starts_with(prefix))) =>
            {
                &source[attr.value_start..attr.value_end]
            }
            "src" | "poster" if allow_external_resources && is_web => {
                &source[attr.value_start..attr.value_end]
            }
            "href" => "#",
            _ => "about:blank",
        };
        out.push_str(&source[pos..attr.value_start]);
        out.push_str(replacement);
        pos = attr.value_end;
    }
    out.push_str(&source[pos..]);
    out
}

/// Legacy test helper for the former remote-HTML preview path.
#[cfg(test)]
pub(super) fn sanitize_remote_html_urls(source: &str) -> String {
    sanitize_untrusted_html_urls(source, true, false)
}

/// 一个属性值:本地目标转 `file://`,其余原样(排除清单同原版正则)。
fn rewrite_html_value(value: &str, base_dir: &Path) -> String {
    const SKIP: [&str; 8] = [
        "http:",
        "https:",
        "data:",
        "blob:",
        "mailto:",
        "tel:",
        "javascript:",
        "file:",
    ];
    let target = value.trim();
    let lower = target.to_ascii_lowercase();
    if target.is_empty() || target.starts_with('#') || SKIP.iter().any(|p| lower.starts_with(p)) {
        return value.to_string();
    }
    match resolve_image_src(target, base_dir) {
        MdImageSrc::Local(path) => to_file_url(&path).unwrap_or_else(|| value.to_string()),
        _ => value.to_string(),
    }
}

#[cfg(test)]
#[path = "html_urls_tests.rs"]
mod tests;
