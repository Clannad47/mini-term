; 来源: crate tree-sitter-nginx 1.0.1 自带的 queries/highlights.scm (MIT),
; 它的 Rust 绑定没把这份导出成常量,只好抄一份
; https://github.com/opa-oz/tree-sitter-nginx
; 捕获名是 nvim 口径,注册时经 zed_captures 改写

(comment) @comment @spell

(value) @variable

(attribute (keyword) @attribute)

[
  (location_modifier)
  "="
] @operator

[
  (keyword)
  "location"
] @keyword

[
  "if"
  "map"
] @keyword.conditional

(directive (keyword) @constant)

(boolean) @boolean

[
  (auto)
  (constant)
  (level)
  (connection_method)
  (var)
  condition: (condition)
] @variable.builtin

[
  (string_literal)
  (quoted_string_literal)
  (file)
  (mask)
] @string

(directive (variable) @variable.parameter)

(directive (variable (keyword) @variable.parameter))

(location_route) @string.special
";" @punctuation.delimiter

[
  (numeric_literal)
  (time)
  (size)
  (cpumask)
] @number

[
  "{"
  "}"
] @punctuation.bracket
