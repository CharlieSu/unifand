use serde::Deserialize;

pub const RGB_REPORT_LEN: usize = 353;
pub const MODE_STATIC: u8 = 0x01;
pub const MODE_BREATHING: u8 = 0x02;
pub const MODE_RUNWAY: u8 = 0x1C;
pub const SPEEDS: [u8; 5] = [0x02, 0x01, 0x00, 0xFF, 0xFE];
const TRANSACTION_ID: u8 = 0xE0;
const LEDS_PER_CHANNEL: usize = 96; // 6 fans x 16 LEDs; uniform fill is count-agnostic

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct ColorStop {
    pub duty: u8,
    pub color: [u8; 3],
}

pub fn start_packet(channel: u8, num_fans: u8) -> [u8; RGB_REPORT_LEN] {
    let mut p = [0u8; RGB_REPORT_LEN];
    p[0] = TRANSACTION_ID;
    p[1] = 0x10;
    p[2] = 0x60;
    p[3] = ((channel - 1) << 4) + num_fans;
    p
}

pub fn commit_packet(channel: u8, mode: u8, speed: u8, brightness: u8) -> [u8; RGB_REPORT_LEN] {
    let mut p = [0u8; RGB_REPORT_LEN];
    p[0] = TRANSACTION_ID;
    p[1] = 0x10 + (channel - 1);
    p[2] = mode;
    p[3] = speed;
    p[4] = 0x00; // direction
    p[5] = brightness;
    p
}

/// Multi-color packet cycling through colors for 96 LEDs. Wire order is R,B,G.
/// A single-element slice fills the channel with a uniform color. Empty slice
/// returns a header-only packet.
pub fn multi_color_packet(channel: u8, colors: &[(u8, u8, u8)]) -> [u8; RGB_REPORT_LEN] {
    let mut p = [0u8; RGB_REPORT_LEN];
    p[0] = TRANSACTION_ID;
    p[1] = 0x30 + (channel - 1);
    if colors.is_empty() {
        return p;
    }
    for i in 0..LEDS_PER_CHANNEL {
        let (r, g, b) = colors[i % colors.len()];
        p[2 + i * 3] = r;
        p[2 + i * 3 + 1] = b;
        p[2 + i * 3 + 2] = g;
    }
    p
}

/// Per-component linear interpolation over sorted stops, clamped at both ends.
pub fn duty_color(stops: &[ColorStop], duty: u8) -> (u8, u8, u8) {
    debug_assert!(!stops.is_empty());
    let d = duty as f64;
    if d <= stops[0].duty as f64 {
        let c = stops[0].color;
        return (c[0], c[1], c[2]);
    }
    let last = stops[stops.len() - 1];
    if d >= last.duty as f64 {
        return (last.color[0], last.color[1], last.color[2]);
    }
    for w in stops.windows(2) {
        let (a, b) = (w[0], w[1]);
        if d <= b.duty as f64 {
            let frac = (d - a.duty as f64) / (b.duty as f64 - a.duty as f64);
            let mix = |x: u8, y: u8| (x as f64 + frac * (y as f64 - x as f64)).round() as u8;
            return (
                mix(a.color[0], b.color[0]),
                mix(a.color[1], b.color[1]),
                mix(a.color[2], b.color[2]),
            );
        }
    }
    (last.color[0], last.color[1], last.color[2])
}

/// Quantize duty 0..=100 into `buckets` bins (0..buckets-1).
pub fn bucket(duty: u8, buckets: u8) -> u8 {
    let d = duty.min(100) as u16;
    let b = buckets as u16;
    ((d * (b - 1)) / 100) as u8
}

/// Representative duty (the bucket's lower edge) for bucket index `b`,
/// inverting `bucket()`: `bucket(bucket_duty(b, buckets), buckets) == b`
/// for every `b` in `0..buckets`. Lets a caller derive a color that only
/// changes when the bucket changes, instead of on every raw-duty wiggle.
pub fn bucket_duty(b: u8, buckets: u8) -> u8 {
    let denom = (buckets - 1) as u32;
    let num = b as u32 * 100;
    (num.div_ceil(denom)).min(100) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stops() -> Vec<ColorStop> {
        vec![
            ColorStop {
                duty: 30,
                color: [0, 0, 255],
            },
            ColorStop {
                duty: 50,
                color: [0, 255, 0],
            },
            ColorStop {
                duty: 100,
                color: [255, 0, 0],
            },
        ]
    }

    #[test]
    fn packets_are_353_bytes_with_correct_headers() {
        let s = start_packet(1, 4);
        assert_eq!(s.len(), RGB_REPORT_LEN);
        assert_eq!(&s[..4], &[0xE0, 0x10, 0x60, 0x04]); // ch0=0, 4 fans
        let s2 = start_packet(2, 4);
        assert_eq!(&s2[..4], &[0xE0, 0x10, 0x60, 0x14]); // ch0=1
        let c = commit_packet(1, MODE_STATIC, SPEEDS[0], 0);
        assert_eq!(&c[..6], &[0xE0, 0x10, 0x01, 0x02, 0x00, 0x00]);
        let c2 = commit_packet(3, MODE_STATIC, SPEEDS[0], 2);
        assert_eq!(&c2[..6], &[0xE0, 0x12, 0x01, 0x02, 0x00, 0x02]);
    }

    #[test]
    fn single_color_multi_color_packet_uses_rbg_order_for_96_leds() {
        let p = multi_color_packet(1, &[(10, 20, 30)]); // r,g,b
        assert_eq!(p.len(), RGB_REPORT_LEN);
        assert_eq!(&p[..2], &[0xE0, 0x30]);
        // wire order is R,B,G
        assert_eq!(&p[2..5], &[10, 30, 20]);
        // 96th LED still populated, remainder zero
        assert_eq!(&p[2 + 95 * 3..2 + 96 * 3], &[10, 30, 20]);
        assert!(p[2 + 96 * 3..].iter().all(|&b| b == 0));
        let p2 = multi_color_packet(2, &[(1, 2, 3)]);
        assert_eq!(p2[1], 0x31);
    }

    #[test]
    fn gradient_interpolates_and_clamps() {
        assert_eq!(duty_color(&stops(), 10), (0, 0, 255)); // clamp low
        assert_eq!(duty_color(&stops(), 30), (0, 0, 255)); // exact
        assert_eq!(duty_color(&stops(), 40), (0, 128, 128)); // halfway 30..50
        assert_eq!(duty_color(&stops(), 100), (255, 0, 0)); // exact top
        assert_eq!(duty_color(&stops(), 120), (255, 0, 0)); // clamp high
    }

    #[test]
    fn bucket_quantizes_duty() {
        assert_eq!(bucket(0, 8), 0);
        assert_eq!(bucket(100, 8), 7);
        assert_eq!(bucket(50, 8), 3); // 50*8/101
                                      // adjacent duties in the same bucket
        assert_eq!(bucket(51, 8), bucket(50, 8));
    }

    #[test]
    fn bucket_duty_round_trips_through_bucket() {
        for buckets in [2u8, 8, 10, 20] {
            for b in 0..buckets {
                let d = bucket_duty(b, buckets);
                assert_eq!(
                    bucket(d, buckets),
                    b,
                    "bucket_duty({b}, {buckets}) = {d} should map back to bucket {b}"
                );
            }
        }
    }

    #[test]
    fn bucket_duty_is_stable_within_a_bucket() {
        // Every duty in the same bucket as `d` must produce the same
        // bucket_duty-derived representative once re-quantized.
        let buckets = 8;
        for duty in 0..=100u8 {
            let b = bucket(duty, buckets);
            assert_eq!(bucket(bucket_duty(b, buckets), buckets), b);
        }
    }

    #[test]
    fn commit_packet_carries_mode_and_speed() {
        let c = commit_packet(1, MODE_BREATHING, SPEEDS[4], 0);
        assert_eq!(&c[..6], &[0xE0, 0x10, 0x02, 0xFE, 0x00, 0x00]);
        let r = commit_packet(2, MODE_RUNWAY, SPEEDS[0], 3);
        assert_eq!(&r[..6], &[0xE0, 0x11, 0x1C, 0x02, 0x00, 0x03]);
    }

    #[test]
    fn multi_color_packet_cycles_colors_in_rbg_order() {
        let p = multi_color_packet(1, &[(255, 0, 80), (255, 0, 0)]);
        assert_eq!(&p[2..5], &[255, 80, 0]); // orange, R,B,G
        assert_eq!(&p[5..8], &[255, 0, 0]); // red
        assert_eq!(&p[8..11], &[255, 80, 0]); // cycles
                                              // single color == uniform fill across all 96 LEDs
        let uniform = multi_color_packet(1, &[(1, 2, 3)]);
        for i in 0..LEDS_PER_CHANNEL {
            assert_eq!(&uniform[2 + i * 3..2 + i * 3 + 3], &[1, 3, 2]); // R,B,G
        }
    }

    #[test]
    fn multi_color_packet_empty_is_header_only() {
        let p = multi_color_packet(1, &[]);
        assert_eq!(&p[..2], &[0xE0, 0x30]); // header
        assert!(p[2..].iter().all(|&b| b == 0)); // remaining bytes zero
        let p2 = multi_color_packet(3, &[]);
        assert_eq!(&p2[..2], &[0xE0, 0x32]); // header with channel 3
    }
}
