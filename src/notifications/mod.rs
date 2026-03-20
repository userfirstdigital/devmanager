use std::path::PathBuf;
use std::process::Command;
use std::thread;

pub fn play_notification_sound(sound_id: Option<&str>) {
    let sound_id = sound_id.unwrap_or("glass");
    if sound_id.eq_ignore_ascii_case("none") {
        return;
    }

    let sound_path = sound_file_path(sound_id);
    if !sound_path.exists() {
        eprintln!(
            "[notifications] sound file not found: {}",
            sound_path.display()
        );
        return;
    }

    let path_str = sound_path.to_string_lossy().to_string();
    thread::spawn(move || {
        play_wav_file(&path_str);
    });
}

fn sound_file_path(sound_id: &str) -> PathBuf {
    let sanitized: String = sound_id
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("sounds")
        .join(format!("{sanitized}.wav"))
}

fn play_wav_file(path: &str) {
    if cfg!(target_os = "windows") {
        let escaped = path.replace('\'', "''");
        let script = format!("$p = New-Object System.Media.SoundPlayer '{escaped}'; $p.PlaySync()");
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output();
    } else if cfg!(target_os = "macos") {
        let _ = Command::new("afplay").arg(path).output();
    } else {
        let _ = Command::new("aplay").arg("-q").arg(path).output();
    }
}
