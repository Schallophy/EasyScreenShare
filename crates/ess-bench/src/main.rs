fn main() {
    let (w, h) = (64u32, 64u32);
    let data = vec![128u8; (w * h * 4) as usize];

    let mut best = std::time::Duration::from_secs(999);
    let mut bytes = 0;
    for _ in 0..200 {
        let t = std::time::Instant::now();
        let enc = webp::Encoder::from_rgba(&data, w, h);
        let out = enc.encode_lossless();
        bytes = out.len();
        best = best.min(t.elapsed());
    }
    println!("webp lossless 64x64: {bytes} bytes, best {:?}", best);

    let img = image::RgbaImage::new(1920, 1080);
    println!("image crate ok: {}x{}", img.width(), img.height());
}
