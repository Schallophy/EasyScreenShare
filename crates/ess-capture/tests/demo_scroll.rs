//! 合成屏必须是一段**真滚动**，否则滚动检测返回 None，无损通路把整屏平移当成
//! 整屏重绘 —— 实测代价 4–5 Mbps 且只有 10 fps，而真实滚动内容只需 1.19 Mbps、
//! 能跑满 30 fps。验收若跑在检测不到的内容上，结论就落到了错误的一条路径上（O-23）。

use ess_capture::{ScreenSource, SyntheticScreen};

#[test]
fn demo_scroll_is_recognised_as_a_translation() {
    // 纯滚动 = 关掉那块独立移动的红块（它把平移破坏成局部编辑，本来就该走差分）
    let mut src = SyntheticScreen::new(1280, 720, 80);
    src.block = false;
    let a = src.frame(0);
    let b = src.frame(33_333); // 30 fps 下的同一动画格（100 ms 一格）
    let c = src.frame(100_000); // 下一格：整帧上移 16 px
    assert_eq!(a.px, b.px, "同一动画格内两帧必须完全一致");
    let d = ess_core::scroll::detect(&a, &c).expect("16 px 整帧上移必须被判为滚动");
    assert_eq!(d.dy, 16, "位移量");
    assert!(d.match_ratio > 0.99, "重叠区匹配率只有 {}", d.match_ratio);
}

/// 反向对照：红块开着的时候**不该**被判成滚动——它验证的是这台判据确实分得开
/// 两种内容，而不是"随便什么都算平移"。
#[test]
fn moving_block_is_not_mistaken_for_a_scroll() {
    let mut src = SyntheticScreen::new(1280, 720, 80);
    let a = src.frame(0);
    let c = src.frame(100_000);
    assert!(ess_core::scroll::detect(&a, &c).is_none(), "移动物体被当成滚动了");
}
