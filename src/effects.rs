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

        // Primary: ROM /odm/etc/vibrator; secondary: /data/local/tmp/vibrator for live testing
        for base in ["/odm/etc/vibrator", "/data/local/tmp/vibrator"] {
            let json_path = Path::new(base).join("vibrator_effect.json");
            if let Ok(text) = fs::read_to_string(&json_path) {
                lib.parse_oplus_json(&text, base);
                if !lib.def_effects.is_empty() {
                    crate::alogi!(
                        "loaded OPlus vibrator_effect.json from {}: def={} soft={}",
                        base,
                        lib.def_effects.len(),
                        lib.soft_effects.len()
                    );
                    break;
                }
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

    fn parse_oplus_json(&mut self, text: &str, base_dir: &str) {
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
                        if in_soft {
                            self.soft_effects.insert(id, entry);
                        } else {
                            self.def_effects.insert(id, entry);
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
        if touch_style == 1 {
            if let Some(entry) = self.soft_effects.get_mut(&effect_id) {
                if let Some(pcm) = load_or_cache(entry) {
                    return Some(pcm);
                }
            }
        }
        if let Some(entry) = self.def_effects.get_mut(&effect_id) {
            if let Some(pcm) = load_or_cache(entry) {
                return Some(pcm);
            }
        }
        if let Some(entry) = self.xiaomi_fallback.get_mut(&effect_id) {
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
