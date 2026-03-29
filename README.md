# ferrosdr

**RTL-SDR spectrum, waterfall and audio demodulation server — written in Rust**

A lightweight SDR web server intended for VHF/UHF FM and AM reception using an RTL-SDR USB dongle on a headless Linux system. The server streams a live spectrum display, waterfall and demodulated audio to any browser on any device on the same network — including mobile phones with touchscreen control.

Other repositories port this to small machines such as the Raspberry Pi Zero 2W.

---

## Screenshots

*Spectrum + waterfall with PMR446 channel markers, squelch line and demod slice*
<img width="2270" height="2063" alt="Screenshot from 2026-03-29 14-58-27" src="https://github.com/user-attachments/assets/2a79fece-940c-4c21-9d5c-792cd7aeba84" />

---

## Why Rust

The code is written in Rust for low CPU usage on small embedded systems. Rust binaries compile to a single self-contained executable with no runtime dependencies other than `libusb`. The source code is 35 KB and compiles in approximately 6 seconds on a desktop machine.

Cross-compilation is straightforward — compile on a fast desktop and transfer the binary to the target machine over the network using SCP. No Rust toolchain is required on the target.

---

## Features

- **Live spectrum display** with 1024 FFT bins, Hann-windowed, 10-frame averaging, 25 fps
- **Waterfall** with adjustable colour map (black → blue → cyan → yellow → red)
- **Three demodulation modes:** NBFM, WBFM, AM
- **Draggable demod slice** — click or drag on spectrum or waterfall to tune; mouse wheel or two-finger trackpad scroll shifts frequency in 0.5 kHz steps
- **Touchscreen support** throughout
- **Named frequency profiles** — save all SDR and display parameters per band
- **Channel markers** — pin any frequency with a label; PMR446 channels 1–16 appear automatically as yellow markers when the band is in view
- **Two-mode scanner:**
  - **CH mode** — scans only pinned channel markers using the squelch threshold
  - **WB mode** — scans all 1024 spectrum bins simultaneously using an auto-adapting per-bin noise floor, independently of squelch
- **Squelch** — draggable threshold line on spectrum display; shown in dBFS on same scale as spectrum; saved per profile
- **Settings panel** — all parameters accessible from the browser; no SSH session required during operation
- **Mobile-responsive layout** — split-screen on small screens with spectrum above and settings panel below

---

## Hardware Requirements

- Any Linux machine (x86\_64 or ARM) with a USB port
- RTL-SDR USB dongle (RTL2832U chipset — tested with NESDR SMArt v5)
- `libusb-1.0` installed on the target machine

---

## Quick Start

### On the server machine

Install the runtime dependency:

```
sudo apt install libusb-1.0-0
```

Blacklist the kernel DVB driver so it does not claim the dongle:

```
echo -e 'blacklist dvb_usb_rtl28xxu\nblacklist rtl2832\nblacklist rtl2830' | sudo tee /etc/modprobe.d/rtlsdr-blacklist.conf
sudo rmmod dvb_usb_rtl28xxu 2>/dev/null; sudo rmmod rtl2832 2>/dev/null
```

Add your user to the `plugdev` group so the dongle is accessible without root:

```
sudo usermod -aG plugdev $USER
```

Log out and back in, then run the server:

```
./ferrosdr
```

Open a browser on any device on the same network and navigate to:

```
http://<server-ip>:8080
```

### Building from source

Install Rust if not already present:

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Install the build dependency:

```
sudo apt install libusb-1.0-0-dev pkg-config
```

Build:

```
cargo build --release
```

The binary is at `target/release/ferrosdr`. Copy it to the server:

```
scp target/release/ferrosdr user@<server-ip>:~
```

---

## Cross-Compilation (e.g. for Raspberry Pi)

Install the ARM musl cross-compiler:

```
rustup target add arm-unknown-linux-musleabihf
cd ~/Downloads
wget https://musl.cc/arm-linux-musleabihf-cross.tgz
tar xf arm-linux-musleabihf-cross.tgz -C ~/
echo 'export PATH="$HOME/arm-linux-musleabihf-cross/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

Add the linker to `~/.cargo/config.toml`:

```toml
[target.arm-unknown-linux-musleabihf]
linker = "arm-linux-musleabihf-gcc"
```

Build for ARM:

```
cargo build --release --target arm-unknown-linux-musleabihf
```

Deploy:

```
scp target/arm-unknown-linux-musleabihf/release/ferrosdr user@<pi-ip>:~
```

The resulting binary is statically linked and runs on any ARM Linux without any installed libraries.

---

## Profiles

All SDR and display parameters are saved in `profiles.json` in the same directory as the binary. A `profiles.json` with example frequency profiles is included in this repository.

Each profile stores:

| Field | Description |
|---|---|
| `centre_freq` | Centre frequency in Hz |
| `sample_rate` | Sample rate (0.25 to 2.8 MSPS) |
| `gain_tenths` | Tuner gain × 10 (e.g. 338 = 33.8 dB) |
| `spec_min_db` | Spectrum Y-axis lower bound (dBFS) |
| `spec_max_db` | Spectrum Y-axis upper bound (dBFS) |
| `wf_min_db` | Waterfall colour map lower bound — black (dBFS) |
| `wf_max_db` | Waterfall colour map upper bound — red (dBFS) |
| `squelch_db` | Squelch threshold (dBFS); −100 = always open |
| `demod_mode` | Last selected demod mode (`"NBFM"`, `"WBFM"`, `"AM"`, or null) |
| `markers` | Array of pinned channel markers `{freq_hz, label}` |

---

## Using the Settings Panel

Open the settings panel by pressing **SETTINGS** in the top bar.

- **Select profile** from the dropdown — the receiver retunes immediately
- **Edit parameters** in the form fields
- Press **Enter** to apply the new parameters to the live receiver without saving
- Press **💾 SAVE** to write the current parameters back to the selected profile on disk
- **Save As New** — type a name in the name field and press Save As New to create a new profile
- **Rename / Delete** — use the corresponding buttons in the Profile section
- Press **✕ CLOSE** or **Escape** to close the panel

---

## Squelch

A red dashed horizontal line on the spectrum display marks the squelch threshold. The current level in dBFS is shown in the `SQL` field in the top bar (visible when NBFM or AM is selected).

- **Drag the line** up or down on the spectrum canvas to set the threshold
- **Type a value** directly in the SQL field
- The threshold is saved per profile when you press SAVE
- Squelch gates audio output only — it is independent of the scanner trigger in WB mode

---

## Channel Markers and Scanner

### Pinning a channel marker

1. Select a demod mode (NBFM, WBFM or AM)
2. Move the demod slice to the frequency you want to mark (click, drag or scroll)
3. Press the **📍** button in the top bar
4. Enter a label when prompted
5. Press SAVE to save the marker to the current profile

Pinned markers appear as cyan triangles on the spectrum. PMR446 channels 1–16 appear automatically as yellow triangles whenever that band is in view — no setup required.

### Scanner

Press **◉ SCAN** to start scanning. The button highlights green when active. Select the scan mode with the **CH / WB** button next to it.

**CH mode (channel scan)**

Checks only the visible yellow and cyan channel markers. Uses the squelch threshold as the signal detection level — set the squelch line just above the noise floor before scanning. When a marker frequency exceeds the threshold, the demod slice jumps there and audio plays. Scanning resumes approximately 0.5 seconds after the channel goes quiet.

This mode is ideal for channelised bands such as PMR446 where you want to skip the frequencies between channels entirely.

**WB mode (wideband scan)**

Checks all 1024 spectrum bins simultaneously on every frame. Uses an auto-adapting per-bin noise floor estimate (IIR, α = 0.15, ~1 second settling time) rather than the squelch threshold. Triggers when any bin exceeds its local noise floor by more than the configurable margin (default +12 dB shown in the `+nn dB` field).

The noise floor is re-estimated continuously as a slow-moving average, so it tracks gradual band-level changes (such as the ±6 dB variation typical across a 2 MHz airband capture) without triggering on them.

Wideband scan performance:

> All 1024 spectrum bins are checked simultaneously in one 40 ms frame — effectively 25,600 checks per second. This is approximately 2,500× faster than a conventional scanner at 10 channels/second and 250× faster than a fast modern scanner at 100 channels/second. Signal-to-audio latency is under 200 ms.

---

## DSP Pipeline

### Spectrum

- 2048-point FFT with Hann window
- 10-frame power averaging (reduces noise by ~5 dB)
- FFT shift — DC centred
- DC spike interpolation — the RTL-SDR LO feedthrough spike at centre frequency is suppressed by replacing the centre bin with the average of its neighbours
- Downsampled from 2048 to 1024 output bins
- Sent as binary frames (1024 × i16, dBFS × 10) at 25 fps

There is intentionally **no AGC on the spectrum display**. The noise floor stays at a constant level regardless of signal strength, giving a reliable visual reference for squelch setting and signal evaluation. Strong signals may clip the top of the display range — this is expected behaviour.

### Demodulation

All three modes share a common pipeline:

1. **DC blocker** — single-pole IIR highpass (α = 0.9999, cutoff ~32 Hz at 2 MSPS) removes the RTL-SDR LO feedthrough DC offset from the IQ stream before mixing. Without this, the DC spike produces a low-frequency buzz in the demodulated audio.

2. **Frequency mixer** — complex IQ multiplication shifts the selected channel to baseband (0 Hz).

3. **Coarse decimation** — box-filter averaging reduces the sample rate to an intermediate rate (~250 kHz for NBFM/AM, ~400 kHz for WBFM).

4. **64-tap Blackman-windowed sinc FIR low-pass filter** — applied at the intermediate rate with cutoff matched to the channel bandwidth:
   - NBFM: 6.25 kHz (half of 12.5 kHz channel spacing)
   - WBFM: 90 kHz
   - AM: 5 kHz
   
   The Blackman window gives approximately 74 dB stopband rejection. This rejects adjacent channels and prevents cross-channel interference. For PMR446 with 12.5 kHz channel spacing, adjacent channels are attenuated by ~40–50 dB.

5. **Fine decimation** — simple step-by-N subsampling is safe after the FIR has already removed all energy above the new Nyquist frequency. Decimation factors are computed at runtime so any input sample rate works correctly.

6. **FM discriminator** (NBFM/WBFM) — instantaneous phase difference via `atan2(Im(z·z*_prev), Re(z·z*_prev))`.

7. **De-emphasis** (WBFM only) — single-pole IIR, τ = 50 µs (European FM standard).

8. **AM envelope detector** (AM only) — `|z|` with first-order DC-blocking highpass.

9. **AGC** — per-sample gain control with attack time constant ~1 ms and release ~250 ms. Uses a soft limiter `x / (1 + |x|)` instead of hard clipping, which preserves audio quality on strong signals without hard distortion.

10. **Squelch fade** (NBFM/AM) — 10 ms linear gain ramp on squelch open/close eliminates clicks.

### Audio output to browser

Audio is sent as binary WebSocket frames (i16 mono PCM at the demodulated sample rate). The browser receives frames at approximately 10 ms intervals and schedules them for gapless playback using the Web Audio API.

**Resampling:** The browser's AudioContext runs at the OS audio device rate (typically 44100 or 48000 Hz), while our demodulator outputs at ~50000 Hz. Passing audio at the wrong rate to the AudioContext causes the browser to apply its own low-quality linear interpolation resampler, which produces audible warble on pure tones. ferrosdr resamples in JavaScript using Catmull-Rom cubic Hermite interpolation to the exact AudioContext native rate before scheduling. The browser then plays the buffer with no further resampling.

---

## Ports

| Port | Protocol | Purpose |
|---|---|---|
| 8080 | HTTP | Serves the embedded single-page web application |
| 8081 | WebSocket | Binary spectrum and audio frames; JSON control messages |

The HTML/CSS/JavaScript frontend is embedded directly in the Rust binary at compile time using `include_str!`. No web server setup is required.

---

## Known Limitations

- Sample rate is assumed to be 2.0 MSPS for the demodulation decimation chain. Other rates work but audio output rate will scale accordingly.
- WBFM requires a minimum input sample rate of approximately 360 kHz. It does not work at 0.25 MSPS.
- No RF AGC — strong nearby signals can overload the ADC. This is intentional (see Spectrum section above).
- IQ imbalance correction is not implemented. The RTL-SDR's inherent amplitude and phase imbalance between I and Q channels produces a small image signal. This is a hardware characteristic.

---

## WebSocket Protocol Reference

### Client → Server (JSON)

```json
{"cmd": "set_demod",      "mode": "NBFM"|"WBFM"|"AM"|null, "offset_hz": 0.0, "squelch_db": -60.0}
{"cmd": "apply",          "settings": {...}}
{"cmd": "save_profile",   "name": "...", "settings": {...}}
{"cmd": "select_profile", "name": "..."}
{"cmd": "delete_profile", "name": "..."}
{"cmd": "rename_profile", "old_name": "...", "new_name": "..."}
{"cmd": "add_marker",     "freq_hz": 446006250, "label": "PMR 1"}
{"cmd": "remove_marker",  "freq_hz": 446006250}
{"cmd": "get_profiles"}
```

### Server → Client

Binary frames are prefixed with a type byte:

- `0x53` ('S') — spectrum frame: `[type, 4×i16 display ranges×10, 1024×i16 bins×10]`
- `0x41` ('A') — audio frame: `[type, u16 sample_rate, N×i16 PCM]`

JSON frames:

```json
{"type": "profiles", "current": "...", "profiles": {...}}
```

---

## Acknowledgements

Built using:
- [rtl-sdr-rs](https://crates.io/crates/rtl-sdr-rs) — pure Rust RTL-SDR driver
- [rustfft](https://crates.io/crates/rustfft) — FFT
- [tokio](https://tokio.rs) — async runtime
- [tokio-tungstenite](https://crates.io/crates/tokio-tungstenite) — WebSocket
