//! 运动帧（整帧降采样 JPEG）编解码。

use ess_core::frame::Frame;
use ess_core::motion;

fn gradient(w: u32, h: u32, off: u8) -> Frame {
    let mut f = Frame::new(w, h, 0);
    for y in 0..h {
        for x in 0..w {
            let o = y as usize * f.stride + x as usize * 4;
            f.px[o] = ((x * 255 / w.max(1)) as u8).saturating_add(off);
            f.px[o + 1] = ((y * 255 / h.max(1)) as u8).saturating_add(off);
            f.px[o + 2] = ((x + y) * 255 / (w + h)) as u8;
            f.px[o + 3] = 255;
        }
    }
    f
}

#[test]
fn motion_roundtrip_is_lossy_but_close() {
    let f = gradient(800, 450, 0);
    let (w, h, jpeg) = motion::encode(&f, 800, 450, 55).unwrap();
    assert_eq!((w, h), (800, 450));
    assert!(!jpeg.is_empty());
    let mut canvas = Frame::new(800, 450, 0);
    motion::decode_into_canvas(&jpeg, &mut canvas).unwrap();
    let mut worst = 0i32;
    for y in (0..450).step_by(7) {
        for x in (0..800).step_by(7) {
            let o = y as usize * f.stride + x as usize * 4;
            for c in 0..3 {
                let a = f.px[o + c] as i32;
                let b = canvas.px[o + c] as i32;
                worst = worst.max((a - b).abs());
            }
        }
    }
    assert!(worst < 40, "JPEG 误差过大: {worst}");
}

#[test]
fn motion_scales_into_layout_canvas() {
    let f = gradient(800, 450, 10);
    let (_w, _h, jpeg) = motion::encode(&f, 800, 450, 50).unwrap();
    let mut canvas = Frame::new(1920, 1080, 0);
    motion::decode_into_canvas(&jpeg, &mut canvas).unwrap();
    // 角像素应接近源图对应区域
    let o = 1000 * canvas.stride + 1000 * 4;
    assert!(canvas.px[o] > 0 || canvas.px[o + 1] > 0 || canvas.px[o + 2] > 0);
    assert_eq!(canvas.px[o + 3], 255);
}

#[test]
fn motion_solid_color_channels() {
    let mut f = Frame::new(64, 64, 0);
    for o in (0..f.px.len()).step_by(4) {
        f.px[o] = 30;      // B
        f.px[o + 1] = 200; // G
        f.px[o + 2] = 10;  // R
        f.px[o + 3] = 255;
    }
    let (_w, _h, jpeg) = motion::encode(&f, 64, 64, 90).unwrap();
    let mut c = Frame::new(64, 64, 0);
    motion::decode_into_canvas(&jpeg, &mut c).unwrap();
    let o = 32 * c.stride + 32 * 4;
    println!("decoded BGR = {} {} {}", c.px[o], c.px[o + 1], c.px[o + 2]);
    assert!((c.px[o] as i32 - 30).abs() < 12, "B {}", c.px[o]);
    assert!((c.px[o + 1] as i32 - 200).abs() < 12, "G {}", c.px[o + 1]);
    assert!((c.px[o + 2] as i32 - 10).abs() < 12, "R {}", c.px[o + 2]);
}
