//! ALPM backend implementation.

use crate::cache::CacheManager;
use alpm::{Alpm, SigLevel};
use async_trait::async_trait;
use std::path::Path;
use tracing::{info, warn};
use xpm_core::{
    error::{Error, Result},
    operation::{Operation, OperationKind, OperationResult},
    package::{
        InstallReason, Package, PackageBackend, PackageInfo, PackageStatus, SearchResult,
        UpdateInfo, Version,
    },
    source::{PackageSource, ProgressCallback},
};
use xpm_pacman::{config::PacmanConfig, pacman_conf};

/// Default paths for Arch Linux.
const DEFAULT_ROOT: &str = "/";
const DEFAULT_DBPATH: &str = "/var/lib/pacman";

const TEMP_SYNC_DIR: &str = "xpm-checkupdates";

/// Configuration for the ALPM backend.
#[derive(Debug, Clone)]
pub struct AlpmConfig {
    /// Root directory for package installation.
    pub root: String,
    /// Database path.
    pub dbpath: String,
    /// Cache directories.
    pub cache_dirs: Vec<String>,
    /// Hook directories.
    pub hook_dirs: Vec<String>,
    /// GPG directory.
    pub gpgdir: String,
    /// Log file path.
    pub logfile: String,
}

impl Default for AlpmConfig {
    fn default() -> Self {
        Self {
            root: DEFAULT_ROOT.to_string(),
            dbpath: DEFAULT_DBPATH.to_string(),
            cache_dirs: vec!["/var/cache/pacman/pkg".to_string()],
            hook_dirs: vec![
                "/etc/pacman.d/hooks".to_string(),
                "/usr/share/libalpm/hooks".to_string(),
            ],
            gpgdir: "/etc/pacman.d/gnupg".to_string(),
            logfile: "/var/log/pacman.log".to_string(),
        }
    }
}

/// The pacman/libalpm backend.
pub struct AlpmBackend {
    config: AlpmConfig,
    cache_manager: CacheManager,
}

// ALPM handle is not Send/Sync, so we create it on demand in blocking tasks.
unsafe impl Send for AlpmBackend {}
unsafe impl Sync for AlpmBackend {}

impl AlpmBackend {
    /// Creates a new ALPM backend with default configuration.
    pub fn new() -> Result<Self> {
        Self::with_config(AlpmConfig::default())
    }

    /// Creates a new ALPM backend with custom configuration.
    pub fn with_config(config: AlpmConfig) -> Result<Self> {
        // Verify the database path exists.
        if !Path::new(&config.dbpath).exists() {
            return Err(Error::DatabaseError(format!(
                "Database path does not exist: {}",
                config.dbpath
            )));
        }

        Ok(Self {
            cache_manager: CacheManager::new(&config.cache_dirs),
            config,
        })
    }

    fn open_handle(config: &AlpmConfig, dbpath_override: Option<&Path>) -> Result<Alpm> {
        let dbpath = match dbpath_override {
            Some(path) => path.to_string_lossy().to_string(),
            None => config.dbpath.clone(),
        };

        Alpm::new(config.root.clone(), dbpath).map_err(|e| Error::DatabaseError(e.to_string()))
    }

    fn with_handle<T>(
        config: &AlpmConfig,
        dbpath_override: Option<&Path>,
        f: impl FnOnce(&Alpm) -> Result<T>,
    ) -> Result<T> {
        let handle = Self::open_handle(config, dbpath_override)?;
        f(&handle)
    }

    fn register_default_syncdbs(handle: &Alpm) {
        let repos = Self::repo_configs();
        let siglevel = SigLevel::PACKAGE_OPTIONAL | SigLevel::DATABASE_OPTIONAL;
        for repo in repos {
            handle.register_syncdb(repo.name.as_str(), siglevel).ok();
        }
    }

    fn register_default_syncdbs_with_mirrors(handle: &mut Alpm) {
        let repos = Self::repo_configs();
        let siglevel = SigLevel::PACKAGE_OPTIONAL | SigLevel::DATABASE_OPTIONAL;
        for repo in repos {
            if let Ok(db) = handle.register_syncdb_mut(repo.name.as_str(), siglevel) {
                for server in repo.servers {
                    db.add_server(server).ok();
                }
            }
        }
    }

    fn repo_configs() -> Vec<RepoConfig> {
        let config = match load_pacman_config() {
            Some(config) => config,
            None => {
                warn!("Failed to load pacman configuration via pacman-conf");
                return Vec::new();
            }
        };

        let arch = resolve_arch(&config);
        config
            .repos
            .into_iter()
            .map(|repo| RepoConfig {
                name: repo.name.clone(),
                servers: repo
                    .servers
                    .into_iter()
                    .map(|server| expand_repo_vars(&server, &repo.name, &arch))
                    .collect(),
            })
            .collect()
    }

    fn prepare_temp_dbpath(config: &AlpmConfig) -> std::path::PathBuf {
        let temp_dir = std::env::temp_dir().join(TEMP_SYNC_DIR);
        let temp_dbpath = temp_dir.join("db");

        std::fs::create_dir_all(&temp_dbpath).ok();

        let local_db_src = Path::new(&config.dbpath).join("local");
        let local_db_dst = temp_dbpath.join("local");
        if local_db_src.exists() && !local_db_dst.exists() {
            std::os::unix::fs::symlink(&local_db_src, &local_db_dst).ok();
        }

        temp_dbpath
    }

    fn is_orphan(pkg: &alpm::Package) -> bool {
        pkg.reason() == alpm::PackageReason::Depend
            && pkg.required_by().is_empty()
            && pkg.optional_for().is_empty()
    }

    fn status_for_local_pkg(pkg: &alpm::Package) -> PackageStatus {
        if Self::is_orphan(pkg) {
            PackageStatus::Orphan
        } else {
            PackageStatus::Installed
        }
    }

    fn install_reason(pkg: &alpm::Package) -> Option<InstallReason> {
        Some(match pkg.reason() {
            alpm::PackageReason::Explicit => InstallReason::Explicit,
            alpm::PackageReason::Depend => InstallReason::Dependency,
        })
    }

    fn package_from_alpm(pkg: &alpm::Package, status: PackageStatus, repository: &str) -> Package {
        Package::new(
            pkg.name(),
            Version::new(pkg.version().as_str()),
            pkg.desc().unwrap_or_default(),
            PackageBackend::Pacman,
            status,
            repository,
        )
    }

    fn to_utc(ts: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(ts, 0)
            .unwrap_or_default()
            .with_timezone(&chrono::Utc)
    }

    fn package_info_from_local(pkg: &alpm::Package) -> PackageInfo {
        PackageInfo {
            package: Self::package_from_alpm(pkg, Self::status_for_local_pkg(pkg), "local"),
            url: pkg.url().map(|s| s.to_string()),
            licenses: pkg.licenses().iter().map(|s| s.to_string()).collect(),
            groups: pkg.groups().iter().map(|s| s.to_string()).collect(),
            depends: pkg.depends().iter().map(|d| d.to_string()).collect(),
            optdepends: pkg.optdepends().iter().map(|d| d.to_string()).collect(),
            provides: pkg.provides().iter().map(|d| d.to_string()).collect(),
            conflicts: pkg.conflicts().iter().map(|d| d.to_string()).collect(),
            replaces: pkg.replaces().iter().map(|d| d.to_string()).collect(),
            installed_size: pkg.isize() as u64,
            download_size: pkg.download_size() as u64,
            build_date: Some(Self::to_utc(pkg.build_date())),
            install_date: pkg.install_date().map(Self::to_utc),
            packager: pkg.packager().map(|s| s.to_string()),
            arch: pkg.arch().unwrap_or("any").to_string(),
            reason: Self::install_reason(pkg),
        }
    }

    fn package_info_from_sync(pkg: &alpm::Package, repo: &str) -> PackageInfo {
        PackageInfo {
            package: Self::package_from_alpm(pkg, PackageStatus::Available, repo),
            url: pkg.url().map(|s| s.to_string()),
            licenses: pkg.licenses().iter().map(|s| s.to_string()).collect(),
            groups: pkg.groups().iter().map(|s| s.to_string()).collect(),
            depends: pkg.depends().iter().map(|d| d.to_string()).collect(),
            optdepends: pkg.optdepends().iter().map(|d| d.to_string()).collect(),
            provides: pkg.provides().iter().map(|d| d.to_string()).collect(),
            conflicts: pkg.conflicts().iter().map(|d| d.to_string()).collect(),
            replaces: pkg.replaces().iter().map(|d| d.to_string()).collect(),
            installed_size: pkg.isize() as u64,
            download_size: pkg.download_size() as u64,
            build_date: Some(Self::to_utc(pkg.build_date())),
            install_date: None,
            packager: pkg.packager().map(|s| s.to_string()),
            arch: pkg.arch().unwrap_or("any").to_string(),
            reason: None,
        }
    }
}

#[derive(Debug, Clone)]
struct RepoConfig {
    name: String,
    servers: Vec<String>,
}

fn load_pacman_config() -> Option<PacmanConfig> {
    std::panic::catch_unwind(pacman_conf::get_config).ok()
}

fn resolve_arch(config: &PacmanConfig) -> String {
    let arch = config.architecture.trim();
    if arch.is_empty() || arch.eq_ignore_ascii_case("auto") {
        std::env::consts::ARCH.to_string()
    } else {
        arch.to_string()
    }
}

fn expand_repo_vars(value: &str, repo: &str, arch: &str) -> String {
    value.replace("$repo", repo).replace("$arch", arch)
}

#[async_trait]
impl PackageSource for AlpmBackend {
    fn source_id(&self) -> &str {
        "pacman"
    }

    fn display_name(&self) -> &str {
        "Pacman"
    }

    async fn is_available(&self) -> bool {
        Path::new(&self.config.dbpath).exists()
    }

    async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        let config = self.config.clone();
        let query = query.to_string();

        tokio::task::spawn_blocking(move || {
            Self::with_handle(&config, None, |handle| {
                Self::register_default_syncdbs(handle);

                let mut results = Vec::new();
                let query_lower = query.to_lowercase();

                // Search in sync databases.
                for db in handle.syncdbs() {
                    for pkg in db.pkgs() {
                        let name = pkg.name();
                        let desc = pkg.desc().unwrap_or_default();

                        if name.to_lowercase().contains(&query_lower)
                            || desc.to_lowercase().contains(&query_lower)
                        {
                            let installed_pkg = handle.localdb().pkg(name).ok();
                            let installed = installed_pkg.is_some();
                            let installed_version = installed_pkg
                                .as_ref()
                                .map(|p| Version::new(p.version().as_str()));

                            results.push(SearchResult {
                                name: name.to_string(),
                                version: Version::new(pkg.version().as_str()),
                                description: desc.to_string(),
                                backend: PackageBackend::Pacman,
                                repository: db.name().to_string(),
                                installed,
                                installed_version,
                            });
                        }
                    }
                }
                Ok(results)
            })
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))?
    }

    async fn list_installed(&self) -> Result<Vec<Package>> {
        let config = self.config.clone();

        tokio::task::spawn_blocking(move || {
            Self::with_handle(&config, None, |handle| {
                let mut packages = Vec::new();

                for pkg in handle.localdb().pkgs() {
                    let status = Self::status_for_local_pkg(&pkg);
                    packages.push(Self::package_from_alpm(&pkg, status, "local"));
                }

                Ok(packages)
            })
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))?
    }

    async fn list_updates(&self) -> Result<Vec<UpdateInfo>> {
        let config = self.config.clone();

        tokio::task::spawn_blocking(move || {
            // Use checkupdates approach: sync to temp db, then compare
            // This doesn't require root privileges

            let temp_dbpath = Self::prepare_temp_dbpath(&config);

            // Create handle with temp dbpath for syncing
            let mut handle = Self::open_handle(&config, Some(&temp_dbpath))?;
            Self::register_default_syncdbs_with_mirrors(&mut handle);

            // Sync all databases at once
            if let Err(e) = handle.syncdbs_mut().update(false) {
                warn!("Failed to sync databases: {}", e);
            }

            let mut updates = Vec::new();

            for local_pkg in handle.localdb().pkgs() {
                let name = local_pkg.name();

                // Check sync dbs for newer version.
                for db in handle.syncdbs() {
                    if let Ok(sync_pkg) = db.pkg(name) {
                        let local_ver = local_pkg.version();
                        let sync_ver = sync_pkg.version();

                        if alpm::vercmp(sync_ver.as_str(), local_ver.as_str())
                            == std::cmp::Ordering::Greater
                        {
                            updates.push(UpdateInfo {
                                name: name.to_string(),
                                current_version: Version::new(local_ver.as_str()),
                                new_version: Version::new(sync_ver.as_str()),
                                backend: PackageBackend::Pacman,
                                repository: db.name().to_string(),
                                download_size: sync_pkg.download_size() as u64,
                            });
                            break;
                        }
                    }
                }
            }

            Ok(updates)
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))?
    }

    async fn get_package_info(&self, name: &str) -> Result<PackageInfo> {
        let config = self.config.clone();
        let name = name.to_string();

        tokio::task::spawn_blocking(move || {
            Self::with_handle(&config, None, |handle| {
                Self::register_default_syncdbs(handle);

                // Try local db first.
                if let Ok(pkg) = handle.localdb().pkg(name.as_bytes()) {
                    return Ok(Self::package_info_from_local(&pkg));
                }

                // Try sync dbs.
                for db in handle.syncdbs() {
                    if let Ok(pkg) = db.pkg(name.as_bytes()) {
                        return Ok(Self::package_info_from_sync(&pkg, db.name()));
                    }
                }

                Err(Error::PackageNotFound(name))
            })
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))?
    }

    async fn execute(&self, operation: Operation) -> Result<OperationResult> {
        self.execute_with_progress(operation, Box::new(|_| {}))
            .await
    }

    async fn execute_with_progress(
        &self,
        operation: Operation,
        _progress: ProgressCallback,
    ) -> Result<OperationResult> {
        let start = std::time::Instant::now();

        info!("Executing operation: {:?}", operation.kind);

        // For actual package operations, we would need to call pacman/libalpm
        // with root privileges. For now, return a placeholder result.
        let result = match operation.kind {
            OperationKind::Install
            | OperationKind::Remove
            | OperationKind::RemoveWithDeps
            | OperationKind::Update
            | OperationKind::SystemUpgrade => {
                // TODO: Implement actual package operations via polkit/pkexec.
                warn!("Package operations require root privileges - not implemented yet");
                OperationResult::failure(
                    operation,
                    "Package operations require root privileges",
                    start.elapsed().as_millis() as u64,
                )
            }
            OperationKind::SyncDatabases => {
                // Database sync also needs root for system-wide sync.
                warn!("Database sync requires root privileges");
                OperationResult::failure(
                    operation,
                    "Database sync requires root privileges",
                    start.elapsed().as_millis() as u64,
                )
            }
            OperationKind::CleanCache => {
                let freed = self.cache_manager.clean(3).await?;
                info!("Freed {} bytes from cache", freed);
                OperationResult::success(operation, Vec::new(), start.elapsed().as_millis() as u64)
            }
            OperationKind::RemoveOrphans => {
                let orphans = self.list_orphans().await?;
                if orphans.is_empty() {
                    OperationResult::success(
                        operation,
                        Vec::new(),
                        start.elapsed().as_millis() as u64,
                    )
                } else {
                    OperationResult::failure(
                        operation,
                        "Removing orphans requires root privileges",
                        start.elapsed().as_millis() as u64,
                    )
                }
            }
        };

        Ok(result)
    }

    async fn sync_databases(&self) -> Result<()> {
        // Database sync requires root privileges.
        warn!("Database sync requires root privileges - skipping");
        Ok(())
    }

    async fn get_cache_size(&self) -> Result<u64> {
        self.cache_manager.get_size().await
    }

    async fn clean_cache(&self, keep_versions: usize) -> Result<u64> {
        self.cache_manager.clean(keep_versions).await
    }

    async fn list_orphans(&self) -> Result<Vec<Package>> {
        let config = self.config.clone();

        tokio::task::spawn_blocking(move || {
            Self::with_handle(&config, None, |handle| {
                let mut orphans = Vec::new();

                for pkg in handle.localdb().pkgs() {
                    if Self::is_orphan(&pkg) {
                        orphans.push(Self::package_from_alpm(
                            &pkg,
                            PackageStatus::Orphan,
                            "local",
                        ));
                    }
                }

                Ok(orphans)
            })
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))?
    }
}
