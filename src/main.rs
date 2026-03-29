// ferrosdr/src/main.rs
//
// RTL-SDR spectrum + waterfall + audio demodulation server.
// Build:  cargo build --release
// Deploy: scp target/release/ferrosdr user@192.168.177.155:~
//
// Ports:
//   8080 — HTTP  : embedded single-page app
//   8081 — WS    : binary frames (spectrum + audio) and JSON control
//
// Binary frame format:
//   Spectrum (2057 bytes): [0x53, 4×i16 display ranges, 1024×i16 FFT bins]
//   Audio (3+N×2 bytes):   [0x41, u16 sample_rate, N×i16 mono PCM]
//
// WebSocket commands (client→server JSON):
//   {"cmd":"set_demod", "mode":"NBFM"|"WBFM"|"AM"|null, "offset_hz":0.0}
//   {"cmd":"apply",     "settings":{…}}
//   {"cmd":"save_profile", "name":"…", "settings":{…}}
//   {"cmd":"select_profile", "name":"…"}
//   {"cmd":"delete_profile", "name":"…"}
//   {"cmd":"rename_profile", "old_name":"…", "new_name":"…"}
//   {"cmd":"get_profiles"}

use std::collections::HashMap;
use std::f32::consts::PI;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use num_complex::Complex;
use rtl_sdr_rs::{RtlSdr, TunerGain};
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

// ─── Spectrum constants ───────────────────────────────────────────────────────

const FFT_SIZE: usize       = 2048;
const FFT_AVERAGES: usize   = 10;   // must keep buf divisible by all decimation chains:
                                     // 20480/5=4096/8=512 (WBFM), 20480/8=2560/5=512 (NBFM)
const OUTPUT_BINS: usize    = 1024;
const HTTP_PORT: u16        = 8080;
const WS_PORT: u16          = 8081;
const BROADCAST_CAP: usize  = 16;
const SEND_INTERVAL_MS: u64 = 40;
const PROFILES_PATH: &str   = "./profiles.json";

// ─── Audio / demod constants ──────────────────────────────────────────────────

const FRAME_SPECTRUM: u8 = 0x53; // 'S'
const FRAME_AUDIO:    u8 = 0x41; // 'A'

// FIR anti-aliasing filter — applied after coarse decimation, before FM/AM demod.
// 64-tap Blackman-windowed sinc gives ~74 dB stopband rejection, cleanly
// rejecting adjacent PMR446 / NBFM channels 12.5 kHz away.
const FIR_TAPS: usize = 64;

const HTML: &str = include_str!("index.html");

// ─── Settings ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Marker {
    freq_hz: i64,    // absolute frequency in Hz
    label:   String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    centre_freq: u32,
    sample_rate: u32,
    gain_tenths: i32,
    spec_min_db: f32,
    spec_max_db: f32,
    wf_min_db:   f32,
    wf_max_db:   f32,
    #[serde(default = "default_squelch")]
    squelch_db:  f32,
    #[serde(default)]
    markers:     Vec<Marker>,
    #[serde(default)]
    demod_mode:  Option<String>,  // "NBFM" | "WBFM" | "AM" | null — UI state only, not applied to SDR hardware
}

fn default_squelch() -> f32 { -100.0 }

impl Default for Settings {
    fn default() -> Self {
        Settings {
            centre_freq: 869_525_000,
            sample_rate: 2_000_000,
            gain_tenths: 338,
            spec_min_db: -70.0,
            spec_max_db: -40.0,
            wf_min_db:   -80.0,
            wf_max_db:   -50.0,
            squelch_db:  -100.0,
            markers:     Vec::new(),
            demod_mode:  None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileStore {
    current:  String,
    profiles: HashMap<String, Settings>,
}

impl Default for ProfileStore {
    fn default() -> Self {
        let mut profiles = HashMap::new();
        profiles.insert("Default".to_string(), Settings::default());
        ProfileStore { current: "Default".to_string(), profiles }
    }
}

fn load_profiles() -> ProfileStore {
    std::fs::read_to_string(PROFILES_PATH)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_profiles(store: &ProfileStore) {
    if let Ok(json) = serde_json::to_string_pretty(store) {
        let _ = std::fs::write(PROFILES_PATH, json);
    }
}

// ─── Demod config (shared: WS handler ↔ SDR thread) ─────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum DemodMode { NBFM, WBFM, AM }

#[derive(Debug, Clone)]
struct DemodConfig {
    mode:       Option<DemodMode>,
    offset_hz:  f32,
    squelch_db: f32,  // power threshold in dBFS; -100 = always open (no squelch)
}

impl Default for DemodConfig {
    fn default() -> Self { DemodConfig { mode: None, offset_hz: 0.0, squelch_db: -70.0 } }
}

// ─── Demod run-state (local to SDR thread — never shared) ────────────────────

struct DemodRun {
    mixer_phase:  f32,
    prev_iq:      Complex<f32>,
    agc_peak:     f32,
    deemph:       f32,
    am_prev_x:    f32,
    am_prev_y:    f32,
    prev_mode:    Option<DemodMode>,
    prev_sr:      u32,
    fir_taps:     Vec<f32>,
    fir_delay:    Vec<Complex<f32>>,
    fir_pos:      usize,
    // Squelch fade state
    fade_gain:    f32,
    // DC offset blocker — IIR state for I and Q channels
    // Applied in mix_decimate at full sample rate before mixing.
    // α=0.9999 → cutoff ≈ 32 Hz at 2 MSPS — removes DC without
    // affecting audio (voice starts at 300 Hz).
    dc_i:         f32,
    dc_q:         f32,
}

impl Default for DemodRun {
    fn default() -> Self {
        DemodRun {
            mixer_phase:  0.0,
            prev_iq:      Complex::new(0.0, 0.0),
            agc_peak:     0.01,
            deemph:       0.0,
            am_prev_x:    0.0,
            am_prev_y:    0.0,
            prev_mode:    None,
            prev_sr:      0,
            fir_taps:     Vec::new(),
            fir_delay:    Vec::new(),
            fir_pos:      0,
            fade_gain:    1.0,
            dc_i:         0.0,
            dc_q:         0.0,
        }
    }
}

// ─── Spectrum DSP ─────────────────────────────────────────────────────────────

fn make_hann_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f32 / (n - 1) as f32).cos()))
        .collect()
}

fn compute_spectrum(
    buf:     &[u8],
    fft:     &Arc<dyn Fft<f32>>,
    scratch: &mut Vec<Complex<f32>>,
    window:  &[f32],
) -> Vec<f32> {
    let n = FFT_SIZE;
    let mut power_sum = vec![0f32; n];
    let mut input     = vec![Complex::new(0f32, 0f32); n];

    for avg_i in 0..FFT_AVERAGES {
        let offset = avg_i * n * 2;
        for i in 0..n {
            let re = (buf[offset + i * 2]     as f32 - 127.5) / 32768.0 * window[i];
            let im = (buf[offset + i * 2 + 1] as f32 - 127.5) / 32768.0 * window[i];
            input[i] = Complex::new(re, im);
        }
        fft.process_with_scratch(&mut input, scratch);
        for i in 0..n {
            let shifted = (i + n / 2) % n;
            power_sum[shifted] += input[i].norm_sqr();
        }
    }
    power_sum
        .iter()
        .map(|&p| 10.0 * (p / FFT_AVERAGES as f32 + 1e-30_f32).log10())
        .collect()
}

fn downsample(spectrum: &[f32]) -> Vec<f32> {
    let mut out: Vec<f32> = (0..OUTPUT_BINS)
        .map(|i| (spectrum[i * 2] + spectrum[i * 2 + 1]) * 0.5)
        .collect();
    let dc = OUTPUT_BINS / 2;
    out[dc] = (out[dc - 1] + out[dc + 1]) * 0.5; // suppress DC spike
    out
}

fn encode_spectrum_frame(bins: &[f32], s: &Settings) -> Vec<u8> {
    let mut frame = Vec::with_capacity(1 + 8 + OUTPUT_BINS * 2);
    frame.push(FRAME_SPECTRUM);
    for &v in &[s.spec_min_db, s.spec_max_db, s.wf_min_db, s.wf_max_db] {
        let p = (v * 10.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        frame.extend_from_slice(&p.to_le_bytes());
    }
    for &db in bins {
        let p = (db * 10.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        frame.extend_from_slice(&p.to_le_bytes());
    }
    frame
}

// ─── Audio DSP ────────────────────────────────────────────────────────────────

/// Mix IQ samples down to baseband at -offset_hz, apply a DC blocker to remove
/// the RTL-SDR's LO feedthrough spike, then decimate by `factor` using a box
/// (averaging) anti-alias filter.
///
/// DC blocker: single-pole IIR highpass on I and Q separately.
///   α = 0.9999 → cutoff ≈ 32 Hz at 2 MSPS — kills DC without touching audio.
fn mix_decimate(
    buf:        &[u8],
    offset_hz:  f32,
    input_rate: f32,
    factor:     usize,
    phase:      &mut f32,
    dc_i:       &mut f32,
    dc_q:       &mut f32,
) -> Vec<Complex<f32>> {
    const DC_ALPHA: f32 = 0.9999;
    let n_out     = (buf.len() / 2) / factor;
    let phase_inc = -2.0 * PI * offset_hz / input_rate;
    let mut out   = Vec::with_capacity(n_out);

    for chunk in 0..n_out {
        let mut acc = Complex::new(0.0f32, 0.0);
        for k in 0..factor {
            let i  = chunk * factor + k;
            let mut re = (buf[i * 2]     as f32 - 127.5) / 32768.0;
            let mut im = (buf[i * 2 + 1] as f32 - 127.5) / 32768.0;
            // DC blocker — remove static LO feedthrough before mixing
            *dc_i  = DC_ALPHA * *dc_i + (1.0 - DC_ALPHA) * re;
            *dc_q  = DC_ALPHA * *dc_q + (1.0 - DC_ALPHA) * im;
            re    -= *dc_i;
            im    -= *dc_q;
            let phi = *phase;
            acc    += Complex::new(re, im) * Complex::new(phi.cos(), phi.sin());
            *phase  = (*phase + phase_inc).rem_euclid(2.0 * PI);
        }
        out.push(acc / factor as f32);
    }
    out
}

/// Design a Blackman-windowed sinc low-pass FIR filter.
/// cutoff_hz is the -6 dB point. Blackman window gives ~74 dB stopband rejection.
fn make_lpf_fir(cutoff_hz: f32, sample_rate: f32, num_taps: usize) -> Vec<f32> {
    let fc = cutoff_hz / sample_rate;   // normalised cutoff (0.0 … 0.5)
    let m  = (num_taps - 1) as f32 / 2.0;
    let mut h: Vec<f32> = (0..num_taps).map(|i| {
        let x = i as f32 - m;
        let sinc = if x.abs() < 1e-6 { 2.0 * fc }
                   else { (2.0 * PI * fc * x).sin() / (PI * x) };
        let w = 0.42
              - 0.5  * (2.0 * PI * i as f32 / (num_taps - 1) as f32).cos()
              + 0.08 * (4.0 * PI * i as f32 / (num_taps - 1) as f32).cos();
        sinc * w
    }).collect();
    let sum: f32 = h.iter().sum();
    h.iter_mut().for_each(|x| *x /= sum);
    h
}

/// Apply FIR filter to complex IQ samples using a circular delay buffer.
/// Maintains state between calls so filtering is continuous across block boundaries.
fn apply_fir_c(
    samples: &[Complex<f32>],
    taps:    &[f32],
    delay:   &mut Vec<Complex<f32>>,
    pos:     &mut usize,
) -> Vec<Complex<f32>> {
    let n = taps.len();
    if delay.len() != n {
        *delay = vec![Complex::new(0.0, 0.0); n];
        *pos   = 0;
    }
    let mut out = Vec::with_capacity(samples.len());
    for &s in samples {
        delay[*pos] = s;
        let acc: Complex<f32> = (0..n)
            .map(|k| delay[(*pos + n - k) % n] * taps[k])
            .sum();
        *pos = (*pos + 1) % n;
        out.push(acc);
    }
    out
}

/// FM discriminator: instantaneous frequency via phase difference.
fn fm_demod(samples: &[Complex<f32>], prev: &mut Complex<f32>) -> Vec<f32> {
    let mut out  = Vec::with_capacity(samples.len());
    let mut last = *prev;
    for &s in samples {
        let prod = last.conj() * s;
        out.push(prod.im.atan2(prod.re));
        last = s;
    }
    *prev = last;
    out
}

/// Box-filter decimation of real audio signal.
fn decimate_r(samples: &[f32], factor: usize) -> Vec<f32> {
    let n = samples.len() / factor;
    (0..n)
        .map(|i| samples[i * factor..(i + 1) * factor].iter().sum::<f32>() / factor as f32)
        .collect()
}

/// Single-pole IIR de-emphasis (τ = 50 µs, European FM standard).
fn deemphasis(samples: &mut [f32], state: &mut f32, sample_rate: f32) {
    let alpha = (1.0 / sample_rate) / (50.0e-6 + 1.0 / sample_rate);
    for s in samples.iter_mut() {
        *state += alpha * (*s - *state);
        *s      = *state;
    }
}

/// AM envelope detector with DC-blocking high-pass filter.
fn am_demod(samples: &[Complex<f32>], prev_x: &mut f32, prev_y: &mut f32) -> Vec<f32> {
    const ALPHA: f32 = 0.9975;
    let mut out = Vec::with_capacity(samples.len());
    for s in samples {
        let x   = s.norm();
        let y   = x - *prev_x + ALPHA * *prev_y;
        *prev_x = x;
        *prev_y = y;
        out.push(y);
    }
    out
}

/// Per-sample AGC with moderately fast attack and slow release.
///
/// ATTACK 0.02 → reacts to level changes over ~50 samples (≈1ms at 48kHz)
/// without hunting on individual signal peaks — fixes WBFM distortion.
/// Soft limiter `x/(1+|x|)` replaces hard clamp — smooth saturation with
/// no discontinuity at ±1, preserving audio quality on strong signals.
fn agc(samples: &[f32], peak: &mut f32) -> Vec<f32> {
    const ATTACK:  f32 = 0.02;
    const RELEASE: f32 = 0.001;
    const TARGET:  f32 = 0.85;
    let mut p = *peak;
    let out: Vec<f32> = samples.iter().map(|&s| {
        let a = s.abs();
        p += if a > p { ATTACK * (a - p) } else { RELEASE * (a - p) };
        p  = p.max(1e-5);
        let x = s / p * TARGET;
        x / (1.0 + x.abs()) // soft limiter — smooth saturation, no hard clip
    }).collect();
    *peak = p;
    out
}

/// Pack normalised f32 samples into a binary audio frame.
fn encode_audio_frame(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let mut frame = Vec::with_capacity(3 + samples.len() * 2);
    frame.push(FRAME_AUDIO);
    frame.extend_from_slice(&(sample_rate as u16).to_le_bytes());
    for &s in samples {
        let v = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
        frame.extend_from_slice(&v.to_le_bytes());
    }
    frame
}

/// Apply a per-sample gain ramp for click-free squelch open/close.
/// `gain` is mutated continuously (0.0 = silent, 1.0 = full).
/// `step` = 1 / (sample_rate × fade_seconds).
/// Returns None when the output is entirely silent (saves bandwidth).
fn apply_fade(samples: &[f32], gain: &mut f32, open: bool, step: f32) -> Option<Vec<f32>> {
    let out: Vec<f32> = samples.iter().map(|&s| {
        if open { *gain = (*gain + step).min(1.0); }
        else    { *gain = (*gain - step).max(0.0); }
        s * *gain
    }).collect();
    if *gain < 0.001 && !open { None } else { Some(out) }
}

/// Full demodulation pipeline for one raw IQ buffer.
/// `squelch_open` is pre-computed from the spectrum bins in the SDR thread —
/// perfectly calibrated to the display dBFS scale.
///
/// Pipeline for all modes:
///   1. Mix to baseband + coarse box-filter decimate to ~250 kHz (NBFM/AM) or ~400 kHz (WBFM)
///   2. Blackman-windowed FIR LPF — sharp channel filter, rejects adjacent channels
///   3. Simple step-by-N decimate to audio rate (safe after FIR removed aliasing)
///   4. FM discriminator or AM envelope detection
///   5. Per-sample AGC
///
/// The FIR is designed at the intermediate (post-coarse-decimate) sample rate so the
/// cutoff in Hz is independent of the input sample rate. Works correctly at any SR.
fn process_audio(buf: &[u8], cfg: &DemodConfig, run: &mut DemodRun, input_sr: u32, squelch_open: bool) -> Option<Vec<u8>> {
    let mode = cfg.mode.as_ref()?;
    let fs   = input_sr as f32;

    match mode {
        DemodMode::NBFM => {
            let factor1 = (input_sr / 250_000).max(1) as usize;
            let sr_mid  = input_sr / factor1 as u32;
            let factor2 = (sr_mid  /  50_000).max(1) as usize;
            let out_sr  = sr_mid / factor2 as u32;

            if run.fir_taps.is_empty() {
                run.fir_taps = make_lpf_fir(6_250.0, sr_mid as f32, FIR_TAPS);
            }

            let iq_mid  = mix_decimate(buf, cfg.offset_hz, fs, factor1, &mut run.mixer_phase, &mut run.dc_i, &mut run.dc_q);
            let iq_filt = apply_fir_c(&iq_mid, &run.fir_taps, &mut run.fir_delay, &mut run.fir_pos);
            let iq_out: Vec<Complex<f32>> = iq_filt.into_iter().step_by(factor2).collect();
            let mut fm  = fm_demod(&iq_out, &mut run.prev_iq);
            let scale   = (2_500.0_f32 / out_sr as f32 * 2.0 * PI).max(1e-6);
            for s in fm.iter_mut() { *s /= scale; }
            let audio     = agc(&fm, &mut run.agc_peak);
            let fade_step = 1.0 / (out_sr as f32 * 0.010);
            apply_fade(&audio, &mut run.fade_gain, squelch_open, fade_step)
                .map(|faded| encode_audio_frame(&faded, out_sr))
        }

        DemodMode::WBFM => {
            let factor1 = (input_sr / 400_000).max(1) as usize;
            let sr_mid  = input_sr / factor1 as u32;
            let factor2 = (sr_mid  /  50_000).max(1) as usize;
            let out_sr  = sr_mid / factor2 as u32;

            if run.fir_taps.is_empty() {
                run.fir_taps = make_lpf_fir(90_000.0, sr_mid as f32, FIR_TAPS);
            }

            let iq_mid  = mix_decimate(buf, cfg.offset_hz, fs, factor1, &mut run.mixer_phase, &mut run.dc_i, &mut run.dc_q);
            let iq_filt = apply_fir_c(&iq_mid, &run.fir_taps, &mut run.fir_delay, &mut run.fir_pos);
            let mut fm  = fm_demod(&iq_filt, &mut run.prev_iq);
            let scale   = (75_000.0_f32 / sr_mid as f32 * 2.0 * PI).max(1e-6);
            for s in fm.iter_mut() { *s /= scale; }
            deemphasis(&mut fm, &mut run.deemph, sr_mid as f32);
            let audio = decimate_r(&fm, factor2);
            let out   = agc(&audio, &mut run.agc_peak);
            Some(encode_audio_frame(&out, out_sr))  // WBFM: no squelch
        }

        DemodMode::AM => {
            let factor1 = (input_sr / 250_000).max(1) as usize;
            let sr_mid  = input_sr / factor1 as u32;
            let factor2 = (sr_mid  /  24_000).max(1) as usize;
            let out_sr  = sr_mid / factor2 as u32;

            if run.fir_taps.is_empty() {
                run.fir_taps = make_lpf_fir(5_000.0, sr_mid as f32, FIR_TAPS);
            }

            let iq_mid  = mix_decimate(buf, cfg.offset_hz, fs, factor1, &mut run.mixer_phase, &mut run.dc_i, &mut run.dc_q);
            let iq_filt = apply_fir_c(&iq_mid, &run.fir_taps, &mut run.fir_delay, &mut run.fir_pos);
            let iq_out: Vec<Complex<f32>> = iq_filt.into_iter().step_by(factor2).collect();
            let env       = am_demod(&iq_out, &mut run.am_prev_x, &mut run.am_prev_y);
            let audio     = agc(&env, &mut run.agc_peak);
            let fade_step = 1.0 / (out_sr as f32 * 0.010);
            apply_fade(&audio, &mut run.fade_gain, squelch_open, fade_step)
                .map(|faded| encode_audio_frame(&faded, out_sr))
        }
    }
}

// ─── SDR reader thread ────────────────────────────────────────────────────────

fn sdr_thread(
    settings: Arc<Mutex<Settings>>,
    demod:    Arc<Mutex<DemodConfig>>,
    frame_tx: Arc<broadcast::Sender<Vec<u8>>>,
) {
    let initial = settings.lock().unwrap().clone();

    let mut sdr = match RtlSdr::open(rtl_sdr_rs::DeviceId::Index(0)) {
        Ok(s)  => s,
        Err(e) => { eprintln!("[SDR] Cannot open device: {:?}", e); return; }
    };

    sdr.set_sample_rate(initial.sample_rate).expect("[SDR] set_sample_rate");
    sdr.set_center_freq(initial.centre_freq).expect("[SDR] set_center_freq");
    sdr.set_tuner_gain(TunerGain::Manual(initial.gain_tenths)).expect("[SDR] set_tuner_gain");
    sdr.reset_buffer().expect("[SDR] reset_buffer");

    println!(
        "[SDR] Ready — {:.3} MHz  {:.3} MSPS  {:.1} dB",
        initial.centre_freq as f64 / 1e6,
        initial.sample_rate as f64 / 1e6,
        initial.gain_tenths as f32 / 10.0,
    );

    let mut planner  = FftPlanner::<f32>::new();
    let fft          = planner.plan_fft_forward(FFT_SIZE);
    let mut scratch  = vec![Complex::new(0f32, 0f32); fft.get_inplace_scratch_len()];
    let window       = make_hann_window(FFT_SIZE);

    let buf_len      = FFT_SIZE * FFT_AVERAGES * 2;
    let mut buf      = vec![0u8; buf_len];
    let mut last_applied = initial;
    let mut last_send         = Instant::now();
    let mut demod_run         = DemodRun::default();
    let mut squelch_open_cache = true; // updated from spectrum bins every 40ms

    loop {
        if let Err(e) = sdr.read_sync(&mut buf) {
            eprintln!("[SDR] Read error: {:?}", e);
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }

        // Apply settings changes
        let current = settings.lock().unwrap().clone();
        if current.centre_freq != last_applied.centre_freq {
            let _ = sdr.set_center_freq(current.centre_freq);
            println!("[SDR] Freq → {:.3} MHz", current.centre_freq as f64 / 1e6);
        }
        if current.gain_tenths != last_applied.gain_tenths {
            let _ = sdr.set_tuner_gain(TunerGain::Manual(current.gain_tenths));
            println!("[SDR] Gain → {:.1} dB", current.gain_tenths as f32 / 10.0);
        }
        if current.sample_rate != last_applied.sample_rate {
            let _ = sdr.set_sample_rate(current.sample_rate);
            println!("[SDR] Rate → {:.3} MSPS", current.sample_rate as f64 / 1e6);
        }
        last_applied = current.clone();

        // Spectrum at 25 fps + squelch measurement from spectrum bins
        let now = Instant::now();
        let demod_cfg = demod.lock().unwrap().clone();

        if now.duration_since(last_send) >= Duration::from_millis(SEND_INTERVAL_MS) {
            last_send = now;
            let spec  = compute_spectrum(&buf, &fft, &mut scratch, &window);
            let ds    = downsample(&spec);
            let frame = encode_spectrum_frame(&ds, &current);
            let _ = frame_tx.send(frame);

            // Compute squelch from spectrum bins — perfectly calibrated to the display scale.
            // Find the OUTPUT_BINS bin that corresponds to the demod slice centre frequency,
            // then take the peak power over the channel bandwidth window.
            let channel_bw_hz = match demod_cfg.mode.as_ref() {
                Some(DemodMode::NBFM) => 12_500.0f32,
                Some(DemodMode::AM)   => 10_000.0f32,
                _                     => 0.0f32,
            };
            if channel_bw_hz > 0.0 {
                let bin_width_hz  = current.sample_rate as f32 / OUTPUT_BINS as f32;
                let half_bins     = ((channel_bw_hz / 2.0 / bin_width_hz) as usize).max(1);
                let center        = ((demod_cfg.offset_hz / current.sample_rate as f32 + 0.5)
                                    * OUTPUT_BINS as f32).clamp(0.0, (OUTPUT_BINS-1) as f32) as usize;
                let lo            = center.saturating_sub(half_bins);
                let hi            = (center + half_bins).min(OUTPUT_BINS - 1);
                // Peak dBFS in the channel window — matches what the user sees on the spectrum
                let peak_db       = ds[lo..=hi].iter().cloned().fold(f32::NEG_INFINITY, f32::max);
                squelch_open_cache = peak_db >= demod_cfg.squelch_db;
            } else {
                squelch_open_cache = true; // WBFM or OFF: always open
            }
        }

        // Audio demodulation — every read cycle (~8 ms)
        // Reset demod state on mode or sample rate change
        if demod_cfg.mode != demod_run.prev_mode || current.sample_rate != demod_run.prev_sr {
            let saved_peak       = demod_run.agc_peak;
            demod_run            = DemodRun::default();
            demod_run.agc_peak   = saved_peak;
            demod_run.prev_mode  = demod_cfg.mode.clone();
            demod_run.prev_sr    = current.sample_rate;
        }
        if let Some(audio_frame) = process_audio(&buf, &demod_cfg, &mut demod_run,
                                                  current.sample_rate, squelch_open_cache) {
            let _ = frame_tx.send(audio_frame);
        }
    }
}

// ─── Command processor ────────────────────────────────────────────────────────

fn process_command(
    text:     &str,
    settings: &Arc<Mutex<Settings>>,
    store:    &Arc<Mutex<ProfileStore>>,
    demod:    &Arc<Mutex<DemodConfig>>,
) -> Option<String> {
    let cmd: serde_json::Value = serde_json::from_str(text).ok()?;
    let cmd_type = cmd["cmd"].as_str()?;

    match cmd_type {
        "get_profiles" => { /* fall through to reply */ }

        "add_marker" => {
            let freq_hz = cmd["freq_hz"].as_f64()? as i64;
            let label   = cmd["label"].as_str()?.trim().to_string();
            if label.is_empty() { return None; }
            {
                let mut store = store.lock().unwrap();
                let cur = store.current.clone();
                if let Some(profile) = store.profiles.get_mut(&cur) {
                    // Replace any existing marker at the same frequency
                    profile.markers.retain(|m| m.freq_hz != freq_hz);
                    profile.markers.push(Marker { freq_hz, label });
                    save_profiles(&store);
                }
            }
        }

        "remove_marker" => {
            let freq_hz = cmd["freq_hz"].as_f64()? as i64;
            {
                let mut store = store.lock().unwrap();
                let cur = store.current.clone();
                if let Some(profile) = store.profiles.get_mut(&cur) {
                    profile.markers.retain(|m| m.freq_hz != freq_hz);
                    save_profiles(&store);
                }
            }
        }

        "set_demod" => {
            let mode = match cmd["mode"].as_str() {
                Some("NBFM") => Some(DemodMode::NBFM),
                Some("WBFM") => Some(DemodMode::WBFM),
                Some("AM")   => Some(DemodMode::AM),
                _            => None,
            };
            let offset_hz  = cmd["offset_hz"].as_f64().unwrap_or(0.0) as f32;
            let squelch_db = cmd["squelch_db"].as_f64().unwrap_or(-100.0) as f32;
            let mut d      = demod.lock().unwrap();
            d.mode         = mode;
            d.offset_hz    = offset_hz;
            d.squelch_db   = squelch_db;
            return None;
        }

        "apply" => {
            let new_s: Settings = serde_json::from_value(cmd["settings"].clone()).ok()?;
            *settings.lock().unwrap() = new_s;
            return None;
        }

        "select_profile" => {
            let name: String = cmd["name"].as_str()?.to_string();
            {
                let mut store = store.lock().unwrap();
                if !store.profiles.contains_key(&name) { return None; }
                store.current = name.clone();
            }
            let s = {
                let store = store.lock().unwrap();
                store.profiles.get(&name)?.clone()
            };
            *settings.lock().unwrap() = s;
        }

        "save_profile" => {
            let name: String = cmd["name"].as_str()?.trim().to_string();
            if name.is_empty() { return None; }
            let new_s: Settings = serde_json::from_value(cmd["settings"].clone()).ok()?;
            *settings.lock().unwrap() = new_s.clone();
            {
                let mut store = store.lock().unwrap();
                store.profiles.insert(name.clone(), new_s);
                store.current = name;
                save_profiles(&store);
            }
        }

        "delete_profile" => {
            let name: String = cmd["name"].as_str()?.to_string();
            {
                let mut store = store.lock().unwrap();
                if store.profiles.len() <= 1 { return None; }
                store.profiles.remove(&name);
                if store.current == name {
                    store.current = store.profiles.keys().min().unwrap().clone();
                }
                save_profiles(&store);
            }
        }

        "load_profile" => {
            let name: String = cmd["name"].as_str()?.to_string();
            {
                let mut store = store.lock().unwrap();
                if !store.profiles.contains_key(&name) { return None; }
                store.current = name;
                save_profiles(&store);
            }
        }

        "rename_profile" => {
            let old: String = cmd["old_name"].as_str()?.to_string();
            let new: String = cmd["new_name"].as_str()?.trim().to_string();
            if new.is_empty() { return None; }
            {
                let mut store = store.lock().unwrap();
                let ps = store.profiles.remove(&old)?;
                store.profiles.insert(new.clone(), ps);
                if store.current == old { store.current = new; }
                save_profiles(&store);
            }
        }

        _ => return None,
    }

    let store = store.lock().unwrap();
    Some(serde_json::json!({
        "type":     "profiles",
        "current":  store.current,
        "profiles": store.profiles,
    }).to_string())
}

// ─── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let store    = Arc::new(Mutex::new(load_profiles()));
    let settings = Arc::new(Mutex::new({
        let s = store.lock().unwrap();
        s.profiles.get(&s.current).cloned().unwrap_or_default()
    }));
    let demod = Arc::new(Mutex::new(DemodConfig::default()));

    let (tx_inner, _) = broadcast::channel::<Vec<u8>>(BROADCAST_CAP);
    let frame_tx = Arc::new(tx_inner);

    {
        let s = settings.lock().unwrap();
        println!("╔══════════════════════════════════════════════╗");
        println!("║  ferrosdr — {:.3} MHz                        ║", s.centre_freq as f64 / 1e6);
        println!("╠══════════════════════════════════════════════╣");
        println!("║  Waterfall : http://0.0.0.0:{}              ║", HTTP_PORT);
        println!("║  WebSocket : ws://0.0.0.0:{}                ║", WS_PORT);
        println!("╚══════════════════════════════════════════════╝");
    }

    {
        let settings = Arc::clone(&settings);
        let demod    = Arc::clone(&demod);
        let frame_tx = Arc::clone(&frame_tx);
        std::thread::spawn(move || sdr_thread(settings, demod, frame_tx));
    }

    // HTTP server
    let http = TcpListener::bind(format!("0.0.0.0:{}", HTTP_PORT)).await.unwrap();
    println!("[HTTP] Listening on http://0.0.0.0:{}", HTTP_PORT);
    tokio::spawn(async move {
        loop {
            if let Ok((mut stream, addr)) = http.accept().await {
                tokio::spawn(async move {
                    println!("[HTTP] {}", addr);
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    let body   = HTML.as_bytes();
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                });
            }
        }
    });

    // WebSocket server
    let ws = TcpListener::bind(format!("0.0.0.0:{}", WS_PORT)).await.unwrap();
    println!("[WS]   Listening on ws://0.0.0.0:{}", WS_PORT);

    loop {
        if let Ok((stream, addr)) = ws.accept().await {
            let frame_tx = Arc::clone(&frame_tx);
            let settings = Arc::clone(&settings);
            let store    = Arc::clone(&store);
            let demod    = Arc::clone(&demod);

            tokio::spawn(async move {
                let ws = match accept_async(stream).await {
                    Ok(w)  => w,
                    Err(e) => { eprintln!("[WS] Handshake from {}: {}", addr, e); return; }
                };
                println!("[WS] Connected: {}", addr);

                let (mut tx, mut rx) = ws.split();
                let mut frame_rx = frame_tx.subscribe();

                let init = {
                    let s = store.lock().unwrap();
                    serde_json::json!({
                        "type":     "profiles",
                        "current":  s.current,
                        "profiles": s.profiles,
                    }).to_string()
                };
                let _ = tx.send(Message::Text(init.into())).await;

                loop {
                    tokio::select! {
                        result = frame_rx.recv() => {
                            match result {
                                Ok(data) => {
                                    if tx.send(Message::Binary(data.into())).await.is_err() { break; }
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(_) => break,
                            }
                        }
                        msg = rx.next() => {
                            match msg {
                                Some(Ok(Message::Text(text))) => {
                                    if let Some(reply) =
                                        process_command(&text, &settings, &store, &demod)
                                    {
                                        if tx.send(Message::Text(reply.into())).await.is_err() { break; }
                                    }
                                }
                                Some(Ok(Message::Close(_))) | None => break,
                                _ => {}
                            }
                        }
                    }
                }
                println!("[WS] Disconnected: {}", addr);
            });
        }
    }
}
