# Virtual Cables

Virtual Cables creates virtual audio cables on Windows without a custom audio driver. Each cable is a virtual USB Audio Class device, connected to Windows through USB/IP, so Windows' own built-in USB audio driver turns it into an ordinary playback device and recording device. Whatever is played to a cable's playback side can be recorded from its recording side.

The USB/IP transport is provided by [usbip-win2](https://github.com/vadimgrn/usbip-win2), which is installed separately.

## Features

- Up to 32 independent virtual audio cables
- A playback and a recording endpoint for every cable
- Standard USB Audio Class 2.0 devices, handled by Windows' built-in USB Audio 2.0 driver
- No emulation of proprietary audio hardware
- Add or remove cables from the application
- Bit-exact pass-through with a fixed latency on every cable
- A simple native Windows interface

## How it works

The application runs a local USB/IP server that exports each cable as a USB audio device. usbip-win2 attaches these devices to Windows, which detects every cable as two audio endpoints:

1. `Virtual Cable XX` playback device
2. `Virtual Cable XX` recording device

Audio played to the playback side comes out of the recording side of the same cable, bit for bit, a fixed `latency_ms` later. Both sides run on the same 1 ms USB frame clock that Windows uses, so there is no clock drift to correct: each millisecond of playback audio is stored under its frame number, and the recording side returns the audio of the frame `latency_ms` earlier. If audio for a frame never arrives, only that frame is silent. Volume and mute on either side apply on the way through, and at 100% the audio is untouched.

Each cable is a USB Audio Class 2.0 device, so Windows uses its in-box USB Audio 2.0 driver (`usbaudio2.sys`). That driver refills playback from the audio engine's shared buffer as each transfer completes, so system load does not interrupt it. Windows' older USB Audio 1.0 driver depends on the audio engine for every buffer: under load it stops sending for tens of milliseconds and can fall into resetting the stream after every transfer, which sounds robotic.

This is different from conventional virtual audio cables, which depend on a dedicated kernel-mode audio driver.

## Audio format

Every cable uses a fixed format:

- 48,000 Hz
- Stereo
- 24-bit PCM

The USB Audio Class 2.0 descriptors declare one fixed 48 kHz clock and synchronous isochronous endpoints of one 48-frame packet per 1 ms frame, so neither side needs a feedback endpoint. This does not update or repair the separate usbip-win2 kernel driver.

## Requirements

- Windows 10 version 1703 or later, or Windows 11, for the in-box USB Audio 2.0 driver
- x64 processor
- The usbip-win2 transport driver
- Administrator approval when the cables are attached to Windows

## Getting started

1. Start Virtual Cables.
2. If usbip-win2 is not installed yet, select **Options > Download and install driver**. The application downloads the latest official installer from GitHub and starts it. Reboot if the installer asks you to.
3. Use **Add cable** to create a cable; it asks for the name Windows Sound shows, made of letters, digits, hyphens, underscores, parentheses and single spaces. There are no cables until you add one.
4. Approve the one-time administrator prompt. The application then attaches the cables to Windows, where they appear in Windows Sound settings under their names.

To delete a cable, select it in the list and use **Remove cable**. Changes are saved and applied to Windows straight away.

**Options > Open settings folder** opens the folder that holds `config.toml` and the log.

## Configuration

Settings are read from `config.toml` in the settings folder, `%LOCALAPPDATA%\Virtual Cables`. The file is created with the defaults on first start, and the application adds a `[[cable]]` table for each cable you create:

```toml
# Address of the local USB/IP server.
listen = "127.0.0.1:3240"
# Delay between each cable's playback and recording sides, 1-1000 ms.
# Playback that Windows sends later than this plays after a short gap.
latency_ms = 40
# When playback transfers complete: "end" of their last frame, as USB hardware
# does, or "start" of their first, which gives Windows more time to send the next.
playback_completion = "end"
# Detailed USB/IP traces in the log, for troubleshooting.
diagnostics = false
# For troubleshooting: hold all audio transfers this many milliseconds once a
# minute, to reproduce a system stall. 0 turns it off.
stall_test_ms = 0

[[cable]]
id = 1
serial = "8F3A2C1D"
name = "Game audio"
```

| Setting | Meaning | Default |
|---|---|---|
| `listen` | Address of the local USB/IP server. | `"127.0.0.1:3240"` |
| `latency_ms` | Delay between each cable's playback and recording sides, 1-1000 ms. Windows sends playback about 10 ms ahead, so any value works while streaming normally; the latency is how late, after a system stall, playback can still arrive and play on time. If it arrives later still, the cable moves the rest of that stream later, leaving a short gap, and moves it back, skipping the audio in between, once Windows catches up. | `40` |
| `playback_completion` | When playback transfers are reported complete: `"end"` of their last frame, as USB hardware does, or `"start"` of their first, while another transfer is queued behind it. `"start"` was a workaround for Windows' USB Audio 1.0 driver, which sends each playback transfer only after an earlier one completes and resets the stream whenever a completion leaves nothing queued; the cables now use the USB Audio 2.0 driver. The audio plays at the same time either way. | `"end"` |
| `diagnostics` | Detailed USB/IP traces in the log, for troubleshooting (`true` or `false`). | `false` |
| `stall_test_ms` | For troubleshooting: hold every audio transfer of every cable this many milliseconds once a minute, starting a minute after start-up, to reproduce a system stall on demand. 0-10000; 0 turns it off. | `0` |
| `[[cable]]` | One table per cable, up to 32, in list order. `serial` is created with the cable and is its identity in Windows, so a new cable is always a new device there with its own name, and an existing one keeps its settings after a restart. `id` (1-255) sets the USB product ID (`FFFF:CBxx`) and bus ID (`1-<id>`). `name` is what Windows Sound shows. The application rewrites these tables when cables are added or removed, and removing a cable also deletes its devices from Windows. | none |

Invalid values fall back to the default, and the log says so. The application only ever rewrites the `[[cable]]` tables, so comments and edits elsewhere in the file are kept. If the file is not valid TOML, the application starts with the default settings and no cables, logs the error, and does not overwrite the file until it is fixed.

### Command line

Every setting can also be given on the command line, which overrides `config.toml` for that run:

```
VirtualCables.exe [--cables <COUNT>] [--listen <ADDRESS>] [--latency-ms <MS>] [--playback-completion <WHEN>] [--diagnostics [<BOOL>]] [--stall-test-ms <MS>]
```

| Option | Action |
|---|---|
| `--self-test` | Validate the descriptors and audio path, then exit |
| `--dump-descriptors` | Print each cable's USB descriptors, then exit |
| `-V`, `--version` | Print the version, then exit |
| `-h`, `--help` | List all options |

The executable is a windowed application, so to see command-line output in a console, redirect it, for example `VirtualCables.exe --version | more`.

## Logs and troubleshooting

The log is `VirtualCables_USBIP.log` in the settings folder. It is cleared at each start of the application, so it holds only the current session; check it before restarting after a problem.

- **Crackling or dropouts:** look for `AUDIO HEALTH` lines. They are always logged, at most one every 10 seconds per cable, when something has changed:
  - `missing_ms`: milliseconds of silence the recording side produced while playback was running.
  - `realigned`: how often playback arrived too late to play on time, so its stream was moved later.
  - `caught_up`: how often a stream that had been moved later was moved back once Windows caught up.
  - `playback_resyncs`, `capture_resyncs` and `halt_clears`: stream restarts, and stream resets by Windows.
  - `late_packets`: playback packets Windows sent for frames that had already begun, which are reported back as not sent, as a USB controller does. Their audio still plays.

  `halt_clears` rising by hundreds every 10 seconds means Windows is stuck resetting the playback stream after every transfer, which sounds robotic or crackly; this happened with the USB Audio 1.0 driver, which the cables no longer use; `playback_completion = "start"` may help if it appears. `stall_test_ms = 30` reproduces the stall that can start such a loop once a minute, so settings can be tried quickly. If `missing_ms` rises after stalls without a reset loop, try a larger `latency_ms`.

  For more detail, set `diagnostics = true`, restart, and reproduce the problem. Each logged `HALT CLEAR` (Windows resetting a stream) is then preceded by `ISO TRACE ... before` lines for the last transfers on that endpoint, and followed by `ISO TRACE ... after` lines for the next ones, with their bus frames and timing.
- **No devices in Windows Sound:** check that usbip-win2 is installed and that the administrator prompt was approved. The `usbip.exe` lines in the log show the attach steps and any errors.

## Building from source

Virtual Cables is written in Rust.

1. Install Rust with [rustup](https://rustup.rs). The project builds with either Windows toolchain:
   - **GNU** (no Visual Studio needed):
     ```
     rustup-init.exe --default-host x86_64-pc-windows-gnu
     ```
   - **MSVC**: the default rustup install, which needs the Visual Studio Build Tools and the Windows SDK.
2. Build the release executable:
   ```
   cargo build --release
   ```
   The result is `target\release\VirtualCables.exe`, a single self-contained file. Copy it wherever you keep the application.
3. Run the tests and lints:
   ```
   cargo test
   cargo clippy --all-targets
   ```

After changing dependencies, regenerate the third-party licence notices (see [`LICENSES/README.md`](LICENSES/README.md)).

The application icon and Windows manifest are embedded at build time by `build.rs`: the icon is generated from `favicon-1024x1024.png` in sizes from 16 to 256 px, and the manifest is `assets/virtual-cables.manifest`. No separate resource compiler is needed.

### RustRover

Open the project folder in RustRover. Shared run configurations are in `.run/` and appear in the run menu: **Run**, **Build release**, **Test**, **Clippy** and **Self-test**. **Run** starts the application on the default port, so close any other running copy first.

### Project layout

| Path | Contents |
|---|---|
| `src/main.rs` | Startup, the log, and the server lifecycle |
| `src/cli.rs` | Command-line options |
| `src/config.rs` | `config.toml` |
| `src/uac2.rs` | The USB Audio Class 2.0 device: descriptors and control requests |
| `src/audio.rs` | The delay line, addressed by USB frame, between playback and recording |
| `src/usbip/` | The USB/IP protocol, the server, and the USB frame clock |
| `src/platform/windows/` | The window, the administrator helper, `usbip.exe` attachment and the driver download |
| `src/event.rs` | Precise, interruptible waits for frame timing |

## Project status

Virtual Cables is an experimental project. Its USB descriptors, USB/IP protocol handling, audio path, control requests and stream scheduling are covered by automated tests.

Real-world stability depends on the Windows system, the USB/IP transport, system load and the number of cables in use at once. Test it before relying on it in critical production or live-performance settings.

## Privacy

Virtual Cables processes audio locally. Audio stays in memory on the computer, and the USB/IP server listens only on the local machine by default. The only network access is the optional driver download from GitHub.

## Third-party components

Virtual Cables uses the separately distributed usbip-win2 transport. The executable also contains open-source Rust crates and the Rust standard library; their licences and notices are in the [`LICENSES`](LICENSES) folder.

## Licence

Virtual Cables is licensed under the MIT licence; see [`LICENSE`](LICENSE). Third-party licences are listed in [`LICENSES/README.md`](LICENSES/README.md).
