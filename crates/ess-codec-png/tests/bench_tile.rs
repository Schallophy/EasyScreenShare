use std::time::Instant;
use ess_codec_png::PngCodec;
use ess_core::codec::TileCodec;

fn frame(kind: u8, w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    let mut s = 0x1234_5678u32;
    for y in 0..h {
        for x in 0..w {
            s ^= s << 13; s ^= s >> 17; s ^= s << 5;
            let n = (s & 0x1f) as u32;
            let (r, g, b) = match kind {
                0 => (n, n, n),
                1 => ((x * 3) & 0xff, (y * 3) & 0xff, ((x + y) * 2) & 0xff),
                _ => ((x * 7 + n) & 0xff, (y * 5 + n) & 0xff, ((x ^ y) * 3) & 0xff),
            };
            v.extend_from_slice(&[b as u8, g as u8, r as u8, 255]);
        }
    }
    v
}

#[test]
fn tile_cost() {
    let png = PngCodec;
    for kind in 0..3u8 {
        let raw = frame(kind, 64, 64);
        let t = Instant::now();
        let mut bytes = 0usize;
        for _ in 0..100 { bytes += png.encode(&raw, 64, 64).unwrap().len(); }
        let enc = t.elapsed().as_secs_f64() * 10.0;
        let t = Instant::now();
        for _ in 0..100 {
            let mut dst = vec![0u8; 64 * 64 * 4];
            png.decode_into(&png.encode(&raw, 64, 64).unwrap(), &mut dst, 64 * 4).unwrap();
        }
        let dec = t.elapsed().as_secs_f64() * 10.0;
        println!("kind={kind} png_enc={enc:.3}ms png_enc+dec={dec:.3}ms avg={}B", bytes / 100);
    }
}
