//! Embeds the application icon and manifest in the Windows executable.
//!
//! The resources are written directly as a COFF object with a `.rsrc`
//! section, which the linker merges into the executable's resource directory.
//! This needs no resource compiler (rc.exe or windres), so the GNU toolchain
//! builds the executable on its own.

use std::env;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;

use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageEncoder};

const ICON_SOURCE: &str = "favicon-1024x1024.png";
const MANIFEST: &str = "assets/virtual-cables.manifest";

/// Icon sizes in the executable, largest first, as the previous build had.
const ICON_SIZES: [u32; 5] = [256, 64, 48, 32, 16];

const RT_ICON: u32 = 3;
const RT_GROUP_ICON: u32 = 14;
const RT_MANIFEST: u32 = 24;
const LANG_NEUTRAL: u32 = 0;
const LANG_EN_US: u32 = 0x0409;

/// The icon group the GUI loads with LoadImageW(instance, MAKEINTRESOURCE(1)).
const ICON_GROUP_ID: u32 = 1;
const MANIFEST_ID: u32 = 1; // CREATEPROCESS_MANIFEST_RESOURCE_ID

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={ICON_SOURCE}");
    println!("cargo:rerun-if-changed={MANIFEST}");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut resources = Vec::new();
    let icons = icon_images();
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes()); // reserved
    group.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    group.extend_from_slice(&(icons.len() as u16).to_le_bytes());
    for (index, (size, png)) in icons.into_iter().enumerate() {
        let id = index as u32 + 1;
        let dimension = if size >= 256 { 0 } else { size as u8 }; // 0 means 256
        group.extend_from_slice(&[dimension, dimension, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes()); // planes
        group.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        group.extend_from_slice(&(png.len() as u32).to_le_bytes());
        group.extend_from_slice(&(id as u16).to_le_bytes());
        resources.push(Resource { kind: RT_ICON, id, lang: LANG_NEUTRAL, data: png });
    }
    resources.push(Resource { kind: RT_GROUP_ICON, id: ICON_GROUP_ID, lang: LANG_NEUTRAL, data: group });
    let manifest = fs::read(MANIFEST).unwrap_or_else(|err| panic!("read {MANIFEST}: {err}"));
    resources.push(Resource { kind: RT_MANIFEST, id: MANIFEST_ID, lang: LANG_EN_US, data: manifest });

    let object = coff_object(&resources);
    let path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("resources.o");
    fs::write(&path, object).expect("write resource object");
    println!("cargo:rustc-link-arg-bins={}", path.display());
}

/// Resizes the source icon to each size and encodes it as PNG, which Windows
/// accepts for every icon size since Vista.
fn icon_images() -> Vec<(u32, Vec<u8>)> {
    let source = image::open(ICON_SOURCE).unwrap_or_else(|err| panic!("open {ICON_SOURCE}: {err}")).to_rgba8();
    ICON_SIZES
        .iter()
        .map(|&size| {
            let resized = image::imageops::resize(&source, size, size, FilterType::Lanczos3);
            let mut png = Vec::new();
            PngEncoder::new(Cursor::new(&mut png))
                .write_image(resized.as_raw(), size, size, ExtendedColorType::Rgba8)
                .expect("encode icon PNG");
            (size, png)
        })
        .collect()
}

struct Resource {
    kind: u32,
    id: u32,
    lang: u32,
    data: Vec<u8>,
}

/// Builds an AMD64 COFF object whose `.rsrc` section holds a three-level
/// resource directory (type, name, language). Each data entry's RVA is
/// relocated against the section, as resource compilers emit it.
fn coff_object(resources: &[Resource]) -> Vec<u8> {
    let mut resources: Vec<&Resource> = resources.iter().collect();
    resources.sort_by_key(|r| (r.kind, r.id, r.lang));
    let mut kinds: Vec<u32> = resources.iter().map(|r| r.kind).collect();
    kinds.dedup();

    // Directory tables: the root, one per type, and one per (type, name).
    let dir_size = |entries: usize| 16 + 8 * entries;
    let mut offset = dir_size(kinds.len());
    let mut type_dirs = Vec::new(); // (kind, offset, resources)
    for &kind in &kinds {
        let of_kind: Vec<&Resource> = resources.iter().copied().filter(|r| r.kind == kind).collect();
        type_dirs.push((kind, offset, of_kind.clone()));
        offset += dir_size(of_kind.len());
    }
    let mut name_dirs = Vec::new(); // (offset, resource), one language each
    for (_, _, of_kind) in &type_dirs {
        for &resource in of_kind {
            name_dirs.push((offset, resource));
            offset += dir_size(1);
        }
    }
    let entries_offset = offset;
    offset += 16 * resources.len();
    let mut data_offsets = Vec::new();
    for resource in &resources {
        offset = offset.next_multiple_of(8);
        data_offsets.push(offset);
        offset += resource.data.len();
    }
    let section_size = offset.next_multiple_of(8);

    let mut section = vec![0u8; section_size];
    let put32 = |buf: &mut Vec<u8>, at: usize, value: u32| buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
    let put_dir = |buf: &mut Vec<u8>, at: usize, entries: &[(u32, u32)]| {
        buf[at + 14..at + 16].copy_from_slice(&(entries.len() as u16).to_le_bytes()); // ID entries
        for (i, &(id, target)) in entries.iter().enumerate() {
            put32(buf, at + 16 + 8 * i, id);
            put32(buf, at + 20 + 8 * i, target);
        }
    };

    const SUBDIRECTORY: u32 = 0x8000_0000;
    let root: Vec<(u32, u32)> = type_dirs.iter().map(|(kind, at, _)| (*kind, *at as u32 | SUBDIRECTORY)).collect();
    put_dir(&mut section, 0, &root);
    let mut name_index = 0;
    for (_, at, of_kind) in &type_dirs {
        let mut entries = Vec::new();
        for resource in of_kind {
            entries.push((resource.id, name_dirs[name_index].0 as u32 | SUBDIRECTORY));
            name_index += 1;
        }
        put_dir(&mut section, *at, &entries);
    }
    let mut relocations = Vec::new();
    for (i, (at, resource)) in name_dirs.iter().enumerate() {
        // name_dirs follows the sorted resource order, as do the data entries.
        let entry = entries_offset + 16 * i;
        put_dir(&mut section, *at, &[(resource.lang, entry as u32)]);
        put32(&mut section, entry, data_offsets[i] as u32); // RVA, relocated below
        put32(&mut section, entry + 4, resource.data.len() as u32);
        relocations.push(entry as u32);
        section[data_offsets[i]..data_offsets[i] + resource.data.len()].copy_from_slice(&resource.data);
    }

    const HEADER_SIZE: usize = 20;
    const SECTION_HEADER_SIZE: usize = 40;
    const IMAGE_REL_AMD64_ADDR32NB: u16 = 3;
    let raw_offset = HEADER_SIZE + SECTION_HEADER_SIZE;
    let relocation_offset = raw_offset + section.len();
    let symbol_offset = relocation_offset + 10 * relocations.len();

    let mut object = Vec::new();
    object.extend_from_slice(&0x8664u16.to_le_bytes()); // IMAGE_FILE_MACHINE_AMD64
    object.extend_from_slice(&1u16.to_le_bytes()); // sections
    object.extend_from_slice(&0u32.to_le_bytes()); // timestamp
    object.extend_from_slice(&(symbol_offset as u32).to_le_bytes());
    object.extend_from_slice(&1u32.to_le_bytes()); // symbols
    object.extend_from_slice(&0u16.to_le_bytes()); // optional header size
    object.extend_from_slice(&0u16.to_le_bytes()); // characteristics

    object.extend_from_slice(b".rsrc\0\0\0");
    object.extend_from_slice(&0u32.to_le_bytes()); // virtual size
    object.extend_from_slice(&0u32.to_le_bytes()); // virtual address
    object.extend_from_slice(&(section.len() as u32).to_le_bytes());
    object.extend_from_slice(&(raw_offset as u32).to_le_bytes());
    object.extend_from_slice(&(relocation_offset as u32).to_le_bytes());
    object.extend_from_slice(&0u32.to_le_bytes()); // line numbers
    object.extend_from_slice(&(relocations.len() as u16).to_le_bytes());
    object.extend_from_slice(&0u16.to_le_bytes());
    object.extend_from_slice(&0x4000_0040u32.to_le_bytes()); // initialized data, readable

    object.extend_from_slice(&section);
    for at in relocations {
        object.extend_from_slice(&at.to_le_bytes());
        object.extend_from_slice(&0u32.to_le_bytes()); // symbol 0: the section
        object.extend_from_slice(&IMAGE_REL_AMD64_ADDR32NB.to_le_bytes());
    }

    object.extend_from_slice(b".rsrc\0\0\0");
    object.extend_from_slice(&0u32.to_le_bytes()); // value
    object.extend_from_slice(&1i16.to_le_bytes()); // section number
    object.extend_from_slice(&0u16.to_le_bytes()); // type
    object.push(3); // IMAGE_SYM_CLASS_STATIC
    object.push(0); // auxiliary symbols
    object.extend_from_slice(&4u32.to_le_bytes()); // empty string table
    object
}
