//! 端到端测试：从真实文件进，从真实文件出。
//!
//! 这里刻意只走公开 API（`copy_in` / `copy_out` / `get` / `remove` …），
//! 不碰任何 crate 内部结构，用来钉住「用户实际会遇到的路径」。

use my_little_fs::prelude::*;
use std::path::{Path, PathBuf};

/// 用 xorshift 生成近似不可压缩的字节流（不引入额外依赖）。
fn pseudo_random(len: usize) -> Vec<u8> {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// 分块参数调小，让几万字节的数据也能切出多个块。
fn small_chunks(builder: FsBuilder) -> FsBuilder {
    builder
        .with_min_size(Some(1024))
        .with_avg_size(Some(4096))
        .with_max_size(Some(16384))
}

fn new_fs(root: &Path) -> Fs {
    small_chunks(Fs::builder(root.to_path_buf()))
        .build()
        .unwrap()
}

/// 数据目录里一共有多少个块文件。
fn blob_count(root: &Path) -> usize {
    fn walk(dir: &Path, n: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, n);
            } else {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(&root.join("data"), &mut n);
    n
}

/// 数据目录里所有块文件的总字节数。
#[cfg(feature = "zstd")]
fn blob_bytes(root: &Path) -> u64 {
    fn walk(dir: &Path, n: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, n);
            } else if let Ok(meta) = path.metadata() {
                *n += meta.len();
            }
        }
    }
    let mut n = 0;
    walk(&root.join("data"), &mut n);
    n
}

/// 把内容写进一个真实文件，返回它的路径。
fn write_src(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path
}

/// 把 `id` 的最新版本导出到 `name`，返回读回来的字节。
fn read_back(fs: &Fs, dir: &Path, id: &str, name: &str) -> Vec<u8> {
    let file = fs.get(id, Index::Latest).unwrap().remove(0);
    let dst = dir.join(name);
    fs.copy_out(&dst, file).unwrap();
    std::fs::read(&dst).unwrap()
}

#[test]
fn roundtrips_through_a_brand_new_root() {
    // 回归点：曾经因为不创建 data 子目录，全新 root 上的第一次 copy_in 就失败
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    let content = pseudo_random(5000);
    let src = write_src(dir.path(), "in.bin", &content);

    fs.copy_in(&src, "doc").unwrap();

    assert_eq!(read_back(&fs, dir.path(), "doc", "out.bin"), content);
}

#[test]
fn roundtrips_an_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    let src = write_src(dir.path(), "empty.bin", b"");

    fs.copy_in(&src, "empty").unwrap();

    let mut file = fs.get("empty", Index::Latest).unwrap().remove(0);
    assert_eq!(file.get_size(), 0);
    let dst = dir.path().join("out.bin");
    fs.copy_out(&dst, file).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"");
}

#[test]
fn roundtrips_a_multi_chunk_file() {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    // 远大于 avg_size，必然切出多块
    let content = pseudo_random(200_000);
    let src = write_src(dir.path(), "big.bin", &content);

    fs.copy_in(&src, "big").unwrap();

    assert!(blob_count(dir.path()) > 1, "应当切出了多个块");
    assert_eq!(read_back(&fs, dir.path(), "big", "out.bin"), content);
}

#[test]
fn reading_does_not_depend_on_the_process_working_directory() {
    // 回归点：块路径曾经是相对路径，只有进程 CWD 恰好等于数据目录时才读得到
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    let content = pseudo_random(4000);
    let src = write_src(dir.path(), "in.bin", &content);
    fs.copy_in(&src, "doc").unwrap();

    let data_dir = dir.path().join("data");
    assert_ne!(
        std::env::current_dir().unwrap(),
        data_dir,
        "测试前提：当前工作目录不是数据目录"
    );
    assert_eq!(read_back(&fs, dir.path(), "doc", "out.bin"), content);
}

#[test]
fn identical_content_is_stored_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    let content = pseudo_random(20_000);
    let a = write_src(dir.path(), "a.bin", &content);
    let b = write_src(dir.path(), "b.bin", &content);

    fs.copy_in(&a, "a").unwrap();
    let after_first = blob_count(dir.path());
    assert!(after_first > 0);

    fs.copy_in(&b, "b").unwrap();
    assert_eq!(
        blob_count(dir.path()),
        after_first,
        "内容相同的两个文件不应产生新的块"
    );

    // 两份都还能正确读回
    assert_eq!(read_back(&fs, dir.path(), "a", "a.out"), content);
    assert_eq!(read_back(&fs, dir.path(), "b", "b.out"), content);
}

#[test]
fn shared_chunks_survive_deleting_one_of_the_files() {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    let content = pseudo_random(20_000);
    let a = write_src(dir.path(), "a.bin", &content);
    let b = write_src(dir.path(), "b.bin", &content);
    fs.copy_in(&a, "a").unwrap();
    fs.copy_in(&b, "b").unwrap();

    // 删掉其中一个，另一个仍然必须能完整读回
    assert!(fs.remove("a").unwrap());
    fs.release().unwrap();
    assert_eq!(read_back(&fs, dir.path(), "b", "b.out"), content);
}

#[test]
fn history_accumulates_and_can_be_rolled_back() {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    let v1 = b"the first revision".to_vec();
    let v2 = b"the second revision".to_vec();
    let p1 = write_src(dir.path(), "v1.bin", &v1);
    let p2 = write_src(dir.path(), "v2.bin", &v2);

    fs.copy_in(&p1, "doc").unwrap();
    fs.copy_in(&p2, "doc").unwrap();

    let all = fs.get("doc", Index::All).unwrap();
    assert_eq!(all.len(), 2, "两次导入应留下两个历史版本");

    let read = |file: my_little_fs::prelude::FsFile| {
        let mut file = file;
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut buf).unwrap();
        buf
    };
    assert_eq!(read(all[0].clone()), v1);
    assert_eq!(read(all[1].clone()), v2);
    assert_eq!(read(fs.get("doc", Index::First).unwrap().remove(0)), v1);
    assert_eq!(read(fs.get("doc", Index::Latest).unwrap().remove(0)), v2);
}

#[test]
fn rename_keeps_the_content_reachable() {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    let content = pseudo_random(3000);
    let src = write_src(dir.path(), "in.bin", &content);
    fs.copy_in(&src, "before").unwrap();

    fs.rename("before", "after").unwrap();

    assert_eq!(read_back(&fs, dir.path(), "after", "out.bin"), content);
    assert!(fs.get("before", Index::Latest).is_err());
}

#[test]
fn copy_in_many_imports_every_pair() {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    let one = pseudo_random(2000);
    let two = pseudo_random(3000);
    let p1 = write_src(dir.path(), "one.bin", &one);
    let p2 = write_src(dir.path(), "two.bin", &two);

    fs.copy_in_many([("one", &p1), ("two", &p2)]).unwrap();

    assert_eq!(read_back(&fs, dir.path(), "one", "one.out"), one);
    assert_eq!(read_back(&fs, dir.path(), "two", "two.out"), two);
}

#[test]
fn list_file_reports_exactly_the_imported_ids() {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    let src = write_src(dir.path(), "in.bin", b"payload");
    fs.copy_in(&src, "alpha").unwrap();
    fs.copy_in(&src, "beta").unwrap();

    let mut ids = fs.list_file().unwrap();
    ids.sort();
    assert_eq!(ids, vec!["alpha", "beta"]);
}

#[test]
fn remove_and_release_free_the_disk_space() {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    let content = pseudo_random(20_000);
    let src = write_src(dir.path(), "in.bin", &content);
    fs.copy_in(&src, "doc").unwrap();
    let before = blob_count(dir.path());
    assert!(before > 0);

    assert!(fs.remove("doc").unwrap());
    assert_eq!(fs.release().unwrap(), before, "引用归零后应回收全部块");
    assert_eq!(blob_count(dir.path()), 0);
    assert!(fs.list_file().unwrap().is_empty());
}

#[test]
fn remove_history_keeps_the_other_versions_readable() {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    let keep = pseudo_random(5000);
    let drop = pseudo_random(6000);
    let p1 = write_src(dir.path(), "keep.bin", &keep);
    let p2 = write_src(dir.path(), "drop.bin", &drop);
    fs.copy_in(&p1, "doc").unwrap();
    fs.copy_in(&p2, "doc").unwrap();

    let removed = fs.remove_history("doc", Index::Latest).unwrap();
    assert_eq!(removed.len(), 1);
    fs.release().unwrap();

    assert_eq!(read_back(&fs, dir.path(), "doc", "out.bin"), keep);
}

/// 需要 `zstd` feature：没有后端时 `with_compress` 根本不存在。
#[cfg(feature = "zstd")]
#[test]
fn compression_shrinks_compressible_data_and_never_inflates_incompressible_data() {
    let dir = tempfile::tempdir().unwrap();
    let fs = small_chunks(Fs::builder(dir.path().to_path_buf()))
        .with_compress(Some(Compress::zstd(3)))
        .build()
        .unwrap();

    // 高度可压缩：一万个相同字节
    let compressible = vec![0u8; 50_000];
    let src = write_src(dir.path(), "zeros.bin", &compressible);
    fs.copy_in(&src, "zeros").unwrap();
    let stored = blob_bytes(dir.path());
    assert!(
        stored < compressible.len() as u64 / 2,
        "可压缩数据应当明显变小：{stored} vs {}",
        compressible.len()
    );
    assert_eq!(
        read_back(&fs, dir.path(), "zeros", "zeros.out"),
        compressible
    );

    // 近乎随机：压不动。压缩不应让它反而变大
    let dir2 = tempfile::tempdir().unwrap();
    let fs2 = small_chunks(Fs::builder(dir2.path().to_path_buf()))
        .with_compress(Some(Compress::zstd(3)))
        .build()
        .unwrap();
    let incompressible = pseudo_random(40_000);
    let src2 = write_src(dir2.path(), "rand.bin", &incompressible);
    fs2.copy_in(&src2, "rand").unwrap();
    let stored2 = blob_bytes(dir2.path());
    // 每个块会多出 1 字节的编码标签，这是磁盘格式的固定开销，不算「被放大」
    let tag_overhead = blob_count(dir2.path()) as u64;
    assert!(
        stored2 <= incompressible.len() as u64 + tag_overhead,
        "不可压缩数据不应被压缩放大（只允许每块 1 字节标签）：{stored2} vs {} + {tag_overhead}",
        incompressible.len()
    );
    assert_eq!(
        read_back(&fs2, dir2.path(), "rand", "rand.out"),
        incompressible
    );
}

#[cfg(feature = "zstd")]
#[test]
fn compressed_and_uncompressed_blobs_coexist_in_one_data_directory() {
    // 压缩设置中途改变时，老块（未压缩）与新块（压缩）必须都能读
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let content = vec![7u8; 40_000];
    let other = vec![9u8; 40_000];
    let src = write_src(dir.path(), "a.bin", &content);
    let src2 = write_src(dir.path(), "b.bin", &other);

    {
        let fs = small_chunks(Fs::builder(root.clone())).build().unwrap();
        fs.copy_in(&src, "plain").unwrap();
    }
    {
        let fs = small_chunks(Fs::builder(root.clone()))
            .with_compress(Some(Compress::zstd(3)))
            .build()
            .unwrap();
        fs.copy_in(&src2, "packed").unwrap();
    }
    // 关掉上一个实例后才能重新打开同一个根目录（redb 会锁住数据库文件）
    {
        let fs = small_chunks(Fs::builder(root.clone())).build().unwrap();
        // 老块（未压缩）与新块（压缩）都能读回
        assert_eq!(read_back(&fs, dir.path(), "plain", "plain.out"), content);
        assert_eq!(read_back(&fs, dir.path(), "packed", "packed.out"), other);
    }
}

#[test]
fn two_fs_instances_on_the_same_root_see_the_same_data() {
    let dir = tempfile::tempdir().unwrap();
    let content = pseudo_random(8000);
    let src = write_src(dir.path(), "in.bin", &content);

    {
        let fs = new_fs(dir.path());
        fs.copy_in(&src, "doc").unwrap();
    }
    // 重新打开同一个根目录（模拟进程重启）
    let fs = new_fs(dir.path());
    assert_eq!(read_back(&fs, dir.path(), "doc", "out.bin"), content);
}

#[test]
fn seeking_within_a_multi_chunk_file_reads_the_right_slice() {
    use std::io::{Read, Seek, SeekFrom};

    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    let content = pseudo_random(60_000);
    let src = write_src(dir.path(), "in.bin", &content);
    fs.copy_in(&src, "doc").unwrap();

    let mut file = fs.get("doc", Index::Latest).unwrap().remove(0);
    // 从块边界附近切一段出来
    file.seek(SeekFrom::Start(10_000)).unwrap();
    let mut slice = vec![0u8; 25_000];
    file.read_exact(&mut slice).unwrap();
    assert_eq!(slice, content[10_000..35_000]);
}

/// 各后端写进同一个数据目录，之后用一个**不带压缩设置**的实例全部读回。
///
/// 这正是多后端共存的意义：块自带编码标签，读的一方不需要预先知道
/// 它当初是用哪个后端压的。
///
/// 没有任何压缩后端时 `with_compress` 不存在，这个测试没有意义。
#[cfg(feature = "compress")]
#[test]
fn blobs_written_by_different_backends_coexist_and_all_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    // 每个后端配一份不同的内容，免得互相被去重掉（内容相同只会存第一份）
    let mut cases: Vec<(&str, Vec<u8>, Option<Compress>)> =
        vec![("plain", vec![1u8; 40_000], None)];
    #[cfg(feature = "zstd")]
    cases.push(("zstd", vec![2u8; 41_000], Some(Compress::zstd(3))));
    #[cfg(feature = "gzip")]
    cases.push(("gzip", vec![3u8; 42_000], Some(Compress::gzip(6))));
    #[cfg(feature = "brotli")]
    cases.push(("brotli", vec![4u8; 43_000], Some(Compress::brotli(5))));

    let mut expected = Vec::new();
    for (id, content, compress) in cases {
        let src = dir.path().join(format!("{id}.bin"));
        std::fs::write(&src, &content).unwrap();
        // 写入用的后端由 builder 决定，所以每个后端各开一次实例
        {
            let builder = small_chunks(Fs::builder(root.clone()));
            let fs = builder.with_compress(compress).build().unwrap();
            fs.copy_in(&src, id).unwrap();
        }
        expected.push((id, content));
    }

    // 重新打开时**不设任何压缩**：读到什么后端就用什么后端解
    let fs = small_chunks(Fs::builder(root.clone())).build().unwrap();
    for (id, content) in expected {
        let mut file = fs.get(id, Index::Latest).unwrap().remove(0);
        let mut got = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut got).unwrap();
        assert_eq!(got, content, "id = {id} 的内容读回来不一致");
    }
}
