use crate::assets::asset_path;
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
    asset_path(format!("sounds/{sanitized}.wav"))
}

fn play_wav_file(path: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let escaped = path.replace('\'', "''");
        let script =
            format!("$p = New-Object System.Media.SoundPlayer '{escaped}'; $p.PlaySync()");
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .output();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("afplay").arg(path).output();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = Command::new("aplay").arg("-q").arg(path).output();
    }
}
