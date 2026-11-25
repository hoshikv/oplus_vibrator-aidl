use std::f32::consts::TAU;

use crate::gen::android::hardware::vibrator::CompositeEffect::CompositeEffect;
use crate::gen::android::hardware::vibrator::CompositePrimitive::CompositePrimitive;
use crate::motor::SAMPLE_RATE;

const HE_CONTINUOUS: i32 = 0x1000; // 4096
const HE_TRANSIENT: i32 = 0x1001; // 4097

/// Synthesizes an OPlus/RichTap HE 1.0 or 2.0 `int[]` pattern into a 24 kHz signed 8-bit PCM buffer.
pub fn synthesize_he(
    pattern: &[i32],
    looper: i32,
    interval_ms: i32,
    global_amp: f32,
    freq_offset: i32,
    f0_hz: f32,
) -> Vec<i8> {
    let mut events = parse_he_events(pattern);
    if events.is_empty() {
        // Fallback single crisp transient pulse if pattern format is minimal
        events.push(HeEvent {
            kind: HE_TRANSIENT,
            start_ms: 0,
            duration_ms: 15,
            intensity: 100,
            frequency: 50,
            curves: Vec::new(),
        });
    }

    let max_end_ms = events
        .iter()
        .map(|e| e.start_ms + e.duration_ms.max(12))
        .max()
        .unwrap_or(20)
        .clamp(12, 10_000);

    let single_samples = (max_end_ms as usize * SAMPLE_RATE as usize) / 1000;
    let mut mix = vec![0.0f32; single_samples];

    for ev in &events {
        let start_idx = ((ev.start_ms.max(0) as usize) * SAMPLE_RATE as usize) / 1000;
        let dur_ms = if ev.kind == HE_TRANSIENT {
            ev.duration_ms.clamp(8, 25)
        } else {
            ev.duration_ms.clamp(10, 5000)
        };
        let n_samples = ((dur_ms as usize) * SAMPLE_RATE as usize) / 1000;
        let base_int = (ev.intensity.clamp(0, 100) as f32 / 100.0) * global_amp.clamp(0.0, 1.0);
        // Map RichTap 0..100 frequency parameter around resonant F0 (~0.75*f0 .. 1.35*f0)
        let f_param = (ev.frequency + freq_offset).clamp(0, 100) as f32 / 100.0;
        let base_freq = (f0_hz * (0.75 + 0.60 * f_param)).clamp(80.0, 320.0);

        let mut phase = 0.0f32;
        for i in 0..n_samples {
            let idx = start_idx + i;
            if idx >= mix.len() {
                break;
            }
            let t_ms = (i as f32 * 1000.0) / SAMPLE_RATE as f32;
            let frac = i as f32 / n_samples.max(1) as f32;

            let (env, freq_hz) = if ev.kind == HE_TRANSIENT {
                // Crisp LRA click: fast attack (15%), exponential decay
                let env = if frac < 0.15 {
                    frac / 0.15
                } else {
                    (-(frac - 0.15) * 4.5).exp()
                };
                (env, base_freq)
            } else {
                let (c_int, c_freq) = eval_curve(&ev.curves, t_ms, dur_ms as f32);
                // Smooth 3ms attack/release to prevent audible click
                let fade = (t_ms / 3.0).min(1.0).min(((dur_ms as f32 - t_ms) / 3.0).max(0.0));
                (c_int * fade, (base_freq + c_freq).clamp(80.0, 320.0))
            };

            phase += TAU * freq_hz / SAMPLE_RATE as f32;
            if phase > TAU {
                phase -= TAU;
            }
            mix[idx] += phase.sin() * base_int * env;
        }
    }

    let single_pcm: Vec<i8> = mix
        .into_iter()
        .map(|v| (v * 127.0).round().clamp(-128.0, 127.0) as i8)
        .collect();

    let loops = looper.clamp(1, 16) as usize;
    if loops == 1 {
        return single_pcm;
    }

    let gap_samples = (interval_ms.clamp(0, 2000) as usize * SAMPLE_RATE as usize) / 1000;
    let mut out = Vec::with_capacity((single_pcm.len() + gap_samples) * loops);
    for i in 0..loops {
        out.extend_from_slice(&single_pcm);
        if i + 1 < loops && gap_samples > 0 {
            out.resize(out.len() + gap_samples, 0);
        }
    }
    out
}

/// Synthesizes Android `IVibrator::compose(CompositeEffect[])` into one seamless 24 kHz PCM buffer.
pub fn synthesize_compose(composite: &[CompositeEffect], global_amp: f32, f0_hz: f32) -> Vec<i8> {
    let mut out: Vec<i8> = Vec::new();
    for step in composite {
        let delay_ms = step.delayMs.clamp(0, 5000) as usize;
        if delay_ms > 0 {
            let pad = (delay_ms * SAMPLE_RATE as usize) / 1000;
            out.resize(out.len() + pad, 0);
        }
        if step.primitive == CompositePrimitive::NOOP {
            continue;
        }
        let scale = (step.scale.clamp(0.0, 1.0) * global_amp.clamp(0.0, 1.0)).clamp(0.0, 1.0);
        let prim_pcm = synthesize_primitive(step.primitive, scale, f0_hz);
        out.extend_from_slice(&prim_pcm);
    }
    out
}

fn synthesize_primitive(prim: CompositePrimitive, scale: f32, f0_hz: f32) -> Vec<i8> {
    let (dur_ms, f_start, f_end, rise_frac, decay) = match prim {
        CompositePrimitive::CLICK => (12, f0_hz * 1.05, f0_hz * 1.05, 0.15, 4.5),
        CompositePrimitive::LIGHT_TICK => (8, f0_hz * 1.25, f0_hz * 1.25, 0.15, 5.5),
        CompositePrimitive::LOW_TICK => (14, f0_hz * 0.85, f0_hz * 0.85, 0.15, 4.0),
        CompositePrimitive::THUD => (28, f0_hz * 0.75, f0_hz * 0.65, 0.20, 3.0),
        CompositePrimitive::QUICK_RISE => (45, f0_hz * 0.75, f0_hz * 1.20, 0.85, 1.5),
        CompositePrimitive::SLOW_RISE => (110, f0_hz * 0.70, f0_hz * 1.15, 0.88, 1.2),
        CompositePrimitive::QUICK_FALL => (55, f0_hz * 1.20, f0_hz * 0.75, 0.12, 2.8),
        CompositePrimitive::SPIN => (120, f0_hz * 0.90, f0_hz * 1.15, 0.30, 1.0),
        _ => (12, f0_hz, f0_hz, 0.15, 4.5),
    };
    let n = (dur_ms * SAMPLE_RATE as usize) / 1000;
    let mut out = Vec::with_capacity(n);
    let mut phase = 0.0f32;
    for i in 0..n {
        let frac = i as f32 / n.max(1) as f32;
        let freq = f_start + (f_end - f_start) * frac;
        let env = if frac < rise_frac {
            frac / rise_frac
        } else {
            (-(frac - rise_frac) * decay).exp()
        };
        phase += TAU * freq / SAMPLE_RATE as f32;
        if phase > TAU {
            phase -= TAU;
        }
        let s = (phase.sin() * scale * env * 127.0).round().clamp(-128.0, 127.0) as i8;
        out.push(s);
    }
    out
}

struct HeEvent {
    kind: i32,
    start_ms: i32,
    duration_ms: i32,
    intensity: i32,
    frequency: i32,
    curves: Vec<(f32, f32, f32)>, // (time_ms, intensity 0..1, freq_delta_hz)
}

fn eval_curve(curves: &[(f32, f32, f32)], t_ms: f32, _dur_ms: f32) -> (f32, f32) {
    if curves.is_empty() {
        return (1.0, 0.0);
    }
    if curves.len() == 1 || t_ms <= curves[0].0 {
        return (curves[0].1, curves[0].2);
    }
    for w in curves.windows(2) {
        let (t0, i0, f0) = w[0];
        let (t1, i1, f1) = w[1];
        if t_ms <= t1 {
            let span = (t1 - t0).max(0.001);
            let u = ((t_ms - t0) / span).clamp(0.0, 1.0);
            return (i0 + (i1 - i0) * u, f0 + (f1 - f0) * u);
        }
    }
    let last = curves[curves.len() - 1];
    (last.1, last.2)
}

/// Parses both RichTap HE 1.0 and HE 2.0 `int[]` arrays.
/// Events are marked by `0x1000` (continuous) or `0x1001` (transient).
fn parse_he_events(data: &[i32]) -> Vec<HeEvent> {
    let mut events = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let tag = data[i];
        if tag == HE_TRANSIENT {
            // HE 1.0: [0x1001, relTime, intensity, freq, ...] (5 ints)
            // HE 2.0: [0x1001, len, index, relTime, intensity, freq, ...] (7-8 ints)
            if i + 5 < data.len() && data[i + 1] <= 10 && data[i + 2] >= 0 && data[i + 2] <= 64 {
                // HE 2.0 layout
                let rel_time = data[i + 3].max(0);
                let intensity = data[i + 4].clamp(0, 100);
                let freq = data[i + 5].clamp(0, 100);
                events.push(HeEvent {
                    kind: HE_TRANSIENT,
                    start_ms: rel_time,
                    duration_ms: 15,
                    intensity,
                    frequency: freq,
                    curves: Vec::new(),
                });
                i += 6;
            } else if i + 3 < data.len() {
                // HE 1.0 layout
                let rel_time = data[i + 1].max(0);
                let intensity = data[i + 2].clamp(0, 100);
                let freq = data[i + 3].clamp(0, 100);
                events.push(HeEvent {
                    kind: HE_TRANSIENT,
                    start_ms: rel_time,
                    duration_ms: 15,
                    intensity,
                    frequency: freq,
                    curves: Vec::new(),
                });
                i += 4;
            } else {
                break;
            }
        } else if tag == HE_CONTINUOUS {
            // HE 2.0: [0x1000, len, index, relTime, intensity, freq, duration, pointCount, (t, int, freq)*]
            // HE 1.0: [0x1000, relTime, intensity, freq, duration, pointCount, (t, int, freq)*]
            if i + 7 < data.len() && data[i + 1] <= 64 && data[i + 2] >= 0 && data[i + 2] <= 64 && data[i + 6] > 0 {
                let rel_time = data[i + 3].max(0);
                let intensity = data[i + 4].clamp(0, 100);
                let freq = data[i + 5].clamp(0, 100);
                let dur = data[i + 6].clamp(10, 5000);
                let pcount = data[i + 7].clamp(0, 16) as usize;
                let mut curves = Vec::with_capacity(pcount);
                let mut cur = i + 8;
                for _ in 0..pcount {
                    if cur + 2 >= data.len() {
                        break;
                    }
                    curves.push((
                        data[cur] as f32,
                        (data[cur + 1].clamp(0, 100) as f32) / 100.0,
                        data[cur + 2] as f32,
                    ));
                    cur += 3;
                }
                events.push(HeEvent {
                    kind: HE_CONTINUOUS,
                    start_ms: rel_time,
                    duration_ms: dur,
                    intensity,
                    frequency: freq,
                    curves,
                });
                i = cur;
            } else if i + 5 < data.len() {
                let rel_time = data[i + 1].max(0);
                let intensity = data[i + 2].clamp(0, 100);
                let freq = data[i + 3].clamp(0, 100);
                let dur = data[i + 4].clamp(10, 5000);
                let pcount = data[i + 5].clamp(0, 16) as usize;
                let mut curves = Vec::with_capacity(pcount);
                let mut cur = i + 6;
                for _ in 0..pcount {
                    if cur + 2 >= data.len() {
                        break;
                    }
                    curves.push((
                        data[cur] as f32,
                        (data[cur + 1].clamp(0, 100) as f32) / 100.0,
                        data[cur + 2] as f32,
                    ));
                    cur += 3;
                }
                events.push(HeEvent {
                    kind: HE_CONTINUOUS,
                    start_ms: rel_time,
                    duration_ms: dur,
                    intensity,
                    frequency: freq,
                    curves,
                });
                i = cur;
            } else {
                break;
            }
        } else {
            i += 1;
        }
    }
    events
}
