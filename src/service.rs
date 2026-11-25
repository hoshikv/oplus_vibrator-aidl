use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::{BinderResult, Strong};

use crate::effects::EffectLibrary;
use crate::gen::android::hardware::vibrator::Braking::Braking;
use crate::gen::android::hardware::vibrator::CompositeEffect::CompositeEffect;
use crate::gen::android::hardware::vibrator::CompositePrimitive::CompositePrimitive;
use crate::gen::android::hardware::vibrator::CompositePwleV2::CompositePwleV2;
use crate::gen::android::hardware::vibrator::Effect::Effect;
use crate::gen::android::hardware::vibrator::EffectStrength::EffectStrength;
use crate::gen::android::hardware::vibrator::FrequencyAccelerationMapEntry::FrequencyAccelerationMapEntry as FontMatrixEntry;
use crate::gen::android::hardware::vibrator::IVibrator::IVibrator;
use crate::gen::android::hardware::vibrator::IVibratorCallback::IVibratorCallback;
use crate::gen::android::hardware::vibrator::PrimitivePwle::PrimitivePwle;
use crate::gen::android::hardware::vibrator::VendorEffect::VendorEffect;
use crate::motor::Motor;
use crate::synth;
use crate::{alogd, aloge};

const CAP_ON_CALLBACK: i32 = 1 << 0;
const CAP_PERFORM_CALLBACK: i32 = 1 << 1;
const CAP_AMPLITUDE_CONTROL: i32 = 1 << 2;
const CAP_EXTERNAL_CONTROL: i32 = 1 << 3;
const CAP_COMPOSE_EFFECTS: i32 = 1 << 5;
const CAP_GET_RESONANT_FREQUENCY: i32 = 1 << 7;
const CAP_GET_Q_FACTOR: i32 = 1 << 8;

const MAX_DURATION_MS: u16 = 0xFFFF;
const DEFAULT_RING_PATTERN: &[u16] = &[60, 50, 60, 50, 60, 220];
const DEFAULT_RING_AMP: f32 = 0.85;

pub struct Inner {
    pub(crate) motor: Option<Motor>,
    pub(crate) effects: EffectLibrary,
    /// Standard Android amplitude (0.0..=1.0) used for IVibrator::on() / FF_CONSTANT
    pub(crate) amplitude: f32,
    /// ColorOS per-effect amplitude set by IVibrator::setAmplitude(1.0..=2.0) -> linear (amp - 1.0)
    pub(crate) effect_amplitude: Option<f32>,
    /// Dynamic scale for RichTap HE (0.0..=1.0)
    pub(crate) dynamic_scale: f32,
    /// 0 = Crisp (def_style / 10000.0), 1 = Gentle (soft_style / 10001.0)
    pub(crate) touch_style: i32,
    pub(crate) f0_hz: f32,
    pub(crate) ext_gen: u64,
    pub(crate) ext_active: bool,
    pub(crate) ext_saved_amp: Option<f32>,
    /// Tracks when a short one-shot RTP effect started so premature IVibrator::off() doesn't kill it mid-FIFO.
    pub(crate) short_rtp_until: Option<Instant>,
}

impl Inner {
    pub(crate) fn stop_effect(&mut self) {
        if let Some(m) = self.motor.as_mut() {
            m.stop();
        }
    }

    pub(crate) fn play_constant(&mut self, len_ms: u16, amp: f32) -> bool {
        let a = amp.clamp(0.0, 1.0);
        if a <= 0.001 || len_ms == 0 {
            self.stop_effect();
            return false;
        }
        match self.motor.as_mut() {
            Some(m) => m.play_constant(len_ms, a).is_ok(),
            None => false,
        }
    }

    pub(crate) fn update_live_gain(&mut self) {
        if self.amplitude <= 0.001 {
            self.stop_effect();
            return;
        }
        if let Some(m) = self.motor.as_mut() {
            let _ = m.set_hw_gain(self.amplitude);
        }
    }
}

pub struct VibratorService {
    pub inner: Arc<Mutex<Inner>>,
    pub gen: Arc<AtomicU64>,
}

impl VibratorService {
    pub fn new() -> Self {
        let motor = match Motor::open() {
            Ok(m) => Some(m),
            Err(e) => {
                aloge!("motor open failed: {:?}", e);
                None
            }
        };
        let f0_hz = motor.as_ref().map(|m| m.f0_hz()).unwrap_or(170.0);
        let effects = EffectLibrary::load();
        VibratorService {
            inner: Arc::new(Mutex::new(Inner {
                motor,
                effects,
                amplitude: 1.0,
                effect_amplitude: None,
                dynamic_scale: 1.0,
                touch_style: 0,
                f0_hz,
                ext_gen: 0,
                ext_active: false,
                ext_saved_amp: None,
                short_rtp_until: None,
            })),
            gen: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn next_gen(&self) -> u64 {
        self.gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn stop_and_advance(&self) {
        self.next_gen();
        if let Ok(mut inner) = self.inner.lock() {
            inner.ext_active = false;
            inner.stop_effect();
        }
    }

    /// Streams `pcm` asynchronously on a cancellable worker thread and fires `cb` on completion.
    pub fn spawn_pcm_play(
        inner: Arc<Mutex<Inner>>,
        gen_arc: Arc<AtomicU64>,
        gen: u64,
        pcm: Arc<Vec<i8>>,
        amp: f32,
        cb: Option<Strong<dyn IVibratorCallback>>,
    ) {
        thread::spawn(move || {
            let dur_ms = EffectLibrary::duration_ms(&pcm);
            if let Ok(mut guard) = inner.lock() {
                if gen_arc.load(Ordering::SeqCst) != gen {
                    return;
                }
                if let Some(m) = guard.motor.as_mut() {
                    let _ = m.play_pcm_cancellable(&pcm, amp, &gen_arc, gen);
                }
            }
            // Wait for hardware FIFO to finish playing
            let mut left = dur_ms.max(10) as u64;
            while left > 0 {
                if gen_arc.load(Ordering::SeqCst) != gen {
                    return;
                }
                let step = left.min(15);
                thread::sleep(Duration::from_millis(step));
                left -= step;
            }
            if let Some(c) = cb.as_ref() {
                let _ = c.onComplete();
            }
        });
    }

    fn schedule_finish(
        &self,
        gen: u64,
        delay_ms: u32,
        cb: Option<&Strong<dyn IVibratorCallback>>,
        wait_for_off: bool,
    ) {
        let cb = cb.cloned();
        let inner = Arc::clone(&self.inner);
        let gen_arc = Arc::clone(&self.gen);
        thread::spawn(move || {
            if wait_for_off {
                while gen_arc.load(Ordering::SeqCst) == gen {
                    thread::sleep(Duration::from_millis(25));
                }
            } else {
                let mut left = delay_ms as u64;
                while left > 0 {
                    if gen_arc.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    let step = left.min(20);
                    thread::sleep(Duration::from_millis(step));
                    left -= step;
                }
                if gen_arc.load(Ordering::SeqCst) != gen {
                    return;
                }
                if let Ok(mut guard) = inner.lock() {
                    guard.stop_effect();
                }
            }
            if let Some(c) = cb.as_ref() {
                let _ = c.onComplete();
            }
        });
    }

    fn set_external_control(&self, enabled: bool) {
        let gen = self.next_gen();
        let inner = Arc::clone(&self.inner);
        let gen_arc = Arc::clone(&self.gen);

        if !enabled {
            if let Ok(mut i) = inner.lock() {
                i.ext_active = false;
                i.ext_gen = gen;
                i.stop_effect();
                if let Some(saved) = i.ext_saved_amp.take() {
                    i.amplitude = saved;
                }
            }
            alogd!("setExternalControl(false)");
            return;
        }

        let amp = {
            let Ok(mut i) = inner.lock() else { return };
            if i.ext_active {
                return;
            }
            i.ext_active = true;
            i.ext_gen = gen;
            i.ext_saved_amp = Some(i.amplitude);
            (i.amplitude * DEFAULT_RING_AMP).clamp(0.1, 1.0)
        };

        alogd!("setExternalControl(true) amp={:.2}", amp);
        thread::spawn(move || {
            let mut idx = 0usize;
            while gen_arc.load(Ordering::SeqCst) == gen {
                let ms = DEFAULT_RING_PATTERN[idx % DEFAULT_RING_PATTERN.len()];
                idx += 1;
                let is_pulse = idx % 2 == 1;
                if is_pulse {
                    if let Ok(mut i) = inner.lock() {
                        if !i.ext_active || i.ext_gen != gen {
                            return;
                        }
                        i.play_constant(ms, amp);
                    }
                }
                let mut left = ms as u64;
                while left > 0 {
                    if gen_arc.load(Ordering::SeqCst) != gen {
                        return;
                    }
                    let step = left.min(20);
                    thread::sleep(Duration::from_millis(step));
                    left -= step;
                }
                if is_pulse {
                    if let Ok(mut i) = inner.lock() {
                        if i.ext_active && i.ext_gen == gen {
                            i.stop_effect();
                        }
                    }
                }
            }
        });
    }
}

impl IVibrator for VibratorService {
    fn r#getCapabilities(&self) -> BinderResult<i32> {
        Ok(CAP_ON_CALLBACK
            | CAP_PERFORM_CALLBACK
            | CAP_AMPLITUDE_CONTROL
            | CAP_EXTERNAL_CONTROL
            | CAP_COMPOSE_EFFECTS
            | CAP_GET_RESONANT_FREQUENCY
            | CAP_GET_Q_FACTOR)
    }

    fn r#off(&self) -> BinderResult<()> {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(until) = inner.short_rtp_until {
                if Instant::now() < until {
                    // Let short one-shot RTP effect finish draining aw86927 FIFO naturally
                    alogd!("IVibrator::off() deferred (short RTP still draining)");
                    return Ok(());
                }
            }
            inner.short_rtp_until = None;
        }
        alogd!("IVibrator::off()");
        self.stop_and_advance();
        Ok(())
    }

    fn r#on(
        &self,
        timeout_ms: i32,
        callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> BinderResult<()> {
        let gen = self.next_gen();
        // Short pulses (<= 120ms, e.g. fingerprint unlock / touch feedback) should be a crisp
        // resonant LRA "duk" thud via custom_wave rather than a buzzy FF_CONSTANT jzzzz!
        if timeout_ms > 0 && timeout_ms <= 120 {
            let (dur_ms, amp) = {
                let Ok(mut inner) = self.inner.lock() else {
                    return Ok(());
                };
                let style = inner.touch_style;
                let amp = (inner.amplitude * 0.68).clamp(0.32, 0.72);
                let f0 = inner.f0_hz;
                // effect_4 (21ms @ 166.3Hz, peak=127) is a solid, crisp resonant "duk" on peridot's 169.3Hz LRA
                let pcm = inner
                    .effects
                    .get_pcm(4, style)
                    .unwrap_or_else(|| Arc::new(synth::synthesize_he(&[], 1, 0, 1.0, 0, f0)));
                let dur = EffectLibrary::duration_ms(&pcm);
                inner.short_rtp_until =
                    Some(Instant::now() + Duration::from_millis((dur as u64) + 50));
                if let Some(m) = inner.motor.as_mut() {
                    let _ = m.play_pcm_cancellable(&pcm, amp, &self.gen, gen);
                }
                (dur, amp)
            };
            alogd!("IVibrator::on({}ms) -> sync crisp duk PCM amp={:.2} dur={}ms", timeout_ms, amp, dur_ms);
            if let Some(cb_cloned) = callback.cloned() {
                let gen_arc = Arc::clone(&self.gen);
                thread::spawn(move || {
                    let mut left = (dur_ms as u64) + 20;
                    while left > 0 {
                        if gen_arc.load(Ordering::SeqCst) != gen {
                            return;
                        }
                        let step = left.min(15);
                        thread::sleep(Duration::from_millis(step));
                        left -= step;
                    }
                    let _ = cb_cloned.onComplete();
                });
            }
            return Ok(());
        }

        let len = if timeout_ms > 0 {
            (timeout_ms as u32).min(MAX_DURATION_MS as u32) as u16
        } else {
            MAX_DURATION_MS
        };
        if let Ok(mut inner) = self.inner.lock() {
            inner.short_rtp_until = None;
            let amp = inner.amplitude;
            inner.play_constant(len, amp);
        }
        if timeout_ms <= 0 && callback.is_some() {
            self.schedule_finish(gen, 0, callback, true);
        } else if timeout_ms > 0 {
            self.schedule_finish(gen, len as u32, callback, false);
        }
        Ok(())
    }

    fn r#perform(
        &self,
        effect: Effect,
        strength: EffectStrength,
        callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> BinderResult<i32> {
        // Exact stock OPlus strength mapping (0x3FFF / 0x5FFF / 0x7FFF) used only when ColorOS
        // did not supply a per-effect magnitude via setAmplitude(1.0..=2.0).
        let strength_scale = match strength {
            EffectStrength::LIGHT => 0.50,
            EffectStrength::MEDIUM => 0.75,
            EffectStrength::STRONG => 1.0,
            _ => 0.75,
        };
        let oplus_id = map_android_effect_to_oplus_id(effect);
        let gen = self.next_gen();

        let (pcm, amp, dur_ms, played_sync) = {
            let Ok(mut inner) = self.inner.lock() else {
                return Ok(20);
            };
            let style = inner.touch_style;
            let raw_amp = match inner.effect_amplitude {
                Some(ea) => ea.clamp(0.0, 1.0),
                None => (inner.amplitude * strength_scale).clamp(0.0, 1.0),
            };
            // Per-effect acoustic tuning for peridot's 0809 LRA (f0=169.3Hz):
            // - 105 (volume/brightness slider tick): keep subtle & consistent across slider range
            //   instead of ramping loudly like a haptic intensity slider.
            // - 4 / 315 (fingerprint unlock / lock "duk"): tame full-resonance slam to refined 0.68.
            // - 2 (back gesture crisp click): clean 0.78 snap, distinct from fingerprint "duk".
            // - 111 (Gboard key tap): lift crisp 10.4ms click so typing feels punchy & tucky.
            // - 107 (scroll top/bottom edge bounce): lift low-RMS (23/127) wave at strength 800..1200.
            let amp = if raw_amp <= 0.001 {
                0.0
            } else {
                match oplus_id {
                    105 => 0.24 + 0.10 * raw_amp,
                    4 | 315 => (raw_amp * 0.68).clamp(0.32, 0.72),
                    2 => (raw_amp * 0.78).clamp(0.35, 0.82),
                    111 => (raw_amp * 1.45).clamp(0.60, 1.0),
                    107 => (raw_amp * 1.85).clamp(0.55, 1.0),
                    _ => raw_amp,
                }
            };
            let f0 = inner.f0_hz;
            let pcm = inner.effects.get_pcm(oplus_id, style).unwrap_or_else(|| {
                Arc::new(synth::synthesize_he(&[], 1, 0, 1.0, 0, f0))
            });
            let dur_ms = EffectLibrary::duration_ms(&pcm);
            if dur_ms <= 250 {
                // Play short/medium one-shot effects synchronously into the aw86927 FIFO (<1ms)
                // so there is zero thread-spawn race with IVibrator::off() on rapid gestures or FP unlock.
                inner.short_rtp_until =
                    Some(Instant::now() + Duration::from_millis((dur_ms as u64) + 45));
                if let Some(m) = inner.motor.as_mut() {
                    let _ = m.play_pcm_cancellable(&pcm, amp, &self.gen, gen);
                }
                (pcm, amp, dur_ms, true)
            } else {
                inner.short_rtp_until = None;
                (pcm, amp, dur_ms, false)
            }
        };

        alogd!(
            "IVibrator::perform({:?}, {:?}) -> oplus_id={} amp={:.2} dur={}ms sync={}",
            effect,
            strength,
            oplus_id,
            amp,
            dur_ms,
            played_sync
        );
        if played_sync {
            if let Some(cb_cloned) = callback.cloned() {
                let gen_arc = Arc::clone(&self.gen);
                thread::spawn(move || {
                    let mut left = (dur_ms as u64) + 20;
                    while left > 0 {
                        if gen_arc.load(Ordering::SeqCst) != gen {
                            return;
                        }
                        let step = left.min(15);
                        thread::sleep(Duration::from_millis(step));
                        left -= step;
                    }
                    let _ = cb_cloned.onComplete();
                });
            }
        } else {
            Self::spawn_pcm_play(
                Arc::clone(&self.inner),
                Arc::clone(&self.gen),
                gen,
                pcm,
                amp,
                callback.cloned(),
            );
        }
        // Add +45ms hardware startup margin so VibrationThread does not cancel aw86927 FIFO prematurely.
        Ok(dur_ms + 45)
    }

    fn r#getSupportedEffects(&self) -> BinderResult<Vec<Effect>> {
        let ids = self
            .inner
            .lock()
            .map(|i| i.effects.supported_ids())
            .unwrap_or_else(|_| (0..=420).collect());
        Ok(ids.into_iter().map(Effect).collect())
    }

    fn r#setAmplitude(&self, amplitude: f32) -> BinderResult<()> {
        // Exact reverse-engineered stock OPlus IVibrator::setAmplitude protocol (0xe904..0xe9ec):
        // - 10000.0 => Crisp touch style (0)
        // - 10001.0 => Soft/Gentle touch style (1)
        // - (1.0, 2.0] => per-effect linear magnitude (amplitude - 1.0) for next perform(), no live FF_GAIN write
        // - (0.0, 1.0] => standard Android amplitude + live FF_GAIN update
        if (amplitude - 10001.0).abs() < 0.5 {
            if let Ok(mut inner) = self.inner.lock() {
                inner.touch_style = 1;
            }
            alogd!("IVibrator::setAmplitude(10001) -> touch_style=1 (soft)");
            return Ok(());
        }
        if (amplitude - 10000.0).abs() < 0.5 {
            if let Ok(mut inner) = self.inner.lock() {
                inner.touch_style = 0;
            }
            alogd!("IVibrator::setAmplitude(10000) -> touch_style=0 (crisp)");
            return Ok(());
        }
        if amplitude > 1.0 && amplitude <= 2.0 {
            let eff_amp = (amplitude - 1.0).clamp(0.0, 1.0);
            if let Ok(mut inner) = self.inner.lock() {
                inner.effect_amplitude = Some(eff_amp);
            }
            alogd!("IVibrator::setAmplitude({:.3}) -> effect_amp={:.3}", amplitude, eff_amp);
            return Ok(());
        }
        let amp = amplitude.clamp(0.0, 1.0);
        if let Ok(mut inner) = self.inner.lock() {
            inner.amplitude = amp;
            inner.effect_amplitude = None;
            inner.update_live_gain();
        }
        alogd!("IVibrator::setAmplitude({:.3}) -> const_amp={:.3}", amplitude, amp);
        Ok(())
    }

    fn r#setExternalControl(&self, enabled: bool) -> BinderResult<()> {
        self.set_external_control(enabled);
        Ok(())
    }

    fn r#getCompositionDelayMax(&self) -> BinderResult<i32> {
        Ok(5000)
    }

    fn r#getCompositionSizeMax(&self) -> BinderResult<i32> {
        Ok(128)
    }

    fn r#getSupportedPrimitives(&self) -> BinderResult<Vec<CompositePrimitive>> {
        Ok(vec![
            CompositePrimitive::NOOP,
            CompositePrimitive::CLICK,
            CompositePrimitive::THUD,
            CompositePrimitive::SPIN,
            CompositePrimitive::QUICK_RISE,
            CompositePrimitive::SLOW_RISE,
            CompositePrimitive::QUICK_FALL,
            CompositePrimitive::LIGHT_TICK,
            CompositePrimitive::LOW_TICK,
        ])
    }

    fn r#getPrimitiveDuration(&self, primitive: CompositePrimitive) -> BinderResult<i32> {
        Ok(primitive_duration(primitive) as i32)
    }

    fn r#compose(
        &self,
        composite: &[CompositeEffect],
        callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> BinderResult<()> {
        if composite.is_empty() {
            if let Some(c) = callback {
                let _ = c.onComplete();
            }
            return Ok(());
        }
        let gen = self.next_gen();
        let (amp, f0_hz) = {
            let Ok(inner) = self.inner.lock() else {
                return Ok(());
            };
            (inner.amplitude, inner.f0_hz)
        };
        let pcm = Arc::new(synth::synthesize_compose(composite, 1.0, f0_hz));
        if pcm.is_empty() {
            if let Some(c) = callback {
                let _ = c.onComplete();
            }
            return Ok(());
        }
        alogd!(
            "IVibrator::compose(steps={}, dur={}ms, amp={:.2})",
            composite.len(),
            EffectLibrary::duration_ms(&pcm),
            amp
        );
        Self::spawn_pcm_play(
            Arc::clone(&self.inner),
            Arc::clone(&self.gen),
            gen,
            pcm,
            amp,
            callback.cloned(),
        );
        Ok(())
    }

    fn r#getSupportedAlwaysOnEffects(&self) -> BinderResult<Vec<Effect>> {
        Ok(vec![])
    }

    fn r#alwaysOnEnable(
        &self,
        _id: i32,
        _effect: Effect,
        _strength: EffectStrength,
    ) -> BinderResult<()> {
        Ok(())
    }

    fn r#alwaysOnDisable(&self, _id: i32) -> BinderResult<()> {
        self.stop_and_advance();
        Ok(())
    }

    fn r#getResonantFrequency(&self) -> BinderResult<f32> {
        let f0 = self.inner.lock().map(|i| i.f0_hz).unwrap_or(170.0);
        Ok(f0)
    }

    fn r#getQFactor(&self) -> BinderResult<f32> {
        Ok(15.0)
    }

    fn r#getFrequencyResolution(&self) -> BinderResult<f32> {
        Ok(1.0)
    }

    fn r#getFrequencyMinimum(&self) -> BinderResult<f32> {
        Ok(100.0)
    }

    fn r#getBandwidthAmplitudeMap(&self) -> BinderResult<Vec<f32>> {
        Ok(vec![])
    }

    fn r#getPwlePrimitiveDurationMax(&self) -> BinderResult<i32> {
        Ok(-1)
    }

    fn r#getPwleCompositionSizeMax(&self) -> BinderResult<i32> {
        Ok(-1)
    }

    fn r#getSupportedBraking(&self) -> BinderResult<Vec<Braking>> {
        Ok(vec![Braking::NONE])
    }

    fn r#composePwle(
        &self,
        _composite: &[PrimitivePwle],
        _callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> BinderResult<()> {
        Ok(())
    }

    fn r#performVendorEffect(
        &self,
        _vendor_effect: &VendorEffect,
        _callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> BinderResult<()> {
        Err(rsbinder::StatusCode::ServiceSpecific(4).into())
    }

    fn r#getFrequencyToOutputAccelerationMap(&self) -> BinderResult<Vec<FontMatrixEntry>> {
        Ok(vec![])
    }

    fn r#getPwleV2PrimitiveDurationMaxMillis(&self) -> BinderResult<i32> {
        Ok(-1)
    }

    fn r#getPwleV2CompositionSizeMax(&self) -> BinderResult<i32> {
        Ok(-1)
    }

    fn r#getPwleV2PrimitiveDurationMinMillis(&self) -> BinderResult<i32> {
        Ok(-1)
    }

    fn r#composePwleV2(
        &self,
        _composite: &CompositePwleV2,
        _callback: Option<&Strong<dyn IVibratorCallback>>,
    ) -> BinderResult<()> {
        Ok(())
    }
}

impl rsbinder::Interface for VibratorService {}

fn map_android_effect_to_oplus_id(effect: Effect) -> i32 {
    match effect {
        Effect::TEXTURE_TICK => 6,
        _ => effect.0,
    }
}

fn primitive_duration(primitive: CompositePrimitive) -> u16 {
    match primitive {
        CompositePrimitive::NOOP => 0,
        CompositePrimitive::CLICK => 12,
        CompositePrimitive::LIGHT_TICK => 8,
        CompositePrimitive::LOW_TICK => 14,
        CompositePrimitive::THUD => 28,
        CompositePrimitive::QUICK_RISE => 45,
        CompositePrimitive::SLOW_RISE => 110,
        CompositePrimitive::QUICK_FALL => 55,
        CompositePrimitive::SPIN => 120,
        _ => 15,
    }
}
