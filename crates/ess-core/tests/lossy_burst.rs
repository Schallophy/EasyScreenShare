//! 现场形状：快速打开/最小化窗口（Windows 弹出收回动画）。
//!
//! 动画期间每帧都是大面积变化，一次逐块更新被拆成多条 lossy 消息；拥塞时发送端会
//! 丢掉其中任意一条，而接收端按 `seq` 凑片——凑不齐就整组丢弃（见 §14.7.35）。
//! 动画结束后画面静止，发送端不再产生新的差分：此时唯一能把旧块补回来的，是观看端
//! 500 ms 周期报告驱动的差异重发。
//!
//! 这条测试钉的不变量：**丢过片之后，报告必须把画布补到与发送端逐字节一致**。
//! 用户报的"偶现图块残留"如果成立，就会在这里红——要么报告没要到那些块，要么要到了
//! 却没有落画布。

use ess_core::codec::{RawCodec, TileCodec};
use ess_core::frame::Frame;
use ess_core::model::Message;
use ess_core::pipeline::{CodecSet, RecvPipeline, SendPipeline};
use ess_core::wire::WireMsg;

fn codecs() -> CodecSet<'static> {
    CodecSet {
        codecs: Box::leak(vec![&RawCodec as &dyn TileCodec].into_boxed_slice()),
    }
}

/// 全屏伪随机噪声：每帧都变，保证一次更新覆盖全部块（自然会被拆片）。
fn noise(k: u32) -> Frame {
    let (w, h) = (512u32, 512u32);
    let mut f = Frame::new(w, h, k as u64 * 33_000);
    for y in 0..h {
        for x in 0..w {
            let o = y as usize * f.stride + x as usize * 4;
            let mut v = x.wrapping_mul(0x9e37_79b1)
                ^ y.wrapping_mul(0x85eb_ca6b)
                ^ k.wrapping_mul(0xc2b2_ae35);
            v ^= v >> 15;
            v = v.wrapping_mul(0x2545_f491);
            v ^= v >> 13;
            f.px[o] = v as u8;
            f.px[o + 1] = (v >> 8) as u8;
            f.px[o + 2] = (v >> 16) as u8;
            f.px[o + 3] = 255;
        }
    }
    f
}

/// 把一条更新人工切成 `total` 片（现场是传输层按 60 KB 拆的，这里按块数拆，
/// 效果相同：共享 seq、片号/总数齐全）。
fn split(msg: &WireMsg, total: u32) -> Vec<WireMsg> {
    let WireMsg::Update { seq, layout_id, cols, rows, tiles, repair, .. } = msg else {
        panic!("期望逐块更新，得到 {msg:?}");
    };
    let per = tiles.len().div_ceil(total as usize);
    let mut out = Vec::new();
    for (i, chunk) in tiles.chunks(per.max(1)).enumerate() {
        out.push(WireMsg::Update {
            seq: *seq,
            layout_id: *layout_id,
            cols: *cols,
            rows: *rows,
            repair: *repair,
            part: i as u32,
            total: tiles.chunks(per.max(1)).count() as u32,
            tiles: chunk.to_vec(),
        });
    }
    assert_eq!(out.len() as u32, total, "拆分片数应与请求一致");
    out
}

fn feed_all(recv: &mut RecvPipeline, msgs: Vec<WireMsg>) {
    for m in &msgs {
        let bytes = ess_core::wire::encode(m).expect("wire encode");
        let decoded = ess_core::wire::decode(&bytes).expect("wire decode");
        recv.on_wire(&decoded).expect("recv");
    }
}

fn pick_update(msgs: &[WireMsg]) -> Option<&WireMsg> {
    msgs.iter().find(|m| {
        matches!(m, WireMsg::Update { tiles, .. } if !tiles.is_empty())
    })
}

#[test]
fn report_repairs_in_independent_chunks_so_one_lost_piece_does_not_block_the_rest() {
    let f0 = noise(0);
    let mut p = SendPipeline::new(f0.clone(), codecs());
    p.motion_enabled = false;
    let mut recv = RecvPipeline::new(codecs());
    feed_all(&mut recv, p.on_frame(&f0).unwrap());

    // 一整屏变化、但一片都没凑齐（等于动画期间全部被丢）
    let mut last = f0;
    for k in 1..=6u32 {
        let f = noise(k);
        let msgs = p.on_frame(&f).unwrap();
        let upd = pick_update(&msgs).expect("全屏变化应产生逐块更新");
        let parts = split(upd, 4);
        feed_all(&mut recv, vec![parts[0].clone()]);
        last = f;
    }
    let mut still = last.clone();
    still.ts_us += 33_000;
    assert!(p.on_frame(&still).unwrap().is_empty());

    let Message::Report { layout_id, versions, epochs, .. } = recv.report() else {
        panic!("report");
    };
    let msgs = p.on_report(layout_id, &versions, &epochs, 300).unwrap();
    assert!(
        msgs.len() > 1,
        "整屏差集必须切成多条独立更新，而不是一条共享 seq 的大消息（会被整组丢弃）: {} 条",
        msgs.len()
    );
    for m in &msgs {
        if let WireMsg::Update { total, .. } = m {
            assert_eq!(*total, 0, "每条修复应独立成帧（total=0），不参与拆片凑齐");
        } else {
            panic!("报告修复只应产生 Update，得到 {m:?}");
        }
    }

    // 故意丢掉第一条：其余必须立刻落地，而不是跟着一起作废
    feed_all(&mut recv, msgs[1..].to_vec());
    let left = diff_tiles(&recv.canvas, &last);
    assert!(
        left > 0 && left <= 8,
        "丢一条只应留下那一小条的块（≤8），实际差 {left} 块"
    );

    // 下一份报告只补剩下的
    let Message::Report { layout_id, versions, epochs, .. } = recv.report() else {
        panic!("report");
    };
    let more = p.on_report(layout_id, &versions, &epochs, 800).unwrap();
    feed_all(&mut recv, more);
    assert_eq!(recv.canvas.px, last.px, "第二轮必须把剩余块补全");
}

#[test]
fn a_late_full_frame_does_not_clobber_newer_tiles() {
    // 现场：整帧走可靠通道，1080p 要传一到几秒；这段时间 lossy 通路的新块先到并
    // 落了画布。整帧后到，模型的"整帧不得覆盖更新状态"让它不回退版本，但旧代码
    // 会把画布整体重建再写回整帧内容——画布比版本旧，报告再也补不回来，只有那块
    // 内容再次变化（鼠标划过）才会被新版本盖住。这条测试钉"画布也不许回退"。
    let f0 = noise(0);
    let mut p = SendPipeline::new(f0.clone(), codecs());
    p.motion_enabled = false;
    p.scroll_enabled = false;
    let mut recv = RecvPipeline::new(codecs());

    let first_full = p.on_frame(&f0).unwrap();
    feed_all(&mut recv, first_full.clone());
    assert_eq!(recv.canvas.px, f0.px);

    // 整帧还在路上：一块内容先通过 lossy 通路到达并落画布
    let mut f1 = f0.clone();
    f1.ts_us = 33_000;
    edit_tile(&mut f1, 0, 0);
    let upd = p.on_frame(&f1).unwrap();
    feed_all(&mut recv, upd);
    assert_eq!(recv.canvas.px, f1.px, "先到的新块应落画布");

    // 迟到的整帧（内容是 f0 的、版本也更旧）
    feed_all(&mut recv, first_full);
    assert_eq!(
        recv.canvas.px, f1.px,
        "迟到的整帧盖掉了更新的块——画布比版本旧，报告永远补不回来"
    );
    assert_eq!(recv.rx.version[0], 1, "模型版本不应回退");
}

/// 改一块 64x64（越界自动裁掉）。
fn edit_tile(f: &mut Frame, tx: u32, ty: u32) {
    let (x0, y0) = (tx * 64, ty * 64);
    for y in y0..(y0 + 64).min(f.h) {
        for x in x0..(x0 + 64).min(f.w) {
            let o = y as usize * f.stride + x as usize * 4;
            f.px[o] ^= 0xff;
            f.px[o + 1] ^= 0x5a;
            f.px[o + 2] ^= 0xa5;
        }
    }
}

/// 把两幅图按 64x64 块比较，返回内容不同的块数。
fn diff_tiles(a: &Frame, b: &Frame) -> usize {
    assert_eq!((a.w, a.h), (b.w, b.h));
    let (tw, th) = (64usize, 64usize);
    let mut n = 0;
    for ty in (0..a.h as usize).step_by(th) {
        for tx in (0..a.w as usize).step_by(tw) {
            let mut differs = false;
            'rows: for y in ty..(ty + th).min(a.h as usize) {
                for x in tx..(tx + tw).min(a.w as usize) {
                    let o = y * a.stride + x * 4;
                    if a.px[o..o + 4] != b.px[o..o + 4] {
                        differs = true;
                        break 'rows;
                    }
                }
            }
            if differs {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn report_repairs_the_canvas_after_a_burst_of_half_delivered_split_updates() {
    let f0 = noise(0);
    let mut p = SendPipeline::new(f0.clone(), codecs());
    p.motion_enabled = false;
    let mut recv = RecvPipeline::new(codecs());

    feed_all(&mut recv, p.on_frame(&f0).unwrap());
    assert_eq!(recv.canvas.px, f0.px, "基线整帧应落画布");

    // 动画：每帧全屏变化，拆 4 片，只送到第 0、2 片（模拟拥塞时发送端丢 lossy 消息）。
    // 下一帧的 seq 一到，接收端就该把上一帧的残缺组整组丢弃。
    let mut last = f0;
    for k in 1..=24u32 {
        let f = noise(k);
        let msgs = p.on_frame(&f).unwrap();
        let upd = pick_update(&msgs).expect("全屏变化应产生逐块更新");
        let parts = split(upd, 4);
        feed_all(&mut recv, vec![parts[0].clone(), parts[2].clone()]);
        last = f;
    }

    // 动画结束：同一画面再来一帧，发送端无差分可发（现场正是这一段最考验报告）。
    let mut still = last.clone();
    still.ts_us += 33_000;
    assert!(
        p.on_frame(&still).unwrap().is_empty(),
        "静止帧不应再产生更新，后续修复只能来自报告"
    );

    // 周期报告驱动修复：最多补 4 轮（现场 500 ms 一轮，预算截断时下一轮续上）。
    let mut rounds = 0;
    for round in 0..4u64 {
        rounds = round + 1;
        let Message::Report { layout_id, versions, epochs, .. } = recv.report() else {
            panic!("report 应返回 Message::Report");
        };
        let msgs = p.on_report(layout_id, &versions, &epochs, 300 + round * 500).unwrap();
        feed_all(&mut recv, msgs);
        if recv.canvas.px == last.px {
            break;
        }
    }
    assert_eq!(
        recv.canvas.px, last.px,
        "{rounds} 轮报告后画布仍与发送端不一致——丢片留下的旧块没有被报告补回来"
    );
    assert!(
        rounds <= 3,
        "报告修复用了 {rounds} 轮，超过现场 1.5 s 的可见窗口"
    );
}
