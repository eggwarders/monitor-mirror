# Monitor Mirror

<p align="center">
  <img src="media/logo.png" alt="Monitor Mirror Logo" width="160" height="160">
</p>

<p align="center">
  <b>Ultra-low latency, zero-CPU audio mirroring and device switcher for Windows.</b><br>
  Mirror loopback audio from your primary display or headset to another endpoint with <b>&lt;3 ms latency</b>, real-time volume synchronization, and instant hotkey audio output switching.
</p>

---

## Features

- **⚡ Ultra-Low Latency WASAPI Loopback Engine**:
  - Configurable latency cushion right from the menu (`1 ms` Ultra-Low up to `10 ms` Buffered, default `3 ms`).
  - Direct PCM pass-through (bypasses Windows Audio Resampler MFT when source and destination formats match).
  - Zero-copy silence handling (`AUDCLNT_BUFFERFLAGS_SILENT`) + automatic drift centering during silent gaps.
- **🎧 Sound Switch / Device Cycling via Global Hotkey & Middle-Click**: Cycle between your Windows audio playback devices instantly with a customizable hotkey (default: `Ctrl + Alt + A`) or by middle-clicking the tray icon.
- **⭐ Audio Device Favorites**: Mark preferred audio devices as Favorites (`★`). The hotkey cycles exclusively between your favorited devices, or across all active devices if fewer than 2 are starred.
- **🎨 Color-Coded Native System Tray Menu**:
  - <span style="color:#3B82F6"><b>Blue Section</b></span>: Audio Source (capture device) with live status badge (`● ACTIVE`, `⏸ PAUSED`, `⏳ WAITING`) and active Windows `Default` indicator.
  - <span style="color:#EF4444"><b>Red Section</b></span>: Audio Destination (mirror target).
  - <span style="color:#CBD5E1"><b>Silver Accent</b></span>: Master & Per-Channel Volume Synchronization + Latency Cushion selector.
  - <span style="color:#06B6D4"><b>Cyan Accent</b></span>: Instant Sound Switch with hotkey indicator.
  - <span style="color:#F59E0B"><b>Gold Accent</b></span>: Starred Favorites submenu (stays open on click for seamless toggling) + manual Pause toggle.
- **🖥️ Per-Monitor V2 High-DPI & Dark Mode**: Native High-DPI ClearType rendering cached per-DPI with zero GDI handle churn and dark non-client frame integration (no white outlines).
- **🔊 Volume Synchronization**: Hardware volume keys or taskbar adjustments on the primary device mirror instantly to the secondary output (including left/right channel balance).
- **🪶 Ultra-Light Footprint**:
  - **Memory**: ~2.5 MB Working Set (~1.8 MB Private Bytes) — native `user32` PNG decoding (no `gdiplus.dll` / `shlwapi.dll`), zero-CPU `INFINITE` idle wait when paused, and automatic working set trimming.
  - **CPU**: 0.00% when idle or paused; microsecond DMA copies during playback.
  - **Binary**: ~272 KB standalone native executable with embedded 256×256 icon and zero external runtime dependencies.
- **🚀 Start with Windows**: One-click startup toggle persisted in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
- **🛡️ Auto-Pause & Self-Mirror Protection**: Automatically pauses mirroring (zero active streams, monitors silent) when Windows default audio switches to your headset or another non-source endpoint.

---

## Keyboard Shortcuts

The global hotkey is configurable directly from the system tray menu:
- `Ctrl + Alt + A` (Default)
- `Ctrl + Shift + A`
- `Ctrl + Alt + S`
- `Win + Alt + A`
- `F11`
- `Scroll Lock`

Pressing the hotkey switches the Windows default audio playback device (Console, Multimedia, and Communications) and instantly updates the mirroring stream with 0 ms reconnection delay.

---

## System Tray Menu Overview

Right-click (or left-click) the **Monitor Mirror** tray icon to access (or middle-click to cycle output):
1. **Source** (<span style="color:#3B82F6">Blue</span>): Select the primary audio device being captured; shows live mirror state (`ACTIVE`, `PAUSED`, `WAITING`) and Windows `Default` device badge.
2. **Destination** (<span style="color:#EF4444">Red</span>): Select the target endpoint where audio is mirrored.
3. **Cycle Audio Output** (<span style="color:#06B6D4">Cyan</span>): Manually trigger device cycling.
4. **Favorites** (<span style="color:#F59E0B">Gold</span>): Star devices to restrict hotkey cycling to your favorite outputs (menu stays open while toggling).
5. **Switch Hotkey**: Choose your preferred global hotkey combination.
6. **Latency Cushion**: Tune mirror buffer cushion live between `1 ms` and `10 ms` (persisted across restarts).
7. **Pause Mirroring**: Manually pause/resume mirroring without changing your Windows default device.
8. **Volume Synchronization** (<span style="color:#CBD5E1">Silver</span>): Toggle master and balance sync.
9. **Start with Windows**: Enable or disable launch at login.
10. **Exit**: Gracefully teardown WASAPI audio streams and exit.

---

## Building from Source

### Prerequisites
- Windows 10 or 11 (64-bit)
- Rust toolchain (MSVC)

```powershell
# Clone the repository
git clone https://github.com/eggwarders/monitor-mirror.git
cd monitor-mirror

# Build release executable
cargo build --release
```

The compiled binary will be located at `target/release/monitor-mirror.exe`.

---

## CLI Options

```powershell
# Run in background (system tray only, loads persisted settings)
monitor-mirror.exe

# List all active audio endpoints and IDs
monitor-mirror.exe --list

# Launch with explicit devices and latency cushion
monitor-mirror.exe --src "Speakers" --dst "Monitor" --cushion 3
```

---

## License

MIT License. Feel free to use, modify, and distribute.

