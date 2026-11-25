use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::motor::SAMPLE_RATE;

#[derive(Clone)]
struct EffectEntry {
    path: PathBuf,
    rate_hz: u32,
    cached: Option<Arc<Vec<i8>>>,
}

pub struct EffectLibrary {
    def_effects: HashMap<i32, EffectEntry>,
    soft_effects: HashMap<i32, EffectEntry>,
    xiaomi_fallback: HashMap<i32, EffectEntry>,
}

impl EffectLibrary {
    pub fn load() -> Self {
        let mut lib = Self {
            def_effects: HashMap::new(),
            soft_effects: HashMap::new(),
            xiaomi_fallback: HashMap::new(),
        };

        // Primary: Official OPlus 0809 LRA waveforms (extracted from liboplusvibratoreffect.so),
        // then fill remaining extended IDs (111, 302..370) from 9999 vibrator_effect.json.
        // Exception: keep crisp 9999 effect_2 (8.7ms @ 230Hz) for back gesture so it feels distinct
        // and snappier than 0809 effect_4 (16.5ms @ 182Hz "duk" for fingerprint).
        for base in ["/odm/etc/vibrator", "/data/local/tmp/vibrator"] {
            let json_0809 = Path::new(base).join("vibrator_effect_0809.json");
            if let Ok(text) = fs::read_to_string(&json_0809) {
                lib.parse_oplus_json(&text, base, true);
            }
            let json_path = Path::new(base).join("vibrator_effect.json");
            if let Ok(text) = fs::read_to_string(&json_path) {
                lib.parse_oplus_json(&text, base, false);
            }
            // Use official OPlus 0809/def/effect_2.bin (16ms @ 156Hz "duk") for Quick Launch & Back gesture
            if !lib.def_effects.is_empty() {
                crate::alogi!(
                    "loaded OPlus waveforms from {} (0809 primary + 9999 extended): def={} soft={}",
                    base,
                    lib.def_effects.len(),
                    lib.soft_effects.len()
                );
                break;
            }
        }

        // Fallback: Xiaomi /odm/firmware/*_RTP.bin
        lib.scan_xiaomi_firmware("/odm/firmware");
        crate::alogi!(
            "loaded Xiaomi RTP fallback effects: {}",
            lib.xiaomi_fallback.len()
        );
        lib
    }

    fn parse_oplus_json(&mut self, text: &str, base_dir: &str, overwrite: bool) {
        let mut in_soft = false;
        let mut cur_file: Option<String> = None;
        let mut cur_id: Option<i32> = None;
        let mut cur_rate: u32 = SAMPLE_RATE;

        for line in text.lines() {
            let t = line.trim();
            if t.contains("\"def_style\"") {
                in_soft = false;
            } else if t.contains("\"soft_style\"") {
                in_soft = true;
            } else if let Some(val) = extract_json_str(t, "\"effect_file\"") {
                // Rewrite /odm/etc/vibrator prefix if loaded from an alternate base dir
                let mapped = if base_dir != "/odm/etc/vibrator" {
                    if let Some(rel) = val.strip_prefix("/odm/etc/vibrator/") {
                        format!("{base_dir}/{rel}")
                    } else {
                        val
                    }
                } else {
                    val
                };
                cur_file = Some(mapped);
            } else if let Some(val) = extract_json_int(t, "\"effect_id\"") {
                cur_id = Some(val);
            } else if let Some(val) = extract_json_int(t, "\"play_rate_hz\"") {
                if val > 0 {
                    cur_rate = val as u32;
                }
            } else if t.starts_with('}') {
                if let (Some(id), Some(file)) = (cur_id.take(), cur_file.take()) {
                    let path = PathBuf::from(file);
                    if path.exists() {
                        let entry = EffectEntry {
                            path,
                            rate_hz: cur_rate,
                            cached: None,
                        };
                        let map = if in_soft {
                            &mut self.soft_effects
                        } else {
                            &mut self.def_effects
                        };
                        if overwrite || !map.contains_key(&id) {
                            map.insert(id, entry);
                        }
                    }
                }
                cur_rate = SAMPLE_RATE;
            }
        }
    }

    fn scan_xiaomi_firmware(&mut self, dir: &str) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.to_ascii_lowercase().ends_with("rtp.bin") {
                continue;
            }
            let Some(first) = name.split('_').next() else {
                continue;
            };
            let Ok(id) = first.parse::<i32>() else {
                continue;
            };
            let path = entry.path();
            if fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false) {
                self.xiaomi_fallback.insert(
                    id,
                    EffectEntry {
                        path,
                        rate_hz: SAMPLE_RATE,
                        cached: None,
                    },
                );
            }
        }
    }

    /// Returns cached 24 kHz signed 8-bit PCM for `effect_id` and `touch_style` (0=crisp/def, 1=gentle/soft).
    pub fn get_pcm(&mut self, effect_id: i32, touch_style: i32) -> Option<Arc<Vec<i8>>> {
        // Effect 111 (Gboard key tap): use official OPlus 0809 crisp click (effect_1: 10.42ms @ 192Hz)
        // with a +25% RMS punch boost (soft-clamped to ±127) so typing feels crisp and clearly felt.
        if effect_id == 111 {
            let base = self.get_pcm(1, touch_style)?;
            let boosted: Vec<i8> = base
                .iter()
                .map(|&s| ((s as f32) * 1.25).round().clamp(-128.0, 127.0) as i8)
                .collect();
            return Some(Arc::new(boosted));
        }
        // Effect 7 (scroll top/bottom edge bounce): 0809 effect_7 has very tiny peak (16/127, rms=8.8).
        // Boost PCM 4.5x (peak 16 -> 72, rms 8.8 -> ~40) so scroll-edge bounce feels crisp and clear.
        if effect_id == 7 {
            let base = self.get_pcm_raw(7, touch_style)?;
            let boosted: Vec<i8> = base
                .iter()
                .map(|&s| ((s as f32) * 4.5).round().clamp(-128.0, 127.0) as i8)
                .collect();
            return Some(Arc::new(boosted));
        }
        // Effect 107 (slider top & bottom boundary stop): 0809 effect_107 has low peak (86/127)
        // and low RMS (23/127); boost PCM 2.0x so hitting top/bottom end feels clear and balanced.
        if effect_id == 107 {
            let base = self.get_pcm_raw(107, touch_style)?;
            let boosted: Vec<i8> = base
                .iter()
                .map(|&s| ((s as f32) * 2.0).round().clamp(-128.0, 127.0) as i8)
                .collect();
            return Some(Arc::new(boosted));
        }
        // Effect 315 (SystemUI lock/FP): map to official OPlus 0809 resonant "duk" (effect_4: 16.46ms @ 182.3Hz).
        let resolved_id = match effect_id {
            315 => 4,
            id => id,
        };
        self.get_pcm_raw(resolved_id, touch_style)
    }

    fn get_pcm_raw(&mut self, resolved_id: i32, touch_style: i32) -> Option<Arc<Vec<i8>>> {

        if touch_style == 1 {
            if let Some(entry) = self.soft_effects.get_mut(&resolved_id) {
                if let Some(pcm) = load_or_cache(entry) {
                    return Some(pcm);
                }
            }
        }
        if let Some(entry) = self.def_effects.get_mut(&resolved_id) {
            if let Some(pcm) = load_or_cache(entry) {
                return Some(pcm);
            }
        }
        if let Some(entry) = self.xiaomi_fallback.get_mut(&resolved_id) {
            if let Some(pcm) = load_or_cache(entry) {
                return Some(pcm);
            }
        }
        None
    }

    pub fn duration_ms(pcm: &[i8]) -> i32 {
        ((pcm.len() as u64 * 1000) / SAMPLE_RATE as u64).max(10) as i32
    }

    pub fn supported_ids(&self) -> Vec<i32> {
        let mut ids: Vec<i32> = (0..=21).collect();
        for &id in self.def_effects.keys() {
            ids.push(id);
        }
        for &id in self.soft_effects.keys() {
            ids.push(id);
        }
        for &id in self.xiaomi_fallback.keys() {
            ids.push(id);
        }
        // Also cover ColorOS extended O-Haptics waveform IDs (0..=420) so VibrationThread
        // never silently drops an OplusPrebakedSegment before reaching our HAL.
        for id in 0..=420 {
            ids.push(id);
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

fn load_or_cache(entry: &mut EffectEntry) -> Option<Arc<Vec<i8>>> {
    if let Some(ref c) = entry.cached {
        return Some(Arc::clone(c));
    }
    let raw = fs::read(&entry.path).ok()?;
    if raw.is_empty() {
        return None;
    }
    let pcm: Vec<i8> = if entry.rate_hz == 8000 {
        // Upsample 8 kHz -> 24 kHz (3x linear interpolation)
        let mut out = Vec::with_capacity(raw.len() * 3);
        for i in 0..raw.len() {
            let s0 = raw[i] as i8 as i32;
            let s1 = raw.get(i + 1).copied().unwrap_or(raw[i]) as i8 as i32;
            out.push(s0 as i8);
            out.push(((s0 * 2 + s1) / 3) as i8);
            out.push(((s0 + s1 * 2) / 3) as i8);
        }
        out
    } else if entry.rate_hz == 12000 {
        let mut out = Vec::with_capacity(raw.len() * 2);
        for i in 0..raw.len() {
            let s0 = raw[i] as i8 as i32;
            let s1 = raw.get(i + 1).copied().unwrap_or(raw[i]) as i8 as i32;
            out.push(s0 as i8);
            out.push(((s0 + s1) / 2) as i8);
        }
        out
    } else {
        raw.into_iter().map(|b| b as i8).collect()
    };

    // Preserve original OPlus relative waveform loudness (e.g. effect_105 volume/brightness tick at peak=32
    // must stay light and subtle, not boosted 4x into a heavy intensity thump).
    let arc = Arc::new(pcm);
    entry.cached = Some(Arc::clone(&arc));
    Some(arc)
}

fn extract_json_str(line: &str, key: &str) -> Option<String> {
    let idx = line.find(key)?;
    let rest = &line[idx + key.len()..];
    let colon = rest.find(':')?;
    let val = rest[colon + 1..].trim();
    let start = val.find('"')?;
    let after = &val[start + 1..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

fn extract_json_int(line: &str, key: &str) -> Option<i32> {
    let idx = line.find(key)?;
    let rest = &line[idx + key.len()..];
    let colon = rest.find(':')?;
    let val = rest[colon + 1..]
        .trim()
        .trim_end_matches(',')
        .trim();
    val.parse::<i32>().ok()
}
