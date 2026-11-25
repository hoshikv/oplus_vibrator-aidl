use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
    /// Normalized user/slider intensity 0.0..=1.0
    pub(crate) amplitude: f32,
    /// Dynamic scale for RichTap HE (0.0..=1.0)
    pub(crate) dynamic_scale: f32,
    /// 0 = Crisp (def_style), 1 = Gentle (soft_style)
    pub(crate) touch_style: i32,
    pub(crate) f0_hz: f32,
    pub(crate) ext_gen: u64,
    pub(crate) ext_active: bool,
    pub(crate) ext_saved_amp: Option<f32>,
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
                dynamic_scale: 1.0,
                touch_style: 0,
                f0_hz,
                ext_gen: 0,
                ext_active: false,
                ext_saved_amp: None,
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
        let len = if timeout_ms > 0 {
            (timeout_ms as u32).min(MAX_DURATION_MS as u32) as u16
        } else {
            MAX_DURATION_MS
        };
        if let Ok(mut inner) = self.inner.lock() {
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
        let strength_scale = match strength {
            EffectStrength::LIGHT => 0.55,
            EffectStrength::MEDIUM => 0.80,
            EffectStrength::STRONG => 1.0,
            _ => 0.80,
        };
        let oplus_id = map_android_effect_to_oplus_id(effect);
        let gen = self.next_gen();

        let (pcm_opt, amp, f0_hz) = {
            let Ok(mut inner) = self.inner.lock() else {
                return Ok(20);
            };
            let style = inner.touch_style;
            let amp = (inner.amplitude * strength_scale).clamp(0.0, 1.0);
            let f0 = inner.f0_hz;
            let pcm = inner.effects.get_pcm(oplus_id, style);
            (pcm, amp, f0)
        };

        let pcm = pcm_opt.unwrap_or_else(|| {
            // Synthesize fallback click if .bin file not present
            let fallback = synth::synthesize_he(&[], 1, 0, 1.0, 0, f0_hz);
            Arc::new(fallback)
        });
        let dur_ms = EffectLibrary::duration_ms(&pcm);
        alogd!(
            "IVibrator::perform({:?}, {:?}) -> oplus_id={} amp={:.2} dur={}ms",
            effect,
            strength,
            oplus_id,
            amp,
            dur_ms
        );
        Self::spawn_pcm_play(
            Arc::clone(&self.inner),
            Arc::clone(&self.gen),
            gen,
            pcm,
            amp,
            callback.cloned(),
        );
        Ok(dur_ms)
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
        // ColorOS encodes haptic slider strength (700..2400 mV) as 1.0 + strength/2400.0 (1.292..2.000).
        // Standard Android apps pass 0.0..=1.0 directly.
        let amp = if amplitude > 1.0 {
            let strength = (amplitude - 1.0) * 2400.0;
            let t = ((strength - 700.0) / (2400.0 - 700.0)).clamp(0.0, 1.0);
            0.15 + 0.85 * t.powf(1.5)
        } else {
            amplitude.clamp(0.0, 1.0)
        };
        if let Ok(mut inner) = self.inner.lock() {
            inner.amplitude = amp;
            inner.update_live_gain();
        }
        alogd!("IVibrator::setAmplitude({:.3}) -> {:.3}", amplitude, amp);
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
