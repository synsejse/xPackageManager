use crate::{PackageData, StatsData};

#[derive(Debug, Clone)]
pub enum UiMessage {
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
    ShowProgressPopup(String),
    ProgressOutput(String),
    ProgressPrompt(String),
    ProgressHidePrompt,
    OperationProgress(i32, String),
    OperationDone(bool),
    ShowTerminalFallback(String),
}
