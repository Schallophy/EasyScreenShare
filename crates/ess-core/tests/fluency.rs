//! 弱网流畅度测量（模型级代理，§14.6）。
//!
//! **测什么**：内容变化 → 接收端可见的传播延迟分布（p50 / p95 / max）。
//! 两套场景：
//! - `fluency_matrix`：丢包 × RTT × 更新率 × 出口带宽 的稳态矩阵；
//! - `fluency_transitions`：网络**突变**场景（降速 / 卡顿 / 丢包突现 /
//!   组合打击 / 恢复 / 抖动），每个场景跑预算开/关两组（§6.6 A/B）。
//!
//! **测不了什么**：这是状态机模型，没有真实编码耗时与解码耗时；守门员的
//! 降级决策（画质轨迹）不在其中。M-5/M-6 的正式结论仍需实时管线（B-0b）。
//!
//! 运行：`cargo test -p ess-core --test fluency --release`
//! 结果写入 `screenshare-bench/results-fluency*.json` 并打印表格。

use std::collections::HashMap;

use ess_core::model::{
    Defects, Grid, Link, Message, NetConfig, Network, Receiver, RxDefects, Sender, TileId, TileMsg,
};

const COLS: u16 = 30; // 1920/64
const ROWS: u16 = 17; // 1080/64
const STEP_MS: u64 = 5;
const REPORT_MS: u64 = 500; // 周期报告间隔（对齐 §6.4 的兜底节奏，模型缩短自 2 s）
const GROUP_TIMEOUT_MS: u64 = 200;

struct Run {
    /// (生效时刻, 网络条件)。条件变化即"突变"。
    phases: Vec<(u64, NetConfig)>,
    edit_interval_ms: u64,
    tiles_per_edit: u16,
    sim_ms: u64,
    defects: Defects,
}

struct Metrics {
    p50: u64,
    p95: u64,
    max: u64,
    /// 最后一次突变之后的 p95 / max
    after_p95: u64,
    after_max: u64,
    samples: usize,
    divergent: usize,
    /// 突变后出现过的最大未收敛块数
    peak_divergent: usize,
    /// 突变后到最终收敛的耗时（ms）；`None` = 结束时仍未收敛
    reconverge_ms: Option<u64>,
    max_egress: usize,
    max_egress_after: usize,
    /// 发送端修复统计（预算生效证据）
    repair_throttled: u64,
    escalations: u64,
}

fn sender_update(sender: &Sender, tiles: Vec<TileMsg>) -> Message {
    Message::Update {
        layout_id: sender.layout_id,
        cols: sender.grid.cols,
        rows: sender.grid.rows,
        tiles,
    }
}

fn cfg_at(phases: &[(u64, NetConfig)], now_ms: u64) -> NetConfig {
    phases
        .iter()
        .rev()
        .find(|(at, _)| now_ms >= *at)
        .map(|(_, c)| *c)
        .unwrap_or(phases[0].1)
}

fn run(r: &Run, seed: u64) -> Metrics {
    let grid = Grid::new(COLS, ROWS);
    let mut net = Network::new(cfg_at(&r.phases, 0), seed);
    let mut sender = Sender::with_defects(grid, r.defects);
    let mut receiver = Receiver::with_defects(grid, RxDefects::default());
    let mut link = Link::new();
    let mut content_seq = 1u64;

    // (tile, version) → 创建时刻；接收端追上即记录延迟
    let mut pending: HashMap<(u16, u32), u64> = HashMap::new();
    let mut latencies: Vec<u64> = Vec::new();
    let mut latencies_after: Vec<u64> = Vec::new();
    let mut max_egress = 0usize;
    let mut max_egress_after = 0usize;

    let mut next_edit_at = 200u64;
    let mut last_report = 0u64;

    // 突变追踪
    let mut phase_idx = 0usize;
    let mut transition_at: Option<u64> = None;
    let mut peak_divergent = 0usize;
    let mut last_divergent_ms: Option<u64> = None;

    // 每块最近一次编辑时刻：收敛判定要排除"刚编辑、还在路上"的块
    let mut last_edit = vec![0u64; grid.len()];

    while net.now_ms < r.sim_ms {
        // 带宽提示随时同步给发送端（修复预算依赖它）
        sender.set_bandwidth(net.cfg.bandwidth_bps);

        // 网络条件切换（突变点）
        let idx = r
            .phases
            .iter()
            .rposition(|(at, _)| net.now_ms >= *at)
            .unwrap_or(0);
        if idx != phase_idx {
            phase_idx = idx;
            net.cfg = r.phases[idx].1;
            transition_at = Some(net.now_ms);
            latencies_after.clear();
            max_egress_after = 0;
            peak_divergent = 0;
            last_divergent_ms = None;
        }

        // 编辑：按更新率持续产生内容变化（模拟持续使用中的屏幕）
        if net.now_ms >= next_edit_at {
            sender.set_now(net.now_ms);
            let mut updates = Vec::new();
            for _ in 0..r.tiles_per_edit {
                let t = (content_seq as u16) % (COLS * ROWS);
                content_seq += 1;
                updates.push((TileId(t), content_seq));
            }
            let msg = sender.edit_many(&updates);
            for (id, _) in &updates {
                let i = id.0 as usize;
                pending.insert((id.0, sender.version[i]), net.now_ms);
                last_edit[i] = net.now_ms;
            }
            net.send(msg);
            next_edit_at += r.edit_interval_ms;
        }

        sender.set_now(net.now_ms);
        max_egress = max_egress.max(net.egress_len());
        if transition_at.is_some() {
            max_egress_after = max_egress_after.max(net.egress_len());
        }

        // 投递
        for (seq, msg) in net.advance(STEP_MS) {
            link.observe(seq);
            receiver.on_message(&msg);
        }

        // 周期报告 → 差异重发（受 §6.6 修复预算约束）
        if net.now_ms - last_report >= REPORT_MS {
            last_report = net.now_ms;
            if let Message::Report { layout_id, versions, epochs } = receiver.report() {
                let resends = sender.on_report(layout_id, &versions, &epochs, net.now_ms);
                if !resends.is_empty() {
                    net.send(sender_update(&sender, resends));
                }
                if let Some(m) = sender.take_layout_msg() {
                    net.send(m);
                }
                if let Some(m) = sender.take_escalation() {
                    net.send(m);
                }
            }
        }

        // 组超时（本场景无滚动，仅为完整性）
        let _need = receiver.tick(net.now_ms, GROUP_TIMEOUT_MS);

        // 可见性：接收端追上了哪个版本
        let keys: Vec<(u16, u32)> = pending.keys().copied().collect();
        for key in keys {
            let (t, v) = key;
            let i = t as usize;
            if i < receiver.version.len() && receiver.version[i] >= v {
                let created = pending.remove(&key).unwrap();
                let lat = net.now_ms - created;
                latencies.push(lat);
                if transition_at.is_some() {
                    latencies_after.push(lat);
                }
            }
        }

        // 收敛追踪（排除刚编辑、还在 RTT 路上的块）
        if transition_at.is_some() {
            let div = divergent_count(&sender, &receiver, &last_edit, net.now_ms, net.cfg.one_way_ms, grid.len());
            if div > peak_divergent {
                peak_divergent = div;
            }
            if div > 0 {
                last_divergent_ms = Some(net.now_ms);
            }
        }
    }

    latencies.sort_unstable();
    latencies_after.sort_unstable();
    let pick = |v: &Vec<u64>, q: f64| -> u64 {
        if v.is_empty() {
            return u64::MAX;
        }
        let idx = ((v.len() as f64 - 1.0) * q).round() as usize;
        v[idx.min(v.len() - 1)]
    };
    let divergent = divergent_count(&sender, &receiver, &last_edit, net.now_ms - 1, net.cfg.one_way_ms, grid.len());
    let reconverge = match (transition_at, divergent) {
        (Some(at), 0) => Some(last_divergent_ms.map(|t| t.saturating_sub(at)).unwrap_or(0)),
        _ => None,
    };

    Metrics {
        p50: pick(&latencies, 0.50),
        p95: pick(&latencies, 0.95),
        max: latencies.last().copied().unwrap_or(u64::MAX),
        after_p95: pick(&latencies_after, 0.95),
        after_max: latencies_after.last().copied().unwrap_or(u64::MAX),
        samples: latencies.len(),
        divergent,
        peak_divergent,
        reconverge_ms: reconverge,
        max_egress,
        max_egress_after,
        repair_throttled: sender.stats.repair_throttled,
        escalations: sender.stats.escalations,
    }
}

fn bw_label(bw: Option<u64>) -> String {
    match bw {
        None => "不限".to_string(),
        Some(b) => format!("{}k", b / 1000),
    }
}

/// 未收敛块数，排除 `宽限 = 2×单程延迟 + 报告间隔 + 100 ms` 内刚编辑的块
/// （它们可能还在路上，或刚丢失还没轮到下一轮报告修复，不算失联）。
fn divergent_count(
    sender: &Sender,
    receiver: &Receiver,
    last_edit: &[u64],
    now_ms: u64,
    one_way_ms: u64,
    n: usize,
) -> usize {
    let grace = 2 * one_way_ms + REPORT_MS + 100;
    (0..n)
        .filter(|&i| {
            now_ms.saturating_sub(last_edit[i]) > grace
                && (receiver.version[i] != sender.version[i]
                    || receiver.epoch[i] != sender.epoch[i])
        })
        .count()
}

fn cfg_label(c: &NetConfig) -> String {
    format!(
        "丢{:.0}%/RTT{}ms/带宽{}",
        c.drop_rate * 100.0,
        c.one_way_ms * 2,
        bw_label(c.bandwidth_bps)
    )
}

const CLEAN_2M: NetConfig = NetConfig {
    drop_rate: 0.0,
    dup_rate: 0.0,
    one_way_ms: 10,
    reorder: false,
    bandwidth_bps: Some(2_000_000),
};

// ---------------------------------------------------------------- 稳态矩阵

#[test]
fn fluency_matrix() {
    let drops = [0.0f64, 0.01, 0.05, 0.10];
    let one_ways = [10u64, 40u64, 100u64];
    let intervals = [50u64, 200u64];
    let bandwidths: [Option<u64>; 3] = [None, Some(2_000_000), Some(500_000)];

    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut table = String::from(
        "| 丢包 | RTT | 更新率 | 出口带宽 | p50 | p95 | max | 样本 | 未收敛块 | 出口峰值队列 | 修复被扣 | 升级整帧 |\n|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );

    for &drop in &drops {
        for &one_way in &one_ways {
            for &interval in &intervals {
                for &bw in &bandwidths {
                    let cfg = NetConfig {
                        drop_rate: drop,
                        dup_rate: 0.0,
                        one_way_ms: one_way,
                        reorder: false,
                        bandwidth_bps: bw,
                    };
                    let scenario = Run {
                        phases: vec![(0, cfg)],
                        edit_interval_ms: interval,
                        tiles_per_edit: 4,
                        sim_ms: 8_000,
                        defects: Defects::default(),
                    };
                    let m = run(&scenario, 0xabcd);
                    table.push_str(&format!(
                        "| {:.0}% | {} ms | 每 {} ms | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                        drop * 100.0,
                        one_way * 2,
                        interval,
                        bw_label(bw),
                        m.p50,
                        m.p95,
                        m.max,
                        m.samples,
                        m.divergent,
                        m.max_egress,
                        m.repair_throttled,
                        m.escalations,
                    ));
                    rows.push(serde_json::json!({
                        "drop": drop, "rtt_ms": one_way * 2, "edit_interval_ms": interval,
                        "bandwidth_bps": bw, "p50_ms": m.p50, "p95_ms": m.p95, "max_ms": m.max,
                        "samples": m.samples, "divergent_tiles": m.divergent, "max_egress": m.max_egress,
                        "repair_throttled": m.repair_throttled, "escalations": m.escalations,
                    }));
                }
            }
        }
    }

    println!("{}", table);
    write_out("results-fluency.json", &rows);
}

// ---------------------------------------------------------------- 突变场景

#[test]
fn fluency_transitions() {
    const T: u64 = 3_000; // 突变时刻（前 3 s 稳态）
    const SIM: u64 = 12_000;
    const EDIT: u64 = 50; // 每 50 ms 更新 4 块——激进更新率
    const BW500: NetConfig = NetConfig { bandwidth_bps: Some(500_000), ..CLEAN_2M };
    const BW300_LOSS_RTT: NetConfig = NetConfig {
        drop_rate: 0.10,
        one_way_ms: 100,
        bandwidth_bps: Some(300_000),
        ..CLEAN_2M
    };
    const BW500_LOSS_RTT: NetConfig = NetConfig {
        drop_rate: 0.10,
        one_way_ms: 60,
        bandwidth_bps: Some(500_000),
        ..CLEAN_2M
    };
    const LOSS10: NetConfig = NetConfig { drop_rate: 0.10, ..CLEAN_2M };
    const RTT200: NetConfig = NetConfig { one_way_ms: 100, ..CLEAN_2M };

    let scenarios: Vec<(&str, Vec<(u64, NetConfig)>)> = vec![
        (
            "降速 2M→500k",
            vec![(0, CLEAN_2M), (T, BW500)],
        ),
        (
            "丢包突现 0→10%",
            vec![(0, CLEAN_2M), (T, LOSS10)],
        ),
        (
            "卡顿 RTT 20→200ms",
            vec![(0, CLEAN_2M), (T, RTT200)],
        ),
        (
            "组合：300k+10%丢+200ms RTT",
            vec![(0, CLEAN_2M), (T, BW300_LOSS_RTT)],
        ),
        (
            "恢复：500k+10%丢 → 干净",
            vec![(0, BW500_LOSS_RTT), (T, CLEAN_2M)],
        ),
        (
            "抖动：2M↔500k+5%丢 每 1.5 s",
            vec![
                (0, CLEAN_2M),
                (1_500, NetConfig { drop_rate: 0.05, bandwidth_bps: Some(500_000), ..CLEAN_2M }),
                (3_000, CLEAN_2M),
                (4_500, NetConfig { drop_rate: 0.05, bandwidth_bps: Some(500_000), ..CLEAN_2M }),
                (6_000, CLEAN_2M),
                (7_500, NetConfig { drop_rate: 0.05, bandwidth_bps: Some(500_000), ..CLEAN_2M }),
                (9_000, CLEAN_2M),
                (10_500, NetConfig { drop_rate: 0.05, bandwidth_bps: Some(500_000), ..CLEAN_2M }),
            ],
        ),
    ];

    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut table = String::from(
        "| 场景 | 突变后条件 | 预算 | p95(全程) | max(全程) | p95(突变后) | max(突变后) | 峰值未收敛 | 收敛耗时 | 出口峰值(突变后) | 修复被扣 | 升级整帧 |\n|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );

    for (name, phases) in &scenarios {
        for budget_on in [true, false] {
            let defects = Defects { no_repair_budget: !budget_on, ..Defects::default() };
            let scenario = Run {
                phases: phases.clone(),
                edit_interval_ms: EDIT,
                tiles_per_edit: 4,
                sim_ms: SIM,
                defects,
            };
            let m = run(&scenario, 0xabcd);
            let after = cfg_at(phases, SIM - 1);
            let reconverge = match m.reconverge_ms {
                Some(ms) => format!("{} ms", ms),
                None => "未收敛".to_string(),
            };
            let fmt = |v: u64| if v == u64::MAX { "—".to_string() } else { v.to_string() };
            table.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                name,
                cfg_label(&after),
                if budget_on { "开" } else { "**关**" },
                fmt(m.p95),
                fmt(m.max),
                fmt(m.after_p95),
                fmt(m.after_max),
                m.peak_divergent,
                reconverge,
                m.max_egress_after,
                m.repair_throttled,
                m.escalations,
            ));
            rows.push(serde_json::json!({
                "scenario": name,
                "after_cfg": { "drop": after.drop_rate, "rtt_ms": after.one_way_ms * 2,
                               "bandwidth_bps": after.bandwidth_bps },
                "budget": budget_on,
                "p95_ms": m.p95, "max_ms": m.max,
                "after_p95_ms": m.after_p95, "after_max_ms": m.after_max,
                "peak_divergent": m.peak_divergent,
                "reconverge_ms": m.reconverge_ms,
                "max_egress": m.max_egress, "max_egress_after": m.max_egress_after,
                "repair_throttled": m.repair_throttled, "escalations": m.escalations,
            }));
        }
    }

    println!("{}", table);
    write_out("results-fluency-transitions.json", &rows);
}

fn write_out(name: &str, rows: &[serde_json::Value]) {
    let out = std::env::var("ESS_FLUENCY_OUT").unwrap_or_else(|_| {
        // 测试 cwd = crates/<pkg>；bench 目录在仓库外侧边
        let rel = format!("../../../screenshare-bench/{}", name);
        for cand in [rel.as_str(), name] {
            let p = std::path::Path::new(cand);
            if p.parent().map(|d| d.exists()).unwrap_or(false) {
                return cand.to_string();
            }
        }
        name.to_string()
    });
    std::fs::write(&out, serde_json::to_string_pretty(rows).unwrap()).unwrap();
    println!("saved {}", out);
}
