use super::*;

#[test]
fn 本地预览读取只接受限额内普通文件() {
    let dir = std::env::temp_dir().join(format!("mt-preview-http-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);

    let small = dir.join("small.png");
    std::fs::write(&small, b"small-image").unwrap();
    assert_eq!(
        fetch_local_preview_bytes(&small).unwrap().as_slice(),
        b"small-image"
    );
    assert!(
        fetch_local_preview_bytes(&dir).is_err(),
        "目录不得作为预览资源读取"
    );

    let oversized = dir.join("oversized.png");
    let file = std::fs::File::create(&oversized).unwrap();
    file.set_len(PREVIEW_IMAGE_MAX_BYTES + 1).unwrap();
    drop(file);
    assert!(
        fetch_local_preview_bytes(&oversized).is_err(),
        "超过硬上限的稀疏文件必须在读取前拒绝"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
