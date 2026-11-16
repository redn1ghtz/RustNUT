#[path = "src/icon.rs"]
mod icon;

use std::{env, fs, path::PathBuf};

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="1.0.0.0" processorArchitecture="*" name="RustNUT" type="win32"/>
  <dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1"><application>
    <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
  </application></compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings>
    <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
    <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2,PerMonitor</dpiAwareness>
  </windowsSettings></application>
</assembly>"#;

fn word(out: &mut Vec<u8>, value: u16) {
    out.extend(value.to_le_bytes());
}

fn dword(out: &mut Vec<u8>, value: u32) {
    out.extend(value.to_le_bytes());
}

fn version_block(key: &str, text: bool, value: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    word(&mut out, 0); // Patched with the full block size.
    word(
        &mut out,
        u16::try_from(if text { value.len() / 2 } else { value.len() }).unwrap(),
    );
    word(&mut out, u16::from(text));
    for ch in key.encode_utf16().chain(Some(0)) {
        word(&mut out, ch);
    }
    out.resize(out.len().next_multiple_of(4), 0);
    out.extend_from_slice(value);
    for child in children {
        out.resize(out.len().next_multiple_of(4), 0);
        out.extend_from_slice(child);
    }
    let length = u16::try_from(out.len()).unwrap().to_le_bytes();
    out[..2].copy_from_slice(&length);
    out
}

fn version_string(key: &str, value: &str) -> Vec<u8> {
    let value: Vec<u8> = value
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    version_block(key, true, &value, &[])
}

fn version_resource() -> Vec<u8> {
    let [major, minor, patch] = [
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH"),
    ]
    .map(|part| part.parse::<u16>().unwrap());
    let product = env!("CARGO_PKG_VERSION");
    let file_version = format!("{major}.{minor}.{patch}.0");
    let ms = u32::from(major) << 16 | u32::from(minor);
    let ls = u32::from(patch) << 16;
    let flags = if env!("CARGO_PKG_VERSION_PRE").is_empty() {
        0
    } else {
        2
    }; // VS_FF_PRERELEASE
    let fixed: Vec<u8> = [
        0xfeef04bd, 0x10000, ms, ls, ms, ls, 0x3f, flags, 0x00040004, // VOS_NT_WINDOWS32
        1, 0, 0, 0, // VFT_APP, subtype and file date
    ]
    .into_iter()
    .flat_map(u32::to_le_bytes)
    .collect();
    let strings = [
        ("CompanyName", "redn1ghtz"),
        ("FileDescription", "RustNUT UPS monitor"),
        ("FileVersion", &file_version),
        ("InternalName", "rustnut"),
        ("OriginalFilename", "rustnut.exe"),
        ("ProductName", "RustNUT"),
        ("ProductVersion", product),
        ("Comments", "https://github.com/redn1ghtz"),
    ]
    .map(|(key, value)| version_string(key, value));
    let table = version_block("040904B0", true, &[], &strings);
    let string_info = version_block("StringFileInfo", true, &[], &[table]);
    let translation = version_block("Translation", false, &0x04b00409u32.to_le_bytes(), &[]);
    let var_info = version_block("VarFileInfo", true, &[], &[translation]);
    version_block("VS_VERSION_INFO", false, &fixed, &[string_info, var_info])
}

fn resource(out: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
    dword(out, data.len() as u32);
    dword(out, 32);
    for value in [0xffff, kind, 0xffff, id] {
        word(out, value);
    }
    dword(out, 0);
    word(out, 0);
    word(out, 0);
    dword(out, 0);
    dword(out, 0);
    out.extend(data);
    out.resize(out.len().next_multiple_of(4), 0);
}

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let mut out = Vec::new();
    let sizes = [16usize, 20, 24, 32, 40, 48, 64, 96, 128, 256];
    let mut group = vec![0, 0, 1, 0];
    word(&mut group, sizes.len() as u16);
    resource(&mut out, 0, 0, &[]);
    for (index, size) in sizes.into_iter().enumerate() {
        let id = (index + 1) as u16;
        let pixels = icon::pixels(size);
        let mask_stride = size.div_ceil(32) * 4;
        let mut bitmap = Vec::new();
        dword(&mut bitmap, 40);
        dword(&mut bitmap, size as u32);
        dword(&mut bitmap, (size * 2) as u32);
        word(&mut bitmap, 1);
        word(&mut bitmap, 32);
        bitmap.resize(40, 0);
        let mut mask = vec![0; mask_stride * size];
        for (y, row) in pixels.chunks_exact(size * 4).rev().enumerate() {
            bitmap.extend_from_slice(row);
            for (x, pixel) in row.chunks_exact(4).enumerate() {
                if pixel[3] == 0 {
                    mask[y * mask_stride + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        bitmap.extend(mask);
        group.extend([size as u8, size as u8, 0, 0]);
        word(&mut group, 1);
        word(&mut group, 32);
        dword(&mut group, bitmap.len() as u32);
        word(&mut group, id);
        resource(&mut out, 3, id, &bitmap);
    }
    resource(&mut out, 14, 1, &group);
    resource(&mut out, 16, 1, &version_resource());
    resource(&mut out, 24, 1, MANIFEST.as_bytes());
    let path = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("rustnut.res");
    fs::write(&path, out).unwrap();
    println!("cargo:rustc-link-arg=/MANIFEST:NO");
    println!("cargo:rustc-link-arg={}", path.display());
}
