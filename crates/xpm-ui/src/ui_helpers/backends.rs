use tracing::error;
use xpm_alpm::AlpmBackend;
use xpm_flatpak::FlatpakBackend;

pub struct UiBackends {
    pub alpm: AlpmBackend,
    pub flatpak: FlatpakBackend,
}

pub fn init_backends() -> Option<UiBackends> {
    let alpm = match AlpmBackend::new() {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to initialize ALPM: {}", e);
            return None;
        }
    };

    let flatpak = match FlatpakBackend::new() {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to initialize Flatpak: {}", e);
            return None;
        }
    };

    Some(UiBackends { alpm, flatpak })
}
