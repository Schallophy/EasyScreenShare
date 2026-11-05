//! 布局代次切换的像素一致性（发送端重启 / 整帧丢失）。
//!
//! 1. 收到整帧后，报告里的 layout 必须等于该 layout；
//! 2. 新布局的 Update 先于整帧到达时，画布按 Update 自带网格重建（不混拼坐标）；
//! 3. 该布局还没收到整帧时，报告 layout=0 —— 发送端据此补整帧。

use ess_core::codec::{RawCodec, TileCodec};
use ess_core::model::TileId;
use ess_core::pipeline::{CodecSet, RecvPipeline};
use ess_core::wire::{WireMsg, WireTile};

fn tile(id: u16, w: u32, h: u32, color: u8) -> WireTile {
    WireTile {
        id: TileId(id),
        codec: RawCodec.id(),
        w,
        h,
        x: 0,
        y: 0,
        epoch: 1,
        version: 1,
        payload: vec![color; (w * h * 4) as usize],
    }
}

fn raw() -> RawCodec {
    RawCodec
}

#[test]
fn missing_full_is_requested_and_new_layout_rebuilds_canvas() {
    let codec = raw();
    let codecs = CodecSet { codecs: &[&codec as &dyn TileCodec] };
    let mut rx = RecvPipeline::new(codecs);

    // 旧布局：完整整帧
    let full = WireMsg::Full {
        seq: 1,
        layout_id: 100,
        frame_id: 1,
        w: 64,
        h: 64,
        codec: RawCodec.id(),
        codecs: Vec::new(),
        versions: vec![1],
        epochs: vec![1],
        payloads: vec![vec![9u8; 64 * 64 * 4]],
    };
    rx.on_wire(&full).unwrap();
    assert_eq!(rx.canvas.w, 64);
    assert_eq!(layout_of(&rx.report()), 100, "整帧后报告应带真实 layout");

    // 新布局的首批 Update 先到（整帧还没来）
    let upd = WireMsg::Update {
        seq: 2,
        layout_id: 101,
        cols: 2,
        rows: 2,
        repair: false,
        tiles: vec![tile(0, 64, 64, 5)],
    };
    rx.on_wire(&upd).unwrap();
    assert_eq!((rx.canvas.w, rx.canvas.h), (128, 128), "画布应按新网格重建");
    assert_eq!(
        layout_of(&rx.report()),
        0,
        "未收到整帧的布局必须上报 layout=0，让发送端补帧"
    );

    // 整帧补齐 → 恢复真实 layout 与精确尺寸
    let full2 = WireMsg::Full {
        seq: 3,
        layout_id: 101,
        frame_id: 2,
        w: 128,
        h: 128,
        codec: RawCodec.id(),
        codecs: Vec::new(),
        versions: vec![1; 4],
        epochs: vec![1; 4],
        payloads: vec![vec![7u8; 64 * 64 * 4]; 4],
    };
    rx.on_wire(&full2).unwrap();
    assert_eq!((rx.canvas.w, rx.canvas.h), (128, 128));
    assert_eq!(layout_of(&rx.report()), 101);
    assert_eq!(rx.full_layout(), 101);
}

fn layout_of(msg: &ess_core::model::Message) -> u32 {
    match msg {
        ess_core::model::Message::Report { layout_id, .. } => *layout_id,
        _ => panic!("report expected"),
    }
}
