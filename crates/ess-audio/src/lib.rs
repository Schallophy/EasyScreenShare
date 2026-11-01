//! 系统声音采集：Windows WASAPI 渲染环回（render loopback）。
//!
//! 抓的是默认输出端点的混音结果，跟 RustDesk / UU 远程的「分享系统声音」是同一件事：
//! 不挑应用、不装虚拟声卡。调用方只给一个 stop 开关，拿回混音格式和 10 ms 一块的
//! s16 数据（下一步直接喂 LiveKit `NativeAudioSource`）。

use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::Arc;

#[cfg(windows)]
mod capture;

/// 端点的混音格式。环回只能按它交数据，下游建音频轨也得按它来。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub sample_rate: u32,
    pub channels: u16,
}

/// 交织 s16 的 10 ms 一块
#[derive(Clone, Debug)]
pub struct Chunk {
    pub pcm: Vec<i16>,
    pub samples_per_channel: usize,
}

/// 启动环回采集线程。stop 置位后线程退出；返回混音格式与 10 ms 块的接收端。
pub fn start(stop: Arc<AtomicBool>) -> Result<(Format, mpsc::Receiver<Chunk>), String> {
    #[cfg(windows)]
    {
        capture::start(stop)
    }
    #[cfg(not(windows))]
    {
        let _ = stop;
        Err("环回采集目前只在 Windows 上实现".into())
    }
}

/// 混音格式里样本的实际编码。只认这两种，其余（24-bit PCM、硬件编码的杜比等）直接拒绝。
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleFormat {
    /// 32 位 IEEE float，取值 ±1.0
    F32,
    /// 16 位有符号 PCM
    I16,
}

/// 一块（10 ms）里交织 i16 的个数
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn chunk_len(fmt: &Format) -> usize {
    (fmt.sample_rate as usize / 100) * fmt.channels as usize
}

/// 交织原始字节 → 交织 i16。
///
/// float 按 32767 定标并饱和截断：环回拿到的是混音后的总线数据，应用把音量推满时
/// 真的会给出 |x| > 1.0，不夹的话溢出会绕回成大爆音。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn to_s16(src: &[u8], format: SampleFormat) -> Vec<i16> {
    match format {
        SampleFormat::I16 => src
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect(),
        SampleFormat::F32 => src
            .chunks_exact(4)
            .map(|b| {
                let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]) * 32767.0;
                if v >= 32767.0 {
                    32767
                } else if v <= -32768.0 {
                    -32768
                } else {
                    v as i16
                }
            })
            .collect(),
    }
}

/// 从 staging 头部取出整数个 10 ms 块，凑不满一块的余数留在 staging 里等下一包。
///
/// 必须恰好 10 ms：消费端是按块长排时间轴的，长度一抖就听成变速/卡顿。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn take_chunks(staging: &mut Vec<i16>, per_chunk: usize, channels: usize) -> Vec<Chunk> {
    let mut out = Vec::new();
    if per_chunk == 0 {
        return out;
    }
    while staging.len() >= per_chunk {
        out.push(Chunk {
            samples_per_channel: per_chunk / channels.max(1),
            pcm: staging.drain(..per_chunk).collect(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_to_s16_saturates_instead_of_wrapping() {
        let src: Vec<f32> = vec![1.0, 2.0, -1.0, -5.0, 0.5, 0.0];
        let bytes: Vec<u8> = src.iter().flat_map(|v| v.to_le_bytes()).collect();
        let v = to_s16(&bytes, SampleFormat::F32);
        assert_eq!(
            v,
            vec![32767, 32767, -32767, -32768, 16383, 0],
            "正满幅 32767、过冲夹上界、负满幅与过冲夹下界"
        );
        // 混进 NaN 也不能 panic（float→int 的转换在 Rust 里饱和为 0）
        let mut bad = vec![0.5f32; 3];
        bad[1] = f32::NAN;
        let bytes: Vec<u8> = bad.iter().flat_map(|v| v.to_le_bytes()).collect();
        let v = to_s16(&bytes, SampleFormat::F32);
        assert_eq!(v.len(), 3);
        assert_eq!(v[1], 0, "NaN 应落成 0，不能是脏值");
    }

    #[test]
    fn pcm16_to_s16_is_byte_copy_and_drops_partial_frame() {
        let bytes: Vec<u8> = vec![0x01, 0x00, 0xff, 0xff, 0x7f];
        assert_eq!(to_s16(&bytes, SampleFormat::I16), vec![1, -1]);
    }

    #[test]
    fn chunker_emits_exact_ten_ms_and_holds_the_remainder() {
        // 48 kHz 立体声：一块 = 480 帧 x 2 = 960 个 i16
        let per = 960;
        let mut staging: Vec<i16> = (0..per * 3 + 13).map(|i| i as i16).collect();
        let chunks = take_chunks(&mut staging, per, 2);
        assert_eq!(chunks.len(), 3, "3 块半只该取出 3 块");
        for c in &chunks {
            assert_eq!(c.pcm.len(), per, "块长必须恰好 10 ms");
            assert_eq!(c.samples_per_channel, 480);
        }
        assert_eq!(staging.len(), 13, "余数（这里是半个立体声帧）必须留在缓冲区");
        assert_eq!(chunks[2].pcm[per - 1], (per * 3 - 1) as i16, "取出的内容要按序衔接");

        // 补上剩下的一帧多，凑够第四块才出手
        staging.extend((0..947).map(|i| i as i16));
        let more = take_chunks(&mut staging, per, 2);
        assert_eq!(more.len(), 1);
        assert_eq!(more[0].pcm.len(), per);
        assert_eq!(more[0].pcm[0], 2880, "新块要接在旧余数后面");
        assert!(staging.is_empty());
    }

    #[test]
    fn chunk_len_is_ten_ms_of_interleaved_s16() {
        assert_eq!(chunk_len(&Format { sample_rate: 48000, channels: 2 }), 960);
        assert_eq!(chunk_len(&Format { sample_rate: 44100, channels: 1 }), 441);
    }
}
