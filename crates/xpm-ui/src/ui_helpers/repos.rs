use xpm_pacman::pacman_conf;

pub fn parse_pacman_repos() -> Vec<String> {
    let config = match std::panic::catch_unwind(pacman_conf::get_config).ok() {
        Some(config) => config,
        None => return Vec::new(),
    };

    config.repos.into_iter().map(|repo| repo.name).collect()
}
