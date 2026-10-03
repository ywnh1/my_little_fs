//! 块的存储编码：把原始字节压成磁盘上的形态，以及反向还原。
//!
//! # 磁盘格式
//!
//! 每个块文件是 `[1 字节编码标签][载荷]`。标签的数值固定、与 feature 无关，
//! 所以「用 A 后端写的库，被只编译了 B 后端的程序读到」时，会得到一条明确的
//! 「这个块用了 XX 后端，本构建未启用」错误，而不是乱码或静默失败。
//!
//! # 为什么不用魔数嗅探
//!
//! 早先的实现靠 zstd 魔数（4 字节）猜数据有没有被压缩。多后端下这条路走不通：
//! gzip 的魔数只有 2 字节，未压缩数据撞上的概率是 1/65536 —— 不能接受；
//! brotli 更是压根没有魔数。一个显式标签把这些不确定性一次消掉，
//! 顺带也就省掉了「解压前先偷看 4 个字节」的那套逻辑。
//!
//! # 标签分配
//!
//! 数值一旦发布就属于磁盘格式，不要改动。未启用的后端保留自己的数值，
//! 这样以后启用它时能直接读回旧数据；新增后端一律往后排。

use std::io;

/// 块在磁盘上使用的编码方式。
///
/// 带 `cfg` 的变体只在对应 feature 启用时存在，但他们的**数值是固定的**
/// —— 这正是「未启用也能报出后端名字」的前提，见 [`Codec::tag_name`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Codec {
    /// 原样存储
    None = 0,
    /// zstd（feature `zstd`）
    #[cfg(feature = "zstd")]
    Zstd = 1,
    /// gzip，即 deflate 流加 gzip 封装与 CRC32（feature `gzip`）
    #[cfg(feature = "gzip")]
    Gzip = 2,
    /// brotli（feature `brotli`）
    #[cfg(feature = "brotli")]
    Brotli = 3,
}

impl Codec {
    /// 标签数值对应的后端名字。
    ///
    /// 刻意**不依赖 feature**：正是为了让「没编译进来的后端」也能在错误信息里
    /// 报出真名，而不是一句含糊的「未知编码」。
    #[must_use]
    pub fn tag_name(tag: u8) -> &'static str {
        match tag {
            0 => "未压缩",
            1 => "zstd",
            2 => "gzip",
            3 => "brotli",
            // 4 / 5 预留给 lz4 与 snappy：数值先占好，将来加后端时直接启用，
            // 不用动已有的磁盘格式
            4 => "lz4",
            5 => "snappy",
            _ => "未知编码",
        }
    }

    /// 把磁盘上的标签转成本构建能处理的编码。
    ///
    /// 标签合法但对应后端没编译进来时返回 [`io::ErrorKind::Unsupported`]，
    /// 并且指名道姓说清是哪个后端。
    fn from_tag(tag: u8) -> io::Result<Self> {
        match tag {
            0 => Ok(Self::None),
            #[cfg(feature = "zstd")]
            1 => Ok(Self::Zstd),
            #[cfg(feature = "gzip")]
            2 => Ok(Self::Gzip),
            #[cfg(feature = "brotli")]
            3 => Ok(Self::Brotli),
            other => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "这个块以 {} 存储（标签 {other}），但本构建未启用对应的 feature",
                    Self::tag_name(other)
                ),
            )),
        }
    }
}

/// 压缩设置：用哪个后端、什么级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Compress {
    /// 写新的块时使用哪种编码
    pub codec: Codec,
    /// 级别。**语义归各后端所有**：gzip 是 0-9，brotli 是 0-11，
    /// zstd 是 1-22。lz4 / snappy 没有级别，这个值会被忽略。
    /// 超出范围的值会被夹到合法区间，而不是引起 panic。
    pub level: i32,
}

/// 把级别夹进后端能接受的区间，免得用户的笔误变成 panic。
#[cfg(feature = "compress")]
fn clamp_level(level: i32, lo: i32, hi: i32) -> i32 {
    level.clamp(lo, hi)
}

impl Compress {
    /// zstd，级别 1-22（越大压得越狠、越慢）。
    #[cfg(feature = "zstd")]
    #[must_use]
    pub fn zstd(level: i32) -> Self {
        Self {
            codec: Codec::Zstd,
            level: clamp_level(level, -7, 22),
        }
    }

    /// gzip，级别 0-9（6 是常用默认）。
    #[cfg(feature = "gzip")]
    #[must_use]
    pub fn gzip(level: i32) -> Self {
        Self {
            codec: Codec::Gzip,
            level: clamp_level(level, 0, 9),
        }
    }

    /// brotli，级别 0-11（11 压得最狠也最慢）。
    #[cfg(feature = "brotli")]
    #[must_use]
    pub fn brotli(level: i32) -> Self {
        Self {
            codec: Codec::Brotli,
            level: clamp_level(level, 0, 11),
        }
    }
}

/// 给载荷加上编码标签，拼成磁盘上的形态。
fn with_tag(codec: Codec, mut payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(codec as u8);
    out.append(&mut payload);
    out
}

/// 把原始字节编码成「标签 + 载荷」。
///
/// `compress` 为 `None` 时按 [`Codec::None`] 原样存储。
/// 若压缩结果反而更大（数据不可压缩），自动退回原样存储 ——
/// 调用方不需要关心这种退化。
pub(crate) fn encode(data: Vec<u8>, compress: Option<Compress>) -> io::Result<Vec<u8>> {
    let Some(compress) = compress else {
        return Ok(with_tag(Codec::None, data));
    };

    let packed = pack(&data, compress.codec, compress.level)?;

    // 压不动的数据（随机字节、已经压过的内容）经后端的开销反而会变大，
    // 这种时候老老实实存原始字节。
    Ok(if packed.len() < data.len() {
        with_tag(compress.codec, packed)
    } else {
        with_tag(Codec::None, data)
    })
}

/// 用指定后端把 `data` 压成载荷（不含标签）。
///
/// 每个臂都返回真实值，所以本构建没编译进任何后端时，`match` 只有
/// [`Codec::None`] 一个臂也不会退化成发散类型 —— 那种情况下
/// [`Compress`] 的构造器压根不存在，调用方拿不到别的编码。
fn pack(
    data: &[u8],
    codec: Codec,
    #[cfg_attr(not(feature = "compress"), allow(unused_variables))] level: i32,
) -> io::Result<Vec<u8>> {
    match codec {
        Codec::None => Ok(data.to_vec()),

        #[cfg(feature = "zstd")]
        Codec::Zstd => zstd::encode_all(data, level),

        #[cfg(feature = "gzip")]
        Codec::Gzip => {
            use std::io::Write;

            let mut enc =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level as u32));
            enc.write_all(data)?;
            enc.finish()
        }

        #[cfg(feature = "brotli")]
        Codec::Brotli => {
            let params = brotli::enc::BrotliEncoderParams {
                quality: level,
                ..Default::default()
            };
            let mut out = Vec::new();
            brotli::BrotliCompress(&mut &data[..], &mut out, &params)?;
            Ok(out)
        }
    }
}

/// 把磁盘上的「标签 + 载荷」还原成原始字节。
///
/// 载荷为空（例如长度 0 的哨兵块）时返回空内容。
pub(crate) fn decode(raw: Vec<u8>) -> io::Result<Vec<u8>> {
    let Some((&tag, payload)) = raw.split_first() else {
        return Ok(Vec::new());
    };

    match Codec::from_tag(tag)? {
        Codec::None => Ok(payload.to_vec()),

        #[cfg(feature = "zstd")]
        Codec::Zstd => zstd::decode_all(payload),

        #[cfg(feature = "gzip")]
        Codec::Gzip => {
            let mut out = Vec::new();
            io::copy(&mut flate2::read::GzDecoder::new(payload), &mut out)?;
            Ok(out)
        }

        #[cfg(feature = "brotli")]
        Codec::Brotli => {
            let mut out = Vec::new();
            brotli::BrotliDecompress(&mut &payload[..], &mut out)?;
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个已启用的后端：编码再解码应当拿回原内容，且带上了正确的标签。
    #[cfg(feature = "compress")]
    fn assert_roundtrip(compress: Compress, payload: Vec<u8>) {
        let encoded = encode(payload.clone(), Some(compress)).unwrap();
        assert_eq!(
            encoded[0], compress.codec as u8,
            "编码结果应当以所选后端的标签开头"
        );
        assert_eq!(decode(encoded).unwrap(), payload, "解码后应当与原文一致");
    }

    /// 反复重复、很容易压的字节
    #[cfg(feature = "compress")]
    fn compressible() -> Vec<u8> {
        b"abcdefgh".repeat(4096)
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_roundtrips() {
        assert_roundtrip(Compress::zstd(3), compressible());
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_roundtrips() {
        assert_roundtrip(Compress::gzip(6), compressible());
    }

    #[cfg(feature = "brotli")]
    #[test]
    fn brotli_roundtrips() {
        assert_roundtrip(Compress::brotli(5), compressible());
    }

    #[test]
    fn unset_compression_stores_verbatim() {
        let payload = b"raw bytes".to_vec();
        let encoded = encode(payload.clone(), None).unwrap();
        assert_eq!(encoded[0], Codec::None as u8);
        assert_eq!(&encoded[1..], &payload[..]);
        assert_eq!(decode(encoded).unwrap(), payload);
    }

    #[test]
    fn incompressible_data_falls_back_to_verbatim() {
        // 近似随机的字节，任何通用压缩都只会让它变大
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let payload: Vec<u8> = (0..8192)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();

        // 每个已启用的后端都要能把这种情况退回去。
        // 没有任何后端时这里就是个空列表，循环不执行。
        #[allow(unused_mut)]
        let mut backends: Vec<Compress> = Vec::new();
        #[cfg(feature = "zstd")]
        backends.push(Compress::zstd(3));
        #[cfg(feature = "gzip")]
        backends.push(Compress::gzip(9));
        #[cfg(feature = "brotli")]
        backends.push(Compress::brotli(11));

        for compress in backends {
            let encoded = encode(payload.clone(), Some(compress)).unwrap();
            assert_eq!(
                encoded[0],
                Codec::None as u8,
                "{:?} 对压不动的数据应当退回原样存储",
                compress.codec
            );
            assert_eq!(decode(encoded).unwrap(), payload);
        }
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn every_backend_and_verbatim_can_coexist_in_one_directory() {
        // 同一份逻辑数据用不同后端各存一份，互相都能读回来
        let payload = compressible();
        let mut blobs = vec![encode(payload.clone(), None).unwrap()];

        #[cfg(feature = "zstd")]
        blobs.push(encode(payload.clone(), Some(Compress::zstd(3))).unwrap());
        #[cfg(feature = "brotli")]
        blobs.push(encode(payload.clone(), Some(Compress::brotli(5))).unwrap());
        blobs.push(encode(payload.clone(), Some(Compress::gzip(6))).unwrap());

        for (i, blob) in blobs.iter().enumerate() {
            let tag = blob[0];
            assert_eq!(
                decode(blob.clone()).unwrap(),
                payload,
                "第 {i} 份（标签 {tag} = {}）解出来不对",
                Codec::tag_name(tag)
            );
        }
    }

    #[test]
    fn empty_payload_decodes_to_empty() {
        assert_eq!(decode(Vec::new()).unwrap(), Vec::new());
        let encoded = encode(Vec::new(), None).unwrap();
        assert_eq!(encoded, vec![Codec::None as u8]);
        assert_eq!(decode(encoded).unwrap(), Vec::new());
    }

    #[test]
    fn tag_names_are_stable_regardless_of_features() {
        // 数值属于磁盘格式，改名会改变已写数据的可读性
        assert_eq!(Codec::tag_name(0), "未压缩");
        assert_eq!(Codec::tag_name(1), "zstd");
        assert_eq!(Codec::tag_name(2), "gzip");
        assert_eq!(Codec::tag_name(3), "brotli");
        assert_eq!(Codec::tag_name(99), "未知编码");
    }

    #[test]
    fn unknown_tag_is_a_clear_error() {
        let err = decode(vec![200, 1, 2, 3]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(
            err.to_string().contains("未知编码"),
            "错误信息应当指出标签不认识：{err}"
        );
    }

    /// 没启用某个后端时，读到它的块必须报出**后端名字**而不是乱码。
    #[cfg(not(feature = "zstd"))]
    #[test]
    fn disabled_backend_reports_its_name() {
        let err = decode(vec![1, 0, 0, 0]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(
            err.to_string().contains("zstd"),
            "错误信息应当点名 zstd：{err}"
        );
    }

    #[test]
    fn levels_out_of_range_are_clamped_not_panicking() {
        #[cfg(feature = "zstd")]
        assert_eq!(Compress::zstd(9999).level, 22);
        #[cfg(feature = "gzip")]
        {
            assert_eq!(Compress::gzip(-5).level, 0);
            assert_eq!(Compress::gzip(100).level, 9);
        }
        #[cfg(feature = "brotli")]
        assert_eq!(Compress::brotli(99).level, 11);
    }
}
