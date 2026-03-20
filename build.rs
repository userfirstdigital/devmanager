#[cfg(target_os = "windows")]
fn main() {
    use winresource::{VersionInfo, WindowsResource};

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let packed_version = pack_windows_version(&version);

    let mut res = WindowsResource::new();
    res.set_icon("packaging/icons/devmanager.ico")
        .set_language(0x0409)
        .set("ProductName", "DevManager")
        .set(
            "FileDescription",
            "Native GPUI workspace and terminal manager for projects, servers, AI sessions, and SSH.",
        )
        .set("CompanyName", "UserFirst")
        .set("LegalCopyright", "Copyright (c) UserFirst")
        .set("OriginalFilename", "devmanager.exe")
        .set("InternalName", "devmanager")
        .set("Comments", "DevManager desktop application")
        .set("ProductVersion", &version)
        .set("FileVersion", &version)
        .set_version_info(VersionInfo::PRODUCTVERSION, packed_version)
        .set_version_info(VersionInfo::FILEVERSION, packed_version);
    res.compile().expect("failed to compile windows resources");
}

#[cfg(not(target_os = "windows"))]
fn main() {}

#[cfg(target_os = "windows")]
fn pack_windows_version(version: &str) -> u64 {
    let mut parts = [0u16; 4];
    let normalized = version
        .split_once('-')
        .map(|(base, _)| base)
        .unwrap_or(version)
        .split_once('+')
        .map(|(base, _)| base)
        .unwrap_or(version);

    for (index, part) in normalized.split('.').take(4).enumerate() {
        parts[index] = part.parse::<u16>().unwrap_or(0);
    }

    ((parts[0] as u64) << 48)
        | ((parts[1] as u64) << 32)
        | ((parts[2] as u64) << 16)
        | (parts[3] as u64)
}
