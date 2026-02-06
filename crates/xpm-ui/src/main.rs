//! xPackageManager - A modern package manager for Arch Linux.

use slint::{Model, ModelRc, SharedString, VecModel, Timer, TimerMode};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use serde_json::Value;
use std::rc::Rc;
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;
use xpm_alpm::AlpmBackend;
use xpm_core::source::PackageSource;
use xpm_flatpak::FlatpakBackend;

slint::include_modules!();

/// Messages from backend threads to UI
enum UiMessage {
    PackagesLoaded {
        installed: Vec<PackageData>,
        updates: Vec<PackageData>,
        flatpak: Vec<PackageData>,
        firmware: Vec<PackageData>,
        stats: StatsData,
    },
    SearchResults(Vec<PackageData>),
    CategoryPackages(Vec<PackageData>),
    RepoPackages(Vec<PackageData>),
    SetLoading(bool),
    SetBusy(bool),
    SetStatus(String),
    SetProgress(i32),
    SetProgressText(String),
    ShowTerminal(String),
    TerminalOutput(String),
    TerminalDone(bool),
    HideTerminal,
    // Progress popup messages
    ShowProgressPopup(String),
    OperationProgress(i32, String),
    ProgressOutput(String),
    ProgressPrompt(String),
    ProgressHidePrompt,
    OperationDone(bool),
    ShowTerminalFallback(String),
}

/// Patterns that indicate routine pacman prompts (auto-answer "y")
/// These are safe default-yes [Y/n] prompts that always occur during normal operations.
const PACMAN_AUTO_CONFIRM_PATTERNS: &[&str] = &[
    "Proceed with installation? [Y/n]",
"Proceed with download? [Y/n]",
":: Proceed with installation? [Y/n]",
":: Proceed with download? [Y/n]",
"Do you want to remove these packages? [y/N]",
":: Do you want to remove these packages? [y/N]",
];

/// Patterns that indicate pacman is asking a question that requires user input.
/// These are non-routine prompts where the user should decide.
const PACMAN_USER_PROMPT_PATTERNS: &[&str] = &[
    ":: Replace",
":: Import",
"Enter a number",
"Enter a selection",
"Terminate batch job",
];

/// Patterns that indicate a conflict or error requiring user intervention
const CONFLICT_PATTERNS: &[&str] = &[
    "conflicting files",
"are in conflict",
"exists in filesystem",
"breaks dependency",
"could not satisfy dependencies",
"failed to commit transaction",
];

/// Check if the system is running XeroLinux
fn is_xerolinux_distro() -> bool {
    // Check ID and NAME fields in /etc/os-release
    if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
        for line in content.lines() {
            let line_lower = line.to_lowercase();
            if (line_lower.starts_with("id=") || line_lower.starts_with("name=") || line_lower.starts_with("pretty_name="))
                && line_lower.contains("xerolinux")
                {
                    return true;
                }
        }
    }
    // Fallback: check /etc/lsb-release
    if let Ok(content) = std::fs::read_to_string("/etc/lsb-release") {
        for line in content.lines() {
            let line_lower = line.to_lowercase();
            if (line_lower.starts_with("distrib_id=") || line_lower.starts_with("distrib_description="))
                && line_lower.contains("xerolinux")
                {
                    return true;
                }
        }
    }
    false
}

/// Check if a file path is a valid Arch Linux package
fn is_arch_package(path: &str) -> bool {
    let extensions = [".pkg.tar.zst", ".pkg.tar.xz", ".pkg.tar.gz", ".pkg.tar"];
    extensions.iter().any(|ext| path.ends_with(ext))
}

/// Extract package info from a local package file
fn get_local_package_info(path: &str) -> Option<PackageData> {
    let path_obj = Path::new(path);
    if !path_obj.exists() {
        return None;
    }

    let filename = path_obj.file_name()?.to_str()?;

    let base = filename
    .strip_suffix(".pkg.tar.zst")
    .or_else(|| filename.strip_suffix(".pkg.tar.xz"))
    .or_else(|| filename.strip_suffix(".pkg.tar.gz"))
    .or_else(|| filename.strip_suffix(".pkg.tar"))?;

    let parts: Vec<&str> = base.rsplitn(4, '-').collect();
    let (name, version) = if parts.len() >= 3 {
        let name = parts[3..].join("-");
        let version = format!("{}-{}", parts[2], parts[1]);
        (name, version)
    } else {
        (base.to_string(), "unknown".to_string())
    };

    let size = path_obj
    .metadata()
    .ok()
    .map(|m| format_size(m.len()))
    .unwrap_or_else(|| "Unknown".to_string());

    Some(PackageData {
        name: SharedString::from(&name),
         display_name: SharedString::from(&name),
         version: SharedString::from(&version),
         description: SharedString::from(format!("Local package: {}", filename)),
         repository: SharedString::from("local"),
         backend: 2,
         installed: false,
         has_update: false,
         installed_size: SharedString::from(&size),
         licenses: SharedString::from(""),
         url: SharedString::from(""),
         dependencies: SharedString::from(""),
         required_by: SharedString::from(""),
         icon_name: SharedString::from("package"),
         selected: false,
    })
}

/// Format bytes into human readable size
fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

/// Strip ANSI escape sequences for clean display.
/// Handles CSI sequences (ESC[...), OSC (ESC]...), and simple ESC+char sequences.
/// Normalizes PTY line endings (\r\n → \n). Bare \r (progress bars) is preserved.
fn strip_ansi(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == 0x1b {
            i += 1;
            if i >= len { break; }
            match bytes[i] {
                b'[' => {
                    // CSI sequence: ESC[ (params) (intermediates) final_byte
                    i += 1;
                    while i < len && bytes[i] >= 0x30 && bytes[i] <= 0x3F { i += 1; }
                    while i < len && bytes[i] >= 0x20 && bytes[i] <= 0x2F { i += 1; }
                    if i < len && bytes[i] >= 0x40 && bytes[i] <= 0x7E { i += 1; }
                }
                b']' => {
                    // OSC sequence: ESC] ... (BEL or ST)
                    i += 1;
                    while i < len {
                        if bytes[i] == 0x07 { i += 1; break; }
                        if bytes[i] == 0x1b && i + 1 < len && bytes[i + 1] == b'\\' {
                            i += 2; break;
                        }
                        i += 1;
                    }
                }
                b'(' | b')' | b'*' | b'+' => {
                    i += 1;
                    if i < len { i += 1; }
                }
                _ => {
                    i += 1;
                }
            }
        } else if bytes[i] == b'\r' && i + 1 < len && bytes[i + 1] == b'\n' {
            // \r\n is a PTY line ending — emit just \n
            result.push('\n');
            i += 2;
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

/// Simulate terminal carriage-return behavior on a text buffer.
/// `\r` resets the write position to the start of the current line (overwrite in-place).
/// `\n` commits the current line and starts a new one.
/// Only the last line can be affected, so the prefix before it is copied as-is.
fn apply_terminal_text(buffer: &str, new_text: &str) -> String {
    // Only the last line is affected by \r — keep everything before it unchanged
    let (prefix, current_line) = match buffer.rfind('\n') {
        Some(pos) => (&buffer[..pos + 1], &buffer[pos + 1..]),
        None => ("", buffer),
    };

    let mut line = current_line.to_string();
    let mut result = String::with_capacity(buffer.len() + new_text.len());
    result.push_str(prefix);

    for ch in new_text.chars() {
        match ch {
            '\n' => {
                result.push_str(&line);
                result.push('\n');
                line.clear();
            }
            '\r' => {
                line.clear();
            }
            c => {
                line.push(c);
            }
        }
    }

    result.push_str(&line);
    result
}

/// Spawn a child process in a PTY, returning (master_fd, child_pid)
fn spawn_in_pty(cmd: &str, args: &[&str]) -> Result<(i32, u32), String> {
    use std::os::unix::io::FromRawFd;

    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;

    // Create PTY pair
    let ret = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    if ret != 0 {
        return Err("openpty failed".to_string());
    }

    let child: Result<std::process::Child, std::io::Error> = unsafe {
        // Each Stdio::from_raw_fd takes ownership, so dup for each to avoid double-close
        let stdin_fd = libc::dup(slave);
        let stdout_fd = libc::dup(slave);
        let stderr_fd = libc::dup(slave);
        std::process::Command::new(cmd)
        .args(args)
        .env("TERM", "xterm-256color")
        .stdin(std::process::Stdio::from_raw_fd(stdin_fd))
        .stdout(std::process::Stdio::from_raw_fd(stdout_fd))
        .stderr(std::process::Stdio::from_raw_fd(stderr_fd))
        .pre_exec(move || {
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY, 0);
            Ok(())
        })
        .spawn()
    };

    // Close original slave in parent (dup'd copies are owned by Stdio)
    unsafe { libc::close(slave); }

    match child {
        Ok(c) => Ok((master, c.id())),
        Err(e) => {
            unsafe { libc::close(master); }
            Err(format!("Failed to spawn {}: {}", cmd, e))
        }
    }
}

/// Run a command in the terminal popup with full PTY support
fn run_in_terminal(
    tx: &mpsc::Sender<UiMessage>,
    title: &str,
    cmd: &str,
    args: &[&str],
    input_sender: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pid_holder: &Arc<Mutex<Option<u32>>>,
) {
    let _ = tx.send(UiMessage::ShowTerminal(title.to_string()));

    let (master_fd, child_pid) = match spawn_in_pty(cmd, args) {
        Ok(pair) => pair,
        Err(e) => {
            let _ = tx.send(UiMessage::TerminalOutput(format!("Error: {}\n", e)));
            let _ = tx.send(UiMessage::TerminalDone(false));
            return;
        }
    };

    // Store child PID so cancel can kill it
    *pid_holder.lock().unwrap() = Some(child_pid);

    // Create input channel for this session
    let (in_tx, in_rx) = mpsc::channel::<String>();
    *input_sender.lock().unwrap() = Some(in_tx);

    // Reader thread: PTY master → UI
    let tx_reader = tx.clone();
    let master_fd_reader = master_fd;
    let reader_handle = thread::spawn(move || {
        use std::io::Read;
        let mut file = unsafe { std::fs::File::from_raw_fd(master_fd_reader) };
        let mut buf = [0u8; 4096];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                                      Ok(n) => {
                                          let text = String::from_utf8_lossy(&buf[..n]);
                                          let cleaned = strip_ansi(&text);
                                          if !cleaned.is_empty() {
                                              let _ = tx_reader.send(UiMessage::TerminalOutput(cleaned));
                                          }
                                      }
                                      Err(_) => break,
            }
        }
        // Prevent the File from closing master_fd - we'll handle it
        std::mem::forget(file);
    });

    // Writer thread: input channel → PTY master
    let master_fd_writer = master_fd;
    let writer_handle = thread::spawn(move || {
        use std::io::Write;
        use std::os::unix::io::FromRawFd;
        // Create a dup so we can write independently
        let dup_fd = unsafe { libc::dup(master_fd_writer) };
        if dup_fd < 0 {
            return;
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        while let Ok(input) = in_rx.recv() {
            let data = format!("{}\n", input);
            if file.write_all(data.as_bytes()).is_err() {
                break;
            }
            let _ = file.flush();
        }
    });

    // Wait for child process
    let status = unsafe {
        let mut wstatus: libc::c_int = 0;
        libc::waitpid(child_pid as libc::pid_t, &mut wstatus, 0);
        wstatus
    };

    let success = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;

    // Clean up
    *pid_holder.lock().unwrap() = None;
    *input_sender.lock().unwrap() = None;

    // Close master fd to unblock the reader thread
    unsafe { libc::close(master_fd); }

    let _ = reader_handle.join();
    let _ = writer_handle.join();

    let _ = tx.send(UiMessage::TerminalDone(success));
}

/// Build command and args for a package operation
fn build_pacman_command(action: &str, names: &[String], backend: i32) -> (String, Vec<String>) {
    match (action, backend) {
        ("install", 1) | ("bulk-install", 1) => {
            ("flatpak".to_string(), {
                let mut args = vec!["install".to_string(), "-y".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
        ("remove", 1) | ("bulk-remove", 1) => {
            ("flatpak".to_string(), {
                let mut args = vec!["uninstall".to_string(), "-y".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
        ("update", 1) => {
            ("flatpak".to_string(), {
                let mut args = vec!["update".to_string(), "-y".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
        ("remove", _) | ("bulk-remove", _) => {
            ("pkexec".to_string(), {
                let mut args = vec!["pacman".to_string(), "-R".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
        ("update-all", _) => {
            ("pkexec".to_string(), vec!["pacman".to_string(), "-Syu".to_string()])
        }
        _ => {
            // install, bulk-install, update for pacman
            ("pkexec".to_string(), {
                let mut args = vec!["pacman".to_string(), "-S".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
    }
}

/// Run a managed operation with progress tracking and auto-confirmation.
/// Falls back to full terminal on conflict/error.
fn run_managed_operation(
    tx: &mpsc::Sender<UiMessage>,
    title: &str,
    action: &str,
    names: &[String],
    backend: i32,
    input_sender: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pid_holder: &Arc<Mutex<Option<u32>>>,
) {
    let _ = tx.send(UiMessage::ShowProgressPopup(title.to_string()));

    let (cmd, args) = build_pacman_command(action, names, backend);
    let args_str: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let (master_fd, child_pid) = match spawn_in_pty(&cmd, &args_str) {
        Ok(pair) => pair,
        Err(e) => {
            let _ = tx.send(UiMessage::OperationProgress(0, format!("Error: {}", e)));
            let _ = tx.send(UiMessage::OperationDone(false));
            return;
        }
    };

    *pid_holder.lock().unwrap() = Some(child_pid);

    // Create input channel for auto-confirm
    let (in_tx, in_rx) = mpsc::channel::<String>();
    *input_sender.lock().unwrap() = Some(in_tx.clone());

    // Shared state for escalation
    let escalated = Arc::new(Mutex::new(false));
    let output_buffer = Arc::new(Mutex::new(String::new()));

    // Reader thread: parse output for progress/conflicts
    let tx_reader = tx.clone();
    let master_fd_reader = master_fd;
    let escalated_r = escalated.clone();
    let output_buffer_r = output_buffer.clone();
    let in_tx_r = in_tx;
    let total_packages = names.len().max(1);

    let reader_handle = thread::spawn(move || {
        use std::io::Read;
        let mut file = unsafe { std::fs::File::from_raw_fd(master_fd_reader) };
        let mut buf = [0u8; 4096];
        let mut current_percent: i32 = 0;

        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                                      Ok(n) => {
                                          let text = String::from_utf8_lossy(&buf[..n]);
                                          let cleaned = strip_ansi(&text);
                                          if cleaned.is_empty() {
                                              continue;
                                          }

                                          // Accumulate output (cap at 64KB to prevent memory issues with large updates)
                                          {
                                              let mut buf = output_buffer_r.lock().unwrap();
                                              if buf.len() < 65536 {
                                                  buf.push_str(&cleaned);
                                              }
                                          }

                                          let is_escalated = *escalated_r.lock().unwrap();

                                          if is_escalated {
                                              // Already escalated — forward to terminal
                                              let _ = tx_reader.send(UiMessage::TerminalOutput(cleaned));
                                          } else {
                                              // Send live output to the progress popup
                                              let _ = tx_reader.send(UiMessage::ProgressOutput(cleaned.clone()));

                                              // Check for conflicts/errors
                                              let lower = cleaned.to_lowercase();
                                              let has_conflict = CONFLICT_PATTERNS.iter().any(|p| lower.contains(&p.to_lowercase()));

                                              if has_conflict {
                                                  // Escalate to terminal
                                                  *escalated_r.lock().unwrap() = true;
                                                  let accumulated = output_buffer_r.lock().unwrap().clone();
                                                  let _ = tx_reader.send(UiMessage::ShowTerminalFallback(accumulated));
                                                  continue;
                                              }

                                              // Check for routine auto-confirm prompts
                                              let is_auto_confirm = PACMAN_AUTO_CONFIRM_PATTERNS.iter().any(|p| cleaned.contains(p));
                                              if is_auto_confirm {
                                                  let _ = in_tx_r.send("y".to_string());
                                                  let _ = tx_reader.send(UiMessage::ProgressHidePrompt);
                                              } else {
                                                  // Check for prompts that need user input
                                                  let needs_user_input = PACMAN_USER_PROMPT_PATTERNS.iter().any(|p| cleaned.contains(p))
                                                  || (cleaned.contains("[y/N]") && !is_auto_confirm);
                                                  if needs_user_input {
                                                      // Extract the prompt text (last non-empty line)
                                                      let prompt_text = cleaned.lines()
                                                      .filter(|l| !l.trim().is_empty())
                                                      .last()
                                                      .unwrap_or(&cleaned)
                                                      .trim()
                                                      .to_string();
                                                      let _ = tx_reader.send(UiMessage::ProgressPrompt(prompt_text));
                                                  }
                                              }

                                              // Stage detection for progress
                                              for line in cleaned.lines() {
                                                  let lower_line = line.to_lowercase();
                                                  let trimmed = line.trim().to_string();
                                                  let new_percent = if lower_line.contains("resolving dependencies") {
                                                      10
                                                  } else if lower_line.contains("looking for conflicting") {
                                                      15
                                                  } else if lower_line.contains("downloading") {
                                                      parse_progress_fraction(line, 20, 50, total_packages)
                                                      .unwrap_or(35)
                                                  } else if lower_line.contains("checking keyring") || lower_line.contains("checking integrity") {
                                                      52
                                                  } else if lower_line.contains("checking package integrity") {
                                                      55
                                                  } else if lower_line.contains("loading package files") {
                                                      58
                                                  } else if lower_line.contains("installing") || lower_line.contains("upgrading") || lower_line.contains("removing") || lower_line.contains("reinstalling") {
                                                      parse_progress_fraction(line, 60, 85, total_packages)
                                                      .unwrap_or(72)
                                                  } else if lower_line.contains("running post-transaction hooks") {
                                                      88
                                                  } else if lower_line.contains("arming conditionpathexists") || lower_line.contains("updating linux module") || lower_line.contains("dkms") {
                                                      90
                                                  } else if lower_line.contains("updating linux initcpios") || lower_line.contains("mkinitcpio") {
                                                      92
                                                  } else if lower_line.contains("updating grub") || lower_line.contains("grub-mkconfig") {
                                                      95
                                                  } else if lower_line.contains("updating the info") || lower_line.contains("updating the desktop") || lower_line.contains("updating mime") {
                                                      97
                                                  } else {
                                                      current_percent
                                                  };

                                                  if new_percent > current_percent {
                                                      current_percent = new_percent;
                                                      let _ = tx_reader.send(UiMessage::OperationProgress(current_percent, trimmed));
                                                  } else if new_percent == current_percent && current_percent >= 88 && !trimmed.is_empty() {
                                                      // During post-transaction hooks, keep updating the stage text
                                                      let _ = tx_reader.send(UiMessage::OperationProgress(current_percent, trimmed));
                                                  }
                                              }
                                          }
                                      }
                                      Err(_) => break,
            }
        }
        std::mem::forget(file);
    });

    // Writer thread
    let master_fd_writer = master_fd;
    let writer_handle = thread::spawn(move || {
        use std::io::Write;
        let dup_fd = unsafe { libc::dup(master_fd_writer) };
        if dup_fd < 0 {
            return;
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        while let Ok(input) = in_rx.recv() {
            let data = format!("{}\n", input);
            if file.write_all(data.as_bytes()).is_err() {
                break;
            }
            let _ = file.flush();
        }
    });

    // Wait for child
    let status = unsafe {
        let mut wstatus: libc::c_int = 0;
        libc::waitpid(child_pid as libc::pid_t, &mut wstatus, 0);
        wstatus
    };

    let success = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;

    *pid_holder.lock().unwrap() = None;
    *input_sender.lock().unwrap() = None;

    unsafe { libc::close(master_fd); }

    let _ = reader_handle.join();
    let _ = writer_handle.join();

    let was_escalated = *escalated.lock().unwrap();
    if was_escalated {
        // Already in terminal mode
        let _ = tx.send(UiMessage::TerminalDone(success));
    } else {
        let _ = tx.send(UiMessage::OperationDone(success));
    }
}

/// Parse "(X/Y)" fraction from a line and map it to a progress range
fn parse_progress_fraction(line: &str, range_start: i32, range_end: i32, _total_packages: usize) -> Option<i32> {
    // Look for pattern like "(1/5)" or "( 1/ 5)"
    if let Some(start) = line.find('(') {
        if let Some(end) = line[start..].find(')') {
            let inner = &line[start + 1..start + end];
            let parts: Vec<&str> = inner.split('/').collect();
            if parts.len() == 2 {
                if let (Ok(current), Ok(total)) = (
                    parts[0].trim().parse::<i32>(),
                                                   parts[1].trim().parse::<i32>(),
                ) {
                    if total > 0 {
                        let fraction = current as f64 / total as f64;
                        return Some(range_start + ((range_end - range_start) as f64 * fraction) as i32);
                    }
                }
            }
        }
    }
    None
}

/// Convert a Package to PackageData for the UI
fn package_to_ui(pkg: &xpm_core::package::Package, has_update: bool, desktop_map: &HashMap<String, String>) -> PackageData {
    let backend = match pkg.backend {
        xpm_core::package::PackageBackend::Pacman => 0,
        xpm_core::package::PackageBackend::Flatpak => 1,
    };

    let display_name = humanize_package_name(&pkg.name, desktop_map);

    PackageData {
        name: SharedString::from(pkg.name.as_str()),
        display_name: SharedString::from(&display_name),
        version: SharedString::from(pkg.version.to_string().as_str()),
        description: SharedString::from(pkg.description.as_str()),
        repository: SharedString::from(pkg.repository.as_str()),
        backend,
        installed: matches!(
            pkg.status,
            xpm_core::package::PackageStatus::Installed | xpm_core::package::PackageStatus::Orphan
        ),
        has_update,
        installed_size: SharedString::from(""),
        licenses: SharedString::from(""),
        url: SharedString::from(""),
        dependencies: SharedString::from(""),
        required_by: SharedString::from(""),
        icon_name: SharedString::from(""),
        selected: false,
    }
}

/// Convert UpdateInfo to PackageData for the UI
fn update_to_ui(update: &xpm_core::package::UpdateInfo) -> PackageData {
    let backend = match update.backend {
        xpm_core::package::PackageBackend::Pacman => 0,
        xpm_core::package::PackageBackend::Flatpak => 1,
    };

    let version_str = format!(
        "{} → {}",
        update.current_version.to_string(),
                              update.new_version.to_string()
    );

    PackageData {
        name: SharedString::from(update.name.as_str()),
        display_name: SharedString::from(update.name.as_str()),
        version: SharedString::from(version_str.as_str()),
        description: SharedString::from(version_str.as_str()),
        repository: SharedString::from(update.repository.as_str()),
        backend,
        installed: true,
        has_update: true,
        installed_size: SharedString::from(format_size(update.download_size).as_str()),
        licenses: SharedString::from(""),
        url: SharedString::from(""),
        dependencies: SharedString::from(""),
        required_by: SharedString::from(""),
        icon_name: SharedString::from(""),
        selected: false,
    }
}

/// Helper to update the `selected` field in a VecModel
fn update_selection_in_model(model: &ModelRc<PackageData>, name: &str, backend: i32, selected: bool) {
    let model = model.as_any().downcast_ref::<VecModel<PackageData>>();
    if let Some(vec_model) = model {
        for i in 0..vec_model.row_count() {
            if let Some(mut row) = vec_model.row_data(i) {
                if row.name.as_str() == name && row.backend == backend {
                    row.selected = selected;
                    vec_model.set_row_data(i, row);
                    break;
                }
            }
        }
    }
}

/// Look up whether a package is installed by searching all models
fn find_package_installed(window: &MainWindow, name: &str, backend: i32) -> bool {
    let models: Vec<ModelRc<PackageData>> = vec![
        window.get_installed_packages(),
        window.get_update_packages(),
        window.get_search_packages(),
        window.get_flatpak_packages(),
        window.get_firmware_packages(),
        window.get_category_packages(),
        window.get_repo_packages(),
    ];
    for model in &models {
        if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<PackageData>>() {
            for i in 0..vec_model.row_count() {
                if let Some(row) = vec_model.row_data(i) {
                    if row.name.as_str() == name && row.backend == backend {
                        return row.installed;
                    }
                }
            }
        }
    }
    false
}

/// Update selection state across all package models in the window
fn update_selection_in_models(window: &MainWindow, name: &str, backend: i32, selected: bool) {
    update_selection_in_model(&window.get_installed_packages(), name, backend, selected);
    update_selection_in_model(&window.get_update_packages(), name, backend, selected);
    update_selection_in_model(&window.get_search_packages(), name, backend, selected);
    update_selection_in_model(&window.get_flatpak_packages(), name, backend, selected);
    update_selection_in_model(&window.get_firmware_packages(), name, backend, selected);
    update_selection_in_model(&window.get_category_packages(), name, backend, selected);
    update_selection_in_model(&window.get_repo_packages(), name, backend, selected);
}

fn main() {
    // Initialize logging.
    let subscriber = FmtSubscriber::builder()
    .with_max_level(Level::INFO)
    .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set subscriber");

    info!("Starting xPackageManager");

    // Check for command line arguments (file to install)
    let args: Vec<String> = std::env::args().collect();
    let local_package_path = args.get(1).filter(|arg| is_arch_package(arg)).cloned();

    if let Some(ref path) = local_package_path {
        info!("Opening local package: {}", path);
    }

    // Check if running on XeroLinux
    if !is_xerolinux_distro() {
        let warning = DistroWarning::new().expect("Failed to create warning window");
        warning.on_dismiss(move || {
            std::process::exit(0);
        });
        warning.run().expect("Failed to run warning window");
        return; // Should not reach here, but just in case
    }

    // Create the main window
    let window = MainWindow::new().expect("Failed to create window");

    // Channel for UI updates from background threads
    let (tx, rx) = mpsc::channel::<UiMessage>();
    let rx = Rc::new(RefCell::new(rx));

    // Shared state for terminal PTY communication
    let terminal_input_sender: Arc<Mutex<Option<mpsc::Sender<String>>>> = Arc::new(Mutex::new(None));
    let terminal_child_pid: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

    // Selection tracking: Vec of (name, backend, installed)
    let selected_packages: Rc<RefCell<Vec<(String, i32, bool)>>> = Rc::new(RefCell::new(Vec::new()));

    // Create backend thread sender
    let tx_load = tx.clone();
    let tx_search = tx.clone();

    // Timer to poll for UI messages
    let timer = Timer::default();
    let window_weak = window.as_weak();
    let rx_clone = rx.clone();
    let tx_timer = tx.clone();
    // Terminal output state persisted across timer ticks
    let mut pending_terminal = String::new();
    let mut last_term_flush = std::time::Instant::now();

    timer.start(TimerMode::Repeated, std::time::Duration::from_millis(50), move || {
        if let Some(window) = window_weak.upgrade() {
            let mut flush_now = false;

            while let Ok(msg) = rx_clone.borrow_mut().try_recv() {
                match msg {
                    UiMessage::PackagesLoaded { installed, updates, flatpak, firmware, stats } => {
                        window.set_installed_packages(ModelRc::new(VecModel::from(installed)));
                        window.set_update_packages(ModelRc::new(VecModel::from(updates)));
                        window.set_flatpak_packages(ModelRc::new(VecModel::from(flatpak)));
                        window.set_firmware_packages(ModelRc::new(VecModel::from(firmware)));
                        window.set_stats(stats);
                        window.set_loading(false);
                    }
                    UiMessage::SearchResults(results) => {
                        window.set_search_packages(ModelRc::new(VecModel::from(results)));
                        window.set_loading(false);
                    }
                    UiMessage::CategoryPackages(packages) => {
                        window.set_category_packages(ModelRc::new(VecModel::from(packages)));
                        window.set_loading(false);
                    }
                    UiMessage::RepoPackages(packages) => {
                        window.set_repo_packages(ModelRc::new(VecModel::from(packages)));
                        window.set_loading(false);
                    }
                    UiMessage::SetLoading(loading) => {
                        window.set_loading(loading);
                    }
                    UiMessage::SetBusy(busy) => {
                        window.set_busy(busy);
                    }
                    UiMessage::SetStatus(status) => {
                        window.set_status_message(SharedString::from(&status));
                    }
                    UiMessage::SetProgress(value) => {
                        window.set_progress(value);
                    }
                    UiMessage::SetProgressText(text) => {
                        window.set_progress_text(SharedString::from(&text));
                    }
                    UiMessage::ShowTerminal(title) => {
                        window.set_terminal_title(SharedString::from(&title));
                        window.set_terminal_output(SharedString::from(""));
                        window.set_terminal_done(false);
                        window.set_terminal_success(false);
                        window.set_show_terminal(true);
                        pending_terminal.clear();
                    }
                    UiMessage::TerminalOutput(text) => {
                        pending_terminal.push_str(&text);
                    }
                    UiMessage::TerminalDone(success) => {
                        flush_now = true;
                        window.set_terminal_done(true);
                        window.set_terminal_success(success);
                        // Live-update all package lists after install/remove
                        if success {
                            let tx = tx_timer.clone();
                            let search_query = window.get_search_text().to_string();
                            let current_view = window.get_view();
                            let current_repo = window.get_current_repo_name().to_string();
                            thread::spawn(move || {
                                let rt = tokio::runtime::Runtime::new().expect("Runtime");
                                rt.block_on(async {
                                    load_packages_async(&tx, false).await;
                                    // Re-run search so Install/Remove buttons update
                                    if !search_query.is_empty() {
                                        search_packages_async(&tx, &search_query).await;
                                    }
                                });
                                // Re-load repo view if active
                                if current_view == 8 && !current_repo.is_empty() {
                                    load_repo_packages(&tx, &current_repo);
                                }
                            });
                        }
                    }
                    UiMessage::HideTerminal => {
                        window.set_show_terminal(false);
                        window.set_show_progress_popup(false);
                    }
                    UiMessage::ShowProgressPopup(title) => {
                        window.set_progress_popup_title(SharedString::from(&title));
                        window.set_progress_popup_percent(0);
                        window.set_progress_popup_stage(SharedString::from("Starting..."));
                        window.set_progress_popup_output(SharedString::from(""));
                        window.set_progress_popup_show_input(false);
                        window.set_progress_popup_prompt(SharedString::from(""));
                        window.set_progress_popup_done(false);
                        window.set_progress_popup_success(false);
                        window.set_show_progress_popup(true);
                        window.set_show_terminal(false);
                    }
                    UiMessage::ProgressOutput(text) => {
                        // Append to output, keeping last ~200 lines to prevent UI slowdown
                        let mut current = window.get_progress_popup_output().to_string();
                        current.push_str(&text);
                        // Trim to last 200 lines
                        let lines: Vec<&str> = current.lines().collect();
                        if lines.len() > 200 {
                            let trimmed: String = lines[lines.len() - 200..].join("\n");
                            window.set_progress_popup_output(SharedString::from(&trimmed));
                        } else {
                            window.set_progress_popup_output(SharedString::from(&current));
                        }
                    }
                    UiMessage::ProgressPrompt(prompt) => {
                        window.set_progress_popup_prompt(SharedString::from(&prompt));
                        window.set_progress_popup_show_input(true);
                    }
                    UiMessage::ProgressHidePrompt => {
                        window.set_progress_popup_show_input(false);
                        window.set_progress_popup_prompt(SharedString::from(""));
                    }
                    UiMessage::OperationProgress(percent, stage) => {
                        window.set_progress_popup_percent(percent);
                        window.set_progress_popup_stage(SharedString::from(&stage));
                    }
                    UiMessage::OperationDone(success) => {
                        window.set_progress_popup_percent(100);
                        window.set_progress_popup_done(true);
                        window.set_progress_popup_success(success);
                        window.set_progress_popup_show_input(false);
                        window.set_progress_popup_prompt(SharedString::from(""));
                        // Clear selection after successful operation
                        if success {
                            window.set_selected_count(0);
                        }
                        // Refresh package lists
                        if success {
                            let tx = tx_timer.clone();
                            let search_query = window.get_search_text().to_string();
                            let current_view = window.get_view();
                            let current_repo = window.get_current_repo_name().to_string();
                            thread::spawn(move || {
                                let rt = tokio::runtime::Runtime::new().expect("Runtime");
                                rt.block_on(async {
                                    load_packages_async(&tx, false).await;
                                    if !search_query.is_empty() {
                                        search_packages_async(&tx, &search_query).await;
                                    }
                                });
                                if current_view == 8 && !current_repo.is_empty() {
                                    load_repo_packages(&tx, &current_repo);
                                }
                            });
                        }
                    }
                    UiMessage::ShowTerminalFallback(accumulated) => {
                        // Switch from progress popup to terminal
                        window.set_show_progress_popup(false);
                        window.set_terminal_title(window.get_progress_popup_title());
                        window.set_terminal_output(SharedString::from(""));
                        window.set_terminal_done(false);
                        window.set_terminal_success(false);
                        window.set_show_terminal(true);
                        pending_terminal.clear();
                        pending_terminal.push_str(&accumulated);
                        flush_now = true;
                    }
                }
            }

            // Flush terminal output at most every 150ms (or immediately when done)
            if !pending_terminal.is_empty()
                && (flush_now || last_term_flush.elapsed() >= std::time::Duration::from_millis(150))
                {
                    // Normalize any \r\n split across PTY read boundaries
                    let text = pending_terminal.replace("\r\n", "\n");
                    pending_terminal.clear();
                    let current = window.get_terminal_output().to_string();
                    let combined = apply_terminal_text(&current, &text);
                    let trimmed = if combined.len() > 16384 {
                        combined[combined.len() - 16384..].to_string()
                    } else {
                        combined
                    };
                    window.set_terminal_output(SharedString::from(&trimmed));
                    last_term_flush = std::time::Instant::now();
                }
        }
    });

    // Initial load - spawn background thread
    let tx_initial = tx.clone();
    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
        rt.block_on(async {
            let _ = tx_initial.send(UiMessage::SetLoading(true));
            load_packages_async(&tx_initial, false).await;
        });
    });

    // Handle local package file if passed via command line
    if let Some(ref path) = local_package_path {
        if let Some(pkg_info) = get_local_package_info(path) {
            window.set_local_package(pkg_info);
            window.set_local_package_path(SharedString::from(path.as_str()));
            window.set_show_local_install(true);
            window.set_view(4);
        }
    }

    // Set up refresh callback
    window.on_refresh(move || {
        info!("Refresh requested");
        let tx = tx_load.clone();
        thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
            rt.block_on(async {
                let _ = tx.send(UiMessage::SetLoading(true));
                load_packages_async(&tx, false).await;
            });
        });
    });

    // Set up search callback
    window.on_search(move |query| {
        info!("Search: {}", query);
        let tx = tx_search.clone();
        let query = query.to_string();
        thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
            rt.block_on(async {
                let _ = tx.send(UiMessage::SetLoading(true));
                search_packages_async(&tx, &query).await;
            });
        });
    });

    // Load category callback
    let tx_category = tx.clone();
    window.on_load_category(move |category| {
        info!("Load category: {}", category);
        let tx = tx_category.clone();
        let category = category.to_string();
        thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
            rt.block_on(async {
                let _ = tx.send(UiMessage::SetLoading(true));
                load_category_packages(&tx, &category).await;
            });
        });
    });

    // Parse repos from pacman.conf and populate sidebar
    let repos = parse_pacman_repos();
    let repo_model: Vec<SharedString> = repos.iter().map(|r| SharedString::from(r.as_str())).collect();
    window.set_repos(ModelRc::new(VecModel::from(repo_model)));

    // Load repo packages callback
    let tx_repo = tx.clone();
    window.on_load_repo(move |repo| {
        info!("Load repo: {}", repo);
        let tx = tx_repo.clone();
        let repo = repo.to_string();
        thread::spawn(move || {
            let _ = tx.send(UiMessage::SetLoading(true));
            load_repo_packages(&tx, &repo);
        });
    });

    // Install package callback (called by confirm-operation)
    let tx_install = tx.clone();
    let install_input = terminal_input_sender.clone();
    let install_pid = terminal_child_pid.clone();
    window.on_install_package(move |name, backend| {
        info!("Install: {} (backend: {})", name, backend);
        let tx = tx_install.clone();
        let name = name.to_string();
        let input = install_input.clone();
        let pid = install_pid.clone();

        thread::spawn(move || {
            let title = format!("Installing {}", name);
            run_managed_operation(&tx, &title, "install", &[name], backend, &input, &pid);
        });
    });

    // Remove package callback (called by confirm-operation)
    let tx_remove = tx.clone();
    let remove_input = terminal_input_sender.clone();
    let remove_pid = terminal_child_pid.clone();
    window.on_remove_package(move |name, backend| {
        info!("Remove: {} (backend: {})", name, backend);
        let tx = tx_remove.clone();
        let name = name.to_string();
        let input = remove_input.clone();
        let pid = remove_pid.clone();

        thread::spawn(move || {
            let title = format!("Removing {}", name);
            run_managed_operation(&tx, &title, "remove", &[name], backend, &input, &pid);
        });
    });

    // Update package callback (called by confirm-operation)
    let tx_upd = tx.clone();
    let upd_input = terminal_input_sender.clone();
    let upd_pid = terminal_child_pid.clone();
    window.on_update_package(move |name, backend| {
        info!("Update: {} (backend: {})", name, backend);
        let tx = tx_upd.clone();
        let name = name.to_string();
        let input = upd_input.clone();
        let pid = upd_pid.clone();

        thread::spawn(move || {
            let title = format!("Updating {}", name);
            run_managed_operation(&tx, &title, "update", &[name], backend, &input, &pid);
        });
    });

    // Update all callback - route through managed operation
    let tx_update = tx.clone();
    let update_all_input = terminal_input_sender.clone();
    let update_all_pid = terminal_child_pid.clone();
    window.on_update_all(move || {
        info!("Update all packages");
        let tx = tx_update.clone();
        let input = update_all_input.clone();
        let pid = update_all_pid.clone();

        thread::spawn(move || {
            run_managed_operation(&tx, "System Update", "update-all", &[], 0, &input, &pid);
        });
    });

    // Request install — show confirmation popup
    let window_weak_ri = window.as_weak();
    window.on_request_install(move |name, backend| {
        if let Some(window) = window_weak_ri.upgrade() {
            window.set_confirm_title(SharedString::from("Install Package"));
            window.set_confirm_action(SharedString::from("install"));
            window.set_confirm_package_names(name.clone());
            window.set_confirm_version(SharedString::from(""));
            window.set_confirm_size(SharedString::from(""));
            window.set_confirm_deps(SharedString::from(""));
            window.set_confirm_backend(backend);
            window.set_confirm_package_count(1);
            window.set_show_confirm_popup(true);
        }
    });

    // Request remove — show confirmation popup
    let window_weak_rr = window.as_weak();
    window.on_request_remove(move |name, backend| {
        if let Some(window) = window_weak_rr.upgrade() {
            window.set_confirm_title(SharedString::from("Remove Package"));
            window.set_confirm_action(SharedString::from("remove"));
            window.set_confirm_package_names(name.clone());
            window.set_confirm_version(SharedString::from(""));
            window.set_confirm_size(SharedString::from(""));
            window.set_confirm_deps(SharedString::from(""));
            window.set_confirm_backend(backend);
            window.set_confirm_package_count(1);
            window.set_show_confirm_popup(true);
        }
    });

    // Request update — show confirmation popup
    let window_weak_ru = window.as_weak();
    window.on_request_update(move |name, backend| {
        if let Some(window) = window_weak_ru.upgrade() {
            window.set_confirm_title(SharedString::from("Update Package"));
            window.set_confirm_action(SharedString::from("update"));
            window.set_confirm_package_names(name.clone());
            window.set_confirm_version(SharedString::from(""));
            window.set_confirm_size(SharedString::from(""));
            window.set_confirm_deps(SharedString::from(""));
            window.set_confirm_backend(backend);
            window.set_confirm_package_count(1);
            window.set_show_confirm_popup(true);
        }
    });

    // Confirm operation — dispatch to actual operation via run_managed_operation
    let tx_confirm = tx.clone();
    let confirm_input = terminal_input_sender.clone();
    let confirm_pid = terminal_child_pid.clone();
    let window_weak_co = window.as_weak();
    window.on_confirm_operation(move || {
        if let Some(window) = window_weak_co.upgrade() {
            let action = window.get_confirm_action().to_string();
            let names_str = window.get_confirm_package_names().to_string();
            let backend = window.get_confirm_backend();
            window.set_show_confirm_popup(false);

            let name_list: Vec<String> = names_str.split('\n').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
            let tx = tx_confirm.clone();
            let input = confirm_input.clone();
            let pid = confirm_pid.clone();

            match action.as_str() {
                "install" | "bulk-install" => {
                    let title = if name_list.len() > 1 {
                        format!("Installing {} packages", name_list.len())
                    } else {
                        format!("Installing {}", name_list.first().map(|s| s.as_str()).unwrap_or(""))
                    };
                    thread::spawn(move || {
                        run_managed_operation(&tx, &title, "install", &name_list, backend, &input, &pid);
                    });
                }
                "remove" | "bulk-remove" => {
                    let title = if name_list.len() > 1 {
                        format!("Removing {} packages", name_list.len())
                    } else {
                        format!("Removing {}", name_list.first().map(|s| s.as_str()).unwrap_or(""))
                    };
                    thread::spawn(move || {
                        run_managed_operation(&tx, &title, "remove", &name_list, backend, &input, &pid);
                    });
                }
                "update" => {
                    let title = format!("Updating {}", names_str);
                    thread::spawn(move || {
                        run_managed_operation(&tx, &title, "update", &name_list, backend, &input, &pid);
                    });
                }
                "update-all" => {
                    thread::spawn(move || {
                        run_managed_operation(&tx, "System Update", "update-all", &[], 0, &input, &pid);
                    });
                }
                _ => {}
            }
        }
    });

    // Cancel confirm — hide popup
    let window_weak_cc = window.as_weak();
    window.on_cancel_confirm(move || {
        if let Some(window) = window_weak_cc.upgrade() {
            window.set_show_confirm_popup(false);
        }
    });

    // Close progress popup
    let window_weak_cp = window.as_weak();
    window.on_close_progress_popup(move || {
        if let Some(window) = window_weak_cp.upgrade() {
            window.set_show_progress_popup(false);
        }
    });

    // Send user input from progress popup to the PTY
    let progress_input = terminal_input_sender.clone();
    let window_weak_pp = window.as_weak();
    window.on_progress_popup_send_input(move |text| {
        let text_str = text.to_string();
        if let Some(sender) = progress_input.lock().unwrap().as_ref() {
            let _ = sender.send(text_str);
        }
        if let Some(window) = window_weak_pp.upgrade() {
            window.set_progress_popup_show_input(false);
            window.set_progress_popup_prompt(SharedString::from(""));
        }
    });

    // Toggle package selection
    let selected_pkgs_toggle = selected_packages.clone();
    let window_weak_tps = window.as_weak();
    window.on_toggle_package_selected(move |name, backend, selected| {
        let name_str = name.to_string();
        let mut sel = selected_pkgs_toggle.borrow_mut();

        if let Some(window) = window_weak_tps.upgrade() {
            // Look up installed state from models
            let is_installed = find_package_installed(&window, &name_str, backend);

            if selected {
                if !sel.iter().any(|(n, b, _)| n == &name_str && *b == backend) {
                    sel.push((name_str.clone(), backend, is_installed));
                }
            } else {
                sel.retain(|(n, b, _)| !(n == &name_str && *b == backend));
            }

            window.set_selected_count(sel.len() as i32);
            let installed_count = sel.iter().filter(|(_, _, inst)| *inst).count() as i32;
            window.set_selected_installed_count(installed_count);
            window.set_selected_uninstalled_count(sel.len() as i32 - installed_count);
            update_selection_in_models(&window, &name_str, backend, selected);
        }
    });

    // Clear selection
    let selected_pkgs_clear = selected_packages.clone();
    let window_weak_cs = window.as_weak();
    window.on_clear_selection(move || {
        let mut sel = selected_pkgs_clear.borrow_mut();
        let old_sel: Vec<(String, i32, bool)> = sel.drain(..).collect();
        if let Some(window) = window_weak_cs.upgrade() {
            window.set_selected_count(0);
            window.set_selected_installed_count(0);
            window.set_selected_uninstalled_count(0);
            for (name, backend, _) in &old_sel {
                update_selection_in_models(&window, name, *backend, false);
            }
        }
    });

    // Bulk install — collect only uninstalled packages, show confirm
    let selected_pkgs_bi = selected_packages.clone();
    let window_weak_bi = window.as_weak();
    window.on_bulk_install(move || {
        let sel = selected_pkgs_bi.borrow();
        // Only include uninstalled packages
        let uninstalled: Vec<&(String, i32, bool)> = sel.iter().filter(|(_, _, inst)| !inst).collect();
        if uninstalled.is_empty() { return; }
        let names: Vec<String> = uninstalled.iter().map(|(n, _, _)| n.clone()).collect();
        let backend = uninstalled[0].1;
        let names_str = names.join("\n");

        if let Some(window) = window_weak_bi.upgrade() {
            window.set_confirm_title(SharedString::from("Install Selected Packages"));
            window.set_confirm_action(SharedString::from("bulk-install"));
            window.set_confirm_package_names(SharedString::from(&names_str));
            window.set_confirm_version(SharedString::from(""));
            window.set_confirm_size(SharedString::from(""));
            window.set_confirm_deps(SharedString::from(""));
            window.set_confirm_backend(backend);
            window.set_confirm_package_count(names.len() as i32);
            window.set_show_confirm_popup(true);
        }
    });

    // Bulk remove — collect only installed packages, show confirm
    let selected_pkgs_br = selected_packages.clone();
    let window_weak_br = window.as_weak();
    window.on_bulk_remove(move || {
        let sel = selected_pkgs_br.borrow();
        // Only include installed packages
        let installed: Vec<&(String, i32, bool)> = sel.iter().filter(|(_, _, inst)| *inst).collect();
        if installed.is_empty() { return; }
        let names: Vec<String> = installed.iter().map(|(n, _, _)| n.clone()).collect();
        let backend = installed[0].1;
        let names_str = names.join("\n");

        if let Some(window) = window_weak_br.upgrade() {
            window.set_confirm_title(SharedString::from("Remove Selected Packages"));
            window.set_confirm_action(SharedString::from("bulk-remove"));
            window.set_confirm_package_names(SharedString::from(&names_str));
            window.set_confirm_version(SharedString::from(""));
            window.set_confirm_size(SharedString::from(""));
            window.set_confirm_deps(SharedString::from(""));
            window.set_confirm_backend(backend);
            window.set_confirm_package_count(names.len() as i32);
            window.set_show_confirm_popup(true);
        }
    });

    // Check for updates callback - syncs databases and checks all update sources
    let tx_sync = tx.clone();
    window.on_sync_databases(move || {
        info!("Check for updates");
        let tx = tx_sync.clone();
        thread::spawn(move || {
            let _ = tx.send(UiMessage::SetBusy(true));
            let _ = tx.send(UiMessage::SetProgress(5));
            let _ = tx.send(UiMessage::SetProgressText("Syncing pacman databases...".to_string()));
            let _ = tx.send(UiMessage::SetStatus("Syncing pacman databases...".to_string()));

            // Step 1: Sync pacman databases via polkit
            let pacman_ok = match std::process::Command::new("pkexec")
            .args(["pacman", "-Syy"])
            .output()
            {
                Ok(r) if r.status.success() => {
                    let _ = tx.send(UiMessage::SetProgress(25));
                    let _ = tx.send(UiMessage::SetProgressText("Pacman synced. Checking Flatpak...".to_string()));
                    let _ = tx.send(UiMessage::SetStatus("Pacman synced. Checking Flatpak...".to_string()));
                    true
                }
                Ok(r) => {
                    let stderr = String::from_utf8_lossy(&r.stderr);
                    if stderr.contains("cancelled") || stderr.contains("dismissed")
                        || r.status.code() == Some(126) || r.status.code() == Some(127)
                        {
                            let _ = tx.send(UiMessage::SetStatus("Authentication cancelled".to_string()));
                            let _ = tx.send(UiMessage::SetProgress(0));
                            let _ = tx.send(UiMessage::SetProgressText("".to_string()));
                            let _ = tx.send(UiMessage::SetBusy(false));
                            return;
                        }
                        let _ = tx.send(UiMessage::SetProgress(25));
                    let _ = tx.send(UiMessage::SetProgressText("Pacman sync had issues, continuing...".to_string()));
                    let _ = tx.send(UiMessage::SetStatus("Pacman sync had issues, continuing...".to_string()));
                    false
                }
                Err(_) => {
                    let _ = tx.send(UiMessage::SetProgress(25));
                    let _ = tx.send(UiMessage::SetProgressText("Pacman sync unavailable, continuing...".to_string()));
                    let _ = tx.send(UiMessage::SetStatus("Pacman sync unavailable, continuing...".to_string()));
                    false
                }
            };

            // Step 2: Refresh Flatpak appstream data
            let _ = tx.send(UiMessage::SetProgress(50));
            let _ = tx.send(UiMessage::SetProgressText("Refreshing Flatpak metadata...".to_string()));
            let _ = tx.send(UiMessage::SetStatus("Refreshing Flatpak metadata...".to_string()));
            let _flatpak_ok = match std::process::Command::new("flatpak")
            .args(["update", "--appstream", "-y"])
            .output()
            {
                Ok(r) => r.status.success(),
                      Err(_) => false,
            };

            // Step 3: Refresh firmware metadata (with timeout - fwupdmgr can hang)
            let _ = tx.send(UiMessage::SetProgress(75));
            let _ = tx.send(UiMessage::SetProgressText("Checking firmware...".to_string()));
            let _ = tx.send(UiMessage::SetStatus("Checking firmware...".to_string()));
            let fw_child = std::process::Command::new("fwupdmgr")
            .args(["refresh", "--force"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
            if let Ok(mut child) = fw_child {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                      Ok(None) => {
                          if std::time::Instant::now() >= deadline {
                              let _ = child.kill();
                              let _ = child.wait();
                              break;
                          }
                          std::thread::sleep(std::time::Duration::from_millis(500));
                      }
                      Err(_) => break,
                    }
                }
            }

            // Step 4: Reload all package data to pick up new updates
            let _ = tx.send(UiMessage::SetProgressText("Reloading packages...".to_string()));
            let _ = tx.send(UiMessage::SetStatus("Checking for updates...".to_string()));
            let rt = tokio::runtime::Runtime::new().expect("Runtime");
            rt.block_on(async {
                let _ = tx.send(UiMessage::SetLoading(true));
                load_packages_async(&tx, true).await;
            });

            let _ = tx.send(UiMessage::SetProgress(100));
            let _ = tx.send(UiMessage::SetProgressText("Complete".to_string()));

            let status = if pacman_ok {
                "Update check complete".to_string()
            } else {
                "Update check complete (pacman sync had issues)".to_string()
            };
            let _ = tx.send(UiMessage::SetProgress(0));
            let _ = tx.send(UiMessage::SetProgressText("".to_string()));
            let _ = tx.send(UiMessage::SetBusy(false));
            let _ = tx.send(UiMessage::SetStatus(status));
        });
    });

    // Open URL callback
    window.on_open_url(move |url| {
        info!("Open URL: {}", url);
        let _ = open::that(url.as_str());
    });

    // Install local package callback - use pkexec
    let tx_local = tx.clone();
    let local_input = terminal_input_sender.clone();
    let local_pid = terminal_child_pid.clone();
    let window_weak_local = window.as_weak();
    window.on_install_local_package(move |path| {
        info!("Install local package: {}", path);
        let tx = tx_local.clone();
        let path = path.to_string();
        let input = local_input.clone();
        let pid = local_pid.clone();

        if let Some(window) = window_weak_local.upgrade() {
            window.set_show_local_install(false);
        }

        thread::spawn(move || {
            let title = format!("Installing {}", path);
            run_in_terminal(&tx, &title, "pkexec", &["pacman", "-U", &path], &input, &pid);
        });
    });

    // Cancel local install callback
    let window_weak = window.as_weak();
    window.on_cancel_local_install(move || {
        info!("Cancelled local package install");
        if let Some(window) = window_weak.upgrade() {
            window.set_show_local_install(false);
            window.set_view(0);
        }
    });

    // Terminal send-input callback
    let term_input = terminal_input_sender.clone();
    window.on_terminal_send_input(move |text| {
        let text = text.to_string();
        if let Some(sender) = term_input.lock().unwrap().as_ref() {
            let _ = sender.send(text);
        }
    });

    // Terminal close callback — kills process if still running, then hides
    let tx_close = tx.clone();
    let close_pid = terminal_child_pid.clone();
    let close_input = terminal_input_sender.clone();
    // Troubleshoot: Update Mirrorlists
    let tx_mirrors = tx.clone();
    let mirror_input = terminal_input_sender.clone();
    let mirror_pid = terminal_child_pid.clone();
    window.on_update_mirrorlists(move || {
        info!("Troubleshoot: Update Mirrorlists");
        let tx = tx_mirrors.clone();
        let input = mirror_input.clone();
        let pid = mirror_pid.clone();
        thread::spawn(move || {
            let title = "Updating Mirrorlists".to_string();
            run_in_terminal(&tx, &title, "pkexec", &["bash", "-c",
                            "rate-mirrors --allow-root --protocol https arch | tee /etc/pacman.d/mirrorlist && rate-mirrors --allow-root --protocol https chaotic-aur | tee /etc/pacman.d/chaotic-mirrorlist"
            ], &input, &pid);
        });
    });

    // Troubleshoot: Fix GnuPG Keyring
    let tx_keyring = tx.clone();
    let keyring_input = terminal_input_sender.clone();
    let keyring_pid = terminal_child_pid.clone();
    window.on_fix_keyring(move || {
        info!("Troubleshoot: Fix GnuPG Keyring");
        let tx = tx_keyring.clone();
        let input = keyring_input.clone();
        let pid = keyring_pid.clone();
        thread::spawn(move || {
            let title = "Fixing GnuPG Keyring".to_string();
            run_in_terminal(&tx, &title, "pkexec", &["bash", "-c",
                            "rm -rf /etc/pacman.d/gnupg/* && pacman-key --init && pacman-key --populate && echo 'keyserver hkp://keyserver.ubuntu.com:80' | tee -a /etc/pacman.d/gnupg/gpg.conf && pacman -Syy --noconfirm archlinux-keyring"
            ], &input, &pid);
        });
    });

    window.on_terminal_close(move || {
        info!("Terminal close requested");
        // Kill child process if still running
        if let Some(pid) = *close_pid.lock().unwrap() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        // Drop the input sender to unblock writer thread
        *close_input.lock().unwrap() = None;

        let _ = tx_close.send(UiMessage::HideTerminal);
    });

    info!("Running application");
    window.run().expect("Failed to run application");
}

/// Load packages from backends (runs in background thread)
async fn load_packages_async(tx: &mpsc::Sender<UiMessage>, check_updates: bool) {
    // Initialize backends
    let alpm = match AlpmBackend::new() {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to initialize ALPM: {}", e);
            return;
        }
    };

    let flatpak = match FlatpakBackend::new() {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to initialize Flatpak: {}", e);
            return;
        }
    };

    // Run all data loading concurrently for fast startup
    let installed_fut = alpm.list_installed();
    let cache_fut = alpm.get_cache_size();
    let orphans_fut = alpm.list_orphans();
    let flatpak_avail_fut = flatpak.list_available();
    let desktop_map_fut = tokio::task::spawn_blocking(build_desktop_name_map);
    let flatpak_map_fut = tokio::task::spawn_blocking(build_flatpak_name_map);

    // Only check for updates when explicitly requested
    let flatpak_updates_fut = if check_updates { Some(flatpak.list_updates()) } else { None };
    let checkupdates_fut = if check_updates {
        Some(tokio::task::spawn_blocking(|| {
            std::process::Command::new("checkupdates")
            .output()
            .or_else(|_| std::process::Command::new("pacman").args(["-Qu"]).output())
        }))
    } else { None };
    let plasmoid_fut = if check_updates { Some(tokio::task::spawn_blocking(list_plasmoids_with_updates)) } else { None };
    let firmware_fut = if check_updates { Some(tokio::task::spawn_blocking(list_firmware)) } else { None };

    // Await base data
    let (
        installed_res,
         cache_res,
         orphans_res,
         flatpak_avail_res,
         desktop_map_res,
         flatpak_map_res,
    ) = tokio::join!(
        installed_fut,
        cache_fut,
        orphans_fut,
        flatpak_avail_fut,
        desktop_map_fut,
        flatpak_map_fut,
    );

    // Await update data only if requested
    let flatpak_updates = if let Some(fut) = flatpak_updates_fut {
        fut.await.unwrap_or_else(|e| { error!("Failed to list flatpak updates: {}", e); Vec::new() })
    } else { Vec::new() };
    let checkupdates_res = if let Some(fut) = checkupdates_fut { Some(fut.await) } else { None };
    let (_installed_plasmoids, plasmoid_updates) = if let Some(fut) = plasmoid_fut {
        fut.await.unwrap_or_else(|_| (Vec::new(), Vec::new()))
    } else { (Vec::new(), Vec::new()) };
    let firmware_packages: Vec<PackageData> = if let Some(fut) = firmware_fut {
        fut.await.unwrap_or_else(|_| Vec::new())
    } else { Vec::new() };

    // Process results
    let installed_pacman = installed_res.unwrap_or_else(|e| { error!("Failed to list installed: {}", e); Vec::new() });
    let cache_size = cache_res.unwrap_or(0);
    let orphan_count = orphans_res.map(|o| o.len()).unwrap_or(0);

    let flatpak_packages = flatpak_avail_res.unwrap_or_else(|e| { error!("Failed to list flatpak: {}", e); Vec::new() });

    let desktop_map = desktop_map_res.unwrap_or_default();
    let flatpak_name_map = flatpak_map_res.unwrap_or_default();

    // Parse checkupdates output
    let mut updates: Vec<xpm_core::package::UpdateInfo> = Vec::new();
    if let Some(Ok(Ok(result))) = checkupdates_res {
        if result.status.success() {
            let stdout = String::from_utf8_lossy(&result.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 {
                    updates.push(xpm_core::package::UpdateInfo {
                        name: parts[0].to_string(),
                                 current_version: xpm_core::package::Version::new(parts[1]),
                                 new_version: xpm_core::package::Version::new(parts[3]),
                                 backend: xpm_core::package::PackageBackend::Pacman,
                                 repository: String::new(),
                                 download_size: 0,
                    });
                } else if parts.len() >= 2 {
                    updates.push(xpm_core::package::UpdateInfo {
                        name: parts[0].to_string(),
                                 current_version: xpm_core::package::Version::new(""),
                                 new_version: xpm_core::package::Version::new(parts[1]),
                                 backend: xpm_core::package::PackageBackend::Pacman,
                                 repository: String::new(),
                                 download_size: 0,
                    });
                }
            }
        }
    }

    let update_names: std::collections::HashSet<String> =
    updates.iter().map(|u| u.name.clone()).collect();
    let flatpak_update_names: std::collections::HashSet<String> =
    flatpak_updates.iter().map(|u| u.name.clone()).collect();

    // Convert to UI types
    let installed_ui: Vec<PackageData> = installed_pacman
    .iter()
    .map(|p| package_to_ui(p, update_names.contains(&p.name), &desktop_map))
    .collect();

    let updates_ui: Vec<PackageData> = updates.iter().map(|u| update_to_ui(u)).collect();

    let flatpak_ui: Vec<PackageData> = flatpak_packages
    .iter()
    .map(|p| {
        let has_update = flatpak_update_names.contains(&p.name);
        // Try case-insensitive lookup
        let (display_name, summary) = flatpak_name_map
        .get(&p.name.to_lowercase())
        .cloned()
        .unwrap_or_else(|| {
            // Fallback: extract readable name from app ID
            let fallback_name = p.name
            .split('.')
            .last()
            .unwrap_or(&p.name)
            .replace('_', " ")
            .replace('-', " ");
            (fallback_name, String::new())
        });

        PackageData {
            name: SharedString::from(p.name.as_str()),
         display_name: SharedString::from(&display_name),
         version: SharedString::from(p.version.to_string().as_str()),
         description: SharedString::from(&summary),
         repository: SharedString::from(p.repository.as_str()),
         backend: 1, // Flatpak
         installed: matches!(
             p.status,
             xpm_core::package::PackageStatus::Installed | xpm_core::package::PackageStatus::Orphan
         ),
         has_update,
         installed_size: SharedString::from(""),
         licenses: SharedString::from(""),
         url: SharedString::from(""),
         dependencies: SharedString::from(""),
         required_by: SharedString::from(""),
         icon_name: SharedString::from(""),
         selected: false,
        }
    })
    .collect();

    // Combine all updates (pacman + flatpak + plasmoid + firmware with updates)
    let firmware_update_count = firmware_packages.iter().filter(|f| f.has_update).count();
    let total_updates = updates.len() + flatpak_updates.len() + plasmoid_updates.len() + firmware_update_count;

    // Merge plasmoid updates into updates_ui
    let mut all_updates_ui = updates_ui.clone();
    all_updates_ui.extend(plasmoid_updates.clone());
    // Add firmware updates
    all_updates_ui.extend(firmware_packages.iter().filter(|f| f.has_update).cloned());

    let stats = StatsData {
        pacman_count: installed_pacman.len() as i32,
        flatpak_count: flatpak_packages.len() as i32,
        orphan_count: orphan_count as i32,
        update_count: total_updates as i32,
        cache_size: SharedString::from(format_size(cache_size)),
    };

    let _ = tx.send(UiMessage::PackagesLoaded {
        installed: installed_ui,
        updates: all_updates_ui,
        flatpak: flatpak_ui,
        firmware: firmware_packages,
        stats,
    });
}

/// List installed plasmoids and return those with updates separately
fn list_plasmoids_with_updates() -> (Vec<PackageData>, Vec<PackageData>) {
    let mut plasmoids = Vec::new();
    let mut updates = Vec::new();

    let home = std::env::var("HOME").unwrap_or_default();
    let user_path = std::path::PathBuf::from(&home).join(".local/share/plasma/plasmoids");

    let paths = [
        Some(user_path),
        Some(std::path::PathBuf::from("/usr/share/plasma/plasmoids")),
    ];

    // Fetch KDE Store data once for version comparison
    let store_versions = fetch_store_versions();

    for path_opt in paths.iter().flatten() {
        if let Ok(entries) = std::fs::read_dir(path_opt) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let metadata_json = path.join("metadata.json");
                    let metadata_desktop = path.join("metadata.desktop");

                    let info = if metadata_json.exists() {
                        parse_plasmoid_json(&metadata_json)
                    } else if metadata_desktop.exists() {
                        parse_plasmoid_desktop(&metadata_desktop)
                    } else {
                        PlasmoidInfo {
                            id: entry.file_name().to_string_lossy().to_string(),
                            name: entry.file_name().to_string_lossy().to_string(),
                            version: "unknown".to_string(),
                            description: String::new(),
                        }
                    };

                    let is_user = path_opt.to_string_lossy().contains(".local");

                    // Check for updates from cached store data
                    let (has_update, new_version) = if is_user && !info.name.is_empty() {
                        if let Some((_, store_ver)) = store_versions.iter().find(|(name, _)| name == &info.name) {
                            let is_newer = version_is_newer(store_ver, &info.version);
                            (is_newer, if is_newer { store_ver.clone() } else { String::new() })
                        } else {
                            (false, String::new())
                        }
                    } else {
                        (false, String::new())
                    };

                    let pkg = PackageData {
                        name: SharedString::from(&info.id),
                        display_name: SharedString::from(&info.name),
                        version: SharedString::from(&info.version),
                        description: SharedString::from(&info.description),
                        repository: SharedString::from(if is_user { "kde-store" } else { "system" }),
                        backend: 3, // 3 = plasmoid
                        installed: true,
                        has_update,
                        installed_size: SharedString::from(""),
                        licenses: SharedString::from(""),
                        url: SharedString::from(format!("https://store.kde.org/search?search={}", info.name.replace(' ', "+"))),
                        dependencies: SharedString::from(""),
                        required_by: SharedString::from(""),
                        icon_name: SharedString::from(""),
                        selected: false,
                    };

                    if has_update {
                        let mut update_pkg = pkg.clone();
                        update_pkg.version = SharedString::from(format!("{} → {}", info.version, new_version));
                        updates.push(update_pkg);
                    }

                    plasmoids.push(pkg);
                }
            }
        }
    }

    (plasmoids, updates)
}

/// Fetch version info from KDE Store for installed plasmoids
fn fetch_store_versions() -> Vec<(String, String)> {
    let mut versions = Vec::new();

    // Use the KDE Store API - categories: 705 (Plasma Widgets), 715 (Wallpapers), 719 (KWin Effects), 720 (KWin Scripts)
    let url = "https://api.kde-look.org/ocs/v1/content/data?categories=705&pagesize=200&format=json";

    if let Ok(output) = std::process::Command::new("curl")
        .args(["-s", "--max-time", "15", url])
        .output()
        {
            if output.status.success() {
                let response = String::from_utf8_lossy(&output.stdout);
                if let Ok(json) = serde_json::from_str::<Value>(&response) {
                    if let Some(data) = json.get("ocs").and_then(|o| o.get("data")).and_then(|d| d.as_array()) {
                        for item in data {
                            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let version = item.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            if !name.is_empty() && !version.is_empty() {
                                versions.push((name, version));
                            }
                        }
                    }
                }
            }
        }

        versions
}

/// Compare versions to see if store version is newer
fn version_is_newer(store_version: &str, current_version: &str) -> bool {
    // Simple version comparison
    let store_parts: Vec<u32> = store_version
    .split(|c: char| !c.is_ascii_digit())
    .filter_map(|s| s.parse().ok())
    .collect();
    let current_parts: Vec<u32> = current_version
    .split(|c: char| !c.is_ascii_digit())
    .filter_map(|s| s.parse().ok())
    .collect();

    for i in 0..store_parts.len().max(current_parts.len()) {
        let store_part = store_parts.get(i).copied().unwrap_or(0);
        let current_part = current_parts.get(i).copied().unwrap_or(0);
        if store_part > current_part {
            return true;
        } else if store_part < current_part {
            return false;
        }
    }
    false
}

/// List firmware devices via fwupdmgr
fn list_firmware() -> Vec<PackageData> {
    let mut firmware = Vec::new();

    // Get devices with firmware
    if let Ok(output) = std::process::Command::new("fwupdmgr")
        .args(["get-devices", "--json"])
        .output()
        {
            if output.status.success() {
                let json_str = String::from_utf8_lossy(&output.stdout);
                if let Ok(json) = serde_json::from_str::<Value>(&json_str) {
                    if let Some(devices) = json.get("Devices").and_then(|d| d.as_array()) {
                        for device in devices {
                            let name = device.get("Name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Unknown Device")
                            .to_string();
                            let version = device.get("Version")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                            let vendor = device.get("Vendor")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                            let device_id = device.get("DeviceId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                            let updatable = device.get("Flags")
                            .and_then(|f| f.as_array())
                            .map(|flags| flags.iter().any(|f| f.as_str() == Some("updatable")))
                            .unwrap_or(false);

                            // Only show updatable devices
                            if !updatable {
                                continue;
                            }

                            let description = if vendor.is_empty() {
                                "Firmware device".to_string()
                            } else {
                                format!("Firmware by {}", vendor)
                            };

                            firmware.push(PackageData {
                                name: SharedString::from(&device_id),
                                          display_name: SharedString::from(&name),
                                          version: SharedString::from(&version),
                                          description: SharedString::from(&description),
                                          repository: SharedString::from("fwupd"),
                                          backend: 4, // 4 = firmware
                                          installed: true,
                                          has_update: false, // Will check separately
                                          installed_size: SharedString::from(""),
                                          licenses: SharedString::from(""),
                                          url: SharedString::from(""),
                                          dependencies: SharedString::from(""),
                                          required_by: SharedString::from(""),
                                          icon_name: SharedString::from(""),
                                          selected: false,
                            });
                        }
                    }
                }
            }
        }

        // Check for firmware updates
        if let Ok(output) = std::process::Command::new("fwupdmgr")
            .args(["get-updates", "--json"])
            .output()
            {
                if output.status.success() {
                    let json_str = String::from_utf8_lossy(&output.stdout);
                    if let Ok(json) = serde_json::from_str::<Value>(&json_str) {
                        if let Some(devices) = json.get("Devices").and_then(|d| d.as_array()) {
                            for device in devices {
                                let device_id = device.get("DeviceId")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                                // Mark devices with updates
                                for fw in &mut firmware {
                                    if fw.name.as_str() == device_id {
                                        fw.has_update = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            firmware
}

/// Plasmoid info struct
struct PlasmoidInfo {
    id: String,
    name: String,
    version: String,
    description: String,
}

/// Parse plasmoid metadata.json (KDE Plasma 6 format)
fn parse_plasmoid_json(path: &std::path::Path) -> PlasmoidInfo {
    if let Ok(content) = std::fs::read_to_string(path) {
        if let Ok(json) = serde_json::from_str::<Value>(&content) {
            // KDE Plasma 6 format has fields under "KPlugin" object
            if let Some(kplugin) = json.get("KPlugin") {
                let id = kplugin.get("Id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
                let name = kplugin.get("Name")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string();
                let version = kplugin.get("Version")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
                let desc = kplugin.get("Description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
                return PlasmoidInfo { id, name, version, description: desc };
            }
        }
    }
    PlasmoidInfo {
        id: String::new(),
        name: "Unknown".to_string(),
        version: "unknown".to_string(),
        description: String::new(),
    }
}

/// Parse plasmoid metadata.desktop (older format)
fn parse_plasmoid_desktop(path: &std::path::Path) -> PlasmoidInfo {
    if let Ok(content) = std::fs::read_to_string(path) {
        let mut id = String::new();
        let mut name = "Unknown".to_string();
        let mut version = "unknown".to_string();
        let mut desc = String::new();

        for line in content.lines() {
            if line.starts_with("Name=") && !line.contains('[') {
                name = line.strip_prefix("Name=").unwrap_or("Unknown").to_string();
            } else if line.starts_with("X-KDE-PluginInfo-Version=") {
                version = line.strip_prefix("X-KDE-PluginInfo-Version=").unwrap_or("unknown").to_string();
            } else if line.starts_with("X-KDE-PluginInfo-Name=") {
                id = line.strip_prefix("X-KDE-PluginInfo-Name=").unwrap_or("").to_string();
            } else if line.starts_with("Comment=") && !line.contains('[') {
                desc = line.strip_prefix("Comment=").unwrap_or("").to_string();
            }
        }
        PlasmoidInfo { id, name, version, description: desc }
    } else {
        PlasmoidInfo {
            id: String::new(),
            name: "Unknown".to_string(),
            version: "unknown".to_string(),
            description: String::new(),
        }
    }
}


/// Search packages (runs in background thread)
async fn search_packages_async(tx: &mpsc::Sender<UiMessage>, query: &str) {
    let alpm = match AlpmBackend::new() {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to initialize ALPM: {}", e);
            let _ = tx.send(UiMessage::SearchResults(Vec::new()));
            return;
        }
    };

    let flatpak = match FlatpakBackend::new() {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to initialize Flatpak: {}", e);
            let _ = tx.send(UiMessage::SearchResults(Vec::new()));
            return;
        }
    };

    // Search pacman
    let pacman_results = match alpm.search(query).await {
        Ok(r) => r,
        Err(e) => {
            error!("Pacman search failed: {}", e);
            Vec::new()
        }
    };

    // Search flatpak
    let flatpak_results = match flatpak.search(query).await {
        Ok(r) => r,
        Err(e) => {
            error!("Flatpak search failed: {}", e);
            Vec::new()
        }
    };

    // Build desktop name map for humanization
    let desktop_map = build_desktop_name_map();

    // Convert to UI types
    let mut results: Vec<PackageData> = pacman_results
    .iter()
    .map(|r| {
        let display_name = humanize_package_name(&r.name, &desktop_map);
        PackageData {
            name: SharedString::from(r.name.as_str()),
         display_name: SharedString::from(&display_name),
         version: SharedString::from(r.version.to_string().as_str()),
         description: SharedString::from(r.description.as_str()),
         repository: SharedString::from(r.repository.as_str()),
         backend: 0,
         installed: r.installed,
         has_update: false,
         installed_size: SharedString::from(""),
         licenses: SharedString::from(""),
         url: SharedString::from(""),
         dependencies: SharedString::from(""),
         required_by: SharedString::from(""),
         icon_name: SharedString::from(""),
         selected: false,
        }
    })
    .collect();

    results.extend(flatpak_results.iter().map(|r| PackageData {
        name: SharedString::from(r.name.as_str()),
                                              display_name: SharedString::from(r.name.as_str()),
                                              version: SharedString::from(r.version.to_string().as_str()),
                                              description: SharedString::from(r.description.as_str()),
                                              repository: SharedString::from(r.repository.as_str()),
                                              backend: 1,
                                              installed: r.installed,
                                              has_update: false,
                                              installed_size: SharedString::from(""),
                                              licenses: SharedString::from(""),
                                              url: SharedString::from(""),
                                              dependencies: SharedString::from(""),
                                              required_by: SharedString::from(""),
                                              icon_name: SharedString::from(""),
                                              selected: false,
    }));

    // Limit results
    results.truncate(100);

    let _ = tx.send(UiMessage::SearchResults(results));
}

/// Load packages for a specific category
async fn load_category_packages(tx: &mpsc::Sender<UiMessage>, category: &str) {
    let mut packages = Vec::new();

    // Load from desktop files (native packages)
    let desktop_dirs = [
        "/usr/share/applications",
        "/var/lib/flatpak/exports/share/applications",
    ];

    let home = std::env::var("HOME").unwrap_or_default();
    let user_flatpak = format!("{}/.local/share/flatpak/exports/share/applications", home);

    for dir in desktop_dirs.iter().chain(std::iter::once(&user_flatpak.as_str())) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |e| e == "desktop") {
                    if let Some(pkg) = parse_desktop_file(&path, category) {
                        packages.push(pkg);
                    }
                }
            }
        }
    }

    // Also load from Flatpak appstream for better data
    let appstream_path = "/var/lib/flatpak/appstream/flathub/x86_64";
    if let Ok(entries) = std::fs::read_dir(appstream_path) {
        for entry in entries.flatten() {
            let xml_path = entry.path().join("appstream.xml.gz");
            if xml_path.exists() {
                parse_appstream_for_category(&xml_path, category, &mut packages);
            }
        }
    }

    // Remove duplicates by name
    packages.sort_by(|a, b| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()));
    packages.dedup_by(|a, b| a.name == b.name);

    let _ = tx.send(UiMessage::CategoryPackages(packages));
}

/// Parse a desktop file and return PackageData if it matches the category
fn parse_desktop_file(path: &std::path::Path, target_category: &str) -> Option<PackageData> {
    let content = std::fs::read_to_string(path).ok()?;

    let mut name = String::new();
    let mut generic_name = String::new();
    let mut comment = String::new();
    let mut categories = String::new();
    let mut no_display = false;
    let mut hidden = false;

    for line in content.lines() {
        if line.starts_with("Name=") && !line.contains('[') {
            name = line.strip_prefix("Name=")?.to_string();
        } else if line.starts_with("GenericName=") && !line.contains('[') {
            generic_name = line.strip_prefix("GenericName=")?.to_string();
        } else if line.starts_with("Comment=") && !line.contains('[') {
            comment = line.strip_prefix("Comment=")?.to_string();
        } else if line.starts_with("Categories=") {
            categories = line.strip_prefix("Categories=")?.to_string();
        } else if line.starts_with("NoDisplay=true") {
            no_display = true;
        } else if line.starts_with("Hidden=true") {
            hidden = true;
        }
    }

    // Skip hidden or NoDisplay entries
    if no_display || hidden || name.is_empty() {
        return None;
    }

    // Check if categories match
    let category_list: Vec<&str> = categories.split(';').collect();
    if !category_list.iter().any(|c| c == &target_category) {
        return None;
    }

    let description = if !comment.is_empty() {
        comment
    } else if !generic_name.is_empty() {
        generic_name
    } else {
        String::new()
    };

    // Determine if it's a Flatpak (by path)
    let is_flatpak = path.to_string_lossy().contains("flatpak");
    let backend = if is_flatpak { 1 } else { 0 };
    let repo = if is_flatpak { "flathub" } else { "native" };

    // Get package name from filename
    let pkg_name = path.file_stem()?.to_string_lossy().to_string();

    Some(PackageData {
        name: SharedString::from(&pkg_name),
         display_name: SharedString::from(&name),
         version: SharedString::from(""),
         description: SharedString::from(&description),
         repository: SharedString::from(repo),
         backend,
         installed: true,
         has_update: false,
         installed_size: SharedString::from(""),
         licenses: SharedString::from(""),
         url: SharedString::from(""),
         dependencies: SharedString::from(""),
         required_by: SharedString::from(""),
         icon_name: SharedString::from(""),
         selected: false,
    })
}

/// Parse Flatpak appstream XML for packages in a category
fn parse_appstream_for_category(path: &std::path::Path, target_category: &str, packages: &mut Vec<PackageData>) {
    // Decompress and parse
    if let Ok(file) = std::fs::File::open(path) {
        let decoder = flate2::read::GzDecoder::new(file);
        let reader = std::io::BufReader::new(decoder);
        parse_appstream_xml(reader, target_category, packages);
    }
}

/// Build a map of Flatpak app IDs to human-readable names from appstream data
fn build_flatpak_name_map() -> HashMap<String, (String, String)> {
    let mut name_map = HashMap::new();

    // Find appstream files in both user and system locations
    let home = std::env::var("HOME").unwrap_or_default();
    let search_dirs = [
        format!("{}/.local/share/flatpak/appstream", home),
            "/var/lib/flatpak/appstream".to_string(),
    ];

    for base_dir in &search_dirs {
        let base_path = std::path::Path::new(base_dir);
        if !base_path.exists() {
            continue;
        }

        // Look for appstream.xml.gz in any remote's directory structure
        if let Ok(entries) = std::fs::read_dir(base_path) {
            for remote_entry in entries.flatten() {
                let remote_path = remote_entry.path();
                // Look in x86_64 subdirectory
                let arch_path = remote_path.join("x86_64");
                if arch_path.exists() {
                    if let Ok(hash_entries) = std::fs::read_dir(&arch_path) {
                        for hash_entry in hash_entries.flatten() {
                            let appstream_gz = hash_entry.path().join("appstream.xml.gz");
                            if appstream_gz.exists() {
                                if let Ok(file) = std::fs::File::open(&appstream_gz) {
                                    let decoder = flate2::read::GzDecoder::new(file);
                                    let reader = std::io::BufReader::new(decoder);
                                    parse_appstream_names(reader, &mut name_map);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    name_map
}

/// Parse appstream XML to extract app IDs and their human names
fn parse_appstream_names<R: std::io::Read>(reader: R, name_map: &mut HashMap<String, (String, String)>) {
    use std::io::Read;

    // Read entire content since XML can be minified
    let mut content = String::new();
    let mut buf_reader = std::io::BufReader::new(reader);
    if buf_reader.read_to_string(&mut content).is_err() {
        return;
    }

    // Find all desktop/console application components
    let mut pos = 0;
    while let Some(comp_start) = content[pos..].find("<component type=\"desktop") {
        let abs_start = pos + comp_start;

        // Find end of this component
        if let Some(comp_end) = content[abs_start..].find("</component>") {
            let component = &content[abs_start..abs_start + comp_end + 12];

            // Extract ID
            if let Some(id) = extract_tag_content(component, "id") {
                // Extract name (first one without xml:lang)
                let name = extract_first_name(component);
                // Extract summary
                let summary = extract_tag_content(component, "summary").unwrap_or_default();

                if let Some(name) = name {
                    name_map.insert(id.to_lowercase(), (name, summary));
                }
            }

            pos = abs_start + comp_end + 12;
        } else {
            break;
        }
    }
}

/// Extract content from a simple XML tag
fn extract_tag_content(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);

    if let Some(start) = xml.find(&open) {
        let content_start = start + open.len();
        if let Some(end) = xml[content_start..].find(&close) {
            return Some(xml[content_start..content_start + end].to_string());
        }
    }
    None
}

/// Extract first <name> without xml:lang attribute
fn extract_first_name(xml: &str) -> Option<String> {
    // First try <name>...</name> without attributes
    if let Some(start) = xml.find("<name>") {
        let content_start = start + 6;
        if let Some(end) = xml[content_start..].find("</name>") {
            return Some(xml[content_start..content_start + end].to_string());
        }
    }
    None
}

/// Parse appstream XML content
fn parse_appstream_xml<R: std::io::Read>(reader: R, target_category: &str, packages: &mut Vec<PackageData>) {
    use std::io::BufRead;
    let buf_reader = std::io::BufReader::new(reader);

    let mut current_id = String::new();
    let mut current_name = String::new();
    let mut current_summary = String::new();
    let mut current_categories = Vec::new();
    let mut in_component = false;
    let mut in_categories = false;
    let mut skip_lang = false;

    for line in buf_reader.lines().flatten() {
        let line = line.trim();

        if line.contains("<component") && line.contains("desktop-application") {
            in_component = true;
            current_id.clear();
            current_name.clear();
            current_summary.clear();
            current_categories.clear();
        } else if line.contains("</component>") {
            if in_component && current_categories.iter().any(|c| c == target_category) {
                packages.push(PackageData {
                    name: SharedString::from(&current_id),
                              display_name: SharedString::from(&current_name),
                              version: SharedString::from(""),
                              description: SharedString::from(&current_summary),
                              repository: SharedString::from("flathub"),
                              backend: 1,
                              installed: false, // Will check later
                              has_update: false,
                              installed_size: SharedString::from(""),
                              licenses: SharedString::from(""),
                              url: SharedString::from(""),
                              dependencies: SharedString::from(""),
                              required_by: SharedString::from(""),
                              icon_name: SharedString::from(""),
                              selected: false,
                });
            }
            in_component = false;
        } else if in_component {
            if line.starts_with("<id>") && line.ends_with("</id>") {
                current_id = line.strip_prefix("<id>").unwrap_or("")
                .strip_suffix("</id>").unwrap_or("").to_string();
            } else if line.starts_with("<name>") && line.ends_with("</name>") && current_name.is_empty() {
                current_name = line.strip_prefix("<name>").unwrap_or("")
                .strip_suffix("</name>").unwrap_or("").to_string();
            } else if line.starts_with("<name") && line.contains("xml:lang") {
                skip_lang = true;
            } else if line == "</name>" && skip_lang {
                skip_lang = false;
            } else if line.starts_with("<summary>") && line.ends_with("</summary>") && current_summary.is_empty() {
                current_summary = line.strip_prefix("<summary>").unwrap_or("")
                .strip_suffix("</summary>").unwrap_or("").to_string();
            } else if line == "<categories>" {
                in_categories = true;
            } else if line == "</categories>" {
                in_categories = false;
            } else if in_categories && line.starts_with("<category>") && line.ends_with("</category>") {
                let cat = line.strip_prefix("<category>").unwrap_or("")
                .strip_suffix("</category>").unwrap_or("").to_string();
                current_categories.push(cat);
            }
        }
    }
}

/// Build a mapping of package names to human-readable names from desktop files
fn build_desktop_name_map() -> HashMap<String, String> {
    let mut map = HashMap::new();

    // Scan desktop files to find human-readable names
    let dirs = ["/usr/share/applications"];
    for dir in &dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |e| e == "desktop") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let mut name = String::new();
                        let mut exec = String::new();
                        let mut no_display = false;
                        for line in content.lines() {
                            if line.starts_with("Name=") && !line.contains('[') {
                                name = line.strip_prefix("Name=").unwrap_or("").to_string();
                            } else if line.starts_with("Exec=") {
                                // Extract binary name from Exec line
                                exec = line.strip_prefix("Exec=").unwrap_or("")
                                .split_whitespace().next().unwrap_or("")
                                .rsplit('/').next().unwrap_or("").to_string();
                            } else if line.starts_with("NoDisplay=true") {
                                no_display = true;
                            }
                        }
                        if !name.is_empty() && !no_display {
                            // Map by desktop filename stem
                            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                map.insert(stem.to_lowercase(), name.clone());
                            }
                            // Also map by executable name
                            if !exec.is_empty() {
                                map.entry(exec.to_lowercase()).or_insert(name);
                            }
                        }
                    }
                }
            }
        }
    }

    // Also use pacman -Ql to map packages to desktop files they own
    if let Ok(output) = std::process::Command::new("pacman")
        .args(["-Ql"])
        .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if line.contains("/usr/share/applications/") && line.ends_with(".desktop") {
                        let parts: Vec<&str> = line.splitn(2, ' ').collect();
                        if parts.len() == 2 {
                            let pkg_name = parts[0];
                            let desktop_path = parts[1].trim();
                            let file_stem = std::path::Path::new(desktop_path)
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("")
                            .to_lowercase();
                            // If we have a human name for this desktop file, map the package name to it
                            if let Some(human_name) = map.get(&file_stem) {
                                map.insert(pkg_name.to_lowercase(), human_name.clone());
                            }
                        }
                    }
                }
            }
        }

        map
}

/// Humanize a package name - lookup desktop file names, fallback to title-case
fn humanize_package_name(name: &str, desktop_map: &HashMap<String, String>) -> String {
    // Check desktop file map first (by package name)
    if let Some(human_name) = desktop_map.get(&name.to_lowercase()) {
        return human_name.clone();
    }

    // Title-case: replace hyphens/underscores with spaces, capitalize each word
    name.split(|c: char| c == '-' || c == '_')
    .map(|word| {
        let mut chars = word.chars();
        match chars.next() {
            Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str()),
         None => String::new(),
        }
    })
    .collect::<Vec<_>>()
    .join(" ")
}

/// Parse active repos from /etc/pacman.conf
fn parse_pacman_repos() -> Vec<String> {
    let mut repos = Vec::new();
    if let Ok(content) = std::fs::read_to_string("/etc/pacman.conf") {
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with('[') && line.ends_with(']') {
                let repo = &line[1..line.len() - 1];
                if repo != "options" {
                    repos.push(repo.to_string());
                }
            }
        }
    }
    repos
}

/// Load packages from a specific pacman repo with humanized names via expac
fn load_repo_packages(tx: &std::sync::mpsc::Sender<UiMessage>, repo: &str) {
    // Get installed package names
    let installed_names: std::collections::HashSet<String> = std::process::Command::new("pacman")
    .args(["-Qq"])
    .output()
    .ok()
    .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.to_string()).collect())
    .unwrap_or_default();

    // Build desktop name map for humanization
    let desktop_map = build_desktop_name_map();

    // Use expac -S with %r to get repo, then filter. Format: repo\tname\tversion\tdesc
    let output = std::process::Command::new("expac")
    .args(["-S", "%r\t%n\t%v\t%d"])
    .output();

    let packages: Vec<PackageData> = match output {
        Ok(result) if result.status.success() => {
            let stdout = String::from_utf8_lossy(&result.stdout);
            stdout
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.splitn(4, '\t').collect();
                if parts.len() >= 4 && parts[0] == repo {
                    let name = parts[1];
                    let version = parts[2];
                    let description = parts[3];
                    let is_installed = installed_names.contains(name);
                    let display_name = humanize_package_name(name, &desktop_map);
                    Some(PackageData {
                        name: SharedString::from(name),
                         display_name: SharedString::from(&display_name),
                         version: SharedString::from(version),
                         description: SharedString::from(description),
                         repository: SharedString::from(repo),
                         backend: 0,
                         installed: is_installed,
                         has_update: false,
                         installed_size: SharedString::from(""),
                         licenses: SharedString::from(""),
                         url: SharedString::from(""),
                         dependencies: SharedString::from(""),
                         required_by: SharedString::from(""),
                         icon_name: SharedString::from(""),
                         selected: false,
                    })
                } else {
                    None
                }
            })
            .collect()
        }
        _ => Vec::new(),
    };

    let _ = tx.send(UiMessage::RepoPackages(packages));
}
