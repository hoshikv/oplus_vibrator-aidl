use std::ffi::{c_char, c_int, c_long, c_ulong, c_void, CString};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const EVIOCGNAME_64: c_ulong = 0x8040_4506;
const EVIOCGBIT_FF: c_ulong = 0x8010_4535;
const EVIOCSFF: c_ulong = 0x4030_4580;
const EVIOCRMFF: c_ulong = 0x4004_4581;

const EV_FF: u16 = 0x15;
const FF_GAIN: u16 = 0x60;
const FF_PERIODIC: u16 = 0x51;
const FF_CONSTANT: u16 = 0x52;
const FF_CUSTOM: u16 = 0x5d;

const O_RDWR: c_int = 2;
const EINTR: i32 = 4;

extern "C" {
    fn open(path: *const c_char, oflag: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

pub const SAMPLE_RATE: u32 = 24_000;

pub struct Motor {
    sysfs_dir: PathBuf,
    evdev_fd: c_int,
    wave_fd: c_int,
    custom_wave_id: i16,
    period_size: usize,
    active_ff_id: i16,
    active_is_rtp: bool,
    rtp_done_at: Option<Instant>,
    hw_gain_is_max: bool,
    f0_hz: f32,
    f0_raw: i32,
    lra_resistance: i32,
}

impl Motor {
    pub fn open() -> io::Result<Motor> {
        let sysfs_dir = find_awinic_sysfs()?;
        let evdev_fd = find_awinic_evdev()?;

        let (wave_fd, custom_wave_id, period_size) = open_custom_wave(&sysfs_dir);
        let f0_raw = read_sysfs_i32(&sysfs_dir, "f0_value").unwrap_or(1700);
        let f0_hz = if f0_raw > 500 {
            f0_raw as f32 / 10.0
        } else {
            170.0
        };
        let lra_resistance = read_sysfs_i32(&sysfs_dir, "lra_resistance").unwrap_or(0);

        let _ = fs::write(sysfs_dir.join("auto_boost"), "1");
        let _ = fs::write(sysfs_dir.join("ram_vbat_comp"), "0");

        let mut m = Motor {
            sysfs_dir,
            evdev_fd,
            wave_fd,
            custom_wave_id,
            period_size: period_size.max(512),
            active_ff_id: -1,
            active_is_rtp: false,
            rtp_done_at: None,
            hw_gain_is_max: false,
            f0_hz,
            f0_raw,
            lra_resistance,
        };
        let _ = m.set_hw_gain(1.0);
        crate::alogi!(
            "motor opened: evdev_fd={} wave_fd={} custom_wave_id={} period={} f0={:.1}Hz",
            m.evdev_fd,
            m.wave_fd,
            m.custom_wave_id,
            m.period_size,
            m.f0_hz
        );
        Ok(m)
    }

    pub fn f0_hz(&self) -> f32 {
        self.f0_hz
    }

    pub fn f0_raw(&self) -> i32 {
        self.f0_raw
    }

    pub fn lra_resistance(&self) -> i32 {
        self.lra_resistance
    }

    pub fn calibrate(&mut self) {
        let _ = fs::write(self.sysfs_dir.join("cali"), "1");
        if let Some(v) = read_sysfs_i32(&self.sysfs_dir, "f0_value") {
            if v > 500 {
                self.f0_raw = v;
                self.f0_hz = v as f32 / 10.0;
            }
        }
        if let Some(r) = read_sysfs_i32(&self.sysfs_dir, "lra_resistance") {
            self.lra_resistance = r;
        }
    }

    /// Maps normalized amplitude 0.0..=1.0 using exact stock OPlus FF_GAIN formula (0x1333..=0x7FFF).
    pub fn set_hw_gain(&mut self, amp: f32) -> io::Result<()> {
        let a = amp.clamp(0.0, 1.0);
        let gain = (0x1333u32 + (a * 0x6cccu32 as f32) as u32).min(0x7fff) as i32;
        if gain == 0x7fff && self.hw_gain_is_max {
            return Ok(());
        }
        let res = self.write_ev_ff(FF_GAIN, gain);
        if res.is_ok() {
            self.hw_gain_is_max = gain == 0x7fff;
        }
        res
    }

    fn write_ev_ff(&self, code: u16, value: i32) -> io::Result<()> {
        // struct input_event on 64-bit Linux: 16B timeval + 2B type + 2B code + 4B value = 24 bytes
        let mut ev = [0u8; 24];
        ev[16..18].copy_from_slice(&EV_FF.to_le_bytes());
        ev[18..20].copy_from_slice(&code.to_le_bytes());
        ev[20..24].copy_from_slice(&value.to_le_bytes());
        write_all_retry(self.evdev_fd, &ev)
    }

    fn erase_ff(&mut self) {
        if self.active_ff_id >= 0 {
            // Skip redundant EV_FF stop event if a previous short RTP already finished draining naturally;
            // avoids queueing a spurious aw86927_rtp_work_routine(state=0) + 15ms standby stall on fast typing.
            let already_drained = self.active_is_rtp
                && self
                    .rtp_done_at
                    .map(|t| Instant::now() >= t)
                    .unwrap_or(false);
            if !already_drained {
                let _ = self.write_ev_ff(self.active_ff_id as u16, 0);
            }
            loop {
                let rc = unsafe { ioctl(self.evdev_fd, EVIOCRMFF, self.active_ff_id as c_long) };
                if rc != -1 {
                    break;
                }
                let err = io::Error::last_os_error();
                if err.raw_os_error() != Some(EINTR) {
                    break;
                }
            }
            self.active_ff_id = -1;
            self.active_is_rtp = false;
            self.rtp_done_at = None;
        }
    }

    pub fn stop(&mut self) {
        self.erase_ff();
    }

    /// Plays continuous RAM-loop buzz using FF_CONSTANT for `duration_ms` at `amp` (0.0..=1.0).
    pub fn play_constant(&mut self, duration_ms: u16, amp: f32) -> io::Result<()> {
        self.erase_ff();
        let a = amp.clamp(0.0, 1.0);
        if a <= 0.001 || duration_ms == 0 {
            return Ok(());
        }
        let _ = self.set_hw_gain(a);

        let mut effect = [0u8; 48];
        effect[0..2].copy_from_slice(&FF_CONSTANT.to_le_bytes());
        effect[2..4].copy_from_slice(&(-1i16).to_le_bytes());
        // replay.length at offset 10
        effect[10..12].copy_from_slice(&duration_ms.to_le_bytes());
        // u.constant.level at offset 16
        let level = (0x3fffu32 + (a * 0x4000 as f32) as u32).min(0x7fff) as i16;
        effect[16..18].copy_from_slice(&level.to_le_bytes());

        self.upload_and_start(&mut effect, false)
    }

    /// Plays a preloaded aw86927 hardware SRAM waveform (`ram_effect_id` 0..5, e.g. 0=click, 2=tick, 4=pop)
    /// via AW86927_ACTIVATE_RAM_MODE (~2ms latency, zero RTP startup stall, zero drops on rapid typing).
    pub fn play_ram(&mut self, ram_effect_id: i16, amp: f32) -> io::Result<()> {
        self.erase_ff();
        let a = amp.clamp(0.0, 1.0);
        if a <= 0.001 {
            return Ok(());
        }
        let mut custom_data = [0i16; 3];
        custom_data[0] = ram_effect_id;

        let mut effect = [0u8; 48];
        effect[0..2].copy_from_slice(&FF_PERIODIC.to_le_bytes());
        effect[2..4].copy_from_slice(&(-1i16).to_le_bytes());
        effect[16..18].copy_from_slice(&FF_CUSTOM.to_le_bytes());
        // aw86927_haptic_effect_strength maps gain/magnitude 0x0000..0x7FFF -> level 0x1E..0x80
        let mag = (0x3fffu32 + (a * 0x4000u32 as f32) as u32).min(0x7fff) as u16;
        effect[20..22].copy_from_slice(&mag.to_le_bytes());
        effect[36..40].copy_from_slice(&3u32.to_le_bytes());
        effect[40..48].copy_from_slice(&(custom_data.as_ptr() as u64).to_le_bytes());

        let _ = self.set_hw_gain(a);
        self.upload_and_start(&mut effect, false)?;
        self.rtp_done_at = Some(Instant::now() + Duration::from_millis(25));
        self.active_is_rtp = true;
        Ok(())
    }

    /// Streams 24 kHz signed 8-bit PCM via aw86927 custom_wave RTP FIFO.
    /// `amp` (0.0..=1.0) scales the PCM amplitude cleanly with full hardware headroom.
    /// Checks `gen_arc == expected_gen` between chunks so stop/new-effect cancels immediately.
    pub fn play_pcm_cancellable(
        &mut self,
        pcm: &[i8],
        amp: f32,
        gen_arc: &Arc<AtomicU64>,
        expected_gen: u64,
    ) -> io::Result<()> {
        self.erase_ff();
        let a = amp.clamp(0.0, 1.0);
        if a <= 0.001 || pcm.is_empty() {
            return Ok(());
        }
        if self.wave_fd < 0 || self.custom_wave_id < 0 {
            // Fallback to constant if custom_wave unavailable
            let dur = ((pcm.len() * 1000) / SAMPLE_RATE as usize).clamp(15, 5000) as u16;
            return self.play_constant(dur, a);
        }

        // Keep hardware DAC at full headroom (0x7FFF -> level 0x80) and apply slider scaling
        // cleanly once on PCM samples so max intensity hits full punch without double-attenuation.
        let _ = self.set_hw_gain(1.0);

        let mut custom_data = [0i16; 3];
        custom_data[0] = self.custom_wave_id;

        let mut effect = [0u8; 48];
        effect[0..2].copy_from_slice(&FF_PERIODIC.to_le_bytes());
        effect[2..4].copy_from_slice(&(-1i16).to_le_bytes());
        // u.periodic.waveform at offset 16
        effect[16..18].copy_from_slice(&FF_CUSTOM.to_le_bytes());
        // u.periodic.magnitude at offset 20
        effect[20..22].copy_from_slice(&0x7fffu16.to_le_bytes());
        // u.periodic.custom_len at offset 36 (u32)
        effect[36..40].copy_from_slice(&3u32.to_le_bytes());
        // u.periodic.custom_data ptr at offset 40 (u64)
        effect[40..48].copy_from_slice(&(custom_data.as_ptr() as u64).to_le_bytes());

        self.upload_and_start(&mut effect, true)?;

        let period = self.period_size;
        let mut chunk = Vec::with_capacity(period);
        let mut offset = 0usize;
        while offset < pcm.len() {
            if gen_arc.load(Ordering::SeqCst) != expected_gen {
                self.erase_ff();
                return Ok(());
            }
            let end = (offset + period).min(pcm.len());
            chunk.clear();
            for &s in &pcm[offset..end] {
                let scaled = ((s as f32) * a).round().clamp(-128.0, 127.0) as i8;
                chunk.push(scaled as u8);
            }
            if write_all_retry(self.wave_fd, &chunk).is_err() {
                self.erase_ff();
                return Err(io::Error::last_os_error());
            }
            offset = end;
        }

        // aw86927_custom_wave_store calls rb_end() only when count % period_size != 0 || count < period_size.
        // If total PCM length was an exact multiple of period_size, send 1 zero byte to signal EOS.
        if pcm.len() % period == 0 {
            let _ = write_all_retry(self.wave_fd, &[0u8]);
        }

        let dur_ms = ((pcm.len() as u64 * 1000) / SAMPLE_RATE as u64).max(5);
        self.rtp_done_at = Some(Instant::now() + Duration::from_millis(dur_ms + 25));

        Ok(())
    }

    fn upload_and_start(&mut self, effect: &mut [u8; 48], is_rtp: bool) -> io::Result<()> {
        loop {
            let rc = unsafe { ioctl(self.evdev_fd, EVIOCSFF, effect.as_mut_ptr()) };
            if rc != -1 {
                let id = i16::from_le_bytes([effect[2], effect[3]]);
                self.active_ff_id = id;
                self.active_is_rtp = is_rtp;
                if let Err(e) = self.write_ev_ff(id as u16, 1) {
                    self.erase_ff();
                    return Err(e);
                }
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(EINTR) {
                crate::aloge!("EVIOCSFF failed: {}", err);
                return Err(err);
            }
        }
    }
}

impl Drop for Motor {
    fn drop(&mut self) {
        self.stop();
        if self.wave_fd >= 0 {
            unsafe { close(self.wave_fd) };
        }
        if self.evdev_fd >= 0 {
            unsafe { close(self.evdev_fd) };
        }
    }
}

fn write_all_retry(fd: c_int, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = unsafe { write(fd, buf.as_ptr().cast(), buf.len()) };
        if n > 0 {
            buf = &buf[n as usize..];
            continue;
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(EINTR) {
            continue;
        }
        return Err(err);
    }
    Ok(())
}

fn read_sysfs_i32(dir: &PathBuf, node: &str) -> Option<i32> {
    let raw = fs::read_to_string(dir.join(node)).ok()?;
    raw.split(|c: char| !c.is_ascii_digit() && c != '-')
        .find(|s| !s.is_empty() && *s != "-")
        .and_then(|s| s.parse::<i32>().ok())
}

fn find_awinic_sysfs() -> io::Result<PathBuf> {
    let direct = PathBuf::from("/sys/bus/i2c/drivers/awinic_haptic/2-005a");
    if direct.exists() {
        return Ok(direct);
    }
    for entry in fs::read_dir("/sys/bus/i2c/devices")? {
        let Ok(e) = entry else { continue };
        let p = e.path();
        if let Ok(s) = fs::read_to_string(p.join("name")) {
            if s.trim() == "awinic_haptic" {
                return Ok(p);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "awinic_haptic sysfs dir not found",
    ))
}

fn find_awinic_evdev() -> io::Result<c_int> {
    let mut entries: Vec<_> = fs::read_dir("/dev/input")?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();

    let mut fallback_ff: Option<c_int> = None;
    for path in entries {
        let Some(name) = path.to_str() else { continue };
        if !name.contains("event") {
            continue;
        }
        let Ok(cpath) = CString::new(name) else { continue };
        let fd = unsafe { open(cpath.as_ptr(), O_RDWR) };
        if fd < 0 {
            continue;
        }
        let mut dev_name = [0u8; 64];
        unsafe { ioctl(fd, EVIOCGNAME_64, dev_name.as_mut_ptr()) };
        let dev_str = dev_name
            .split(|&b| b == 0)
            .next()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();

        let mut caps = [0u8; 16];
        unsafe { ioctl(fd, EVIOCGBIT_FF, caps.as_mut_ptr()) };
        let has_ff = (caps[0x0a] & 0x06) != 0;

        if dev_str.contains("awinic_haptic") || dev_str.contains("haptic") {
            if let Some(old) = fallback_ff {
                unsafe { close(old) };
            }
            return Ok(fd);
        }
        if has_ff && fallback_ff.is_none() {
            fallback_ff = Some(fd);
        } else {
            unsafe { close(fd) };
        }
    }

    fallback_ff.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no FF input device found"))
}

fn open_custom_wave(sysfs_dir: &PathBuf) -> (c_int, i16, usize) {
    let path = sysfs_dir.join("custom_wave");
    let Some(path_str) = path.to_str() else {
        return (-1, -1, 512);
    };
    let Ok(cpath) = CString::new(path_str) else {
        return (-1, -1, 512);
    };
    let fd = unsafe { open(cpath.as_ptr(), O_RDWR) };
    if fd < 0 {
        crate::aloge!("open {} failed: {}", path_str, io::Error::last_os_error());
        return (-1, -1, 512);
    }
    let mut buf = [0u8; 256];
    let n = unsafe { read(fd, buf.as_mut_ptr().cast(), buf.len() - 1) };
    if n <= 0 {
        unsafe { close(fd) };
        return (-1, -1, 512);
    }
    let text = String::from_utf8_lossy(&buf[..n as usize]);
    let mut wave_id = -1i16;
    let mut period_size = 512usize;
    for part in text.split(|c: char| c == ';' || c.is_whitespace()) {
        if let Some(v) = part.strip_prefix("custom_wave_id=") {
            if let Ok(id) = v.parse::<i16>() {
                wave_id = id;
            }
        } else if let Some(v) = part.strip_prefix("period_size=") {
            if let Ok(ps) = v.parse::<usize>() {
                if ps > 0 {
                    period_size = ps;
                }
            }
        }
    }
    if wave_id < 0 {
        unsafe { close(fd) };
        return (-1, -1, 512);
    }
    (fd, wave_id, period_size)
}
