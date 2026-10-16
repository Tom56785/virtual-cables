# Third-party licences

Virtual Cables itself is licensed under the MIT licence in [`../LICENSE`](../LICENSE).
The Windows executable also contains, or works with, the components below.

| File | Covers |
|---|---|
| [`Rust-crates.txt`](Rust-crates.txt) | The Rust crates compiled into the executable (clap, serde_json, socket2, windows-sys and their dependencies), with each crate's licence and copyright notice. |
| [`Rust-standard-library.html`](Rust-standard-library.html) | The Rust standard library, which every Rust program contains (MIT or Apache-2.0), and the crates it is built from. |
| [`GCC-runtime-library-exception.txt`](GCC-runtime-library-exception.txt) | Small parts of the GCC and MinGW-w64 runtime that the GNU toolchain links into the executable. The GCC Runtime Library Exception places no conditions on programs built this way; it is included for reference. MinGW-w64 sources: https://www.mingw-w64.org/ |
| [`usbip-win2_BSD-2-Clause.txt`](usbip-win2_BSD-2-Clause.txt) | usbip-win2, the separately installed USB/IP driver the application uses. It is not part of the executable. |

## Updating `Rust-crates.txt`

After adding, removing or upgrading a dependency, regenerate the file from the repository root:

```
powershell -ExecutionPolicy Bypass -File scripts\update-licenses.ps1
```

The script asks Cargo which crates are compiled into the Windows executable (`cargo tree -e normal,no-proc-macro`), then writes each crate's licence, source and notice file, taken from the crate's own files in the Cargo registry. It prints the crates it found and warns about any crate without a licence file.

Crates used only while building (procedural macros such as `clap_derive`, and the `image` crate that resizes the icon) do not end up in the executable, so they are left out.
