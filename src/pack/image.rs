//! 图集尺寸解析：只读文件头，不解码像素。
//!
//! 两处需要它：`docs/PET-PACK.md` §3.10 的网格反推（真实社区包不写 `frame`），
//! 以及 Codex 的「网格必须精确覆盖图集」校验。

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Read;
use std::path::Path;

/// 探测用的文件头字节数（各格式的尺寸字段都在这之前）。
const PROBE_BYTES: usize = 1024;
/// RIFF 头长度：`RIFF` + 长度字段 + `WEBP`。
const RIFF_HEADER: usize = 12;
/// WebP 分块头长度：fourcc + 长度字段。
const CHUNK_HEADER: usize = 8;
/// PNG 签名。
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
/// PNG `IHDR` 负载偏移（签名 8 + 长度 4 + 类型 4）。
const PNG_IHDR_PAYLOAD: usize = 16;
/// PNG 尺寸字段结束偏移（宽 4 + 高 4）。
const PNG_SIZE_END: usize = 24;
/// `VP8X` 负载内画布尺寸偏移（标志 1 + 保留 3）。
const VP8X_CANVAS: usize = 4;
/// `VP8L` 负载内位字段偏移（签名 0x2f 占 1 字节）。
const VP8L_BITS: usize = 1;
/// `VP8 ` 负载内尺寸偏移（帧标签 3 + 起始码 3）。
const VP8_DIMS: usize = 6;
/// 14 位尺寸掩码。
const DIM_MASK: u32 = 0x3fff;

/// 图集像素尺寸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtlasSize {
    /// 画布宽（像素）。
    pub width: u32,
    /// 画布高（像素）。
    pub height: u32,
}

impl AtlasSize {
    /// 宽高是否都非零。
    pub fn is_valid(self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// 读出图集的真实画布尺寸。
pub fn read_size(path: &Path) -> Result<AtlasSize> {
    let raw = read_head(path)?;
    let size = if raw.starts_with(&PNG_SIGNATURE) {
        png_size(&raw)
    } else if is_webp(&raw) {
        webp_size(&raw)
    } else {
        None
    };
    match size {
        Some(size) if size.is_valid() => Ok(size),
        _ => bail!(
            "无法从图集头部读出尺寸（仅支持 WebP 与 PNG）：{}",
            path.display()
        ),
    }
}

/// 读文件头；文件短于 [`PROBE_BYTES`] 时返回实际读到的字节。
fn read_head(path: &Path) -> Result<Vec<u8>> {
    let mut file =
        fs::File::open(path).with_context(|| format!("打开图集失败：{}", path.display()))?;
    let mut buf = vec![0u8; PROBE_BYTES];
    let mut filled = 0;
    while filled < buf.len() {
        let read = file
            .read(&mut buf[filled..])
            .with_context(|| format!("读取图集失败：{}", path.display()))?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buf.truncate(filled);
    Ok(buf)
}

/// 是否 WebP 容器。
fn is_webp(raw: &[u8]) -> bool {
    raw.len() >= RIFF_HEADER && &raw[0..4] == b"RIFF" && &raw[8..12] == b"WEBP"
}

/// 从 PNG `IHDR` 读尺寸（PNG 整数一律大端）。
fn png_size(raw: &[u8]) -> Option<AtlasSize> {
    if raw.len() < PNG_SIZE_END || &raw[12..16] != b"IHDR" {
        return None;
    }
    let width = read_u32_be(raw, PNG_IHDR_PAYLOAD)?;
    let height = read_u32_be(raw, PNG_IHDR_PAYLOAD + 4)?;
    Some(AtlasSize { width, height })
}

/// 逐个 WebP 分块找尺寸字段。
fn webp_size(raw: &[u8]) -> Option<AtlasSize> {
    let mut offset = RIFF_HEADER;
    while offset + CHUNK_HEADER <= raw.len() {
        let fourcc = raw.get(offset..offset + 4)?;
        let payload_len = read_u32(raw, offset + 4)? as usize;
        let payload = offset + CHUNK_HEADER;
        let size = match fourcc {
            b"VP8X" => vp8x_size(raw, payload),
            b"VP8L" => vp8l_size(raw, payload),
            b"VP8 " => vp8_size(raw, payload),
            _ => None,
        };
        if size.is_some() {
            return size;
        }
        // 分块长度按偶数字节对齐
        offset = payload.checked_add(payload_len + (payload_len & 1))?;
    }
    None
}

/// 扩展格式：画布尺寸为 24 位无符号，存的是「宽高 - 1」。
fn vp8x_size(raw: &[u8], payload: usize) -> Option<AtlasSize> {
    let width = read_u24(raw, payload + VP8X_CANVAS)?;
    let height = read_u24(raw, payload + VP8X_CANVAS + 3)?;
    Some(AtlasSize {
        width: width + 1,
        height: height + 1,
    })
}

/// 无损格式：14 位宽 + 14 位高打包在一个小端 `u32` 里，存的也是「宽高 - 1」。
fn vp8l_size(raw: &[u8], payload: usize) -> Option<AtlasSize> {
    let bits = read_u32(raw, payload + VP8L_BITS)?;
    Some(AtlasSize {
        width: (bits & DIM_MASK) + 1,
        height: ((bits >> 14) & DIM_MASK) + 1,
    })
}

/// 有损格式：两个 14 位小端值，直接是像素尺寸。
fn vp8_size(raw: &[u8], payload: usize) -> Option<AtlasSize> {
    let width = read_u16(raw, payload + VP8_DIMS)? & DIM_MASK as u16;
    let height = read_u16(raw, payload + VP8_DIMS + 2)? & DIM_MASK as u16;
    Some(AtlasSize {
        width: u32::from(width),
        height: u32::from(height),
    })
}

/// 读小端 `u16`。
fn read_u16(raw: &[u8], at: usize) -> Option<u16> {
    let end = at.checked_add(2)?;
    Some(u16::from_le_bytes(raw.get(at..end)?.try_into().ok()?))
}

/// 读小端 `u24`。
fn read_u24(raw: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(3)?;
    let bytes = raw.get(at..end)?;
    Some(u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16))
}

/// 读小端 `u32`。
fn read_u32(raw: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    Some(u32::from_le_bytes(raw.get(at..end)?.try_into().ok()?))
}

/// 读大端 `u32`（PNG 使用）。
fn read_u32_be(raw: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    Some(u32::from_be_bytes(raw.get(at..end)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实社区包 `xunjian-miao` 的图集尺寸（8 列 × 11 行）。
    const V2_ATLAS: (u32, u32) = (1536, 2288);
    /// Codex 内置包的图集尺寸（8 列 × 9 行）。
    const V1_ATLAS: (u32, u32) = (1536, 1872);

    /// 造一个只含 `VP8L` 分块的 WebP 头。
    fn vp8l_head(width: u32, height: u32) -> Vec<u8> {
        let bits: u32 = (width - 1) | ((height - 1) << 14);
        let mut raw = Vec::new();
        raw.extend_from_slice(b"RIFF");
        raw.extend_from_slice(&14u32.to_le_bytes());
        raw.extend_from_slice(b"WEBP");
        raw.extend_from_slice(b"VP8L");
        raw.extend_from_slice(&5u32.to_le_bytes());
        raw.push(0x2f);
        raw.extend_from_slice(&bits.to_le_bytes());
        raw
    }

    /// 造一个只含 `VP8X` 分块的 WebP 头。
    fn vp8x_head(width: u32, height: u32) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend_from_slice(b"RIFF");
        raw.extend_from_slice(&22u32.to_le_bytes());
        raw.extend_from_slice(b"WEBP");
        raw.extend_from_slice(b"VP8X");
        raw.extend_from_slice(&10u32.to_le_bytes());
        raw.push(0x10);
        raw.extend_from_slice(&[0, 0, 0]);
        raw.extend_from_slice(&(width - 1).to_le_bytes()[0..3]);
        raw.extend_from_slice(&(height - 1).to_le_bytes()[0..3]);
        raw
    }

    /// 造一个只含 `VP8 ` 分块的 WebP 头。
    fn vp8_head(width: u32, height: u32) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend_from_slice(b"RIFF");
        raw.extend_from_slice(&20u32.to_le_bytes());
        raw.extend_from_slice(b"WEBP");
        raw.extend_from_slice(b"VP8 ");
        raw.extend_from_slice(&10u32.to_le_bytes());
        raw.extend_from_slice(&[0, 0, 0]);
        raw.extend_from_slice(&[0x9d, 0x01, 0x2a]);
        raw.extend_from_slice(&(width as u16).to_le_bytes());
        raw.extend_from_slice(&(height as u16).to_le_bytes());
        raw
    }

    /// 造一个 PNG 头。
    fn png_head(width: u32, height: u32) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend_from_slice(&PNG_SIGNATURE);
        raw.extend_from_slice(&13u32.to_be_bytes());
        raw.extend_from_slice(b"IHDR");
        raw.extend_from_slice(&width.to_be_bytes());
        raw.extend_from_slice(&height.to_be_bytes());
        raw
    }

    /// 三种 WebP 头部都要能读出正确尺寸。
    #[test]
    fn webp_variants_are_parsed() {
        assert_eq!(
            webp_size(&vp8l_head(V2_ATLAS.0, V2_ATLAS.1)),
            Some(AtlasSize {
                width: V2_ATLAS.0,
                height: V2_ATLAS.1
            })
        );
        assert_eq!(
            webp_size(&vp8x_head(V2_ATLAS.0, V2_ATLAS.1)),
            Some(AtlasSize {
                width: V2_ATLAS.0,
                height: V2_ATLAS.1
            })
        );
        assert_eq!(
            webp_size(&vp8_head(V1_ATLAS.0, V1_ATLAS.1)),
            Some(AtlasSize {
                width: V1_ATLAS.0,
                height: V1_ATLAS.1
            })
        );
    }

    /// PNG 头也要能读出尺寸。
    #[test]
    fn png_is_parsed() {
        assert_eq!(
            png_size(&png_head(V1_ATLAS.0, V1_ATLAS.1)),
            Some(AtlasSize {
                width: V1_ATLAS.0,
                height: V1_ATLAS.1
            })
        );
    }

    /// 非图集数据必须报错，不能猜出尺寸。
    #[test]
    fn unknown_format_is_rejected() {
        let dir = std::env::temp_dir().join("litepet-test-image");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建目录失败");
        let path = dir.join("garbage.bin");
        fs::write(&path, b"not an image at all").expect("写文件失败");
        assert!(read_size(&path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
