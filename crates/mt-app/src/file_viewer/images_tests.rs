use super::*;
use crate::file_viewer::markdown::MdImage;

// ─── 图片资源的持有账本与换代去抖 ─────────────────────────────

#[test]
fn 持有账本_最后一个持有者走了才放() {
    let mut table: HoldTable<&str, u32> = HoldTable::default();
    // 看图页签与 README 预览挂着同一张图
    table.acquire("a.png");
    table.acquire("a.png");
    table.note(&"a.png", 7);
    assert_eq!(table.holders(&"a.png"), 2);

    // 先关一个:另一个还在用,不放
    assert_eq!(table.release(&"a.png"), Release::Shared);
    assert_eq!(table.holders(&"a.png"), 1);
    // 再关最后一个:放,带着最近记下的那份位图
    assert_eq!(table.release(&"a.png"), Release::Last(Some(7)));
    assert_eq!(table.holders(&"a.png"), 0);

    // 账已销:再放一次(不该发生)按「别人的图」处理,不能二次放
    assert_eq!(table.release(&"a.png"), Release::Shared);
}

#[test]
fn 持有账本_没取到过位图也照常放缓存条目() {
    let mut table: HoldTable<&str, u32> = HoldTable::default();
    // 还在解码 / 解不出来:没 note 过,放的时候只摘缓存条目、没有纹理可摘
    table.acquire("broken.png");
    assert_eq!(table.release(&"broken.png"), Release::Last(None));
}

#[test]
fn 持有账本_没登记的_key_不记值() {
    let mut table: HoldTable<&str, u32> = HoldTable::default();
    table.note(&"ghost.png", 1);
    assert_eq!(table.holders(&"ghost.png"), 0);
    assert_eq!(table.take_latest(&"ghost.png"), None);
}

#[test]
fn 持有账本_换代只取走位图_持有者不变() {
    let mut table: HoldTable<&str, u32> = HoldTable::default();
    table.acquire("a.png");
    table.acquire("a.png");
    table.note(&"a.png", 1);
    // 磁盘上的图改了:旧位图交出去摘纹理,两个页签仍然挂着这张图
    assert_eq!(table.take_latest(&"a.png"), Some(1));
    assert_eq!(table.take_latest(&"a.png"), None, "取走一次就没了");
    assert_eq!(table.holders(&"a.png"), 2);
    // 重解出来的新位图照常记账,最后一个走的时候放的是新的
    table.note(&"a.png", 2);
    assert_eq!(table.release(&"a.png"), Release::Shared);
    assert_eq!(table.release(&"a.png"), Release::Last(Some(2)));
}

#[test]
fn 换代去抖_只认最后一张票() {
    let mut debounce = ReloadDebounce::default();
    // 一次保存连着报三个 modify 事件
    let first = debounce.bump();
    let second = debounce.bump();
    let last = debounce.bump();
    // 前两个事件的计时即便没被取消、到点了也不动手
    assert!(!debounce.is_current(first));
    assert!(!debounce.is_current(second));
    assert!(debounce.is_current(last));
}

#[test]
fn md_图片资源_与渲染时的_key_同一口径() {
    let image = |url: &str| MdImage {
        url: url.to_string(),
        alt: String::new(),
        title: None,
        link: None,
    };
    let base = Path::new("D:/proj/docs");
    let blocks = vec![
        (
            0.0,
            MdBlock::Images(vec![
                image("shots/main.png"),
                image("https://img.shields.io/badge/x.svg"),
                image("data:image/png;base64,AAAA"),
            ]),
        ),
        // 交给 TextView 的段里的内联图片不归这里管(走组件自己的 img)
        (0.0, MdBlock::Text("![c](inline.png)".into())),
        (0.0, MdBlock::Images(vec![image("shots/main.png")])),
    ];
    let live = md_image_resources(&blocks, base);
    assert_eq!(
        live.len(),
        2,
        "同一张图出现两次只算一个 key,data: 不加载没有 key"
    );
    // 与 render_md_local_image / render_md_remote_image 构 key 的函数逐字一致,
    // 否则重切分块时会把还在文档里的图当成过期的放掉
    assert!(live.contains(&local_image_resource(&base.join("shots/main.png"))));
    assert!(live.contains(&remote_image_resource("https://img.shields.io/badge/x.svg")));
    assert!(!live.contains(&local_image_resource(&base.join("inline.png"))));
}
