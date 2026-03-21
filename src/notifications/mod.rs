use crate::assets::asset_path;
use std::path::PathBuf;
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

    thread::spawn(move || {
        play_wav_file(&sound_path);
    });
}

fn sound_file_path(sound_id: &str) -> PathBuf {
    let sanitized: String = sound_id
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    asset_path(format!("sounds/{sanitized}.wav"))
}

fn play_wav_file(path: &PathBuf) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        #[link(name = "winmm")]
        unsafe extern "system" {
            fn PlaySoundW(psz_sound: *const u16, hmod: *const (), fdw_sound: u32) -> i32;
        }

        const SND_FILENAME: u32 = 0x0002_0000;
        const SND_NODEFAULT: u32 = 0x0000_0002;
        const SND_SYNC: u32 = 0x0000_0000;

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            PlaySoundW(wide.as_ptr(), std::ptr::null(), SND_FILENAME | SND_NODEFAULT | SND_SYNC);
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let _ = Command::new("afplay").arg(path).output();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::process::Command;
        let _ = Command::new("aplay").arg("-q").arg(path).output();
    }
}
