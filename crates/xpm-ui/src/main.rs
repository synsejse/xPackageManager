
use serde::{Deserialize, Serialize};
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

enum UiMessage {
    PackagesLoaded {
        installed: Vec<PackageData>,
        updates: Vec<PackageData>,
        flatpak: Vec<PackageData>,
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
    ShowProgressPopup(String),
    OperationProgress(i32, String),
    ProgressOutput(String),
    ProgressPrompt(String),
    ProgressHidePrompt,
    OperationDone(bool),
    ShowTerminalFallback(String),
}

const PACMAN_AUTO_CONFIRM_PATTERNS: &[&str] = &[
    "Proceed with installation? [Y/n]",
"Proceed with download? [Y/n]",
":: Proceed with installation? [Y/n]",
":: Proceed with download? [Y/n]",
"Do you want to remove these packages? [y/N]",
":: Do you want to remove these packages? [y/N]",
];

const PACMAN_USER_PROMPT_PATTERNS: &[&str] = &[
    ":: Replace",
":: Import",
"Enter a number",
"Enter a selection",
"Terminate batch job",
];

const CONFLICT_PATTERNS: &[&str] = &[
    "conflicting files",
"are in conflict",
"exists in filesystem",
"breaks dependency",
"could not satisfy dependencies",
"failed to commit transaction",
];


#[derive(Serialize, Deserialize, Clone)]
struct AppConfig {
    theme: String,
    custom_colors: Option<CustomColors>,
    flatpak_enabled: bool,
    check_updates_on_start: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct CustomColors {
    base: String,
    surface: String,
    text: String,
    accent: String,
    success: String,
    danger: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: "catppuccin-macchiato".to_string(),
            custom_colors: None,
            flatpak_enabled: true,
            check_updates_on_start: false,
        }
    }
}

struct ThemeColors {
    base: &'static str,
    mantle: &'static str,
    crust: &'static str,
    surface0: &'static str,
    surface1: &'static str,
    surface2: &'static str,
    overlay0: &'static str,
    overlay1: &'static str,
    overlay2: &'static str,
    subtext0: &'static str,
    subtext1: &'static str,
    text: &'static str,
    mauve: &'static str,
    red: &'static str,
    green: &'static str,
    yellow: &'static str,
    blue: &'static str,
    lavender: &'static str,
    peach: &'static str,
    teal: &'static str,
    pink: &'static str,
    flamingo: &'static str,
    rosewater: &'static str,
    sapphire: &'static str,
    sky: &'static str,
    maroon: &'static str,
}

const CATPPUCCIN_MACCHIATO: ThemeColors = ThemeColors {
    base: "#24273a", mantle: "#1e2030", crust: "#181926",
    surface0: "#363a4f", surface1: "#494d64", surface2: "#5b6078",
    overlay0: "#6e738d", overlay1: "#8087a2", overlay2: "#939ab7",
    subtext0: "#a5adcb", subtext1: "#b8c0e0", text: "#cad3f5",
    mauve: "#c6a0f6", red: "#ed8796", green: "#a6da95",
    yellow: "#eed49f", blue: "#8aadf4", lavender: "#b7bdf8",
    peach: "#f5a97f", teal: "#8bd5ca", pink: "#f5bde6",
    flamingo: "#f0c6c6", rosewater: "#f4dbd6", sapphire: "#7dc4e4",
    sky: "#91d7e3", maroon: "#ee99a0",
};

const DEFAULT_DARK: ThemeColors = ThemeColors {
    base: "#1a1b26", mantle: "#16161e", crust: "#13131a",
    surface0: "#292e42", surface1: "#33384d", surface2: "#414868",
    overlay0: "#565f89", overlay1: "#6b7394", overlay2: "#787c9e",
    subtext0: "#9aa5ce", subtext1: "#a9b1d6", text: "#c0caf5",
    mauve: "#bb9af7", red: "#f7768e", green: "#9ece6a",
    yellow: "#e0af68", blue: "#7aa2f7", lavender: "#b4c0e9",
    peach: "#ff9e64", teal: "#73daca", pink: "#f5c2e7",
    flamingo: "#f2cdcd", rosewater: "#f5e0dc", sapphire: "#7dcfff",
    sky: "#89ddff", maroon: "#eb6f92",
};

fn parse_hex_color(hex: &str) -> slint::Color {
    let hex = hex.trim().trim_start_matches('#');
    let r = u8::from_str_radix(hex.get(0..2).unwrap_or("00"), 16).unwrap_or(0);
    let g = u8::from_str_radix(hex.get(2..4).unwrap_or("00"), 16).unwrap_or(0);
    let b = u8::from_str_radix(hex.get(4..6).unwrap_or("00"), 16).unwrap_or(0);
    slint::Color::from_rgb_u8(r, g, b)
}

fn apply_theme_to_window(window: &MainWindow, theme: &ThemeColors) {
    let cat = window.global::<Cat>();
    cat.set_base(parse_hex_color(theme.base));
    cat.set_mantle(parse_hex_color(theme.mantle));
    cat.set_crust(parse_hex_color(theme.crust));
    cat.set_surface0(parse_hex_color(theme.surface0));
    cat.set_surface1(parse_hex_color(theme.surface1));
    cat.set_surface2(parse_hex_color(theme.surface2));
    cat.set_overlay0(parse_hex_color(theme.overlay0));
    cat.set_overlay1(parse_hex_color(theme.overlay1));
    cat.set_overlay2(parse_hex_color(theme.overlay2));
    cat.set_subtext0(parse_hex_color(theme.subtext0));
    cat.set_subtext1(parse_hex_color(theme.subtext1));
    cat.set_text(parse_hex_color(theme.text));
    cat.set_mauve(parse_hex_color(theme.mauve));
    cat.set_red(parse_hex_color(theme.red));
    cat.set_green(parse_hex_color(theme.green));
    cat.set_yellow(parse_hex_color(theme.yellow));
    cat.set_blue(parse_hex_color(theme.blue));
    cat.set_lavender(parse_hex_color(theme.lavender));
    cat.set_peach(parse_hex_color(theme.peach));
    cat.set_teal(parse_hex_color(theme.teal));
    cat.set_pink(parse_hex_color(theme.pink));
    cat.set_flamingo(parse_hex_color(theme.flamingo));
    cat.set_rosewater(parse_hex_color(theme.rosewater));
    cat.set_sapphire(parse_hex_color(theme.sapphire));
    cat.set_sky(parse_hex_color(theme.sky));
    cat.set_maroon(parse_hex_color(theme.maroon));
}

fn apply_custom_theme_to_window(window: &MainWindow) {
    let base = window.get_custom_base().to_string();
    let surface = window.get_custom_surface().to_string();
    let text = window.get_custom_text().to_string();
    let accent = window.get_custom_accent().to_string();
    let success = window.get_custom_success().to_string();
    let danger = window.get_custom_danger().to_string();

    let base_c = parse_hex_color(&base);
    let surface_c = parse_hex_color(&surface);
    let text_c = parse_hex_color(&text);
    let accent_c = parse_hex_color(&accent);

    fn lerp_color(a: slint::Color, b: slint::Color, t: f32) -> slint::Color {
        let r = (a.red() as f32 * (1.0 - t) + b.red() as f32 * t) as u8;
        let g = (a.green() as f32 * (1.0 - t) + b.green() as f32 * t) as u8;
        let bl = (a.blue() as f32 * (1.0 - t) + b.blue() as f32 * t) as u8;
        slint::Color::from_rgb_u8(r, g, bl)
    }

    let cat = window.global::<Cat>();
    cat.set_base(base_c);
    cat.set_mantle(lerp_color(base_c, slint::Color::from_rgb_u8(0,0,0), 0.1));
    cat.set_crust(lerp_color(base_c, slint::Color::from_rgb_u8(0,0,0), 0.2));
    cat.set_surface0(surface_c);
    cat.set_surface1(lerp_color(surface_c, text_c, 0.08));
    cat.set_surface2(lerp_color(surface_c, text_c, 0.16));
    cat.set_overlay0(lerp_color(surface_c, text_c, 0.35));
    cat.set_overlay1(lerp_color(surface_c, text_c, 0.45));
    cat.set_overlay2(lerp_color(surface_c, text_c, 0.55));
    cat.set_subtext0(lerp_color(text_c, surface_c, 0.25));
    cat.set_subtext1(lerp_color(text_c, surface_c, 0.12));
    cat.set_text(text_c);
    cat.set_mauve(accent_c);
    cat.set_red(parse_hex_color(&danger));
    cat.set_green(parse_hex_color(&success));
    cat.set_yellow(lerp_color(accent_c, parse_hex_color(&danger), 0.4));
    cat.set_blue(accent_c);
    cat.set_lavender(lerp_color(accent_c, text_c, 0.3));
    cat.set_peach(lerp_color(accent_c, parse_hex_color(&danger), 0.5));
    cat.set_teal(lerp_color(accent_c, parse_hex_color(&success), 0.5));
    cat.set_pink(lerp_color(accent_c, parse_hex_color(&danger), 0.3));
    cat.set_flamingo(lerp_color(text_c, parse_hex_color(&danger), 0.2));
    cat.set_rosewater(lerp_color(text_c, parse_hex_color(&danger), 0.1));
    cat.set_sapphire(lerp_color(accent_c, parse_hex_color(&success), 0.3));
    cat.set_sky(lerp_color(accent_c, parse_hex_color(&success), 0.4));
    cat.set_maroon(lerp_color(parse_hex_color(&danger), accent_c, 0.2));
}

fn config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(format!("{}/.config/xpm/config.json", home))
}

fn load_config() -> AppConfig {
    let path = config_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(config) = serde_json::from_str::<AppConfig>(&content) {
                return config;
            }
        }
    }
    AppConfig::default()
}

fn save_config(config: &AppConfig) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(&path, json);
    }
}

fn build_config(window: &MainWindow) -> AppConfig {
    let theme = window.get_setting_theme().to_string();
    let custom_colors = if theme == "custom" {
        Some(CustomColors {
            base: window.get_custom_base().to_string(),
            surface: window.get_custom_surface().to_string(),
            text: window.get_custom_text().to_string(),
            accent: window.get_custom_accent().to_string(),
            success: window.get_custom_success().to_string(),
            danger: window.get_custom_danger().to_string(),
        })
    } else { None };
    AppConfig {
        theme,
        custom_colors,
        flatpak_enabled: window.get_setting_flatpak_enabled(),
        check_updates_on_start: window.get_setting_check_updates_on_start(),
    }
}

fn is_arch_package(path: &str) -> bool {
    let extensions = [".pkg.tar.zst", ".pkg.tar.xz", ".pkg.tar.gz", ".pkg.tar"];
    extensions.iter().any(|ext| path.ends_with(ext))
}

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

fn strip_ansi(input: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = String::new();

    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '\x1b' {
            i += 1;
            if i >= len { break; }
            match chars[i] {
                '[' => {
                    i += 1;
                    while i < len && (chars[i] >= '0' && chars[i] <= '?') { i += 1; }
                    while i < len && (chars[i] >= ' ' && chars[i] <= '/') { i += 1; }
                    if i < len && (chars[i] >= '@' && chars[i] <= '~') { i += 1; }
                }
                ']' => {
                    i += 1;
                    while i < len {
                        if chars[i] == '\x07' { i += 1; break; }
                        if chars[i] == '\x1b' && i + 1 < len && chars[i + 1] == '\\' {
                            i += 2; break;
                        }
                        i += 1;
                    }
                }
                '(' | ')' | '*' | '+' => {
                    i += 1;
                    if i < len { i += 1; }
                }
                _ => { i += 1; }
            }
        } else if chars[i] == '\r' {
            current_line.clear();
            i += 1;
            if i < len && chars[i] == '\n' {
                lines.push(std::mem::take(&mut current_line));
                i += 1;
            }
        } else if chars[i] == '\n' {
            lines.push(std::mem::take(&mut current_line));
            i += 1;
        } else {
            current_line.push(chars[i]);
            i += 1;
        }
    }
    if !current_line.is_empty() {
        lines.push(current_line);
    }
    lines.join("\n")
}

fn is_progress_bar_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() { return true; }
    let total = trimmed.chars().count();
    if total < 3 { return false; }
    let bar_chars = trimmed.chars().filter(|&c|
        c == '#' || c == '.' || c == '-' || c == '=' || c == '>'
        || c == '█' || c == '░' || c == '▒' || c == '▓' || c == '▏'
        || c == '▎' || c == '▍' || c == '▌' || c == '▋' || c == '▊'
        || c == '▉' || c == '━' || c == '─' || c == '╸' || c == '╺'
        || c == ' '
    ).count();
    bar_chars > total / 2
}

fn apply_terminal_text(buffer: &str, new_text: &str) -> String {
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

fn spawn_in_pty(cmd: &str, args: &[&str]) -> Result<(i32, u32), String> {
    use std::os::unix::io::FromRawFd;

    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;

    let ret = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    if ret != 0 {
        return Err("openpty failed".to_string());
    }

    let child: Result<std::process::Child, std::io::Error> = unsafe {
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

    unsafe { libc::close(slave); }

    match child {
        Ok(c) => Ok((master, c.id())),
        Err(e) => {
            unsafe { libc::close(master); }
            Err(format!("Failed to spawn {}: {}", cmd, e))
        }
    }
}

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

    *pid_holder.lock().unwrap() = Some(child_pid);

    let (in_tx, in_rx) = mpsc::channel::<String>();
    *input_sender.lock().unwrap() = Some(in_tx);

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
        std::mem::forget(file);
    });

    let master_fd_writer = master_fd;
    let writer_handle = thread::spawn(move || {
        use std::io::Write;
        use std::os::unix::io::FromRawFd;
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

    let _ = tx.send(UiMessage::TerminalDone(success));
}

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
            ("pkexec".to_string(), {
                let mut args = vec!["pacman".to_string(), "-S".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
    }
}

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

    let (in_tx, in_rx) = mpsc::channel::<String>();
    *input_sender.lock().unwrap() = Some(in_tx.clone());

    let escalated = Arc::new(Mutex::new(false));
    let output_buffer = Arc::new(Mutex::new(String::new()));

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

                                          {
                                              let mut buf = output_buffer_r.lock().unwrap();
                                              if buf.len() < 65536 {
                                                  buf.push_str(&cleaned);
                                              }
                                          }

                                          let is_escalated = *escalated_r.lock().unwrap();

                                          if is_escalated {
                                              let _ = tx_reader.send(UiMessage::TerminalOutput(cleaned));
                                          } else {
                                              for line in cleaned.lines() {
                                                  let trimmed = line.trim();
                                                  if trimmed.is_empty() { continue; }
                                                  if is_progress_bar_line(trimmed) { continue; }
                                                  pending_output.push_str(trimmed);
                                                  pending_output.push('\n');
                                              }

                                              let now = std::time::Instant::now();
                                              if !pending_output.is_empty() && now.duration_since(last_output_flush) >= OUTPUT_FLUSH_INTERVAL {
                                                  let lines: Vec<&str> = pending_output.lines().collect();
                                                  if lines.len() > MAX_OUTPUT_LINES {
                                                      pending_output = lines[lines.len() - MAX_OUTPUT_LINES..].join("\n");
                                                      pending_output.push('\n');
                                                  }
                                                  let _ = tx_reader.send(UiMessage::ProgressOutput(pending_output.clone()));
                                                  last_output_flush = now;
                                              }

                                              let lower = cleaned.to_lowercase();
                                              let has_conflict = CONFLICT_PATTERNS.iter().any(|p| lower.contains(&p.to_lowercase()));

                                              if has_conflict {
                                                  *escalated_r.lock().unwrap() = true;
                                                  let accumulated = output_buffer_r.lock().unwrap().clone();
                                                  let _ = tx_reader.send(UiMessage::ShowTerminalFallback(accumulated));
                                                  continue;
                                              }

                                              let is_auto_confirm = PACMAN_AUTO_CONFIRM_PATTERNS.iter().any(|p| cleaned.contains(p));
                                              if is_auto_confirm {
                                                  let _ = in_tx_r.send("y".to_string());
                                                  let _ = tx_reader.send(UiMessage::ProgressHidePrompt);
                                              } else {
                                                  let needs_user_input = PACMAN_USER_PROMPT_PATTERNS.iter().any(|p| cleaned.contains(p))
                                                  || (cleaned.contains("[y/N]") && !is_auto_confirm);
                                                  if needs_user_input {
                                                      let prompt_text = cleaned.lines()
                                                      .filter(|l| !l.trim().is_empty())
                                                      .last()
                                                      .unwrap_or(&cleaned)
                                                      .trim()
                                                      .to_string();
                                                      let _ = tx_reader.send(UiMessage::ProgressPrompt(prompt_text));
                                                  }
                                              }

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
                                                      let _ = tx_reader.send(UiMessage::OperationProgress(current_percent, trimmed));
                                                  }
                                              }
                                          }
                                      }
                                      Err(_) => break,
            }
        }
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
        let _ = tx.send(UiMessage::TerminalDone(success));
    } else {
        let _ = tx.send(UiMessage::OperationDone(success));
    }
}

fn parse_progress_fraction(line: &str, range_start: i32, range_end: i32, _total_packages: usize) -> Option<i32> {
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

fn find_package_installed(window: &MainWindow, name: &str, backend: i32) -> bool {
    let models: Vec<ModelRc<PackageData>> = vec![
        window.get_installed_packages(),
        window.get_update_packages(),
        window.get_search_installed(),
        window.get_search_available(),
        window.get_flatpak_packages(),
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

fn update_selection_in_models(window: &MainWindow, name: &str, backend: i32, selected: bool) {
    update_selection_in_model(&window.get_installed_packages(), name, backend, selected);
    update_selection_in_model(&window.get_update_packages(), name, backend, selected);
    update_selection_in_model(&window.get_search_installed(), name, backend, selected);
    update_selection_in_model(&window.get_search_available(), name, backend, selected);
    update_selection_in_model(&window.get_flatpak_packages(), name, backend, selected);
    update_selection_in_model(&window.get_category_packages(), name, backend, selected);
    update_selection_in_model(&window.get_repo_packages(), name, backend, selected);
}

fn main() {
    let subscriber = FmtSubscriber::builder()
    .with_max_level(Level::INFO)
    .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set subscriber");

    info!("Starting xPackageManager");

    let args: Vec<String> = std::env::args().collect();
    let local_package_path = args.get(1).filter(|arg| is_arch_package(arg)).cloned();

    if let Some(ref path) = local_package_path {
        info!("Opening local package: {}", path);
    }


    let window = MainWindow::new().expect("Failed to create window");

    let (tx, rx) = mpsc::channel::<UiMessage>();
    let rx = Rc::new(RefCell::new(rx));

    let terminal_input_sender: Arc<Mutex<Option<mpsc::Sender<String>>>> = Arc::new(Mutex::new(None));
    let terminal_child_pid: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

    let selected_packages: Rc<RefCell<Vec<(String, i32, bool)>>> = Rc::new(RefCell::new(Vec::new()));

    let page_size: i32 = 50;
    let full_installed: Rc<RefCell<Vec<PackageData>>> = Rc::new(RefCell::new(Vec::new()));
    let full_repo: Rc<RefCell<Vec<PackageData>>> = Rc::new(RefCell::new(Vec::new()));
    let full_category: Rc<RefCell<Vec<PackageData>>> = Rc::new(RefCell::new(Vec::new()));

    let tx_load = tx.clone();
    let tx_search = tx.clone();

    let timer = Timer::default();
    let window_weak = window.as_weak();
    let rx_clone = rx.clone();
    let tx_timer = tx.clone();
    let mut pending_terminal = String::new();
    let mut last_term_flush = std::time::Instant::now();
    let full_installed_timer = full_installed.clone();
    let full_repo_timer = full_repo.clone();
    let full_category_timer = full_category.clone();

    timer.start(TimerMode::Repeated, std::time::Duration::from_millis(50), move || {
        if let Some(window) = window_weak.upgrade() {
            let mut flush_now = false;

            while let Ok(msg) = rx_clone.borrow_mut().try_recv() {
                match msg {
                    UiMessage::PackagesLoaded { installed, updates, flatpak, stats } => {
                        let detail_name = window.get_detail_package().name.to_string();
                        let detail_backend = window.get_detail_package().backend;
                        if !detail_name.is_empty() {
                            if let Some(updated) = installed.iter()
                                .chain(flatpak.iter())
                                .find(|p| p.name.as_str() == detail_name && p.backend == detail_backend)
                            {
                                window.set_detail_package(updated.clone());
                            } else {
                                let mut dp = window.get_detail_package();
                                let was_installed = dp.installed;
                                dp.installed = installed.iter().any(|p| p.name.as_str() == detail_name && p.backend == detail_backend);
                                if dp.installed != was_installed {
                                    window.set_detail_package(dp);
                                }
                            }
                        }
                        *full_installed_timer.borrow_mut() = installed;
                        let ps = page_size as usize;
                        let inst = full_installed_timer.borrow();
                        let total = ((inst.len() + ps - 1) / ps).max(1) as i32;
                        let page: Vec<PackageData> = inst.iter().take(ps).cloned().collect();
                        window.set_installed_packages(ModelRc::new(VecModel::from(page)));
                        window.set_current_page(0);
                        window.set_total_pages(total);
                        drop(inst);
                        window.set_update_packages(ModelRc::new(VecModel::from(updates)));
                        window.set_flatpak_packages(ModelRc::new(VecModel::from(flatpak)));
                        window.set_stats(stats);
                        window.set_loading(false);
                    }
                    UiMessage::SearchResults(results) => {
                        let detail_name = window.get_detail_package().name.to_string();
                        let detail_backend = window.get_detail_package().backend;
                        let installed: Vec<PackageData> = results.iter().filter(|p| p.installed).cloned().collect();
                        let available: Vec<PackageData> = results.iter().filter(|p| !p.installed).cloned().collect();
                        if !detail_name.is_empty() {
                            if let Some(updated) = results.iter()
                                .find(|p| p.name.as_str() == detail_name && p.backend == detail_backend)
                            {
                                window.set_detail_package(updated.clone());
                            }
                        }
                        window.set_search_installed(ModelRc::new(VecModel::from(installed)));
                        window.set_search_available(ModelRc::new(VecModel::from(available)));
                        window.set_loading(false);
                    }
                    UiMessage::CategoryPackages(packages) => {
                        *full_category_timer.borrow_mut() = packages;
                        let ps = page_size as usize;
                        let cat = full_category_timer.borrow();
                        let total = ((cat.len() + ps - 1) / ps).max(1) as i32;
                        let page: Vec<PackageData> = cat.iter().take(ps).cloned().collect();
                        window.set_category_packages(ModelRc::new(VecModel::from(page)));
                        window.set_current_page(0);
                        window.set_total_pages(total);
                        window.set_loading(false);
                    }
                    UiMessage::RepoPackages(packages) => {
                        *full_repo_timer.borrow_mut() = packages;
                        let ps = page_size as usize;
                        let rp = full_repo_timer.borrow();
                        let total = ((rp.len() + ps - 1) / ps).max(1) as i32;
                        let page: Vec<PackageData> = rp.iter().take(ps).cloned().collect();
                        window.set_repo_packages(ModelRc::new(VecModel::from(page)));
                        window.set_current_page(0);
                        window.set_total_pages(total);
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
                        if pending_terminal.len() > 65536 {
                            let cut = pending_terminal.len() - 32768;
                            pending_terminal.drain(..cut);
                        }
                    }
                    UiMessage::TerminalDone(success) => {
                        flush_now = true;
                        window.set_terminal_done(true);
                        window.set_terminal_success(success);
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
                        window.set_show_progress_logs(false);
                        window.set_show_progress_popup(true);
                        window.set_show_terminal(false);
                    }
                    UiMessage::ProgressOutput(text) => {
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
                        if success {
                            window.set_selected_count(0);
                            let weak = window.as_weak();
                            let auto_close = Timer::default();
                            auto_close.start(TimerMode::SingleShot, std::time::Duration::from_millis(1500), move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_show_progress_popup(false);
                                    w.set_show_progress_logs(false);
                                }
                            });
                            std::mem::forget(auto_close);
                        } else {
                            window.set_show_progress_logs(true);
                        }
                        {
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

            if !pending_terminal.is_empty()
                && (flush_now || last_term_flush.elapsed() >= std::time::Duration::from_millis(150))
                {
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

    let tx_initial = tx.clone();
    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
        rt.block_on(async {
            let _ = tx_initial.send(UiMessage::SetLoading(true));
            load_packages_async(&tx_initial, false).await;
        });
    });

    if let Some(ref path) = local_package_path {
        if let Some(pkg_info) = get_local_package_info(path) {
            window.set_local_package(pkg_info);
            window.set_local_package_path(SharedString::from(path.as_str()));
            window.set_show_local_install(true);
            window.set_view(4);
        }
    }

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

    let repos = parse_pacman_repos();
    let repo_model: Vec<SharedString> = repos.iter().map(|r| SharedString::from(r.as_str())).collect();
    window.set_repos(ModelRc::new(VecModel::from(repo_model)));

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

    let full_installed_page = full_installed.clone();
    let full_repo_page = full_repo.clone();
    let full_category_page = full_category.clone();
    let window_weak_lp = window.as_weak();
    window.on_load_page(move |page| {
        if let Some(window) = window_weak_lp.upgrade() {
            let ps = page_size as usize;
            let start = page as usize * ps;
            let view = window.get_view();
            match view {
                0 => {
                    let data = full_installed_page.borrow();
                    let page_data: Vec<PackageData> = data.iter().skip(start).take(ps).cloned().collect();
                    let total = ((data.len() + ps - 1) / ps).max(1) as i32;
                    window.set_installed_packages(ModelRc::new(VecModel::from(page_data)));
                    window.set_total_pages(total);
                }
                7 => {
                    let data = full_category_page.borrow();
                    let page_data: Vec<PackageData> = data.iter().skip(start).take(ps).cloned().collect();
                    let total = ((data.len() + ps - 1) / ps).max(1) as i32;
                    window.set_category_packages(ModelRc::new(VecModel::from(page_data)));
                    window.set_total_pages(total);
                }
                8 => {
                    let data = full_repo_page.borrow();
                    let page_data: Vec<PackageData> = data.iter().skip(start).take(ps).cloned().collect();
                    let total = ((data.len() + ps - 1) / ps).max(1) as i32;
                    window.set_repo_packages(ModelRc::new(VecModel::from(page_data)));
                    window.set_total_pages(total);
                }
                _ => {}
            }
        }
    });

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

    let tx_req_install = tx.clone();
    let req_install_input = terminal_input_sender.clone();
    let req_install_pid = terminal_child_pid.clone();
    window.on_request_install(move |name, backend| {
        let tx = tx_req_install.clone();
        let n = name.to_string();
        let input = req_install_input.clone();
        let pid = req_install_pid.clone();
        thread::spawn(move || {
            let title = format!("Installing {}", n);
            run_managed_operation(&tx, &title, "install", &[n], backend, &input, &pid);
        });
    });

    let tx_req_remove = tx.clone();
    let req_remove_input = terminal_input_sender.clone();
    let req_remove_pid = terminal_child_pid.clone();
    window.on_request_remove(move |name, backend| {
        let tx = tx_req_remove.clone();
        let n = name.to_string();
        let input = req_remove_input.clone();
        let pid = req_remove_pid.clone();
        thread::spawn(move || {
            let title = format!("Removing {}", n);
            run_managed_operation(&tx, &title, "remove", &[n], backend, &input, &pid);
        });
    });

    let tx_req_update = tx.clone();
    let req_update_input = terminal_input_sender.clone();
    let req_update_pid = terminal_child_pid.clone();
    window.on_request_update(move |name, backend| {
        let tx = tx_req_update.clone();
        let n = name.to_string();
        let input = req_update_input.clone();
        let pid = req_update_pid.clone();
        thread::spawn(move || {
            let title = format!("Updating {}", n);
            run_managed_operation(&tx, &title, "update", &[n], backend, &input, &pid);
        });
    });


    let window_weak_cp = window.as_weak();
    window.on_close_progress_popup(move || {
        if let Some(window) = window_weak_cp.upgrade() {
            window.set_show_progress_popup(false);
        }
    });

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

    let selected_pkgs_toggle = selected_packages.clone();
    let window_weak_tps = window.as_weak();
    window.on_toggle_package_selected(move |name, backend, selected| {
        let name_str = name.to_string();
        let mut sel = selected_pkgs_toggle.borrow_mut();

        if let Some(window) = window_weak_tps.upgrade() {
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

    let selected_pkgs_bi = selected_packages.clone();
    let tx_bulk_install = tx.clone();
    let bulk_install_input = terminal_input_sender.clone();
    let bulk_install_pid = terminal_child_pid.clone();
    window.on_bulk_install(move || {
        let sel = selected_pkgs_bi.borrow();
        let uninstalled: Vec<&(String, i32, bool)> = sel.iter().filter(|(_, _, inst)| !inst).collect();
        if uninstalled.is_empty() { return; }
        let names: Vec<String> = uninstalled.iter().map(|(n, _, _)| n.clone()).collect();
        let backend = uninstalled[0].1;
        let tx = tx_bulk_install.clone();
        let input = bulk_install_input.clone();
        let pid = bulk_install_pid.clone();
        let title = format!("Installing {} packages", names.len());
        thread::spawn(move || {
            run_managed_operation(&tx, &title, "install", &names, backend, &input, &pid);
        });
    });

    let selected_pkgs_br = selected_packages.clone();
    let tx_bulk_remove = tx.clone();
    let bulk_remove_input = terminal_input_sender.clone();
    let bulk_remove_pid = terminal_child_pid.clone();
    window.on_bulk_remove(move || {
        let sel = selected_pkgs_br.borrow();
        let installed: Vec<&(String, i32, bool)> = sel.iter().filter(|(_, _, inst)| *inst).collect();
        if installed.is_empty() { return; }
        let names: Vec<String> = installed.iter().map(|(n, _, _)| n.clone()).collect();
        let backend = installed[0].1;
        let tx = tx_bulk_remove.clone();
        let input = bulk_remove_input.clone();
        let pid = bulk_remove_pid.clone();
        let title = format!("Removing {} packages", names.len());
        thread::spawn(move || {
            run_managed_operation(&tx, &title, "remove", &names, backend, &input, &pid);
        });
    });

    let tx_clean = tx.clone();
    let clean_input = terminal_input_sender.clone();
    let clean_pid = terminal_child_pid.clone();
    window.on_clean_package_cache(move || {
        info!("Clean package cache");
        let tx = tx_clean.clone();
        let input = clean_input.clone();
        let pid = clean_pid.clone();
        thread::spawn(move || {
            run_in_terminal(&tx, "Cleaning Package Cache", "pkexec", &["pacman", "-Scc"], &input, &pid);
        });
    });

    let tx_orphans = tx.clone();
    let orphan_input = terminal_input_sender.clone();
    let orphan_pid = terminal_child_pid.clone();
    window.on_remove_orphans(move || {
        info!("Remove orphans");
        let tx = tx_orphans.clone();
        let input = orphan_input.clone();
        let pid = orphan_pid.clone();
        thread::spawn(move || {
            run_in_terminal(&tx, "Removing Orphan Packages", "pkexec",
                &["bash", "-c", "pacman -Qdtq | pacman -Rns -"], &input, &pid);
        });
    });


    let tx_sync = tx.clone();
    window.on_sync_databases(move || {
        info!("Check for updates");
        let tx = tx_sync.clone();
        thread::spawn(move || {
            let _ = tx.send(UiMessage::SetBusy(true));
            let _ = tx.send(UiMessage::SetProgress(5));
            let _ = tx.send(UiMessage::SetProgressText("Syncing pacman databases...".to_string()));
            let _ = tx.send(UiMessage::SetStatus("Syncing pacman databases...".to_string()));

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

            let _ = tx.send(UiMessage::SetProgress(75));
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

    window.on_open_url(move |url| {
        info!("Open URL: {}", url);
        let _ = open::that(url.as_str());
    });

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

    let window_weak = window.as_weak();
    window.on_cancel_local_install(move || {
        info!("Cancelled local package install");
        if let Some(window) = window_weak.upgrade() {
            window.set_show_local_install(false);
            window.set_view(0);
        }
    });

    let term_input = terminal_input_sender.clone();
    window.on_terminal_send_input(move |text| {
        let text = text.to_string();
        if let Some(sender) = term_input.lock().unwrap().as_ref() {
            let _ = sender.send(text);
        }
    });

    let tx_close = tx.clone();
    let close_pid = terminal_child_pid.clone();
    let close_input = terminal_input_sender.clone();
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
        if let Some(pid) = *close_pid.lock().unwrap() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        *close_input.lock().unwrap() = None;

        let _ = tx_close.send(UiMessage::HideTerminal);
    });

    let window_weak_at = window.as_weak();
    window.on_apply_theme(move |theme_name| {
        if let Some(window) = window_weak_at.upgrade() {
            let name = theme_name.to_string();
            window.set_setting_theme(SharedString::from(&name));
            match name.as_str() {
                "default-dark" => apply_theme_to_window(&window, &DEFAULT_DARK),
                "catppuccin-macchiato" => apply_theme_to_window(&window, &CATPPUCCIN_MACCHIATO),
                "custom" => apply_custom_theme_to_window(&window),
                _ => {}
            }
            let config = build_config(&window);
            save_config(&config);
        }
    });

    let window_weak_act = window.as_weak();
    window.on_apply_custom_theme(move || {
        if let Some(window) = window_weak_act.upgrade() {
            apply_custom_theme_to_window(&window);
            let config = build_config(&window);
            save_config(&config);
        }
    });

    let window_weak_ss = window.as_weak();
    window.on_save_settings(move || {
        if let Some(window) = window_weak_ss.upgrade() {
            let config = build_config(&window);
            save_config(&config);
        }
    });

    let config = load_config();
    window.set_setting_theme(SharedString::from(&config.theme));
    window.set_setting_flatpak_enabled(config.flatpak_enabled);
    window.set_setting_check_updates_on_start(config.check_updates_on_start);
    if let Some(ref custom) = config.custom_colors {
        window.set_custom_base(SharedString::from(&custom.base));
        window.set_custom_surface(SharedString::from(&custom.surface));
        window.set_custom_text(SharedString::from(&custom.text));
        window.set_custom_accent(SharedString::from(&custom.accent));
        window.set_custom_success(SharedString::from(&custom.success));
        window.set_custom_danger(SharedString::from(&custom.danger));
    }
    match config.theme.as_str() {
        "default-dark" => apply_theme_to_window(&window, &DEFAULT_DARK),
        "custom" => apply_custom_theme_to_window(&window),
        _ => {}
    }

    info!("Running application");
    window.run().expect("Failed to run application");
}

async fn load_packages_async(tx: &mpsc::Sender<UiMessage>, check_updates: bool) {
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

    let installed_fut = alpm.list_installed();
    let cache_fut = alpm.get_cache_size();
    let orphans_fut = alpm.list_orphans();
    let flatpak_avail_fut = flatpak.list_available();
    let desktop_map_fut = tokio::task::spawn_blocking(build_desktop_name_map);
    let flatpak_map_fut = tokio::task::spawn_blocking(build_flatpak_name_map);

    let flatpak_updates_fut = if check_updates { Some(flatpak.list_updates()) } else { None };
    let checkupdates_fut = if check_updates {
        Some(tokio::task::spawn_blocking(|| {
            std::process::Command::new("checkupdates")
            .output()
            .or_else(|_| std::process::Command::new("pacman").args(["-Qu"]).output())
        }))
    } else { None };
    let plasmoid_fut = if check_updates { Some(tokio::task::spawn_blocking(list_plasmoids_with_updates)) } else { None };

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

    let flatpak_updates = if let Some(fut) = flatpak_updates_fut {
        fut.await.unwrap_or_else(|e| { error!("Failed to list flatpak updates: {}", e); Vec::new() })
    } else { Vec::new() };
    let checkupdates_res = if let Some(fut) = checkupdates_fut { Some(fut.await) } else { None };
    let (_installed_plasmoids, plasmoid_updates) = if let Some(fut) = plasmoid_fut {
        fut.await.unwrap_or_else(|_| (Vec::new(), Vec::new()))
    } else { (Vec::new(), Vec::new()) };
    let installed_pacman = installed_res.unwrap_or_else(|e| { error!("Failed to list installed: {}", e); Vec::new() });
    let cache_size = cache_res.unwrap_or(0);
    let orphan_count = orphans_res.map(|o| o.len()).unwrap_or(0);

    let flatpak_packages = flatpak_avail_res.unwrap_or_else(|e| { error!("Failed to list flatpak: {}", e); Vec::new() });

    let desktop_map = desktop_map_res.unwrap_or_default();
    let flatpak_name_map = flatpak_map_res.unwrap_or_default();

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

    let installed_ui: Vec<PackageData> = installed_pacman
    .iter()
    .map(|p| package_to_ui(p, update_names.contains(&p.name), &desktop_map))
    .collect();

    let updates_ui: Vec<PackageData> = updates.iter().map(|u| update_to_ui(u)).collect();

    let flatpak_ui: Vec<PackageData> = flatpak_packages
    .iter()
    .map(|p| {
        let has_update = flatpak_update_names.contains(&p.name);
        let (display_name, summary) = flatpak_name_map
        .get(&p.name.to_lowercase())
        .cloned()
        .unwrap_or_else(|| {
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
         backend: 1,
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

    let total_updates = updates.len() + flatpak_updates.len() + plasmoid_updates.len();

    let mut all_updates_ui = updates_ui.clone();
    all_updates_ui.extend(plasmoid_updates.clone());

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
        stats,
    });
}

fn list_plasmoids_with_updates() -> (Vec<PackageData>, Vec<PackageData>) {
    let mut plasmoids = Vec::new();
    let mut updates = Vec::new();

    let home = std::env::var("HOME").unwrap_or_default();
    let user_path = std::path::PathBuf::from(&home).join(".local/share/plasma/plasmoids");

    let paths = [
        Some(user_path),
        Some(std::path::PathBuf::from("/usr/share/plasma/plasmoids")),
    ];

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
                        backend: 3,
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

fn fetch_store_versions() -> Vec<(String, String)> {
    let mut versions = Vec::new();

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

fn version_is_newer(store_version: &str, current_version: &str) -> bool {
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

struct PlasmoidInfo {
    id: String,
    name: String,
    version: String,
    description: String,
}

fn parse_plasmoid_json(path: &std::path::Path) -> PlasmoidInfo {
    if let Ok(content) = std::fs::read_to_string(path) {
        if let Ok(json) = serde_json::from_str::<Value>(&content) {
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

    let pacman_results = match alpm.search(query).await {
        Ok(r) => r,
        Err(e) => {
            error!("Pacman search failed: {}", e);
            Vec::new()
        }
    };

    let flatpak_results = match flatpak.search(query).await {
        Ok(r) => r,
        Err(e) => {
            error!("Flatpak search failed: {}", e);
            Vec::new()
        }
    };

    let desktop_map = build_desktop_name_map();

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

    results.truncate(100);

    let _ = tx.send(UiMessage::SearchResults(results));
}

async fn load_category_packages(tx: &mpsc::Sender<UiMessage>, category: &str) {
    let mut packages = Vec::new();

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

    let appstream_path = "/var/lib/flatpak/appstream/flathub/x86_64";
    if let Ok(entries) = std::fs::read_dir(appstream_path) {
        for entry in entries.flatten() {
            let xml_path = entry.path().join("appstream.xml.gz");
            if xml_path.exists() {
                parse_appstream_for_category(&xml_path, category, &mut packages);
            }
        }
    }

    packages.sort_by(|a, b| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()));
    packages.dedup_by(|a, b| a.name == b.name);

    let _ = tx.send(UiMessage::CategoryPackages(packages));
}

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

    if no_display || hidden || name.is_empty() {
        return None;
    }

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

    let is_flatpak = path.to_string_lossy().contains("flatpak");
    let backend = if is_flatpak { 1 } else { 0 };
    let repo = if is_flatpak { "flathub" } else { "native" };

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

fn parse_appstream_for_category(path: &std::path::Path, target_category: &str, packages: &mut Vec<PackageData>) {
    if let Ok(file) = std::fs::File::open(path) {
        let decoder = flate2::read::GzDecoder::new(file);
        let reader = std::io::BufReader::new(decoder);
        parse_appstream_xml(reader, target_category, packages);
    }
}

fn build_flatpak_name_map() -> HashMap<String, (String, String)> {
    let mut name_map = HashMap::new();

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

        if let Ok(entries) = std::fs::read_dir(base_path) {
            for remote_entry in entries.flatten() {
                let remote_path = remote_entry.path();
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

fn parse_appstream_names<R: std::io::Read>(reader: R, name_map: &mut HashMap<String, (String, String)>) {
    use std::io::Read;

    let mut content = String::new();
    let mut buf_reader = std::io::BufReader::new(reader);
    if buf_reader.read_to_string(&mut content).is_err() {
        return;
    }

    let mut pos = 0;
    while let Some(comp_start) = content[pos..].find("<component type=\"desktop") {
        let abs_start = pos + comp_start;

        if let Some(comp_end) = content[abs_start..].find("</component>") {
            let component = &content[abs_start..abs_start + comp_end + 12];

            if let Some(id) = extract_tag_content(component, "id") {
                let name = extract_first_name(component);
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

fn extract_first_name(xml: &str) -> Option<String> {
    if let Some(start) = xml.find("<name>") {
        let content_start = start + 6;
        if let Some(end) = xml[content_start..].find("</name>") {
            return Some(xml[content_start..content_start + end].to_string());
        }
    }
    None
}

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
                              installed: false,
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

fn build_desktop_name_map() -> HashMap<String, String> {
    let mut map = HashMap::new();

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
                                exec = line.strip_prefix("Exec=").unwrap_or("")
                                .split_whitespace().next().unwrap_or("")
                                .rsplit('/').next().unwrap_or("").to_string();
                            } else if line.starts_with("NoDisplay=true") {
                                no_display = true;
                            }
                        }
                        if !name.is_empty() && !no_display {
                            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                map.insert(stem.to_lowercase(), name.clone());
                            }
                            if !exec.is_empty() {
                                map.entry(exec.to_lowercase()).or_insert(name);
                            }
                        }
                    }
                }
            }
        }
    }

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

fn humanize_package_name(name: &str, desktop_map: &HashMap<String, String>) -> String {
    if let Some(human_name) = desktop_map.get(&name.to_lowercase()) {
        return human_name.clone();
    }

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

fn load_repo_packages(tx: &std::sync::mpsc::Sender<UiMessage>, repo: &str) {
    let installed_names: std::collections::HashSet<String> = std::process::Command::new("pacman")
    .args(["-Qq"])
    .output()
    .ok()
    .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.to_string()).collect())
    .unwrap_or_default();

    let desktop_map = build_desktop_name_map();

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
