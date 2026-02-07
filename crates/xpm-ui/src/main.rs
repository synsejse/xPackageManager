//! xPackageManager - A modern package manager for Arch Linux.

use slint::{Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

slint::include_modules!();

mod ui_helpers;
use ui_helpers::{
    apply_terminal_text, get_local_package_info, is_arch_package, is_xerolinux_distro,
    load_category_packages, load_packages_async, load_repo_packages, parse_pacman_repos,
    refresh_after_operation, run_async, search_packages_async, strip_ansi,
};
/// Spawn a child process in a PTY, returning (master_fd, child_pid)
fn spawn_in_pty(cmd: &str, args: &[&str]) -> Result<(i32, u32), String> {
    use std::os::unix::io::FromRawFd;

    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;

    // Create PTY pair
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
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
    unsafe {
        libc::close(slave);
    }

    match child {
        Ok(c) => Ok((master, c.id())),
        Err(e) => {
            unsafe {
                libc::close(master);
            }
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
    unsafe {
        libc::close(master_fd);
    }

    let _ = reader_handle.join();
    let _ = writer_handle.join();

    let _ = tx.send(UiMessage::TerminalDone(success));
}

/// Build command and args for a package operation
fn build_pacman_command(action: &str, names: &[String], backend: i32) -> (String, Vec<String>) {
    match (action, backend) {
        ("install", 1) | ("bulk-install", 1) => ("flatpak".to_string(), {
            let mut args = vec!["install".to_string(), "-y".to_string()];
            args.extend(names.iter().cloned());
            args
        }),
        ("remove", 1) | ("bulk-remove", 1) => ("flatpak".to_string(), {
            let mut args = vec!["uninstall".to_string(), "-y".to_string()];
            args.extend(names.iter().cloned());
            args
        }),
        ("update", 1) => ("flatpak".to_string(), {
            let mut args = vec!["update".to_string(), "-y".to_string()];
            args.extend(names.iter().cloned());
            args
        }),
        ("remove", _) | ("bulk-remove", _) => ("pkexec".to_string(), {
            let mut args = vec!["pacman".to_string(), "-R".to_string()];
            args.extend(names.iter().cloned());
            args
        }),
        ("update-all", _) => (
            "pkexec".to_string(),
            vec!["pacman".to_string(), "-Syu".to_string()],
        ),
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
        // Throttle UI output updates to avoid flooding the renderer
        let mut pending_output = String::new();
        let mut last_output_flush = std::time::Instant::now();
        const OUTPUT_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);
        const MAX_OUTPUT_LINES: usize = 80;

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
                        // Buffer output and flush at throttled intervals
                        // Filter out noisy progress bar lines (contain lots of # or %)
                        for line in cleaned.lines() {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            // Skip raw progress bar fragments (e.g. "####..." or repeated dots)
                            if trimmed.len() > 5
                                && (trimmed.chars().filter(|&c| c == '#').count()
                                    > trimmed.len() / 2
                                    || trimmed.chars().filter(|&c| c == '.').count()
                                        > trimmed.len() / 2)
                            {
                                continue;
                            }
                            pending_output.push_str(trimmed);
                            pending_output.push('\n');
                        }

                        // Flush pending output to UI at throttled rate
                        let now = std::time::Instant::now();
                        if !pending_output.is_empty()
                            && now.duration_since(last_output_flush) >= OUTPUT_FLUSH_INTERVAL
                        {
                            // Trim to last N lines before sending
                            let lines: Vec<&str> = pending_output.lines().collect();
                            if lines.len() > MAX_OUTPUT_LINES {
                                pending_output = lines[lines.len() - MAX_OUTPUT_LINES..].join("\n");
                                pending_output.push('\n');
                            }
                            let _ =
                                tx_reader.send(UiMessage::ProgressOutput(pending_output.clone()));
                            last_output_flush = now;
                            // Don't clear — keep as rolling buffer so next flush has context
                        }

                        // Check for conflicts/errors
                        let lower = cleaned.to_lowercase();
                        let has_conflict = CONFLICT_PATTERNS
                            .iter()
                            .any(|p| lower.contains(&p.to_lowercase()));

                        if has_conflict {
                            // Escalate to terminal
                            *escalated_r.lock().unwrap() = true;
                            let accumulated = output_buffer_r.lock().unwrap().clone();
                            let _ = tx_reader.send(UiMessage::ShowTerminalFallback(accumulated));
                            continue;
                        }

                        // Check for routine auto-confirm prompts
                        let is_auto_confirm = PACMAN_AUTO_CONFIRM_PATTERNS
                            .iter()
                            .any(|p| cleaned.contains(p));
                        if is_auto_confirm {
                            let _ = in_tx_r.send("y".to_string());
                            let _ = tx_reader.send(UiMessage::ProgressHidePrompt);
                        } else {
                            // Check for prompts that need user input
                            let needs_user_input = PACMAN_USER_PROMPT_PATTERNS
                                .iter()
                                .any(|p| cleaned.contains(p))
                                || (cleaned.contains("[y/N]") && !is_auto_confirm);
                            if needs_user_input {
                                // Extract the prompt text (last non-empty line)
                                let prompt_text = cleaned
                                    .lines()
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
                                parse_progress_fraction(line, 20, 50, total_packages).unwrap_or(35)
                            } else if lower_line.contains("checking keyring")
                                || lower_line.contains("checking integrity")
                            {
                                52
                            } else if lower_line.contains("checking package integrity") {
                                55
                            } else if lower_line.contains("loading package files") {
                                58
                            } else if lower_line.contains("installing")
                                || lower_line.contains("upgrading")
                                || lower_line.contains("removing")
                                || lower_line.contains("reinstalling")
                            {
                                parse_progress_fraction(line, 60, 85, total_packages).unwrap_or(72)
                            } else if lower_line.contains("running post-transaction hooks") {
                                88
                            } else if lower_line.contains("arming conditionpathexists")
                                || lower_line.contains("updating linux module")
                                || lower_line.contains("dkms")
                            {
                                90
                            } else if lower_line.contains("updating linux initcpios")
                                || lower_line.contains("mkinitcpio")
                            {
                                92
                            } else if lower_line.contains("updating grub")
                                || lower_line.contains("grub-mkconfig")
                            {
                                95
                            } else if lower_line.contains("updating the info")
                                || lower_line.contains("updating the desktop")
                                || lower_line.contains("updating mime")
                            {
                                97
                            } else {
                                current_percent
                            };

                            if new_percent > current_percent {
                                current_percent = new_percent;
                                let _ = tx_reader
                                    .send(UiMessage::OperationProgress(current_percent, trimmed));
                            } else if new_percent == current_percent
                                && current_percent >= 88
                                && !trimmed.is_empty()
                            {
                                // During post-transaction hooks, keep updating the stage text
                                let _ = tx_reader
                                    .send(UiMessage::OperationProgress(current_percent, trimmed));
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
        // Final flush of any remaining output
        if !pending_output.is_empty() {
            let lines: Vec<&str> = pending_output.lines().collect();
            if lines.len() > MAX_OUTPUT_LINES {
                pending_output = lines[lines.len() - MAX_OUTPUT_LINES..].join("\n");
                pending_output.push('\n');
            }
            let _ = tx_reader.send(UiMessage::ProgressOutput(pending_output));
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

    unsafe {
        libc::close(master_fd);
    }

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
fn parse_progress_fraction(
    line: &str,
    range_start: i32,
    range_end: i32,
    _total_packages: usize,
) -> Option<i32> {
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
                        return Some(
                            range_start + ((range_end - range_start) as f64 * fraction) as i32,
                        );
                    }
                }
            }
        }
    }
    None
}

/// Convert a Package to PackageData for the UI
fn package_to_ui(
    pkg: &xpm_core::package::Package,
    has_update: bool,
    desktop_map: &HashMap<String, String>,
) -> PackageData {
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
fn update_selection_in_model(
    model: &ModelRc<PackageData>,
    name: &str,
    backend: i32,
    selected: bool,
) {
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
    let terminal_input_sender: Arc<Mutex<Option<mpsc::Sender<String>>>> =
        Arc::new(Mutex::new(None));
    let terminal_child_pid: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

    // Selection tracking: Vec of (name, backend, installed)
    let selected_packages: Rc<RefCell<Vec<(String, i32, bool)>>> =
        Rc::new(RefCell::new(Vec::new()));

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

    timer.start(
        TimerMode::Repeated,
        std::time::Duration::from_millis(50),
        move || {
            if let Some(window) = window_weak.upgrade() {
                let mut flush_now = false;

                while let Ok(msg) = rx_clone.borrow_mut().try_recv() {
                    match msg {
                        UiMessage::PackagesLoaded {
                            installed,
                            updates,
                            flatpak,
                            firmware,
                            stats,
                        } => {
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
                                let search_query = window.get_search_text().to_string();
                                let current_view = window.get_view();
                                let current_repo = window.get_current_repo_name().to_string();
                                refresh_after_operation(
                                    tx_timer.clone(),
                                    search_query,
                                    current_view,
                                    current_repo,
                                );
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
                            // Replace output directly — the reader thread sends a pre-trimmed rolling buffer
                            window.set_progress_popup_output(SharedString::from(&text));
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
                                let search_query = window.get_search_text().to_string();
                                let current_view = window.get_view();
                                let current_repo = window.get_current_repo_name().to_string();
                                refresh_after_operation(
                                    tx_timer.clone(),
                                    search_query,
                                    current_view,
                                    current_repo,
                                );
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
                    && (flush_now
                        || last_term_flush.elapsed() >= std::time::Duration::from_millis(150))
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
        },
    );

    // Initial load - spawn background thread
    let tx_initial = tx.clone();
    run_async(async move {
        let _ = tx_initial.send(UiMessage::SetLoading(true));
        load_packages_async(&tx_initial, false).await;
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
        run_async(async move {
            let _ = tx.send(UiMessage::SetLoading(true));
            load_packages_async(&tx, false).await;
        });
    });

    // Set up search callback
    window.on_search(move |query| {
        info!("Search: {}", query);
        let tx = tx_search.clone();
        let query = query.to_string();
        run_async(async move {
            let _ = tx.send(UiMessage::SetLoading(true));
            search_packages_async(&tx, &query).await;
        });
    });

    // Load category callback
    let tx_category = tx.clone();
    window.on_load_category(move |category| {
        info!("Load category: {}", category);
        let tx = tx_category.clone();
        let category = category.to_string();
        run_async(async move {
            let _ = tx.send(UiMessage::SetLoading(true));
            load_category_packages(&tx, &category).await;
        });
    });

    // Parse repos from pacman.conf and populate sidebar
    let repos = parse_pacman_repos();
    let repo_model: Vec<SharedString> = repos
        .iter()
        .map(|r| SharedString::from(r.as_str()))
        .collect();
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

            let name_list: Vec<String> = names_str
                .split('\n')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            let tx = tx_confirm.clone();
            let input = confirm_input.clone();
            let pid = confirm_pid.clone();

            match action.as_str() {
                "install" | "bulk-install" => {
                    let title = if name_list.len() > 1 {
                        format!("Installing {} packages", name_list.len())
                    } else {
                        format!(
                            "Installing {}",
                            name_list.first().map(|s| s.as_str()).unwrap_or("")
                        )
                    };
                    thread::spawn(move || {
                        run_managed_operation(
                            &tx, &title, "install", &name_list, backend, &input, &pid,
                        );
                    });
                }
                "remove" | "bulk-remove" => {
                    let title = if name_list.len() > 1 {
                        format!("Removing {} packages", name_list.len())
                    } else {
                        format!(
                            "Removing {}",
                            name_list.first().map(|s| s.as_str()).unwrap_or("")
                        )
                    };
                    thread::spawn(move || {
                        run_managed_operation(
                            &tx, &title, "remove", &name_list, backend, &input, &pid,
                        );
                    });
                }
                "update" => {
                    let title = format!("Updating {}", names_str);
                    thread::spawn(move || {
                        run_managed_operation(
                            &tx, &title, "update", &name_list, backend, &input, &pid,
                        );
                    });
                }
                "update-all" => {
                    thread::spawn(move || {
                        run_managed_operation(
                            &tx,
                            "System Update",
                            "update-all",
                            &[],
                            0,
                            &input,
                            &pid,
                        );
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
        let uninstalled: Vec<&(String, i32, bool)> =
            sel.iter().filter(|(_, _, inst)| !inst).collect();
        if uninstalled.is_empty() {
            return;
        }
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
        let installed: Vec<&(String, i32, bool)> =
            sel.iter().filter(|(_, _, inst)| *inst).collect();
        if installed.is_empty() {
            return;
        }
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
            let _ = tx.send(UiMessage::SetProgressText(
                "Syncing pacman databases...".to_string(),
            ));
            let _ = tx.send(UiMessage::SetStatus(
                "Syncing pacman databases...".to_string(),
            ));

            // Step 1: Sync pacman databases via polkit
            let pacman_ok = match std::process::Command::new("pkexec")
                .args(["pacman", "-Syy"])
                .output()
            {
                Ok(r) if r.status.success() => {
                    let _ = tx.send(UiMessage::SetProgress(25));
                    let _ = tx.send(UiMessage::SetProgressText(
                        "Pacman synced. Checking Flatpak...".to_string(),
                    ));
                    let _ = tx.send(UiMessage::SetStatus(
                        "Pacman synced. Checking Flatpak...".to_string(),
                    ));
                    true
                }
                Ok(r) => {
                    let stderr = String::from_utf8_lossy(&r.stderr);
                    if stderr.contains("cancelled")
                        || stderr.contains("dismissed")
                        || r.status.code() == Some(126)
                        || r.status.code() == Some(127)
                    {
                        let _ =
                            tx.send(UiMessage::SetStatus("Authentication cancelled".to_string()));
                        let _ = tx.send(UiMessage::SetProgress(0));
                        let _ = tx.send(UiMessage::SetProgressText("".to_string()));
                        let _ = tx.send(UiMessage::SetBusy(false));
                        return;
                    }
                    let _ = tx.send(UiMessage::SetProgress(25));
                    let _ = tx.send(UiMessage::SetProgressText(
                        "Pacman sync had issues, continuing...".to_string(),
                    ));
                    let _ = tx.send(UiMessage::SetStatus(
                        "Pacman sync had issues, continuing...".to_string(),
                    ));
                    false
                }
                Err(_) => {
                    let _ = tx.send(UiMessage::SetProgress(25));
                    let _ = tx.send(UiMessage::SetProgressText(
                        "Pacman sync unavailable, continuing...".to_string(),
                    ));
                    let _ = tx.send(UiMessage::SetStatus(
                        "Pacman sync unavailable, continuing...".to_string(),
                    ));
                    false
                }
            };

            // Step 2: Refresh Flatpak appstream data
            let _ = tx.send(UiMessage::SetProgress(50));
            let _ = tx.send(UiMessage::SetProgressText(
                "Refreshing Flatpak metadata...".to_string(),
            ));
            let _ = tx.send(UiMessage::SetStatus(
                "Refreshing Flatpak metadata...".to_string(),
            ));
            let _flatpak_ok = match std::process::Command::new("flatpak")
                .args(["update", "--appstream", "-y"])
                .output()
            {
                Ok(r) => r.status.success(),
                Err(_) => false,
            };

            // Step 3: Refresh firmware metadata (with timeout - fwupdmgr can hang)
            let _ = tx.send(UiMessage::SetProgress(75));
            let _ = tx.send(UiMessage::SetProgressText(
                "Checking firmware...".to_string(),
            ));
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
            let _ = tx.send(UiMessage::SetProgressText(
                "Reloading packages...".to_string(),
            ));
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
            run_in_terminal(
                &tx,
                &title,
                "pkexec",
                &["pacman", "-U", &path],
                &input,
                &pid,
            );
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

// UI data loading moved to ui_helpers::data.
