// SPDX-License-Identifier: GPL-3.0-only
//! The backend's media streams: Opus RTP packets to `AudioFrame`s with
//! `ChannelSignal`s (§6.1, §9), and spectrum2 rows to `WaterfallRow`s
//! (§6.2).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use station_backend::{AudioStream, ChannelOutput, RowStream};
use station_proto::{limits, v1};
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::client::{AudioPacket, AudioReceiver, Ka9qClient, SpectrumReceiver};
use crate::spectrum::{self, Quantization, SpectrumFrame};
use crate::status::ChannelStatus;

use super::map;

/// Time between S-meter checks: at most 5 `ChannelSignal`s per second (§5).
pub const SIGNAL_INTERVAL: Duration = Duration::from_millis(200);
/// zstd level for waterfall rows: fast, and plenty for 8-bit spectra.
pub const ZSTD_LEVEL: i32 = 3;

/// A channel's squelch threshold, shared with its stream so that the
/// `ChannelSignal.squelch_open` flag follows `UpdateChannel`.
pub type SharedSquelch = Arc<Mutex<Option<f32>>>;

/// Where an audio stream reads the channel's status from.
#[derive(Debug)]
pub struct StatusSource {
    /// The client that owns the channel.
    pub client: Ka9qClient,
    /// The channel's SSRC.
    pub ssrc: u32,
}

/// Maps radiod packets to protocol frames: the client's `seq` counts
/// delivered packets, so the network losses it reports in `lost_before`
/// are added back, and every gap (network or queue drop) shows up as a jump
/// in `AudioFrame.seq` (media.proto: "gaps mean frames were dropped").
#[derive(Debug, Default)]
pub struct AudioFramer {
    lost: u32,
}

impl AudioFramer {
    /// The frame for `p`, or `None` if its payload is not a valid Opus
    /// packet size (1..=1275 bytes, §5).
    pub fn frame(&mut self, channel_id: u32, p: AudioPacket) -> Option<v1::AudioFrame> {
        self.lost = self.lost.wrapping_add(p.lost_before);
        if !(limits::MIN_OPUS_PACKET..=limits::MAX_OPUS_PACKET).contains(&p.payload.len()) {
            return None;
        }
        Some(v1::AudioFrame {
            channel_id,
            seq: p.seq.wrapping_add(self.lost),
            timestamp: p.timestamp,
            payload: p.payload,
        })
    }
}

struct AudioPump {
    channel_id: u32,
    rx: AudioReceiver,
    status: StatusSource,
    squelch: SharedSquelch,
    stop: CancellationToken,
    tick: Interval,
    framer: AudioFramer,
    last_status: Option<Arc<ChannelStatus>>,
}

impl AudioPump {
    /// A `ChannelSignal` if radiod sent a new status since the last one.
    fn signal(&mut self) -> Option<v1::ChannelSignal> {
        let st = self.status.client.status_of(self.status.ssrc)?;
        if self
            .last_status
            .as_ref()
            .is_some_and(|last| Arc::ptr_eq(last, &st))
        {
            return None;
        }
        self.last_status = Some(Arc::clone(&st));
        let squelch = *self.squelch.lock().unwrap_or_else(PoisonError::into_inner);
        map::channel_signal(self.channel_id, &st, squelch)
    }

    async fn next(&mut self) -> Option<ChannelOutput> {
        loop {
            tokio::select! {
                biased;
                () = self.stop.cancelled() => return None,
                p = self.rx.recv() => {
                    let p = p?;
                    if let Some(frame) = self.framer.frame(self.channel_id, p) {
                        return Some(ChannelOutput::Audio(frame));
                    }
                }
                _ = self.tick.tick() => {
                    if let Some(signal) = self.signal() {
                        return Some(ChannelOutput::Signal(signal));
                    }
                }
            }
        }
    }
}

/// The output of one channel: its audio, plus a `ChannelSignal` whenever
/// radiod reported a new status (checked every [`SIGNAL_INTERVAL`]). Ends
/// when `stop` is cancelled or the channel is closed.
pub fn audio_stream(
    channel_id: u32,
    rx: AudioReceiver,
    status: StatusSource,
    squelch: SharedSquelch,
    stop: CancellationToken,
) -> AudioStream {
    let mut tick = tokio::time::interval_at(Instant::now() + SIGNAL_INTERVAL, SIGNAL_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let pump = AudioPump {
        channel_id,
        rx,
        status,
        squelch,
        stop,
        tick,
        framer: AudioFramer::default(),
        last_status: None,
    };
    Box::pin(futures_util::stream::unfold(pump, |mut pump| async move {
        let item = pump.next().await?;
        Some((item, pump))
    }))
}

/// Encodes spectrum2 frames as rows of one subscription.
#[derive(Debug)]
pub struct RowEncoder {
    /// The hub's subscription ID.
    pub subscription_id: u32,
    /// `WaterfallSubscribed.bins`.
    pub bins: usize,
    /// The subscription's quantization.
    pub quantization: Quantization,
    /// zstd (true) or none.
    pub zstd: bool,
    /// Clock origin for `timestamp_us`.
    pub epoch: Instant,
    /// Rows dropped: wrong size, or too large compressed.
    pub dropped: u64,
    seq: u32,
}

impl RowEncoder {
    /// An encoder for `sub`.
    pub fn new(sub: &v1::WaterfallSubscribed, epoch: Instant) -> Self {
        let q = sub.quantization.unwrap_or_default();
        Self {
            subscription_id: sub.subscription_id,
            bins: usize::try_from(sub.bins).unwrap_or(0),
            quantization: Quantization::new(q.offset_db, q.step_db),
            zstd: sub.compression == v1::WaterfallCompression::Zstd as i32,
            epoch,
            dropped: 0,
            seq: 0,
        }
    }

    /// The row for `frame`: requantized to dB/Hz with this subscription's
    /// scale (radiod rescales every reply), then compressed. `None` (and
    /// counted) if the frame has the wrong number of bins or the row would
    /// exceed the protocol's 16 KiB (§5); `seq` only counts sent rows.
    pub fn encode(&mut self, frame: &SpectrumFrame) -> Option<v1::WaterfallRow> {
        if frame.bins.len() != self.bins {
            self.dropped += 1;
            return None;
        }
        let row = spectrum::requantize(frame, &self.quantization);
        let payload = if self.zstd {
            zstd::bulk::compress(&row, ZSTD_LEVEL).ok()
        } else {
            Some(row)
        };
        let Some(payload) = payload.filter(|p| p.len() <= limits::MAX_WATERFALL_COMPRESSED) else {
            self.dropped += 1;
            tracing::warn!(
                subscription_id = self.subscription_id,
                "waterfall row dropped: it does not fit the 16 KiB row limit"
            );
            return None;
        };
        let row = v1::WaterfallRow {
            subscription_id: self.subscription_id,
            seq: self.seq,
            timestamp_us: u64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(u64::MAX),
            payload,
        };
        self.seq = self.seq.wrapping_add(1);
        Some(row)
    }
}

struct RowPump {
    rx: SpectrumReceiver,
    encoder: RowEncoder,
    stop: CancellationToken,
}

/// The rows of one subscription. Ends when `stop` is cancelled or the
/// spectrum channel is closed.
pub fn row_stream(rx: SpectrumReceiver, encoder: RowEncoder, stop: CancellationToken) -> RowStream {
    let pump = RowPump { rx, encoder, stop };
    Box::pin(futures_util::stream::unfold(pump, |mut pump| async move {
        loop {
            tokio::select! {
                biased;
                () = pump.stop.cancelled() => return None,
                frame = pump.rx.recv() => {
                    if let Some(row) = pump.encoder.encode(&frame?) {
                        return Some((row, pump));
                    }
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use station_proto::validate::WaterfallDecoder;

    fn packet(seq: u32, lost_before: u32, len: usize) -> AudioPacket {
        AudioPacket {
            seq,
            timestamp: seq * 960,
            lost_before,
            payload_type: 111,
            payload: vec![0x28; len],
        }
    }

    #[test]
    fn framer_shows_losses_as_gaps_and_drops_bad_payloads() {
        let mut f = AudioFramer::default();
        let a = f.frame(4, packet(0, 0, 30)).unwrap();
        assert_eq!((a.channel_id, a.seq, a.timestamp), (4, 0, 0));
        // Two packets lost on the network before this one.
        let b = f.frame(4, packet(1, 2, 30)).unwrap();
        assert_eq!(b.seq, 3);
        // A queue drop is already a jump in the client's seq.
        let c = f.frame(4, packet(3, 0, 30)).unwrap();
        assert_eq!(c.seq, 5);
        assert!(f.frame(4, packet(4, 0, 0)).is_none());
        assert!(f.frame(4, packet(5, 0, 1276)).is_none());
        assert_eq!(f.frame(4, packet(6, 0, 1275)).unwrap().seq, 8);
    }

    fn subscribed(bins: u32, compression: v1::WaterfallCompression) -> v1::WaterfallSubscribed {
        v1::WaterfallSubscribed {
            subscription_id: 9,
            receiver_id: 1,
            range: None,
            bins,
            row_interval_ms: 100,
            quantization: Some(v1::Quantization {
                offset_db: -130.0,
                step_db: 0.5,
            }),
            compression: compression as i32,
        }
    }

    fn frame(bins: Vec<u8>) -> SpectrumFrame {
        SpectrumFrame {
            center_hz: 10e6,
            bin_width_hz: 1000.0,
            base_db: -50.0,
            step_db: 0.5,
            bins,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rows_are_requantized_and_decodable() {
        for compression in [
            v1::WaterfallCompression::Zstd,
            v1::WaterfallCompression::None,
        ] {
            let sub = subscribed(64, compression);
            let mut enc = RowEncoder::new(&sub, Instant::now());
            tokio::time::advance(Duration::from_millis(5)).await;
            let row = enc.encode(&frame(vec![0; 64])).unwrap();
            assert_eq!(row.subscription_id, 9);
            assert_eq!(row.seq, 0);
            assert_eq!(row.timestamp_us, 5000);
            let mut out = Vec::new();
            WaterfallDecoder::new()
                .decode(&row.payload, compression as i32, 64, &mut out)
                .unwrap();
            // -50 dB per 1 kHz bin = -80 dB/Hz = byte 100 on (-130, 0.5).
            assert!(out.iter().all(|&v| v == 100));
            // Wrong size: dropped, seq unchanged.
            assert!(enc.encode(&frame(vec![0; 65])).is_none());
            assert_eq!(enc.dropped, 1);
            assert_eq!(enc.encode(&frame(vec![0; 64])).unwrap().seq, 1);
        }
    }

    #[tokio::test]
    async fn oversized_rows_are_dropped() {
        let sub = subscribed(20_000, v1::WaterfallCompression::None);
        let mut enc = RowEncoder::new(&sub, Instant::now());
        assert!(enc.encode(&frame(vec![1; 20_000])).is_none());
        assert_eq!(enc.dropped, 1);
    }
}
