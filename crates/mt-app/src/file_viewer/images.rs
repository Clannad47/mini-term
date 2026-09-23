//! 查看器图片(看图页签 / md 预览):自己的资源类型、进程级持有账本、看图页签的
//! 换代去抖。gpui 的资源缓存与图集纹理进程级共享、不自动淘汰,放不放由这里的
//! 账本判(理由见 [`HoldTable`])。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::hash::Hash;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;

use gpui::{App, ImageAssetLoader, Resource, Window};

use super::markdown::{MdBlock, MdImageSrc, resolve_image_src};

// ─── 图片(看图页签 / md 预览):进程级缓存按视图计数释放 ─────────────

/// 文件查看器自己的图片资源类型:看图页签与 md 预览里的本地 / 网络图片都走它。
///
/// 加载原样转交 [`ImageAssetLoader`](读盘 / 经 [`super::PreviewHttpClient`] 拉网、解码、
/// svg 栅格化全在里面),**只换资源类型**:gpui 的资源缓存按「资源类型 + key」
/// 进程级共享,而窗口级背景图(`mt_ui::background`)用的正是 `ImageAssetLoader` +
/// `Resource::Path` —— 在文件树里点开当前皮肤的背景图时两边会共用一份缓存,
/// 这边关页签一放,窗口背景就被连带放掉、重解一遍、闪一下。换成自己的类型,
/// 缓存条目只归查看器管,[`ViewerImageHolds`] 的账才算得清。
pub(super) enum ViewerImage {}

impl gpui::Asset for ViewerImage {
    type Source = Resource;
    type Output = <ImageAssetLoader as gpui::Asset>::Output;

    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        <ImageAssetLoader as gpui::Asset>::load(source, cx)
    }
}

/// 本地图片的资源 key。看图页签、md 预览、换代与释放**必须**走同一个构造,
/// 否则账本里的 key 与缓存里的对不上,放不掉。
pub(super) fn local_image_resource(path: &Path) -> Resource {
    Resource::Path(Arc::from(path))
}

/// 网络图片的资源 key(口径同上)。
pub(super) fn remote_image_resource(url: &str) -> Resource {
    Resource::Uri(gpui::SharedUri::from(url.to_string()))
}

/// md 图片落点 → 资源 key;`data:` 之类不加载的没有 key。
fn md_image_resource(src: &MdImageSrc) -> Option<Resource> {
    match src {
        MdImageSrc::Local(path) => Some(local_image_resource(path)),
        MdImageSrc::Remote(url) => Some(remote_image_resource(url)),
        MdImageSrc::Unsupported => None,
    }
}

/// 分块里还引用着的图片资源。远程文档里没获准加载的也算在内 —— 这里只用来判
/// 「哪些已经不在文档里了」,多算一个不会多加载一张。
pub(super) fn md_image_resources(blocks: &[(f32, MdBlock)], base_dir: &Path) -> HashSet<Resource> {
    blocks
        .iter()
        .filter_map(|(_, block)| match block {
            MdBlock::Images(images) => Some(images),
            _ => None,
        })
        .flatten()
        .filter_map(|image| md_image_resource(&resolve_image_src(&image.url, base_dir)))
        .collect()
}

/// 「一个 key 此刻被几个视图用着」的账本,外加最近一次取到的值(对位图是
/// 弱引用:只为放的时候找得到它的图集纹理,不给它续命)。
///
/// 为什么要按视图计数:资源缓存按 key 进程级共享,同一张图可能同时挂在两个
/// 页签上(看图页签 + 引用它的 README 预览,或两篇引用同一张图的文档)。
/// 一个页签关了就直接放的话,另一个不会崩也不会永久空白(依据见
/// [`evict_viewer_image`]),但下一帧 `use_asset` 取不到、从头重解,这一两帧
/// 画的是占位卡片 —— md 列表里整块塌下去再撑回来,滚动位置跟着跳。
/// 所以最后一个持有者走了才真放。
pub(super) struct HoldTable<K, V> {
    entries: HashMap<K, Hold<V>>,
}

struct Hold<V> {
    holders: usize,
    latest: Option<V>,
}

/// [`HoldTable::release`] 的结论。
#[derive(Debug, PartialEq)]
enum Release<V> {
    /// 还有别的持有者 —— 或者这个 key 根本没登记过:宁可漏放,不能放掉别人的图
    Shared,
    /// 最后一个持有者走了,账目已删;带着最近记下的值,调用方据此放资源
    Last(Option<V>),
}

impl<K, V> Default for HoldTable<K, V> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash, V> HoldTable<K, V> {
    /// 多一个持有者。同一个视图对同一个 key 只该登记一次(视图自己记着要过哪些)。
    pub(super) fn acquire(&mut self, key: K) {
        self.entries
            .entry(key)
            .or_insert(Hold {
                holders: 0,
                latest: None,
            })
            .holders += 1;
    }

    /// 记下这个 key 最近一次取到的值。没登记过的不记 —— 没人持有就没人来放。
    pub(super) fn note(&mut self, key: &K, value: V) {
        if let Some(hold) = self.entries.get_mut(key) {
            hold.latest = Some(value);
        }
    }

    /// 取走最近记下的值,持有者计数不动 —— 资源换代(磁盘上的图被改写)时用。
    pub(super) fn take_latest(&mut self, key: &K) -> Option<V> {
        self.entries
            .get_mut(key)
            .and_then(|hold| hold.latest.take())
    }

    /// 少一个持有者。
    fn release(&mut self, key: &K) -> Release<V> {
        let Some(hold) = self.entries.get_mut(key) else {
            return Release::Shared;
        };
        if hold.holders > 1 {
            hold.holders -= 1;
            return Release::Shared;
        }
        Release::Last(self.entries.remove(key).and_then(|hold| hold.latest))
    }

    #[cfg(test)]
    fn holders(&self, key: &K) -> usize {
        self.entries.get(key).map_or(0, |hold| hold.holders)
    }
}

/// 进程级:查看器图片的持有账本。key 是 [`ViewerImage`] 的资源,值是最近画过的
/// 那份位图。
#[derive(Default)]
pub(super) struct ViewerImageHolds(pub(super) HoldTable<Resource, Weak<gpui::RenderImage>>);

impl gpui::Global for ViewerImageHolds {}

/// 撤掉一个视图对这些图的持有;最后一个持有者走了才真放。`window` 的口径同
/// [`super::mermaid::release_mermaid_assets`]。
pub(super) fn release_viewer_images(
    keys: impl IntoIterator<Item = Resource>,
    cx: &mut App,
    mut window: Option<&mut Window>,
) {
    for key in keys {
        if let Release::Last(latest) = cx.default_global::<ViewerImageHolds>().0.release(&key) {
            evict_viewer_image(&key, latest, cx, window.as_deref_mut());
        }
    }
}

/// 把一张图从资源缓存与图集里放掉,不看账本(调用方已经判过该放)。
///
/// 纹理按账本里的弱引用找(还活着就是缓存里那份),**不用 `fetch_asset`**:它对
/// 缓存里没有的 key 会先发起一次加载(读盘 + 整张解码),而「换代后还没重画过」
/// 的 key 正是缓存里没有的。
///
/// 放早了不会崩(gpui-pre 0.3.5):`remove_asset` 只摘缓存条目(`app.rs:2689`),
/// 还要这张图的视图下一帧 `use_asset` 取不到就从头加载、完成后被通知重画
/// (`asset_cache.rs` 的 `CachedLoad::use_by`);`drop_image` 只摘图集条目
/// (`window.rs:4894`),同一份位图再被画到时 `paint_image` 经
/// `get_or_insert_with` 重新上传(`window.rs:4800`)。唯一的雷是**不重画就再呈现**:
/// 窗口不脏时(高频输入下)会把上一帧场景原样再交给渲染器,场景里的图块若
/// 正好是刚摘的、所在图集纹理又随之释放,DirectX 后端取纹理直接 unwrap 到
/// `None`(`directx_atlas.rs` 的 `texture()`)。所以在绘制之外放的,调用方
/// 必须同时把窗口弄脏(关页签时父视图自会重画;换代见 `reload_image`)。
pub(super) fn evict_viewer_image(
    key: &Resource,
    latest: Option<Weak<gpui::RenderImage>>,
    cx: &mut App,
    window: Option<&mut Window>,
) {
    if let Some(image) = latest.and_then(|image| image.upgrade()) {
        cx.drop_image(image, window);
    }
    cx.remove_asset::<ViewerImage>(key);
}

/// 看图页签「磁盘上的图变了」的去抖代次:每来一个事件领一张新票,计时到点时
/// 手上的票还是最新的才动手 —— 一串事件只在最后一个之后重读一次。
#[derive(Default)]
pub(super) struct ReloadDebounce {
    generation: u64,
}

impl ReloadDebounce {
    pub(super) fn bump(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    pub(super) fn is_current(&self, ticket: u64) -> bool {
        self.generation == ticket
    }
}

/// 看图页签的图在磁盘上被改写后,最后一个变更事件过去这么久才重读(见
/// [`super::FileViewer::schedule_image_reload`])。一次保存是一串事件,写到一半就读
/// 只会读到半截文件。
pub(super) const IMAGE_RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);

#[cfg(test)]
#[path = "images_tests.rs"]
mod tests;
