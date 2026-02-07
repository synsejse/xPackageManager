use slint::SharedString;
use std::path::Path;

use crate::PackageData;

/// Check if the system is running XeroLinux
pub fn is_xerolinux_distro() -> bool {
    // Check ID and NAME fields in /etc/os-release
    if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
        for line in content.lines() {
            let line_lower = line.to_lowercase();
            if (line_lower.starts_with("id=")
                || line_lower.starts_with("name=")
                || line_lower.starts_with("pretty_name="))
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
            if (line_lower.starts_with("distrib_id=")
                || line_lower.starts_with("distrib_description="))
                && line_lower.contains("xerolinux")
            {
                return true;
            }
        }
    }
    false
}

/// Check if a file path is a valid Arch Linux package
pub fn is_arch_package(path: &str) -> bool {
    let extensions = [".pkg.tar.zst", ".pkg.tar.xz", ".pkg.tar.gz", ".pkg.tar"];
    extensions.iter().any(|ext| path.ends_with(ext))
}

/// Extract package info from a local package file
pub fn get_local_package_info(path: &str) -> Option<PackageData> {
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
pub fn format_size(bytes: u64) -> String {
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
pub fn strip_ansi(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == 0x1b {
            i += 1;
            if i >= len {
                break;
            }
            match bytes[i] {
                b'[' => {
                    // CSI sequence: ESC[ (params) (intermediates) final_byte
                    i += 1;
                    while i < len && bytes[i] >= 0x30 && bytes[i] <= 0x3F {
                        i += 1;
                    }
                    while i < len && bytes[i] >= 0x20 && bytes[i] <= 0x2F {
                        i += 1;
                    }
                    if i < len && bytes[i] >= 0x40 && bytes[i] <= 0x7E {
                        i += 1;
                    }
                }
                b']' => {
                    // OSC sequence: ESC] ... (BEL or ST)
                    i += 1;
                    while i < len {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < len && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                b'(' | b')' | b'*' | b'+' => {
                    i += 1;
                    if i < len {
                        i += 1;
                    }
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
pub fn apply_terminal_text(buffer: &str, new_text: &str) -> String {
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
