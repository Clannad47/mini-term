; 自写(svelte 语法 crate 自带的查询只覆盖 svelte 特有记号,标签/属性那部分靠
; nvim 的 `; inherits: html` 继承机制,组件库没有这机制,这里把两半合在一份里)。
; 语法: tree-sitter-grammars/tree-sitter-svelte (crate tree-sitter-svelte-ng 1.0),
; 它是 tree-sitter-html 的派生,标签/属性节点名与组件库 html/highlights.scm 一致。

; ---- 标签与属性(照抄组件库 html 那份)----
(tag_name) @tag
(erroneous_end_tag_name) @tag
(doctype) @constant
(attribute_name) @attribute
(attribute_value) @string
(comment) @comment

[
  "<"
  ">"
  "</"
  "/>"
  "<!"
] @punctuation.bracket

; ---- svelte 块标签 {#if} {:else} {/each} {@html} {@render} ----
[
  "as"
  "key"
  "html"
  "snippet"
  "render"
  "if"
  "else"
  "else if"
  "then"
  "each"
  "await"
  "catch"
  "debug"
  "const"
] @keyword

(snippet_name) @function

[
  "{"
  "}"
] @punctuation.bracket

[
  "#"
  ":"
  "/"
  "@"
] @punctuation.special
