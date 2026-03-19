use std::process::Command;
use std::thread;
use std::time::Duration;

pub fn play_notification_sound(sound_id: Option<&str>) {
    let sound_id = sound_id.unwrap_or("glass");
    if sound_id.eq_ignore_ascii_case("none") {
        return;
    }

    let pattern = notification_pattern(sound_id);
    thread::spawn(move || {
        if cfg!(target_os = "windows") {
            let script = pattern
                .iter()
                .map(|(frequency, duration_ms)| {
                    format!("[console]::Beep({frequency}, {duration_ms})")
                })
                .collect::<Vec<_>>()
                .join("; ");
            let _ = Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .spawn();
        } else {
            for (_, duration_ms) in pattern {
                eprint!("\x07");
                thread::sleep(Duration::from_millis(duration_ms));
            }
        }
    });
}

fn notification_pattern(sound_id: &str) -> Vec<(u32, u64)> {
    match sound_id {
        "chord" => vec![(659, 90), (784, 90), (988, 120)],
        "glisten" => vec![(1047, 60), (1319, 80), (1568, 120)],
        "polite" => vec![(740, 70), (880, 90)],
        "calm" => vec![(523, 120), (659, 140)],
        "sharp" => vec![(1245, 70), (988, 70)],
        "jinja" => vec![(784, 70), (1175, 90), (1568, 110)],
        "cloud" => vec![(587, 110), (659, 110), (784, 130)],
        _ => vec![(988, 70), (1319, 90)],
    }
}
