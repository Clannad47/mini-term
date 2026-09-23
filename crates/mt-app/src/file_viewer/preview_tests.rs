use super::*;

#[test]
fn 远程图片必须先获批准_本地图片保持自动加载() {
    assert!(!markdown_image_can_load(true, false));
    assert!(markdown_image_can_load(true, true));
    assert!(markdown_image_can_load(false, false));
}

#[test]
fn svg_判定_不被查询串骗到() {
    // 徽章 URL 常带 `?style=`,扩展名只看路径那一截
    assert!(is_svg_target(
        "https://img.shields.io/badge/a-b.svg?style=flat"
    ));
    assert!(is_svg_target("D:\\icons\\a.SVG"));
    assert!(!is_svg_target("https://x.dev/a.png"));
    assert!(!is_svg_target("a/b.svg.png"), "只看最后一段扩展名");
}
