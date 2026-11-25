use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rsbinder::{BinderResult, ParcelFileDescriptor, Strong};

extern "C" {
    #[link_name = "dup"]
    fn libc_dup(fd: std::ffi::c_int) -> std::ffi::c_int;
}

use crate::effects::EffectLibrary;
use crate::gen_oplus::vendor::aac::hardware::richtap::vibrator::IRichtapCallback::IRichtapCallback;
use crate::gen_oplus::vendor::oplus::hardware::oplusvibrator::IOplusVibrator::IOplusVibrator;
use crate::service::Inner;
use crate::synth;
use crate::{alogd, alogi};

pub struct OplusVibrator {
    inner: Arc<Mutex<Inner>>,
    gen: Arc<AtomicU64>,
}

impl OplusVibrator {
    pub fn new(inner: Arc<Mutex<Inner>>, gen: Arc<AtomicU64>) -> Self {
        OplusVibrator { inner, gen }
    }

    fn next_gen(&self) -> u64 {
        self.gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Maps ColorOS strength (0..=2400 mV) linearly to 0.0..=1.0, matching stock OPlus HAL.
    fn vmax_to_amplitude(strength: i32) -> f32 {
        if strength <= 0 {
            0.0
        } else if strength <= 255 {
            (strength as f32 / 255.0).clamp(0.0, 1.0)
        } else {
            (strength as f32 / 2400.0).clamp(0.0, 1.0)
        }
    }

    fn byte_strength_scale(strength: i8) -> f32 {
        match strength {
            0 => 0.55,
            1 => 0.80,
            2 => 1.0,
            s => ((s as u8) as f32 / 255.0).clamp(0.15, 1.0),
        }
    }

    fn spawn_pcm_with_richtap_cb(
        inner: Arc<Mutex<Inner>>,
        gen_arc: Arc<AtomicU64>,
        gen: u64,
        pcm: Arc<Vec<i8>>,
        amp: f32,
        cb: Option<Strong<dyn IRichtapCallback>>,
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
                let _ = c.onCallback(0);
            }
        });
    }
}

impl IOplusVibrator for OplusVibrator {
    fn r#setVmax(&self, strength: i32) -> BinderResult<()> {
        let amp = Self::vmax_to_amplitude(strength);
        if let Ok(mut inner) = self.inner.lock() {
            inner.amplitude = amp;
            inner.update_live_gain();
        }
        alogi!("setVmax({}) -> amp={:.3}", strength, amp);
        Ok(())
    }

    fn r#linearMotorVibratorOn(&self, duration: i32, strength: i32, _extra: bool) -> BinderResult<()> {
        let amp = Self::vmax_to_amplitude(strength);
        let gen = self.next_gen();
        if duration > 0 && duration <= 120 {
            if let Ok(mut inner) = self.inner.lock() {
                inner.amplitude = amp;
                let style = inner.touch_style;
                let f0 = inner.f0_hz;
                let pcm = inner
                    .effects
                    .get_pcm(4, style)
                    .unwrap_or_else(|| Arc::new(synth::synthesize_he(&[], 1, 0, 1.0, 0, f0)));
                let dur = EffectLibrary::duration_ms(&pcm);
                inner.short_rtp_until =
                    Some(std::time::Instant::now() + Duration::from_millis((dur as u64) + 50));
                if let Some(m) = inner.motor.as_mut() {
                    let _ = m.play_pcm_cancellable(&pcm, amp.max(0.38), &self.gen, gen);
                }
            }
            alogi!("linearMotorVibratorOn({}ms, strength={}) -> crisp duk amp={:.3}", duration, strength, amp);
            return Ok(());
        }
        let len = (duration.max(10) as u32).min(u16::MAX as u32) as u16;
        alogi!("linearMotorVibratorOn dur={} strength={} amp={:.3}", len, strength, amp);
        if let Ok(mut inner) = self.inner.lock() {
            inner.short_rtp_until = None;
            inner.amplitude = amp;
            inner.play_constant(len, amp);
        }
        let inner = Arc::clone(&self.inner);
        let gen_arc = Arc::clone(&self.gen);
        thread::spawn(move || {
            let mut left = len as u64;
            while left > 0 {
                if gen_arc.load(Ordering::SeqCst) != gen {
                    return;
                }
                let step = left.min(20);
                thread::sleep(Duration::from_millis(step));
                left -= step;
            }
            if gen_arc.load(Ordering::SeqCst) == gen {
                if let Ok(mut g) = inner.lock() {
                    g.stop_effect();
                }
            }
        });
        Ok(())
    }

    fn r#linearMotorVibratorOff(&self) -> BinderResult<()> {
        alogd!("linearMotorVibratorOff");
        self.next_gen();
        if let Ok(mut inner) = self.inner.lock() {
            inner.stop_effect();
        }
        Ok(())
    }

    fn r#init(&self, cb: Option<&Strong<dyn IRichtapCallback>>) -> BinderResult<()> {
        alogd!("oplus init");
        if let Some(cb) = cb {
            let _ = cb.onCallback(0);
        }
        Ok(())
    }

    fn r#setDynamicScale(&self, scale: i32, cb: Option<&Strong<dyn IRichtapCallback>>) -> BinderResult<()> {
        let norm = if scale <= 100 {
            (scale.clamp(0, 100) as f32) / 100.0
        } else {
            (scale.clamp(0, 255) as f32) / 255.0
        };
        if let Ok(mut inner) = self.inner.lock() {
            inner.dynamic_scale = norm;
        }
        alogd!("oplus setDynamicScale({}) -> {:.3}", scale, norm);
        if let Some(cb) = cb {
            let _ = cb.onCallback(0);
        }
        Ok(())
    }

    fn r#setF0(&self, f0: i32, cb: Option<&Strong<dyn IRichtapCallback>>) -> BinderResult<()> {
        if f0 > 50 {
            let hz = if f0 > 500 { f0 as f32 / 10.0 } else { f0 as f32 };
            if let Ok(mut inner) = self.inner.lock() {
                inner.f0_hz = hz.clamp(100.0, 300.0);
            }
        }
        if let Some(cb) = cb {
            let _ = cb.onCallback(0);
        }
        Ok(())
    }

    fn r#stop(&self, cb: Option<&Strong<dyn IRichtapCallback>>) -> BinderResult<()> {
        alogd!("oplus stop");
        self.next_gen();
        if let Ok(mut inner) = self.inner.lock() {
            inner.stop_effect();
        }
        if let Some(cb) = cb {
            let _ = cb.onCallback(0);
        }
        Ok(())
    }

    fn r#setAmplitude(&self, amplitude: i32, cb: Option<&Strong<dyn IRichtapCallback>>) -> BinderResult<()> {
        let amp = if amplitude <= 255 {
            (amplitude.clamp(0, 255) as f32) / 255.0
        } else {
            Self::vmax_to_amplitude(amplitude)
        };
        if let Ok(mut inner) = self.inner.lock() {
            inner.amplitude = amp;
            inner.update_live_gain();
        }
        alogd!("oplus setAmplitude({}) -> {:.3}", amplitude, amp);
        if let Some(cb) = cb {
            let _ = cb.onCallback(0);
        }
        Ok(())
    }

    fn r#performHeParam(&self, interval: i32, amplitude: i32, freq: i32, cb: Option<&Strong<dyn IRichtapCallback>>) -> BinderResult<()> {
        alogd!("oplus performHeParam(interval={}, amp={}, freq={})", interval, amplitude, freq);
        if amplitude >= 0 {
            let norm = if amplitude <= 100 {
                amplitude as f32 / 100.0
            } else {
                (amplitude.min(255) as f32) / 255.0
            };
            if let Ok(mut inner) = self.inner.lock() {
                inner.dynamic_scale = norm;
                inner.update_live_gain();
            }
        }
        if let Some(cb) = cb {
            let _ = cb.onCallback(0);
        }
        Ok(())
    }

    fn r#off(&self, cb: Option<&Strong<dyn IRichtapCallback>>) -> BinderResult<()> {
        alogd!("oplus off");
        self.next_gen();
        if let Ok(mut inner) = self.inner.lock() {
            inner.stop_effect();
        }
        if let Some(cb) = cb {
            let _ = cb.onCallback(0);
        }
        Ok(())
    }

    fn r#on(&self, timeout_or_amp: i32, cb: Option<&Strong<dyn IRichtapCallback>>) -> BinderResult<()> {
        let gen = self.next_gen();
        if timeout_or_amp > 0 && timeout_or_amp <= 120 {
            let dur = if let Ok(mut inner) = self.inner.lock() {
                let style = inner.touch_style;
                let amp = inner.amplitude.clamp(0.38, 1.0);
                let f0 = inner.f0_hz;
                let pcm = inner
                    .effects
                    .get_pcm(4, style)
                    .unwrap_or_else(|| Arc::new(synth::synthesize_he(&[], 1, 0, 1.0, 0, f0)));
                let dur = EffectLibrary::duration_ms(&pcm);
                inner.short_rtp_until =
                    Some(std::time::Instant::now() + Duration::from_millis((dur as u64) + 50));
                if let Some(m) = inner.motor.as_mut() {
                    let _ = m.play_pcm_cancellable(&pcm, amp, &self.gen, gen);
                }
                dur
            } else {
                21
            };
            if let Some(c) = cb.cloned() {
                let gen_arc = Arc::clone(&self.gen);
                thread::spawn(move || {
                    let mut left = (dur as u64) + 20;
                    while left > 0 {
                        if gen_arc.load(Ordering::SeqCst) != gen {
                            return;
                        }
                        let step = left.min(15);
                        thread::sleep(Duration::from_millis(step));
                        left -= step;
                    }
                    let _ = c.onCallback(0);
                });
            }
            return Ok(());
        }
        let len = (timeout_or_amp.clamp(10, 5000)) as u16;
        if let Ok(mut inner) = self.inner.lock() {
            inner.short_rtp_until = None;
            let amp = inner.amplitude;
            inner.play_constant(len, amp);
        }
        let inner = Arc::clone(&self.inner);
        let gen_arc = Arc::clone(&self.gen);
        let cb_cloned = cb.cloned();
        thread::spawn(move || {
            let mut left = len as u64;
            while left > 0 {
                if gen_arc.load(Ordering::SeqCst) != gen {
                    return;
                }
                let step = left.min(20);
                thread::sleep(Duration::from_millis(step));
                left -= step;
            }
            if gen_arc.load(Ordering::SeqCst) == gen {
                if let Ok(mut g) = inner.lock() {
                    g.stop_effect();
                }
                if let Some(c) = cb_cloned.as_ref() {
                    let _ = c.onCallback(0);
                }
            }
        });
        Ok(())
    }

    fn r#perform(&self, effect_id: i32, strength: i8, cb: Option<&Strong<dyn IRichtapCallback>>) -> BinderResult<i32> {
        let gen = self.next_gen();
        let s_scale = Self::byte_strength_scale(strength);
        let (pcm_opt, amp, f0_hz) = {
            let Ok(mut inner) = self.inner.lock() else {
                return Ok(0);
            };
            let style = inner.touch_style;
            let amp = (inner.amplitude * inner.dynamic_scale * s_scale).clamp(0.0, 1.0);
            let f0 = inner.f0_hz;
            let pcm = inner.effects.get_pcm(effect_id, style);
            (pcm, amp, f0)
        };

        let pcm = pcm_opt.unwrap_or_else(|| {
            Arc::new(synth::synthesize_he(&[], 1, 0, 1.0, 0, f0_hz))
        });
        let dur_ms = EffectLibrary::duration_ms(&pcm);
        alogd!(
            "oplus perform(id={}, strength={}) -> amp={:.3} dur={}ms",
            effect_id,
            strength,
            amp,
            dur_ms
        );
        Self::spawn_pcm_with_richtap_cb(
            Arc::clone(&self.inner),
            Arc::clone(&self.gen),
            gen,
            pcm,
            amp,
            cb.cloned(),
        );
        Ok(dur_ms)
    }

    fn r#performEnvelope(
        &self,
        env_info: &[i32],
        fast_flag: bool,
        cb: Option<&Strong<dyn IRichtapCallback>>,
    ) -> BinderResult<()> {
        let gen = self.next_gen();
        let (user_amp, dyn_scale, f0_hz) = {
            let Ok(inner) = self.inner.lock() else {
                return Ok(());
            };
            (inner.amplitude, inner.dynamic_scale, inner.f0_hz)
        };
        let total_amp = (user_amp * dyn_scale).clamp(0.0, 1.0);
        alogd!(
            "oplus performEnvelope(len={}, fast={}) amp={:.3}",
            env_info.len(),
            fast_flag,
            total_amp
        );
        let pcm = Arc::new(synth::synthesize_he(env_info, 1, 0, 1.0, 0, f0_hz));
        Self::spawn_pcm_with_richtap_cb(
            Arc::clone(&self.inner),
            Arc::clone(&self.gen),
            gen,
            pcm,
            total_amp,
            cb.cloned(),
        );
        Ok(())
    }

    fn r#performRtp(
        &self,
        fd: &ParcelFileDescriptor,
        cb: Option<&Strong<dyn IRichtapCallback>>,
    ) -> BinderResult<()> {
        let gen = self.next_gen();
        let raw_fd = fd.as_raw_fd();
        let mut bytes = Vec::new();
        if raw_fd >= 0 {
            let dup_fd = unsafe { libc_dup(raw_fd) };
            if dup_fd >= 0 {
                let mut f = unsafe { std::fs::File::from_raw_fd(dup_fd) };
                let _ = f.read_to_end(&mut bytes);
            }
        }
        let (user_amp, dyn_scale) = {
            let Ok(inner) = self.inner.lock() else {
                return Ok(());
            };
            (inner.amplitude, inner.dynamic_scale)
        };
        let total_amp = (user_amp * dyn_scale).clamp(0.0, 1.0);
        alogd!(
            "oplus performRtp(bytes={}, amp={:.3})",
            bytes.len(),
            total_amp
        );
        if !bytes.is_empty() {
            let pcm: Vec<i8> = bytes.into_iter().map(|b| b as i8).collect();
            Self::spawn_pcm_with_richtap_cb(
                Arc::clone(&self.inner),
                Arc::clone(&self.gen),
                gen,
                Arc::new(pcm),
                total_amp,
                cb.cloned(),
            );
        } else if let Some(c) = cb {
            let _ = c.onCallback(0);
        }
        Ok(())
    }

    fn r#performHe(
        &self,
        looper: i32,
        interval: i32,
        amplitude: i32,
        freq: i32,
        pattern: &[i32],
        cb: Option<&Strong<dyn IRichtapCallback>>,
    ) -> BinderResult<()> {
        let gen = self.next_gen();
        let (user_amp, dyn_scale, f0_hz) = {
            let Ok(inner) = self.inner.lock() else {
                return Ok(());
            };
            (inner.amplitude, inner.dynamic_scale, inner.f0_hz)
        };
        let he_amp = if amplitude > 0 {
            if amplitude <= 100 {
                amplitude as f32 / 100.0
            } else {
                (amplitude.min(255) as f32) / 255.0
            }
        } else {
            1.0
        };
        let total_amp = (user_amp * dyn_scale * he_amp).clamp(0.0, 1.0);
        let pcm = Arc::new(synth::synthesize_he(pattern, looper, interval, 1.0, freq, f0_hz));
        alogd!(
            "oplus performHe(looper={} interval={} amp={} freq={} pat_len={}) -> total_amp={:.3} dur={}ms",
            looper,
            interval,
            amplitude,
            freq,
            pattern.len(),
            total_amp,
            EffectLibrary::duration_ms(&pcm)
        );
        Self::spawn_pcm_with_richtap_cb(
            Arc::clone(&self.inner),
            Arc::clone(&self.gen),
            gen,
            pcm,
            total_amp,
            cb.cloned(),
        );
        Ok(())
    }

    fn r#setHapticParam(&self, _params: &[i32], _length: i32, cb: Option<&Strong<dyn IRichtapCallback>>) -> BinderResult<()> {
        if let Some(cb) = cb {
            let _ = cb.onCallback(0);
        }
        Ok(())
    }

    fn r#startCalibrate(&self) -> BinderResult<()> {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(m) = inner.motor.as_mut() {
                m.calibrate();
                inner.f0_hz = m.f0_hz();
            }
        }
        Ok(())
    }

    fn r#getCalibrateResults(&self) -> BinderResult<Vec<i32>> {
        if let Ok(inner) = self.inner.lock() {
            if let Some(m) = inner.motor.as_ref() {
                return Ok(vec![m.f0_raw(), m.lra_resistance()]);
            }
        }
        Ok(vec![1700, 0])
    }

    fn r#getStatus(&self) -> BinderResult<i32> {
        Ok(1)
    }

    fn r#setMotorOldTest(&self, _test: i32) -> BinderResult<()> {
        Ok(())
    }

    fn r#getSupportFeatures(&self) -> BinderResult<Vec<u8>> {
        Ok(vec![0x07])
    }

    fn r#writeGunType(&self, _type: i32) -> BinderResult<()> {
        Ok(())
    }

    fn r#writeGunMode(&self, _mode: i32) -> BinderResult<()> {
        Ok(())
    }

    fn r#writeBulletNum(&self, _num: i32) -> BinderResult<()> {
        Ok(())
    }

    fn r#writeHapticAudio(&self, _data: &str) -> BinderResult<()> {
        Ok(())
    }

    fn r#setVibratorTouchStyle(&self, style: i32) -> BinderResult<()> {
        if let Ok(mut inner) = self.inner.lock() {
            inner.touch_style = style;
        }
        alogi!("setVibratorTouchStyle({})", style);
        Ok(())
    }

    fn r#getVibratorTouchStyle(&self) -> BinderResult<i32> {
        let style = self.inner.lock().map(|i| i.touch_style).unwrap_or(0);
        Ok(style)
    }

    fn r#setPowerOnVibratorSwitch(&self, _switch: &str) -> BinderResult<()> {
        Ok(())
    }
}

impl rsbinder::Interface for OplusVibrator {}
