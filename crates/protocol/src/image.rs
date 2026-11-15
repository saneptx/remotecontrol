//! 剪贴板图片的 PNG 编解码（RGBA8）。

pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    enc.write_header().ok()?.write_image_data(rgba).ok()?;
    Some(out)
}

/// 解码为 RGBA8，返回 (宽, 高, 像素)。
pub fn decode_png(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let px = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Indexed => return None, // normalize_to_color8 已展开调色板
    };
    Some((info.width, info.height, rgba))
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let rgba: Vec<u8> = (0..4 * 3 * 2).map(|i| i as u8 * 9).collect();
        let png = super::encode_png(3, 2, &rgba).unwrap();
        assert_eq!(super::decode_png(&png), Some((3, 2, rgba)));
        assert_eq!(super::decode_png(b"not png"), None);
    }
}
