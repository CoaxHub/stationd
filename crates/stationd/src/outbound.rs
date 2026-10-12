// SPDX-License-Identifier: GPL-3.0-only
//! The outbound queue of one tunnel connection (station-protocol §6.3).
//!
//! One bounded queue, two priority classes:
//!
//! - **high:** `Hello`, responses, events, audio frames;
//! - **low:** waterfall rows, `ChannelSignal`, `StationStatus`, and
//!   measurement reports (`NoiseFloorReport`, §6.4).
//!
//! High is always sent first; within a class, oldest first. "The queue
//! exceeds 250 ms of data" is measured as queueing delay (how long an item
//! has waited), since the station cannot know the link's throughput:
//!
//! 1. waterfall rows (and channel signals) that waited more than 250 ms are
//!    dropped, oldest first;
//! 2. audio frames are dropped only after waiting more than 1 s;
//! 3. only the latest `StationStatus` is kept (a newer one replaces a queued
//!    one: it supersedes it);
//! 4. measurement reports are dropped once they waited more than 10 s
//!    (the measurement design §4), counted in `measurement_reports_dropped_total`;
//!    measurement events (duty, stopped) are events, never dropped for age;
//! 5. responses, `Hello` and events are never dropped by age. Responses are
//!    bounded by the protocol (≤ 32 outstanding requests, §4); events are
//!    capped at [`QueueLimits::max_events`] (new events beyond that are
//!    dropped and counted).
//!
//! Each droppable class also has a count bound so memory stays bounded when
//! the link stalls. Drops of audio and rows are counted in [`DropCounters`]
//! and reported in `StationStatus` (§6.3).
//!
//! [`OutboundQueue`] is the pure policy with an explicit clock;
//! [`Outbound`] wraps it for concurrent producers and the gRPC stream.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures_util::Stream;
use station_proto::{limits, v1};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use v1::station_message::Body;

/// Queue bounds (§6.3 plus memory caps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLimits {
    /// Waterfall rows and signals older than this are dropped (§6.3: 250 ms).
    pub waterfall_max_age: Duration,
    /// Audio frames older than this are dropped (§6.3: 1 s).
    pub audio_max_age: Duration,
    /// Most audio frames queued (oldest dropped beyond).
    pub max_audio: usize,
    /// Most waterfall rows queued (oldest dropped beyond).
    pub max_waterfall: usize,
    /// Most channel signals queued (oldest dropped beyond).
    pub max_signals: usize,
    /// Most events queued (newer events dropped beyond).
    pub max_events: usize,
    /// Measurement reports older than this are dropped (§6.3: 10 s).
    pub measurement_max_age: Duration,
    /// Most measurement reports queued (oldest dropped beyond).
    pub max_measurements: usize,
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            waterfall_max_age: limits::QUEUE_WATERFALL_DROP_AFTER,
            audio_max_age: limits::QUEUE_AUDIO_DROP_AFTER,
            // 20 s of 20 ms frames: far above the 1 s age limit.
            max_audio: 1024,
            max_waterfall: 256,
            max_signals: 64,
            max_events: 256,
            measurement_max_age: limits::QUEUE_MEASUREMENT_DROP_AFTER,
            // A report a minute: a few cover any stall shorter than the age limit.
            max_measurements: 8,
        }
    }
}

/// Messages dropped since `stationd` started (§6.3, `StationStatus`).
#[derive(Debug, Default)]
pub struct DropCounters {
    audio: AtomicU64,
    waterfall: AtomicU64,
    signals: AtomicU64,
    events: AtomicU64,
    measurements: AtomicU64,
}

impl DropCounters {
    /// Audio frames dropped.
    pub fn audio(&self) -> u64 {
        self.audio.load(Ordering::Relaxed)
    }

    /// Waterfall rows dropped.
    pub fn waterfall(&self) -> u64 {
        self.waterfall.load(Ordering::Relaxed)
    }

    /// Channel signals dropped.
    pub fn signals(&self) -> u64 {
        self.signals.load(Ordering::Relaxed)
    }

    /// Events dropped because the event cap was reached.
    pub fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    /// Measurement reports dropped (older than 10 s, or over the cap).
    pub fn measurements(&self) -> u64 {
        self.measurements.load(Ordering::Relaxed)
    }

    fn add(counter: &AtomicU64, n: usize) {
        if n > 0 {
            counter.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
        }
    }
}

/// The priority class of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// `Hello`, `Response`, `Event` (and an empty message): never dropped
    /// by age.
    Control,
    /// `AudioFrame`.
    Audio,
    /// `WaterfallRow`.
    Waterfall,
    /// `ChannelSignal`.
    Signal,
    /// `StationStatus`.
    Status,
    /// `NoiseFloorReport` (§6.4).
    Measurement,
}

impl Class {
    /// The class of `msg`.
    pub fn of(msg: &v1::StationMessage) -> Self {
        match &msg.body {
            Some(Body::Audio(_)) => Self::Audio,
            Some(Body::Waterfall(_)) => Self::Waterfall,
            Some(Body::Signal(_)) => Self::Signal,
            Some(Body::Status(_)) => Self::Status,
            Some(Body::Noise(_)) => Self::Measurement,
            Some(Body::Hello(_) | Body::Response(_) | Body::Event(_)) | None => Self::Control,
        }
    }

    /// Whether the class goes before the low-priority classes.
    pub fn is_high(self) -> bool {
        matches!(self, Self::Control | Self::Audio)
    }
}

#[derive(Debug)]
struct Entry {
    seq: u64,
    at: Instant,
    msg: v1::StationMessage,
}

/// The queue policy; see the [module docs](self).
#[derive(Debug)]
pub struct OutboundQueue {
    limits: QueueLimits,
    drops: Arc<DropCounters>,
    seq: u64,
    control: VecDeque<Entry>,
    audio: VecDeque<Entry>,
    waterfall: VecDeque<Entry>,
    signals: VecDeque<Entry>,
    measurements: VecDeque<Entry>,
    status: Option<Entry>,
    closed: bool,
}

impl OutboundQueue {
    /// An empty queue counting drops in `drops`.
    pub fn new(limits: QueueLimits, drops: Arc<DropCounters>) -> Self {
        Self {
            limits,
            drops,
            seq: 0,
            control: VecDeque::new(),
            audio: VecDeque::new(),
            waterfall: VecDeque::new(),
            signals: VecDeque::new(),
            measurements: VecDeque::new(),
            status: None,
            closed: false,
        }
    }

    /// Number of queued messages.
    pub fn len(&self) -> usize {
        self.control.len()
            + self.audio.len()
            + self.waterfall.len()
            + self.signals.len()
            + self.measurements.len()
            + usize::from(self.status.is_some())
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether [`close`](Self::close) was called.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Stops accepting messages; queued ones can still be popped.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Queues `msg` at `now`. Returns `false` if it was not queued (closed,
    /// or an event over the cap).
    pub fn push(&mut self, msg: v1::StationMessage, now: Instant) -> bool {
        if self.closed {
            return false;
        }
        let class = Class::of(&msg);
        let entry = Entry {
            seq: self.seq,
            at: now,
            msg,
        };
        self.seq = self.seq.wrapping_add(1);
        match class {
            Class::Control => {
                let is_event = matches!(entry.msg.body, Some(Body::Event(_)));
                let events = self
                    .control
                    .iter()
                    .filter(|e| matches!(e.msg.body, Some(Body::Event(_))))
                    .count();
                if is_event && events >= self.limits.max_events {
                    DropCounters::add(&self.drops.events, 1);
                    return false;
                }
                self.control.push_back(entry);
            }
            Class::Audio => {
                bounded_push(
                    &mut self.audio,
                    entry,
                    self.limits.max_audio,
                    &self.drops.audio,
                );
            }
            Class::Waterfall => bounded_push(
                &mut self.waterfall,
                entry,
                self.limits.max_waterfall,
                &self.drops.waterfall,
            ),
            Class::Signal => bounded_push(
                &mut self.signals,
                entry,
                self.limits.max_signals,
                &self.drops.signals,
            ),
            Class::Status => self.status = Some(entry),
            Class::Measurement => bounded_push(
                &mut self.measurements,
                entry,
                self.limits.max_measurements,
                &self.drops.measurements,
            ),
        }
        self.expire(now);
        true
    }

    /// Applies the age rules of §6.3 at `now`.
    pub fn expire(&mut self, now: Instant) {
        let wf = self.limits.waterfall_max_age;
        let n = drop_older(&mut self.waterfall, now, wf);
        DropCounters::add(&self.drops.waterfall, n);
        let n = drop_older(&mut self.signals, now, wf);
        DropCounters::add(&self.drops.signals, n);
        let n = drop_older(&mut self.audio, now, self.limits.audio_max_age);
        DropCounters::add(&self.drops.audio, n);
        let n = drop_older(&mut self.measurements, now, self.limits.measurement_max_age);
        DropCounters::add(&self.drops.measurements, n);
    }

    /// The next message to send at `now`, after applying the age rules.
    pub fn pop(&mut self, now: Instant) -> Option<v1::StationMessage> {
        self.expire(now);
        let high = oldest(&mut [&mut self.control, &mut self.audio]);
        if let Some(entry) = high {
            return Some(entry.msg);
        }
        let status_seq = self.status.as_ref().map(|e| e.seq);
        let low_seq = [
            self.waterfall.front(),
            self.signals.front(),
            self.measurements.front(),
        ]
        .into_iter()
        .flatten()
        .map(|e| e.seq)
        .min();
        match (status_seq, low_seq) {
            (Some(s), Some(l)) if s < l => self.status.take().map(|e| e.msg),
            (Some(_), None) => self.status.take().map(|e| e.msg),
            _ => oldest(&mut [
                &mut self.waterfall,
                &mut self.signals,
                &mut self.measurements,
            ])
            .map(|e| e.msg),
        }
    }
}

fn bounded_push(q: &mut VecDeque<Entry>, entry: Entry, max: usize, counter: &AtomicU64) {
    q.push_back(entry);
    let excess = q.len().saturating_sub(max);
    q.drain(..excess);
    DropCounters::add(counter, excess);
}

fn drop_older(q: &mut VecDeque<Entry>, now: Instant, max_age: Duration) -> usize {
    let mut n = 0;
    while q
        .front()
        .is_some_and(|e| now.saturating_duration_since(e.at) > max_age)
    {
        q.pop_front();
        n += 1;
    }
    n
}

/// Pops the entry with the lowest sequence number among the queue fronts.
fn oldest(queues: &mut [&mut VecDeque<Entry>]) -> Option<Entry> {
    let idx = queues
        .iter()
        .enumerate()
        .filter_map(|(i, q)| q.front().map(|e| (i, e.seq)))
        .min_by_key(|&(_, seq)| seq)
        .map(|(i, _)| i)?;
    queues.get_mut(idx)?.pop_front()
}

/// A shared [`OutboundQueue`]: any task may push; the gRPC request stream
/// ([`Outbound::stream`]) is the single consumer.
#[derive(Debug)]
pub struct Outbound {
    queue: Mutex<OutboundQueue>,
    notify: Notify,
    finished: watch::Sender<bool>,
}

impl Outbound {
    /// A new open queue.
    pub fn new(limits: QueueLimits, drops: Arc<DropCounters>) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(OutboundQueue::new(limits, drops)),
            notify: Notify::new(),
            finished: watch::Sender::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, OutboundQueue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `msg`; `false` if it was not queued (see
    /// [`OutboundQueue::push`]).
    pub fn push(&self, msg: v1::StationMessage) -> bool {
        let queued = self.lock().push(msg, Instant::now());
        if queued {
            self.notify.notify_one();
        }
        queued
    }

    /// Queues a message with `body`.
    pub fn push_body(&self, body: Body) -> bool {
        self.push(v1::StationMessage { body: Some(body) })
    }

    /// Stops accepting messages. The stream ends once the queue is empty.
    pub fn close(&self) {
        self.lock().close();
        self.notify.notify_one();
    }

    /// Number of queued messages.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// The next message; `None` once closed and empty.
    pub async fn next(&self) -> Option<v1::StationMessage> {
        loop {
            let notified = self.notify.notified();
            {
                let mut q = self.lock();
                if let Some(msg) = q.pop(Instant::now()) {
                    return Some(msg);
                }
                if q.is_closed() {
                    self.finished.send_replace(true);
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Resolves once the stream has handed out every message and ended.
    pub async fn finished(&self) {
        let mut rx = self.finished.subscribe();
        // Only fails if the sender is dropped, which cannot happen while
        // `self` is alive.
        let _ = rx.wait_for(|done| *done).await;
    }

    /// The consuming stream for the gRPC request.
    pub fn stream(self: Arc<Self>) -> impl Stream<Item = v1::StationMessage> + Send + 'static {
        futures_util::stream::unfold(self, |q| async move {
            let msg = q.next().await?;
            Some((msg, q))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;

    fn audio(seq: u32) -> v1::StationMessage {
        v1::StationMessage {
            body: Some(Body::Audio(v1::AudioFrame {
                seq,
                ..Default::default()
            })),
        }
    }

    fn row(seq: u32) -> v1::StationMessage {
        v1::StationMessage {
            body: Some(Body::Waterfall(v1::WaterfallRow {
                seq,
                ..Default::default()
            })),
        }
    }

    fn response(id: u64) -> v1::StationMessage {
        v1::StationMessage {
            body: Some(Body::Response(v1::Response {
                request_id: id,
                ..Default::default()
            })),
        }
    }

    fn event(detail: &str) -> v1::StationMessage {
        v1::StationMessage {
            body: Some(Body::Event(v1::Event {
                detail: detail.into(),
                ..Default::default()
            })),
        }
    }

    fn status(uptime_s: u64) -> v1::StationMessage {
        v1::StationMessage {
            body: Some(Body::Status(v1::StationStatus {
                uptime_s,
                ..Default::default()
            })),
        }
    }

    fn signal() -> v1::StationMessage {
        v1::StationMessage {
            body: Some(Body::Signal(v1::ChannelSignal::default())),
        }
    }

    fn describe(msg: &v1::StationMessage) -> String {
        match &msg.body {
            Some(Body::Audio(a)) => format!("a{}", a.seq),
            Some(Body::Waterfall(w)) => format!("w{}", w.seq),
            Some(Body::Response(r)) => format!("r{}", r.request_id),
            Some(Body::Event(e)) => format!("e{}", e.detail),
            Some(Body::Status(s)) => format!("s{}", s.uptime_s),
            Some(Body::Signal(_)) => "g".into(),
            Some(Body::Hello(_)) => "h".into(),
            Some(Body::Noise(n)) => format!("n{}", n.window_start_unix_ms),
            None => "-".into(),
        }
    }

    fn drain(q: &mut OutboundQueue, now: Instant) -> Vec<String> {
        std::iter::from_fn(|| q.pop(now))
            .map(|m| describe(&m))
            .collect()
    }

    fn queue() -> (OutboundQueue, Arc<DropCounters>) {
        let drops = Arc::new(DropCounters::default());
        (
            OutboundQueue::new(QueueLimits::default(), Arc::clone(&drops)),
            drops,
        )
    }

    #[test]
    fn classes() {
        assert_eq!(Class::of(&audio(0)), Class::Audio);
        assert_eq!(Class::of(&row(0)), Class::Waterfall);
        assert_eq!(Class::of(&response(1)), Class::Control);
        assert_eq!(Class::of(&event("")), Class::Control);
        assert_eq!(Class::of(&status(0)), Class::Status);
        assert_eq!(Class::of(&signal()), Class::Signal);
        assert_eq!(Class::of(&v1::StationMessage::default()), Class::Control);
        assert!(Class::Audio.is_high() && Class::Control.is_high());
        assert!(
            !Class::Waterfall.is_high() && !Class::Status.is_high() && !Class::Signal.is_high()
        );
    }

    #[test]
    fn high_priority_first_fifo_within_class() {
        let (mut q, _) = queue();
        let t = Instant::now();
        for m in [
            row(1),
            status(1),
            audio(1),
            response(1),
            row(2),
            signal(),
            event("x"),
            audio(2),
        ] {
            assert!(q.push(m, t));
        }
        assert_eq!(q.len(), 8);
        assert_eq!(
            drain(&mut q, t),
            ["a1", "r1", "ex", "a2", "w1", "s1", "w2", "g"]
        );
        assert!(q.is_empty());
    }

    #[test]
    fn waterfall_dropped_after_250ms_audio_after_1s() {
        let (mut q, drops) = queue();
        let t0 = Instant::now();
        q.push(row(1), t0);
        q.push(audio(1), t0);
        q.push(response(1), t0);
        q.push(status(1), t0);
        q.push(row(2), t0 + Duration::from_millis(200));
        // At 300 ms: row 1 is 300 ms old (dropped), row 2 is 100 ms old.
        q.expire(t0 + Duration::from_millis(300));
        assert_eq!(drops.waterfall(), 1);
        assert_eq!(drops.audio(), 0);
        // At 1.1 s: audio is 1.1 s old (dropped); row 2 too; the response
        // and the status are never dropped by age.
        let out = drain(&mut q, t0 + Duration::from_millis(1100));
        assert_eq!(out, ["r1", "s1"]);
        assert_eq!(drops.waterfall(), 2);
        assert_eq!(drops.audio(), 1);
    }

    fn noise(minute: i64) -> v1::StationMessage {
        v1::StationMessage {
            body: Some(Body::Noise(v1::NoiseFloorReport {
                window_start_unix_ms: minute,
                ..Default::default()
            })),
        }
    }

    #[test]
    fn measurement_reports_are_low_priority_and_age_after_10s() {
        let (mut q, drops) = queue();
        let t0 = Instant::now();
        assert_eq!(Class::of(&noise(0)), Class::Measurement);
        assert!(!Class::Measurement.is_high());
        q.push(noise(1), t0);
        q.push(audio(1), t0);
        q.push(row(1), t0);
        q.push(event("duty"), t0);
        assert_eq!(drain(&mut q, t0), ["a1", "eduty", "n1", "w1"]);
        q.push(noise(2), t0);
        q.push(event("duty"), t0);
        let out = drain(&mut q, t0 + Duration::from_secs(11));
        assert_eq!(out, ["eduty"]);
        assert_eq!(drops.measurements(), 1);
        // Bounded in count too.
        for m in 0..20 {
            q.push(noise(m), t0);
        }
        assert_eq!(q.len(), 8);
        assert_eq!(drops.measurements(), 13);
    }

    #[test]
    fn signals_age_like_waterfall() {
        let (mut q, drops) = queue();
        let t0 = Instant::now();
        q.push(signal(), t0);
        assert!(q.pop(t0 + Duration::from_millis(251)).is_none());
        assert_eq!(drops.signals(), 1);
    }

    #[test]
    fn only_the_latest_status_is_kept() {
        let (mut q, _) = queue();
        let t = Instant::now();
        q.push(status(1), t);
        q.push(status(2), t);
        assert_eq!(drain(&mut q, t), ["s2"]);
    }

    #[test]
    fn count_bounds_drop_oldest() {
        let drops = Arc::new(DropCounters::default());
        let limits = QueueLimits {
            max_audio: 2,
            max_waterfall: 1,
            max_signals: 1,
            max_events: 2,
            ..QueueLimits::default()
        };
        let mut q = OutboundQueue::new(limits, Arc::clone(&drops));
        let t = Instant::now();
        for i in 0..4 {
            q.push(audio(i), t);
            q.push(row(i), t);
            q.push(signal(), t);
        }
        assert!(q.push(event("1"), t));
        assert!(q.push(event("2"), t));
        assert!(!q.push(event("3"), t), "event over the cap");
        assert!(q.push(response(9), t), "responses are never capped");
        assert_eq!(drops.audio(), 2);
        assert_eq!(drops.waterfall(), 3);
        assert_eq!(drops.signals(), 3);
        assert_eq!(drops.events(), 1);
        assert_eq!(drain(&mut q, t), ["a2", "a3", "e1", "e2", "r9", "w3", "g"]);
    }

    #[test]
    fn closed_queue_rejects_but_drains() {
        let (mut q, _) = queue();
        let t = Instant::now();
        q.push(response(1), t);
        q.close();
        assert!(q.is_closed());
        assert!(!q.push(response(2), t));
        assert_eq!(drain(&mut q, t), ["r1"]);
    }

    #[test]
    fn status_after_older_low_priority_items() {
        let (mut q, _) = queue();
        let t = Instant::now();
        q.push(row(1), t);
        q.push(status(1), t);
        q.push(row(2), t);
        assert_eq!(drain(&mut q, t), ["w1", "s1", "w2"]);
    }

    #[tokio::test(start_paused = true)]
    async fn stream_waits_for_items_and_ends_after_close() {
        let out = Outbound::new(QueueLimits::default(), Arc::default());
        let mut stream = Box::pin(Arc::clone(&out).stream());
        assert!(out.push(response(1)));
        assert!(out.push_body(Body::Audio(v1::AudioFrame::default())));
        assert_eq!(out.len(), 2);
        assert!(!out.is_empty());
        assert_eq!(describe(&stream.next().await.expect("r1")), "r1");
        assert_eq!(describe(&stream.next().await.expect("a0")), "a0");
        let producer = {
            let out = Arc::clone(&out);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                out.push(event("late"));
                out.close();
            })
        };
        assert_eq!(describe(&stream.next().await.expect("late")), "elate");
        assert!(stream.next().await.is_none());
        out.finished().await;
        producer.await.expect("producer");
        assert!(!out.push(response(3)));
    }
}
