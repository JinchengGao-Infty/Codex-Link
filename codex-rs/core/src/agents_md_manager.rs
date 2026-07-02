use crate::agents_md::AgentsMdFocusPath;
use crate::agents_md::LoadedAgentsMd;
use crate::agents_md::load_project_instructions_with_focus_paths;
use crate::config::Config;
use crate::environment_selection::TurnEnvironmentSnapshot;
use codex_extension_api::UserInstructions;
use codex_protocol::protocol::TurnEnvironmentSelection;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Owns the inputs and cached result of AGENTS.md discovery for a session.
pub(crate) struct AgentsMdManager {
    user_instructions: Option<UserInstructions>,
    cache: Mutex<AgentsMdCache>,
}

#[derive(Default)]
struct AgentsMdCache {
    selections: Option<Vec<TurnEnvironmentSelection>>,
    focus_paths: Vec<AgentsMdFocusPath>,
    loaded: Option<Arc<LoadedAgentsMd>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RefreshMode {
    IfSelectionChanged,
    Force,
}

impl AgentsMdManager {
    pub(crate) fn new(user_instructions: Option<UserInstructions>) -> Self {
        Self {
            user_instructions: user_instructions
                .filter(|instructions| !instructions.text.trim().is_empty()),
            cache: Mutex::new(AgentsMdCache::default()),
        }
    }

    pub(crate) async fn refresh(&self, config: &Config, environments: &TurnEnvironmentSnapshot) {
        let focus_paths = self.cache.lock().await.focus_paths.clone();
        self.refresh_with_mode(
            config,
            environments,
            focus_paths,
            RefreshMode::IfSelectionChanged,
        )
        .await;
    }

    pub(crate) async fn refresh_for_focus_paths(
        &self,
        config: &Config,
        environments: &TurnEnvironmentSnapshot,
        focus_paths: Vec<AgentsMdFocusPath>,
    ) {
        self.refresh_with_mode(
            config,
            environments,
            focus_paths,
            RefreshMode::IfSelectionChanged,
        )
        .await;
    }

    pub(crate) async fn add_focus_paths(
        &self,
        config: &Config,
        environments: &TurnEnvironmentSnapshot,
        focus_paths: Vec<AgentsMdFocusPath>,
    ) {
        if focus_paths.is_empty() {
            return;
        }

        let mut next_focus_paths = self.cache.lock().await.focus_paths.clone();
        let mut seen = next_focus_paths.iter().cloned().collect::<HashSet<_>>();
        for focus_path in focus_paths {
            if seen.insert(focus_path.clone()) {
                next_focus_paths.push(focus_path);
            }
        }

        self.refresh_with_mode(
            config,
            environments,
            next_focus_paths,
            RefreshMode::IfSelectionChanged,
        )
        .await;
    }

    pub(crate) async fn force_refresh(
        &self,
        config: &Config,
        environments: &TurnEnvironmentSnapshot,
    ) {
        let focus_paths = self.cache.lock().await.focus_paths.clone();
        self.refresh_with_mode(config, environments, focus_paths, RefreshMode::Force)
            .await;
    }

    async fn refresh_with_mode(
        &self,
        config: &Config,
        environments: &TurnEnvironmentSnapshot,
        focus_paths: Vec<AgentsMdFocusPath>,
        mode: RefreshMode,
    ) {
        let selections = environments.to_selections();
        {
            let cache = self.cache.lock().await;
            if mode == RefreshMode::IfSelectionChanged
                && cache.selections.as_ref() == Some(&selections)
                && cache.focus_paths == focus_paths
            {
                return;
            }
        }

        let loaded = load_project_instructions_with_focus_paths(
            config,
            self.user_instructions.clone(),
            environments,
            &focus_paths,
        )
        .await
        .map(Arc::new);
        let mut cache = self.cache.lock().await;
        cache.selections = Some(selections);
        cache.focus_paths = focus_paths;
        cache.loaded = loaded;
    }

    pub(crate) async fn get_loaded(&self) -> Option<Arc<LoadedAgentsMd>> {
        self.cache.lock().await.loaded.clone()
    }

    pub(crate) fn user_instructions(&self) -> Option<UserInstructions> {
        self.user_instructions.clone()
    }
}
