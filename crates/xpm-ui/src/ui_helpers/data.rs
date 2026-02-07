use slint::SharedString;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;
use xpm_alpm::AlpmBackend;
use xpm_core::package::{PackageBackend, PackageStatus, UpdateInfo, Version};
use xpm_core::source::PackageSource;
use xpm_flatpak::FlatpakBackend;

use super::types::UiMessage;
use crate::{PackageData, StatsData};

use super::backends::{init_backends, UiBackends};
use super::utils::format_size;

/// Load packages from backends (runs in background thread)
pub async fn load_packages_async(tx: &std::sync::mpsc::Sender<UiMessage>, check_updates: bool) {
    let UiBackends { alpm, flatpak } = match init_backends() {
        Some(backends) => backends,
        None => return,
    };

    // Run all data loading concurrently for fast startup
    let installed_fut = alpm.list_installed();
    let cache_fut = alpm.get_cache_size();
    let orphans_fut = alpm.list_orphans();
    let flatpak_avail_fut = flatpak.list_available();
    let desktop_map_fut = tokio::task::spawn_blocking(build_desktop_name_map);
    let flatpak_map_fut = tokio::task::spawn_blocking(build_flatpak_name_map);

    // Only check for updates when explicitly requested
    let flatpak_updates_fut = if check_updates {
        Some(flatpak.list_updates())
    } else {
        None
    };
    let checkupdates_fut = if check_updates {
        Some(tokio::task::spawn_blocking(|| {
            std::process::Command::new("checkupdates")
                .output()
                .or_else(|_| std::process::Command::new("pacman").args(["-Qu"]).output())
        }))
    } else {
        None
    };
    let plasmoid_fut = if check_updates {
        Some(tokio::task::spawn_blocking(list_plasmoids_with_updates))
    } else {
        None
    };
    let firmware_fut = if check_updates {
        Some(tokio::task::spawn_blocking(list_firmware))
    } else {
        None
    };

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
        fut.await.unwrap_or_else(|e| {
            tracing::error!("Failed to list flatpak updates: {}", e);
            Vec::new()
        })
    } else {
        Vec::new()
    };
    let checkupdates_res = if let Some(fut) = checkupdates_fut {
        Some(fut.await)
    } else {
        None
    };
    let (_installed_plasmoids, plasmoid_updates) = if let Some(fut) = plasmoid_fut {
        fut.await.unwrap_or_else(|_| (Vec::new(), Vec::new()))
    } else {
        (Vec::new(), Vec::new())
    };
    let firmware_packages: Vec<PackageData> = if let Some(fut) = firmware_fut {
        fut.await.unwrap_or_else(|_| Vec::new())
    } else {
        Vec::new()
    };

    // Process results
    let installed_pacman = installed_res.unwrap_or_else(|e| {
        tracing::error!("Failed to list installed: {}", e);
        Vec::new()
    });
    let cache_size = cache_res.unwrap_or(0);
    let orphan_count = orphans_res.map(|o| o.len()).unwrap_or(0);

    let flatpak_packages = flatpak_avail_res.unwrap_or_else(|e| {
        tracing::error!("Failed to list flatpak: {}", e);
        Vec::new()
    });

    let desktop_map = desktop_map_res.unwrap_or_default();
    let flatpak_name_map = flatpak_map_res.unwrap_or_default();

    // Parse checkupdates output
    let mut updates: Vec<UpdateInfo> = Vec::new();
    if let Some(Ok(Ok(result))) = checkupdates_res {
        if result.status.success() {
            let stdout = String::from_utf8_lossy(&result.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 {
                    updates.push(UpdateInfo {
                        name: parts[0].to_string(),
                        current_version: Version::new(parts[1]),
                        new_version: Version::new(parts[3]),
                        backend: PackageBackend::Pacman,
                        repository: String::new(),
                        download_size: 0,
                    });
                } else if parts.len() >= 2 {
                    updates.push(UpdateInfo {
                        name: parts[0].to_string(),
                        current_version: Version::new(""),
                        new_version: Version::new(parts[1]),
                        backend: PackageBackend::Pacman,
                        repository: String::new(),
                        download_size: 0,
                    });
                }
            }
        }
    }

    let update_names: HashSet<String> = updates.iter().map(|u| u.name.clone()).collect();
    let flatpak_update_names: HashSet<String> =
        flatpak_updates.iter().map(|u| u.name.clone()).collect();

    // Convert to UI types
    let installed_ui: Vec<PackageData> = installed_pacman
        .iter()
        .map(|p| package_to_ui(p, update_names.contains(&p.name), &desktop_map))
        .collect();

    let updates_ui: Vec<PackageData> = updates.iter().map(update_to_ui).collect();

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
                    let fallback_name = p
                        .name
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
                installed: matches!(p.status, PackageStatus::Installed | PackageStatus::Orphan),
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
    let total_updates =
        updates.len() + flatpak_updates.len() + plasmoid_updates.len() + firmware_update_count;

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

/// Search packages (runs in background thread)
pub async fn search_packages_async(tx: &std::sync::mpsc::Sender<UiMessage>, query: &str) {
    let UiBackends { alpm, flatpak } = match init_backends() {
        Some(backends) => backends,
        None => {
            let _ = tx.send(UiMessage::SearchResults(Vec::new()));
            return;
        }
    };

    // Search pacman
    let pacman_results = match alpm.search(query).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Pacman search failed: {}", e);
            Vec::new()
        }
    };

    // Search flatpak
    let flatpak_results = match flatpak.search(query).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Flatpak search failed: {}", e);
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
pub async fn load_category_packages(tx: &std::sync::mpsc::Sender<UiMessage>, category: &str) {
    let mut packages = Vec::new();

    // Load from desktop files (native packages)
    let desktop_dirs = [
        "/usr/share/applications",
        "/var/lib/flatpak/exports/share/applications",
    ];

    let home = std::env::var("HOME").unwrap_or_default();
    let user_flatpak = format!("{}/.local/share/flatpak/exports/share/applications", home);

    for dir in desktop_dirs
        .iter()
        .chain(std::iter::once(&user_flatpak.as_str()))
    {
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
    packages.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
    });
    packages.dedup_by(|a, b| a.name == b.name);

    let _ = tx.send(UiMessage::CategoryPackages(packages));
}

/// Load packages from a specific pacman repo with humanized names via expac
pub fn load_repo_packages(tx: &std::sync::mpsc::Sender<UiMessage>, repo: &str) {
    // Get installed package names
    let installed_names: HashSet<String> = std::process::Command::new("pacman")
        .args(["-Qq"])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.to_string())
                .collect()
        })
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
                        if let Some((_, store_ver)) =
                            store_versions.iter().find(|(name, _)| name == &info.name)
                        {
                            let is_newer = version_is_newer(store_ver, &info.version);
                            (
                                is_newer,
                                if is_newer {
                                    store_ver.clone()
                                } else {
                                    String::new()
                                },
                            )
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
                        repository: SharedString::from(if is_user {
                            "kde-store"
                        } else {
                            "system"
                        }),
                        backend: 3, // 3 = plasmoid
                        installed: true,
                        has_update,
                        installed_size: SharedString::from(""),
                        licenses: SharedString::from(""),
                        url: SharedString::from(format!(
                            "https://store.kde.org/search?search={}",
                            info.name.replace(' ', "+")
                        )),
                        dependencies: SharedString::from(""),
                        required_by: SharedString::from(""),
                        icon_name: SharedString::from(""),
                        selected: false,
                    };

                    if has_update {
                        let mut update_pkg = pkg.clone();
                        update_pkg.version =
                            SharedString::from(format!("{} → {}", info.version, new_version));
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
    let url =
        "https://api.kde-look.org/ocs/v1/content/data?categories=705&pagesize=200&format=json";

    if let Ok(output) = std::process::Command::new("curl")
        .args(["-s", "--max-time", "15", url])
        .output()
    {
        if output.status.success() {
            let response = String::from_utf8_lossy(&output.stdout);
            if let Ok(json) = serde_json::from_str::<Value>(&response) {
                if let Some(data) = json
                    .get("ocs")
                    .and_then(|o| o.get("data"))
                    .and_then(|d| d.as_array())
                {
                    for item in data {
                        let name = item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let version = item
                            .get("version")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
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
                        let name = device
                            .get("Name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Unknown Device")
                            .to_string();
                        let version = device
                            .get("Version")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let vendor = device
                            .get("Vendor")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let device_id = device
                            .get("DeviceId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let updatable = device
                            .get("Flags")
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
                        let device_id = device
                            .get("DeviceId")
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
fn parse_plasmoid_json(path: &Path) -> PlasmoidInfo {
    if let Ok(content) = std::fs::read_to_string(path) {
        if let Ok(json) = serde_json::from_str::<Value>(&content) {
            // KDE Plasma 6 format has fields under "KPlugin" object
            if let Some(kplugin) = json.get("KPlugin") {
                let id = kplugin
                    .get("Id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = kplugin
                    .get("Name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown")
                    .to_string();
                let version = kplugin
                    .get("Version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let desc = kplugin
                    .get("Description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                return PlasmoidInfo {
                    id,
                    name,
                    version,
                    description: desc,
                };
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
fn parse_plasmoid_desktop(path: &Path) -> PlasmoidInfo {
    if let Ok(content) = std::fs::read_to_string(path) {
        let mut id = String::new();
        let mut name = "Unknown".to_string();
        let mut version = "unknown".to_string();
        let mut desc = String::new();

        for line in content.lines() {
            if line.starts_with("Name=") && !line.contains('[') {
                name = line.strip_prefix("Name=").unwrap_or("Unknown").to_string();
            } else if line.starts_with("X-KDE-PluginInfo-Version=") {
                version = line
                    .strip_prefix("X-KDE-PluginInfo-Version=")
                    .unwrap_or("unknown")
                    .to_string();
            } else if line.starts_with("X-KDE-PluginInfo-Name=") {
                id = line
                    .strip_prefix("X-KDE-PluginInfo-Name=")
                    .unwrap_or("")
                    .to_string();
            } else if line.starts_with("Comment=") && !line.contains('[') {
                desc = line.strip_prefix("Comment=").unwrap_or("").to_string();
            }
        }
        PlasmoidInfo {
            id,
            name,
            version,
            description: desc,
        }
    } else {
        PlasmoidInfo {
            id: String::new(),
            name: "Unknown".to_string(),
            version: "unknown".to_string(),
            description: String::new(),
        }
    }
}

/// Parse a desktop file and return PackageData if it matches the category
fn parse_desktop_file(path: &Path, target_category: &str) -> Option<PackageData> {
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
fn parse_appstream_for_category(
    path: &Path,
    target_category: &str,
    packages: &mut Vec<PackageData>,
) {
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
        let base_path = Path::new(base_dir);
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
fn parse_appstream_names<R: std::io::Read>(
    reader: R,
    name_map: &mut HashMap<String, (String, String)>,
) {
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
fn parse_appstream_xml<R: std::io::Read>(
    reader: R,
    target_category: &str,
    packages: &mut Vec<PackageData>,
) {
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
                current_id = line
                    .strip_prefix("<id>")
                    .unwrap_or("")
                    .strip_suffix("</id>")
                    .unwrap_or("")
                    .to_string();
            } else if line.starts_with("<name>")
                && line.ends_with("</name>")
                && current_name.is_empty()
            {
                current_name = line
                    .strip_prefix("<name>")
                    .unwrap_or("")
                    .strip_suffix("</name>")
                    .unwrap_or("")
                    .to_string();
            } else if line.starts_with("<name") && line.contains("xml:lang") {
                skip_lang = true;
            } else if line == "</name>" && skip_lang {
                skip_lang = false;
            } else if line.starts_with("<summary>")
                && line.ends_with("</summary>")
                && current_summary.is_empty()
            {
                current_summary = line
                    .strip_prefix("<summary>")
                    .unwrap_or("")
                    .strip_suffix("</summary>")
                    .unwrap_or("")
                    .to_string();
            } else if line == "<categories>" {
                in_categories = true;
            } else if line == "</categories>" {
                in_categories = false;
            } else if in_categories
                && line.starts_with("<category>")
                && line.ends_with("</category>")
            {
                let cat = line
                    .strip_prefix("<category>")
                    .unwrap_or("")
                    .strip_suffix("</category>")
                    .unwrap_or("")
                    .to_string();
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
                                exec = line
                                    .strip_prefix("Exec=")
                                    .unwrap_or("")
                                    .split_whitespace()
                                    .next()
                                    .unwrap_or("")
                                    .rsplit('/')
                                    .next()
                                    .unwrap_or("")
                                    .to_string();
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
    if let Ok(output) = std::process::Command::new("pacman").args(["-Ql"]).output() {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.contains("/usr/share/applications/") && line.ends_with(".desktop") {
                    let parts: Vec<&str> = line.splitn(2, ' ').collect();
                    if parts.len() == 2 {
                        let pkg_name = parts[0];
                        let desktop_path = parts[1].trim();
                        let file_stem = Path::new(desktop_path)
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
pub fn humanize_package_name(name: &str, desktop_map: &HashMap<String, String>) -> String {
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

fn package_to_ui(
    package: &xpm_core::package::Package,
    has_update: bool,
    desktop_map: &HashMap<String, String>,
) -> PackageData {
    let display_name = humanize_package_name(&package.name, desktop_map);
    PackageData {
        name: SharedString::from(package.name.as_str()),
        display_name: SharedString::from(&display_name),
        version: SharedString::from(package.version.to_string().as_str()),
        description: SharedString::from(package.description.as_str()),
        repository: SharedString::from(package.repository.as_str()),
        backend: 0,
        installed: matches!(
            package.status,
            PackageStatus::Installed | PackageStatus::Orphan
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

fn update_to_ui(update: &UpdateInfo) -> PackageData {
    PackageData {
        name: SharedString::from(update.name.as_str()),
        display_name: SharedString::from(update.name.as_str()),
        version: SharedString::from(update.new_version.to_string().as_str()),
        description: SharedString::from(format!(
            "{} → {}",
            update.current_version, update.new_version
        )),
        repository: SharedString::from(update.repository.as_str()),
        backend: 0,
        installed: true,
        has_update: true,
        installed_size: SharedString::from(""),
        licenses: SharedString::from(""),
        url: SharedString::from(""),
        dependencies: SharedString::from(""),
        required_by: SharedString::from(""),
        icon_name: SharedString::from(""),
        selected: false,
    }
}
