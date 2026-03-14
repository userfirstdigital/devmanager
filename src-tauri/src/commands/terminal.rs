#[tauri::command]
pub fn open_terminal(folder_path: String) -> Result<(), String> {
    // Quote the folder path to handle spaces
    let quoted_path = format!("\"{}\"", folder_path);

    // Try Windows Terminal first
    let wt_result = std::process::Command::new("cmd")
        .args(["/C", "start", "wt", "-d", &quoted_path])
        .output();

    match wt_result {
        Ok(output) if output.status.success() => Ok(()),
        _ => {
            // Fall back to cmd.exe
            std::process::Command::new("cmd")
                .args(["/C", "start", "cmd", "/K", &format!("cd /d {}", quoted_path)])
                .output()
                .map_err(|e| format!("Failed to open terminal: {}", e))?;
            Ok(())
        }
    }
}
