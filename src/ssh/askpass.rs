//! `SSH_ASKPASS` bridge that types a saved password into an interactive SSH
//! terminal, the way 0.4.1 did.
//!
//! The host launches `devmanager-host --ssh-askpass-exec <file> -- ssh …`.
//! That wrapper points `SSH_ASKPASS` back at this executable and execs ssh, so
//! the terminal's process is ssh itself. ssh then runs this executable for
//! every prompt: the first password prompt is answered from the saved file,
//! and everything else -- host-key confirmation, key passphrases, a retry after
//! a wrong password -- is asked on the terminal ssh is running in, exactly as
//! if no askpass were configured.
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub const EXEC_FLAG: &str = "--ssh-askpass-exec";
const FILE_ENV: &str = "DEVMANAGER_SSH_ASKPASS_FILE";

/// A saved password written for one terminal's askpass, readable only by this
/// user. It and its "already answered" marker are removed on drop, which the
/// host ties to the terminal's lifetime.
pub(crate) struct AskpassSecretFile {
    path: PathBuf,
}

impl AskpassSecretFile {
    pub(crate) fn write(dir: &Path, password: &str) -> std::io::Result<Self> {
        use std::io::Write;
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = dir.join(uuid::Uuid::new_v4().simple().to_string());
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let written = file
            .write_all(password.as_bytes())
            .and_then(|_| file.sync_all());
        let secret = Self { path };
        written.map(|_| secret)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for AskpassSecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(marker_for(&self.path));
    }
}

fn marker_for(path: &Path) -> PathBuf {
    let mut marker = path.as_os_str().to_owned();
    marker.push(".used");
    PathBuf::from(marker)
}

/// Claims the one automatic answer a terminal gets. A wrong saved password
/// must not be replayed into every retry, locking the user out of typing.
fn first_use(file: &Path) -> bool {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker_for(file))
        .is_ok()
}

/// Run this executable's askpass roles; `None` when neither applies.
pub fn run_subcommand(args: &[String]) -> Option<ExitCode> {
    if args.first().map(String::as_str) == Some(EXEC_FLAG) {
        return Some(exec_ssh(&args[1..]));
    }
    let file = std::env::var_os(FILE_ENV)?;
    if args.len() > 1 {
        return None;
    }
    Some(answer(
        Path::new(&file),
        args.first().map(String::as_str).unwrap_or(""),
    ))
}

#[cfg(unix)]
fn exec_ssh(args: &[String]) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let [file, separator, program, rest @ ..] = args else {
        eprintln!("devmanager: malformed SSH launch");
        return ExitCode::FAILURE;
    };
    if separator != "--" {
        eprintln!("devmanager: malformed SSH launch");
        return ExitCode::FAILURE;
    }
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            eprintln!("devmanager: the DevManager host executable is unavailable: {error}");
            return ExitCode::FAILURE;
        }
    };
    let _ = std::fs::remove_file(marker_for(Path::new(file)));
    let error = std::process::Command::new(program)
        .args(rest)
        .env("SSH_ASKPASS", executable)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env(FILE_ENV, file)
        .exec();
    eprintln!("devmanager: could not start OpenSSH: {error}");
    ExitCode::FAILURE
}

#[cfg(not(unix))]
fn exec_ssh(_args: &[String]) -> ExitCode {
    eprintln!("devmanager: saved SSH passwords are not supported on this platform");
    ExitCode::FAILURE
}

fn wants_saved_password(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    lower.contains("password") && !lower.contains("passphrase")
}

fn is_secret_prompt(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    lower.contains("password") || lower.contains("passphrase") || lower.contains("pin")
}

fn answer(file: &Path, prompt: &str) -> ExitCode {
    if wants_saved_password(prompt) && first_use(file) {
        if let Ok(password) = std::fs::read(file).map(zeroize::Zeroizing::new) {
            if write_answer(&password) {
                return ExitCode::SUCCESS;
            }
        }
    }
    ask_terminal(prompt, is_secret_prompt(prompt))
}

fn write_answer(answer: &[u8]) -> bool {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    out.write_all(answer)
        .and_then(|_| out.write_all(b"\n"))
        .and_then(|_| out.flush())
        .is_ok()
}

#[cfg(unix)]
fn ask_terminal(prompt: &str, hidden: bool) -> ExitCode {
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::AsRawFd;
    let Ok(tty) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    else {
        return ExitCode::FAILURE;
    };
    let mut writer = &tty;
    let _ = writer.write_all(prompt.as_bytes());
    let _ = writer.flush();
    let fd = tty.as_raw_fd();
    let saved = if hidden { disable_echo(fd) } else { None };
    let mut line = zeroize::Zeroizing::new(String::new());
    let read = BufReader::new(&tty).read_line(&mut line);
    if let Some(saved) = saved {
        // SAFETY: restores the attributes read from this same open terminal.
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, &saved);
        }
        let _ = writer.write_all(b"\n");
    }
    match read {
        Ok(read) if read > 0 => {
            if write_answer(line.trim_end_matches(['\r', '\n']).as_bytes()) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        _ => ExitCode::FAILURE,
    }
}

#[cfg(unix)]
fn disable_echo(fd: std::os::fd::RawFd) -> Option<libc::termios> {
    // SAFETY: `termios` is plain data; tcgetattr fills it for an open fd.
    unsafe {
        let mut attributes: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut attributes) != 0 {
            return None;
        }
        let saved = attributes;
        attributes.c_lflag &= !libc::ECHO;
        if libc::tcsetattr(fd, libc::TCSANOW, &attributes) != 0 {
            return None;
        }
        Some(saved)
    }
}

#[cfg(not(unix))]
fn ask_terminal(_prompt: &str, _hidden: bool) -> ExitCode {
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_password_prompts_take_the_saved_password() {
        assert!(wants_saved_password("deploy@example.test's password: "));
        assert!(wants_saved_password("Password: "));
        assert!(!wants_saved_password("Enter passphrase for key '/k': "));
        assert!(!wants_saved_password(
            "Are you sure you want to continue connecting (yes/no/[fingerprint])? "
        ));
        assert!(is_secret_prompt("Enter passphrase for key '/k': "));
        assert!(!is_secret_prompt(
            "Are you sure you want to continue connecting? "
        ));
    }

    #[test]
    fn the_saved_password_answers_once_and_is_removed_with_the_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let secret = AskpassSecretFile::write(&dir.path().join("askpass"), "hunter2").unwrap();
        let path = secret.path().to_path_buf();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hunter2");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o077,
                0,
                "only this user can read the saved password"
            );
        }
        assert!(first_use(&path), "the first password prompt is answered");
        assert!(!first_use(&path), "a retry is left to the user");
        drop(secret);
        assert!(!path.exists());
        assert!(!marker_for(&path).exists());
    }

    #[test]
    fn unrelated_invocations_are_not_askpass() {
        assert!(run_subcommand(&["--foreground".into(), "x".into()]).is_none());
    }
}
