use std::future::Future;
use std::sync::mpsc;
use std::thread;

use crate::UiMessage;

use super::data::{load_packages_async, load_repo_packages, search_packages_async};

pub fn run_async(task: impl Future<Output = ()> + Send + 'static) {
    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
        rt.block_on(task);
    });
}

pub fn refresh_after_operation(
    tx: mpsc::Sender<UiMessage>,
    search_query: String,
    current_view: i32,
    current_repo: String,
) {
    let tx_async = tx.clone();
    run_async(async move {
        load_packages_async(&tx_async, false).await;
        if !search_query.is_empty() {
            search_packages_async(&tx_async, &search_query).await;
        }
    });

    if current_view == 8 && !current_repo.is_empty() {
        let tx_repo = tx.clone();
        thread::spawn(move || {
            load_repo_packages(&tx_repo, &current_repo);
        });
    }
}
