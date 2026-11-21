// SPDX-License-Identifier: GPL-3.0-only
//! Beacons on the [`FakeBackend`](crate::FakeBackend)'s waterfall: a subscription of 3 kHz
//! around one of the five frequencies of the NCDXF/IARU International Beacon Project, in 64
//! bins, gets rows that follow the project's published schedule instead of the generic ones.
//! In each 10-second slot of the 3-minute cycle one beacon keys its callsign at 22 wpm and then
//! four 1-second dashes, 10 dB apart (100 W, 10 W, 1 W and 100 mW), over a noise floor, each
//! beacon with an SNR of its own. A test or a development station can hear beacons with no
//! radio, whatever band its simulated receiver is on.
//!
//! - **Schedule** (<https://www.ncdxf.org/beacon/beaconschedule.html>): the slot is
//!   `⌊(unix_s mod 180) / 10⌋` and the beacon on band `b` (0 for 14.100 MHz to 4 for
//!   28.200 MHz) is number `(slot − b) mod 18` in [`CALLSIGNS`]' order.
//! - **Keying** (<https://www.ncdxf.org/beacon/beaconcontroller.html>): a Morse element is
//!   54 ms, a letter space three, the word space before the dashes seven, and the dashes are
//!   a second each with a letter space between. Every transmission ends about 8.8 s into its
//!   slot.
//! - **Rows** carry the tone in one bin at the centre (23 Hz above the carrier), its power
//!   over the noise the beacon's SNR plus the part's level, averaged over the 100 ms before
//!   the row's time as a spectrum is. YV5B is listed off the air and sends nothing.
//!
//! Rows are a function of the wall clock the backend was given
//! ([`FakeBackend::with_wall_clock`](crate::FakeBackend::with_wall_clock)), the row's `seq`
//! and a seed, so a test with a fixed clock sees the same row every time.

use station_proto::v1;

use crate::fake::mix;

/// The five carriers, Hz, in schedule order (band 0 to 4).
pub const CARRIERS_HZ: [u64; 5] = [14_100_000, 18_110_000, 21_150_000, 24_930_000, 28_200_000];

/// The 18 beacons' callsigns in schedule order.
pub const CALLSIGNS: [&str; 18] = [
    "4U1UN", "VE8AT", "W6WX", "KH6RS", "ZL6B", "VK6RBP", "JA2IGY", "RR9O", "VR2B", "4S7B", "ZS6DN",
    "5Z4B", "4X6TU", "OH2B", "CS3B", "LU4AA", "OA4B", "YV5B",
];

/// The beacon listed off the air (YV5B): it sends nothing.
pub const OFF_AIR: usize = 17;

/// The span of a beacon subscription around its carrier, Hz.
pub const SPAN_HZ: u64 = 3_000;

/// The bins of a beacon subscription.
pub const BINS: u32 = 64;

/// A slot, ms.
const SLOT_MS: i64 = 10_000;
/// The cycle, s.
const CYCLE_S: i64 = 180;
/// Slots in a cycle, and beacons.
const CYCLE_SLOTS: usize = 18;
/// A Morse element, ms (22 wpm).
const UNIT_MS: i64 = 54;
/// A dash after the callsign, ms.
const DASH_MS: i64 = 1_000;
/// The word space before the first dash, and the letter space between dashes, in elements.
const WORD_SPACE_UNITS: i64 = 7;
const LETTER_SPACE_UNITS: i64 = 3;
/// The level of the callsign and each dash, dB: 100 W, 10 W, 1 W and 100 mW.
const DASH_POWER_DB: [f32; 4] = [0.0, -10.0, -20.0, -30.0];
/// The noise floor of the rows, dB/Hz.
pub const NOISE_DB_HZ: f32 = -140.0;
/// What a row's spectrum takes in before its time, ms.
const INTEGRATION_MS: i64 = 100;
/// The bin the tone is in: the one just above the carrier.
const TONE_BIN: usize = 32;

/// The band (0 to 4) of a subscription of `range` in `bins` that is a beacon's: 3 kHz in 64 bins
/// around one of the five carriers.
pub fn carrier_of(range: &v1::FrequencyRange, bins: u32) -> Option<usize> {
    if bins != BINS || range.end_hz.checked_sub(range.start_hz) != Some(SPAN_HZ) {
        return None;
    }
    let centre = range.start_hz + SPAN_HZ / 2;
    CARRIERS_HZ.iter().position(|&c| c == centre)
}

/// The beacon (an index into [`CALLSIGNS`]) on `band` in the slot that holds `unix_s`.
pub fn beacon_number(band: usize, unix_s: i64) -> usize {
    let slot = usize::try_from(unix_s.rem_euclid(CYCLE_S) / (SLOT_MS / 1000)).unwrap_or(0);
    (slot + CYCLE_SLOTS - band % CYCLE_SLOTS) % CYCLE_SLOTS
}

/// How far over the noise in a bin beacon `number`'s 100 W parts stand, dB: 30 dB for the first
/// beacon in schedule order, 1.5 dB less for each after it. `None` for the one off the air.
pub fn snr_db(number: usize) -> Option<f32> {
    let n = u8::try_from(number)
        .ok()
        .filter(|_| number != OFF_AIR && number < CYCLE_SLOTS)?;
    Some(30.0 - 1.5 * f32::from(n))
}

fn morse(c: char) -> &'static str {
    match c {
        'A' => ".-",
        'B' => "-...",
        'C' => "-.-.",
        'D' => "-..",
        'E' => ".",
        'F' => "..-.",
        'G' => "--.",
        'H' => "....",
        'I' => "..",
        'J' => ".---",
        'K' => "-.-",
        'L' => ".-..",
        'M' => "--",
        'N' => "-.",
        'O' => "---",
        'P' => ".--.",
        'Q' => "--.-",
        'R' => ".-.",
        'S' => "...",
        'T' => "-",
        'U' => "..-",
        'V' => "...-",
        'W' => ".--",
        'X' => "-..-",
        'Y' => "-.--",
        'Z' => "--..",
        '0' => "-----",
        '1' => ".----",
        '2' => "..---",
        '3' => "...--",
        '4' => "....-",
        '5' => ".....",
        '6' => "-....",
        '7' => "--...",
        '8' => "---..",
        '9' => "----.",
        _ => "",
    }
}

/// A keyed interval: start and end in ms from the slot's start, and its level in dB.
type Keyed = (i64, i64, f32);

/// What `callsign`'s beacon keys in its slot: the callsign's elements at 0 dB, then the four dashes.
fn keying(callsign: &str) -> Vec<Keyed> {
    let mut out = Vec::new();
    let mut t = 0_i64;
    for (i, c) in callsign.chars().enumerate() {
        if i > 0 {
            t += LETTER_SPACE_UNITS * UNIT_MS;
        }
        for (j, e) in morse(c).chars().enumerate() {
            if j > 0 {
                t += UNIT_MS;
            }
            let len = if e == '-' { 3 * UNIT_MS } else { UNIT_MS };
            out.push((t, t + len, DASH_POWER_DB[0]));
            t += len;
        }
    }
    t += WORD_SPACE_UNITS * UNIT_MS;
    for (d, power) in DASH_POWER_DB.iter().enumerate() {
        if d > 0 {
            t += LETTER_SPACE_UNITS * UNIT_MS;
        }
        out.push((t, t + DASH_MS, *power));
        t += DASH_MS;
    }
    out
}

/// The power over the noise in the tone's bin of the row at `t_ms` into a slot, linear: the keyed
/// parts of the 100 ms before it, weighted by how long each was on.
fn tone_power(number: usize, t_ms: i64) -> f32 {
    let Some(snr) = snr_db(number) else {
        return 0.0;
    };
    let lo = t_ms - INTEGRATION_MS;
    let mut acc = 0.0_f32;
    for (start, end, level) in keying(CALLSIGNS[number]) {
        let overlap = (end.min(t_ms) - start.max(lo)).max(0);
        #[allow(clippy::cast_precision_loss, reason = "at most 100 ms")]
        let share = overlap as f32 / INTEGRATION_MS as f32;
        acc += share * 10_f32.powf((snr + level) / 10.0);
    }
    acc
}

/// The row of a beacon subscription on `band` at `unix_ms`: [`BINS`] bytes quantized with `q`, a
/// noise floor with a ripple (a few dB, like a spectrum of a few averages) and the tone of the
/// beacon the schedule has on the air, keyed as it keys. The same arguments give the same row.
pub fn row(band: usize, unix_ms: i64, seq: u32, seed: u64, q: v1::Quantization) -> Vec<u8> {
    let number = beacon_number(band, unix_ms.div_euclid(1000));
    let power = tone_power(number, unix_ms.rem_euclid(SLOT_MS));
    (0..BINS as usize)
        .map(|i| {
            let r = mix(seed ^ mix((u64::from(seq) << 32) | u64::try_from(i).unwrap_or(0)));
            let sum: i32 = r.to_le_bytes()[..4].iter().map(|&b| i32::from(b)).sum();
            // The sum has mean 510 and σ 147: scale it to σ 2 dB.
            #[allow(clippy::cast_precision_loss, reason = "a few hundred")]
            let ripple = (sum - 510) as f32 * 2.0 / 147.0;
            let mut db = NOISE_DB_HZ + ripple;
            if i == TONE_BIN && power > 0.0 {
                let linear = 10_f32.powf(db / 10.0) + 10_f32.powf(NOISE_DB_HZ / 10.0) * power;
                db = 10.0 * linear.log10();
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to a byte"
            )]
            let v = ((db - q.offset_db) / q.step_db).round().clamp(0.0, 255.0) as u8;
            v
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published schedule, read 2026-11-21: the callsigns on 14.100, 18.110, 21.150,
    /// 24.930 and 28.200 MHz at a time (mm:ss into the 3-minute cycle).
    const PUBLISHED: [(i64, [&str; 5]); 8] = [
        (0, ["4U1UN", "YV5B", "OA4B", "LU4AA", "CS3B"]),
        (10, ["VE8AT", "4U1UN", "YV5B", "OA4B", "LU4AA"]),
        (20, ["W6WX", "VE8AT", "4U1UN", "YV5B", "OA4B"]),
        (30, ["KH6RS", "W6WX", "VE8AT", "4U1UN", "YV5B"]),
        (40, ["ZL6B", "KH6RS", "W6WX", "VE8AT", "4U1UN"]),
        (50, ["VK6RBP", "ZL6B", "KH6RS", "W6WX", "VE8AT"]),
        (140, ["CS3B", "OH2B", "4X6TU", "5Z4B", "ZS6DN"]),
        (150, ["LU4AA", "CS3B", "OH2B", "4X6TU", "5Z4B"]),
    ];

    const Q: v1::Quantization = v1::Quantization {
        offset_db: -160.0,
        step_db: 0.5,
    };

    #[test]
    fn the_schedule_is_the_published_one() {
        for (cycle_s, row) in PUBLISHED {
            // A time in the cycle, whatever the cycle.
            for cycle in [0, 7_654, 9_999_999] {
                let unix_s = cycle * CYCLE_S + cycle_s;
                for (band, callsign) in row.iter().enumerate() {
                    assert_eq!(
                        CALLSIGNS[beacon_number(band, unix_s)],
                        *callsign,
                        "band {band} at {cycle_s} s"
                    );
                }
            }
        }
    }

    #[test]
    fn a_subscription_is_a_beacons_when_it_is_3_khz_in_64_bins_around_a_carrier() {
        let around = |centre: u64, span: u64| v1::FrequencyRange {
            start_hz: centre - span / 2,
            end_hz: centre + span / 2,
        };
        for (band, f) in CARRIERS_HZ.iter().enumerate() {
            assert_eq!(carrier_of(&around(*f, 3_000), 64), Some(band));
        }
        assert_eq!(carrier_of(&around(14_100_000, 3_000), 128), None);
        assert_eq!(carrier_of(&around(14_100_000, 4_000), 64), None);
        assert_eq!(carrier_of(&around(14_101_000, 3_000), 64), None);
        assert_eq!(carrier_of(&around(7_100_000, 3_000), 64), None);
    }

    #[test]
    fn the_keying_is_the_callsign_then_four_dashes_ending_before_the_slots_end() {
        for callsign in CALLSIGNS {
            let k = keying(callsign);
            let dashes: Vec<&Keyed> = k.iter().filter(|(s, e, _)| e - s == DASH_MS).collect();
            assert_eq!(dashes.len(), 4, "{callsign}");
            assert_eq!(
                dashes.iter().map(|d| d.2).collect::<Vec<_>>(),
                DASH_POWER_DB
            );
            // The published figures are rounded: JA2IGY's last dash ends at 8.81 s.
            assert!(k.last().expect("keyed").1 <= 8_850, "{callsign}");
            assert!(k.windows(2).all(|w| w[0].1 < w[1].0), "{callsign}");
        }
        // 4U1UN: "....-" is four dots and a dash, 54 ms each and 162 ms.
        let k = keying("4U1UN");
        assert_eq!(k[0], (0, 54, 0.0));
        assert_eq!(k[1], (108, 162, 0.0));
        assert_eq!(k[4], (432, 594, 0.0));
    }

    #[test]
    fn the_scheduled_beacon_is_in_its_bin_while_it_keys_and_not_after() {
        // 14.100 MHz, the slot at 00:00 of a cycle: 4U1UN at 30 dB; the first dash is
        // keyed from the end of the callsign and the word space.
        let slot_s = 1_790_942_400 / CYCLE_S * CYCLE_S;
        let dash = keying("4U1UN")
            .iter()
            .find(|k| k.1 - k.0 == DASH_MS)
            .copied()
            .expect("dash");
        let at = |t_ms: i64| slot_s * 1000 + t_ms;
        let tone = |t_ms: i64| {
            let r = row(0, at(t_ms), 1, 7, Q);
            assert_eq!(r.len(), 64);
            let noise: Vec<u8> = r
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != TONE_BIN)
                .map(|(_, v)| *v)
                .collect();
            let floor = f32::from(*noise.iter().max().expect("bins"));
            (f32::from(r[TONE_BIN]) - floor) * 0.5
        };
        // Mid-dash: about 30 dB over the noise, well over its brightest bin.
        assert!(tone(dash.0 + 500) > 20.0, "{}", tone(dash.0 + 500));
        // After the transmission: the same bin is noise.
        assert!(tone(9_500) < 8.0, "{}", tone(9_500));
        // The second dash is 10 dB under the first.
        let second = keying("4U1UN")
            .iter()
            .filter(|k| k.1 - k.0 == DASH_MS)
            .nth(1)
            .copied()
            .expect("dash");
        let (a, b) = (tone(dash.0 + 500), tone(second.0 + 500));
        assert!((6.0..14.0).contains(&(a - b)), "{a} {b}");
        // A beacon of its own SNR on another band in the same slot: 17 m has YV5B, off the
        // air: noise throughout.
        let off_air = row(1, at(dash.0 + 500), 1, 7, Q);
        let floor = f32::from(*off_air.iter().max().expect("bins"));
        assert!(
            (f32::from(off_air[TONE_BIN]) - floor) * 0.5 < 8.0,
            "off air"
        );
        // The row is a function of its arguments.
        assert_eq!(row(0, at(1234), 5, 9, Q), row(0, at(1234), 5, 9, Q));
        assert_ne!(row(0, at(1234), 5, 9, Q), row(0, at(1234), 6, 9, Q));
    }

    #[test]
    fn every_beacon_but_the_one_off_the_air_has_an_snr_and_they_differ() {
        let snrs: Vec<f32> = (0..CYCLE_SLOTS).filter_map(snr_db).collect();
        assert_eq!(snrs.len(), 17);
        assert!(snrs.windows(2).all(|w| w[0] > w[1]));
        assert_eq!(snr_db(OFF_AIR), None);
        assert_eq!(snr_db(99), None);
    }
}
