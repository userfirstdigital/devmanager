#[cfg(target_os = "windows")]
fn main() {
    validate_web_bundle();
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
fn main() {
    validate_web_bundle();
}

const WEB_BUNDLE_RECOVERY: &str = "npm --prefix web ci && npm --prefix web run build";
const REQUIRED_ICONS: [&str; 4] = [
    "icons/devmanager-180.png",
    "icons/devmanager-192.png",
    "icons/devmanager-512.png",
    "icons/devmanager-maskable-512.png",
];
const FINGERPRINT_FILES: [&str; 6] = [
    "index.html",
    "package.json",
    "package-lock.json",
    "tsconfig.json",
    "tsconfig.node.json",
    "vite.config.ts",
];

fn validate_web_bundle() {
    let web_dir = std::path::Path::new("web");
    if !web_dir.exists() {
        println!("cargo:rustc-env=DEVMANAGER_WEB_BUILD_ID=unavailable");
        return;
    }

    for path in FINGERPRINT_FILES {
        println!("cargo:rerun-if-changed=web/{path}");
    }
    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-changed=web/public");
    println!("cargo:rerun-if-changed=web/config");
    println!("cargo:rerun-if-changed=web/bundle");
    emit_bundle_rerun_directives(&web_dir.join("bundle"));

    if let Err(errors) = validate_web_bundle_at(web_dir) {
        panic!(
            "Embedded web bundle validation failed:\n- {}\nRecovery command: {}",
            errors.join("\n- "),
            WEB_BUNDLE_RECOVERY
        );
    }

    let fingerprint = std::fs::read_to_string(web_dir.join("bundle/source-fingerprint.txt"))
        .expect("validated web bundle fingerprint must remain readable");
    println!(
        "cargo:rustc-env=DEVMANAGER_WEB_BUILD_ID={}",
        fingerprint.trim()
    );
}

fn emit_bundle_rerun_directives(bundle: &std::path::Path) {
    let mut files = Vec::new();
    if collect_source_files(bundle, bundle, &mut files).is_ok() {
        for file in files {
            println!("cargo:rerun-if-changed=web/bundle/{file}");
        }
    }
}

fn validate_web_bundle_at(web_dir: &std::path::Path) -> Result<(), Vec<String>> {
    let bundle = web_dir.join("bundle");
    let mut errors = Vec::new();
    for path in [
        "index.html",
        "manifest.webmanifest",
        "sw.js",
        "source-fingerprint.txt",
    ]
    .into_iter()
    .chain(REQUIRED_ICONS)
    {
        if !bundle.join(path).is_file() {
            errors.push(format!("missing web/bundle/{path}"));
        }
    }

    let index = read_required_text(
        &bundle.join("index.html"),
        "web/bundle/index.html",
        &mut errors,
    );
    let manifest = read_required_text(
        &bundle.join("manifest.webmanifest"),
        "web/bundle/manifest.webmanifest",
        &mut errors,
    );
    let worker = read_required_text(&bundle.join("sw.js"), "web/bundle/sw.js", &mut errors);
    let fingerprint = read_required_text(
        &bundle.join("source-fingerprint.txt"),
        "web/bundle/source-fingerprint.txt",
        &mut errors,
    )
    .trim()
    .to_string();

    if !fingerprint_is_valid(&fingerprint) {
        errors.push(
            "web/bundle/source-fingerprint.txt must contain 16 lowercase hex characters"
                .to_string(),
        );
    }

    match compute_source_fingerprint(web_dir) {
        Ok(expected) if fingerprint != expected => errors.push(format!(
            "source fingerprint is stale: bundle has {fingerprint}, sources require {expected}"
        )),
        Err(error) => errors.push(error),
        _ => {}
    }

    let references = local_html_references(&index);
    if !references.iter().any(|path| path == "manifest.webmanifest") {
        errors.push("web/bundle/index.html does not reference manifest.webmanifest".to_string());
    }

    let mut hashed_assets = Vec::new();
    for reference in &references {
        if !safe_bundle_reference(reference) {
            errors.push(format!(
                "unsafe local reference in web/bundle/index.html: {reference}"
            ));
            continue;
        }
        if !bundle.join(reference).is_file() {
            errors.push(format!(
                "web/bundle/index.html references missing {reference}"
            ));
        }
        if reference.starts_with("assets/") {
            if !is_hashed_asset(reference) {
                errors.push(format!(
                    "web/bundle/index.html references unhashed asset {reference}"
                ));
            }
            hashed_assets.push(reference.clone());
        }
    }
    if hashed_assets.is_empty() {
        errors.push("web/bundle/index.html does not reference any hashed assets".to_string());
    }
    let referenced_assets = validate_asset_graph(&bundle, &hashed_assets, &mut errors);

    for needle in [
        "\"id\":\"/\"",
        "\"scope\":\"/\"",
        "\"start_url\":\"/sessions?source=pwa\"",
        "\"display\":\"standalone\"",
    ] {
        if !manifest.contains(needle) {
            errors.push(format!(
                "web/bundle/manifest.webmanifest is missing {needle}"
            ));
        }
    }
    if manifest.contains("\"orientation\"") {
        errors.push("web/bundle/manifest.webmanifest must not lock orientation".to_string());
    }
    for icon in REQUIRED_ICONS {
        if icon != "icons/devmanager-180.png" && !manifest.contains(icon) {
            errors.push(format!(
                "web/bundle/manifest.webmanifest does not reference {icon}"
            ));
        }
    }

    for reference in referenced_assets
        .iter()
        .map(String::as_str)
        .chain(std::iter::once("index.html"))
        .chain(std::iter::once("manifest.webmanifest"))
        .chain(REQUIRED_ICONS)
    {
        if !worker.contains(reference) {
            errors.push(format!("web/bundle/sw.js does not precache {reference}"));
        }
    }
    if worker.contains("source-fingerprint.txt") {
        errors.push("web/bundle/sw.js must not precache source-fingerprint.txt".to_string());
    }
    for marker in ["\"/api\"", "\"/api/\"", "\"/pair\""] {
        if !worker.contains(marker) {
            errors.push(format!(
                "web/bundle/sw.js is missing NetworkOnly policy marker {marker}"
            ));
        }
    }

    let fingerprint_meta = format!("name=\"devmanager-web-build\" content=\"{fingerprint}\"");
    if !index.contains(&fingerprint_meta) {
        errors.push(
            "web/bundle/index.html fingerprint does not match source-fingerprint.txt".to_string(),
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn read_required_text(path: &std::path::Path, label: &str, errors: &mut Vec<String>) -> String {
    match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) => {
            errors.push(format!("cannot read {label}: {error}"));
            String::new()
        }
    }
}

fn fingerprint_is_valid(fingerprint: &str) -> bool {
    fingerprint.len() == 16
        && fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn compute_source_fingerprint(web_dir: &std::path::Path) -> Result<String, String> {
    let mut files = Vec::new();
    for path in FINGERPRINT_FILES {
        let source = web_dir.join(path);
        if !source.is_file() {
            return Err(format!("missing web source file web/{path}"));
        }
        files.push(path.to_string());
    }
    for root in ["src", "public", "config"] {
        collect_source_files(web_dir, &web_dir.join(root), &mut files)?;
    }
    files.sort();
    files.dedup();

    let mut hash = 0xcbf29ce484222325_u64;
    for relative in files {
        update_fingerprint(&mut hash, relative.as_bytes());
        update_fingerprint(&mut hash, &[0]);
        let contents = std::fs::read(web_dir.join(&relative))
            .map_err(|error| format!("cannot read web/{relative}: {error}"))?;
        update_fingerprint(&mut hash, &contents);
        update_fingerprint(&mut hash, &[0]);
    }
    Ok(format!("{hash:016x}"))
}

fn collect_source_files(
    web_dir: &std::path::Path,
    directory: &std::path::Path,
    files: &mut Vec<String>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(directory)
        .map_err(|error| format!("cannot scan {}: {error}", directory.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("cannot scan {}: {error}", directory.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if file_type.is_dir() {
            collect_source_files(web_dir, &path, files)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(web_dir)
                .map_err(|error| format!("cannot relativize {}: {error}", path.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            files.push(relative);
        }
    }
    Ok(())
}

fn update_fingerprint(hash: &mut u64, bytes: &[u8]) {
    let mut index = 0;
    while index < bytes.len() {
        let byte = if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            index += 2;
            b'\n'
        } else {
            let byte = bytes[index];
            index += 1;
            byte
        };
        *hash ^= u64::from(byte);
        *hash = (*hash).wrapping_mul(0x100000001b3);
    }
}

fn local_html_references(html: &str) -> Vec<String> {
    let mut references = Vec::new();
    for (marker, quote) in [
        ("src=\"", '"'),
        ("href=\"", '"'),
        ("src='", '\''),
        ("href='", '\''),
    ] {
        let mut remaining = html;
        while let Some(start) = remaining.find(marker) {
            let value = &remaining[start + marker.len()..];
            let Some(end) = value.find(quote) else {
                break;
            };
            if let Some(reference) = normalize_local_reference(&value[..end]) {
                references.push(reference);
            }
            remaining = &value[end + 1..];
        }
    }
    references.sort();
    references.dedup();
    references
}

fn normalize_local_reference(reference: &str) -> Option<String> {
    if reference.is_empty()
        || reference.starts_with('#')
        || reference.starts_with("//")
        || reference.contains("://")
        || reference.starts_with("data:")
    {
        return None;
    }
    let without_fragment = reference.split(['?', '#']).next().unwrap_or("");
    let normalized = without_fragment.trim_start_matches('/');
    (!normalized.is_empty()).then(|| normalized.to_string())
}

fn safe_bundle_reference(reference: &str) -> bool {
    !reference.contains("..") && !reference.contains('\\') && !reference.contains(':')
}

fn validate_asset_graph(
    bundle: &std::path::Path,
    roots: &[String],
    errors: &mut Vec<String>,
) -> std::collections::BTreeSet<String> {
    let mut referenced = std::collections::BTreeSet::new();
    let mut pending = std::collections::VecDeque::from(roots.to_vec());

    while let Some(asset) = pending.pop_front() {
        if !referenced.insert(asset.clone()) {
            continue;
        }
        if !safe_bundle_reference(&asset) || !is_hashed_asset(&asset) {
            errors.push(format!(
                "generated asset reference is not a safe hashed path: {asset}"
            ));
            continue;
        }

        let path = bundle.join(&asset);
        if !path.is_file() {
            errors.push(format!("generated asset graph references missing {asset}"));
            continue;
        }
        if !asset.ends_with(".js") && !asset.ends_with(".css") {
            continue;
        }
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) => {
                errors.push(format!("cannot read generated asset {asset}: {error}"));
                continue;
            }
        };
        pending.extend(relative_asset_references(&asset, &contents));
    }

    let mut generated_assets = Vec::new();
    if let Err(error) = collect_source_files(bundle, &bundle.join("assets"), &mut generated_assets)
    {
        errors.push(error);
    }
    for asset in generated_assets {
        if !referenced.contains(&asset) {
            errors.push(format!(
                "web/bundle contains unreferenced generated asset {asset}"
            ));
        }
    }

    referenced
}

fn relative_asset_references(current: &str, contents: &str) -> Vec<String> {
    let parent = std::path::Path::new(current)
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""));
    let mut references = Vec::new();

    for (marker, quote) in [("\"./", '"'), ("'./", '\'')] {
        let mut remaining = contents;
        while let Some(start) = remaining.find(marker) {
            let value = &remaining[start + marker.len()..];
            let Some(end) = value.find(quote) else {
                break;
            };
            let target = value[..end].split(['?', '#']).next().unwrap_or("");
            if !target.is_empty() {
                let reference = parent.join(target).to_string_lossy().replace('\\', "/");
                if reference.starts_with("assets/") {
                    references.push(reference);
                }
            }
            remaining = &value[end + 1..];
        }
    }

    references.sort();
    references.dedup();
    references
}

fn is_hashed_asset(path: &str) -> bool {
    if !path.starts_with("assets/") {
        return false;
    }
    let filename = path.rsplit('/').next().unwrap_or(path);
    let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
    let bytes = stem.as_bytes();
    bytes.len() > 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'-')
}

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
