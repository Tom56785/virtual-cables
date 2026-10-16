# Virtual Cables

Virtual Cables creates virtual audio cables on Windows without a custom audio driver. Each cable is a virtual USB Audio Class device, connected to Windows through USB/IP, so Windows' own built-in USB audio driver turns it into an ordinary playback device and recording device. Whatever is played to a cable's playback side can be recorded from its recording side.

The USB/IP transport is provided by [usbip-win2](https://github.com/vadimgrn/usbip-win2), which is installed separately.

## Features

- Up to 32 independent virtual audio cables
- A playback and a recording endpoint for every cable
- Standard USB Audio Class 1.0 devices, handled by Windows' built-in USB audio driver
- No emulation of proprietary audio hardware
- Add or remove cables from the application
- An independent audio buffer for every cable
- A simple native Windows interface

## How it works

The application runs a local USB/IP server that exports each cable as a USB audio device. usbip-win2 attaches these devices to Windows, which detects every cable as two audio endpoints:

1. `Virtual Cable XX` playback device
2. `Virtual Cable XX` recording device

Audio written to the playback side is stored in a ring buffer and returned through the recording side of the same cable. The server paces both sides on the same USB frame clock that Windows uses, and a small elastic reserve absorbs scheduling jitter.

This is different from conventional virtual audio cables, which depend on a dedicated kernel-mode audio driver.

## Audio format

Every cable uses a fixed format:

- 48,000 Hz
- Stereo
- 24-bit PCM

The USB Audio Class descriptors and isochronous endpoint sizes are configured for packed 24-bit PCM. This does not update or repair the separate usbip-win2 kernel driver.

## Requirements

- Windows 10 or Windows 11
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
# Ring-buffer capacity per cable, 20-2000 ms.
buffer_ms = 250
# Audio kept buffered against scheduling hiccups, from 10 ms to half of buffer_ms.
reserve_ms = 40
# Detailed USB/IP traces in the log, for troubleshooting.
diagnostics = false

[[cable]]
id = 1
serial = "8F3A2C1D"
name = "Game audio"
```

| Setting | Meaning | Default |
|---|---|---|
| `listen` | Address of the local USB/IP server. | `"127.0.0.1:3240"` |
| `buffer_ms` | Ring-buffer capacity per cable, 20-2000 ms. Only matters when audio backs up, for example after a system stall. | `250` |
| `reserve_ms` | Audio kept buffered against scheduling hiccups, from 10 ms to half of `buffer_ms`. This is the latency a cable adds: lower is more responsive, higher is more tolerant of system load. | `40` |
| `diagnostics` | Detailed USB/IP traces in the log, for troubleshooting (`true` or `false`). | `false` |
| `[[cable]]` | One table per cable, up to 32, in list order. `serial` is created with the cable and is its identity in Windows, so a new cable is always a new device there with its own name, and an existing one keeps its settings after a restart. `id` (1-255) sets the USB product ID (`FFFF:CAxx`) and bus ID (`1-<id>`). `name` is what Windows Sound shows. The application rewrites these tables when cables are added or removed, and removing a cable also deletes its devices from Windows. | none |

Invalid values fall back to the default, and the log says so. The application only ever rewrites the `[[cable]]` tables, so comments and edits elsewhere in the file are kept. If the file is not valid TOML, the application starts with the default settings and no cables, logs the error, and does not overwrite the file until it is fixed.

### Command line

Every setting can also be given on the command line, which overrides `config.toml` for that run:

```
VirtualCables.exe [--cables <COUNT>] [--listen <ADDRESS>] [--buffer-ms <BUFFER_MS>] [--reserve-ms <RESERVE_MS>] [--diagnostics [<BOOL>]]
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

- **Crackling or dropouts:** set `diagnostics = true`, restart, and reproduce the problem. Lines containing `underrun_bytes` that keep increasing mean the recording side is running out of audio; `HALT CLEAR` lines show Windows resetting a stream after a system stall. If dropouts happen during normal load, try a larger `reserve_ms`.
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
| `src/uac1.rs` | The USB Audio Class 1.0 device: descriptors and control requests |
| `src/audio.rs` | The ring buffer and rate correction between playback and recording |
| `src/usbip/` | The USB/IP protocol, the server, and the USB frame clock |
| `src/platform/windows/` | The window, the administrator helper, `usbip.exe` attachment and the driver download |
| `src/event.rs` | Precise, interruptible waits for frame timing |

## Project status

Virtual Cables is an experimental project. Its USB descriptors, USB/IP protocol handling, audio buffers, control requests and stream scheduling are covered by automated tests.

Real-world stability depends on the Windows system, the USB/IP transport, system load and the number of cables in use at once. Test it before relying on it in critical production or live-performance settings.

## Privacy

Virtual Cables processes audio locally. Audio stays in memory on the computer, and the USB/IP server listens only on the local machine by default. The only network access is the optional driver download from GitHub.

## Third-party components

Virtual Cables uses the separately distributed usbip-win2 transport. The executable also contains open-source Rust crates and the Rust standard library; their licences and notices are in the [`LICENSES`](LICENSES) folder.

## Licence

Virtual Cables is licensed under the MIT licence; see [`LICENSE`](LICENSE). Third-party licences are listed in [`LICENSES/README.md`](LICENSES/README.md).
