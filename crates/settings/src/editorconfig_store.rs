use anyhow::{Context as _, Result};
use collections::{BTreeMap, BTreeSet, HashSet, btree_map};
use ec4rs::{ConfigParser, PropertiesSource, Section};
use fs::Fs;
use futures::StreamExt;
use gpui::{Context, EventEmitter, Task};
use paths::EDITORCONFIG_NAME;
use smallvec::SmallVec;
use std::{path::Path, str::FromStr, sync::Arc};
use util::{ResultExt as _, rel_path::RelPath};

use crate::{InvalidSettingsError, LocalSettingsPath, WorktreeId, watch_config_file};

pub type EditorconfigProperties = ec4rs::Properties;

#[derive(Clone)]
pub struct Editorconfig {
    pub is_root: bool,
    pub sections: SmallVec<[Section; 5]>,
}

impl FromStr for Editorconfig {
    type Err = anyhow::Error;

    fn from_str(contents: &str) -> Result<Self, Self::Err> {
        let parser = ConfigParser::new_buffered(contents.as_bytes())
            .context("creating editorconfig parser")?;
        let is_root = parser.is_root;
        let sections = parser
            .collect::<Result<SmallVec<_>, _>>()
            .context("parsing editorconfig sections")?;
        Ok(Self { is_root, sections })
    }
}

#[derive(Clone, Default)]
struct EditorconfigData {
    content: String,
    parsed: Option<Editorconfig>,
}

#[derive(Clone, Debug)]
pub struct EditorconfigEvent {
    pub path: LocalSettingsPath,
    pub content: Option<String>,
    pub worktree_ids: Vec<WorktreeId>,
}

#[derive(Default)]
pub struct EditorconfigStore {
    /// External editorconfig files shared across multiple worktrees
    external_configs: BTreeMap<Arc<Path>, EditorconfigData>,
    external_config_watchers: BTreeMap<Arc<Path>, Task<()>>,
    worktree_editorconfig_state: BTreeMap<WorktreeId, WorktreeEditorconfigState>,
}

impl EventEmitter<EditorconfigEvent> for EditorconfigStore {}

#[derive(Default)]
pub struct WorktreeEditorconfigState {
    internal_configs: BTreeMap<Arc<RelPath>, EditorconfigData>,
    external_config_paths: Option<BTreeSet<Arc<Path>>>,
    external_configs_loading_task: Option<Task<()>>,
}

impl EditorconfigStore {
    pub(crate) fn set_configs(
        &mut self,
        worktree_id: WorktreeId,
        path: LocalSettingsPath,
        content: Option<&str>,
    ) -> std::result::Result<(), InvalidSettingsError> {
        match (&path, content) {
            (LocalSettingsPath::InWorktree(rel_path), None) => {
                if let Some(state) = self.worktree_editorconfig_state.get_mut(&worktree_id) {
                    state.internal_configs.remove(rel_path);
                }
            }
            (LocalSettingsPath::OutsideWorktree(abs_path), None) => {
                if let Some(state) = self.worktree_editorconfig_state.get_mut(&worktree_id) {
                    if let Some(paths) = &mut state.external_config_paths {
                        paths.remove(abs_path);
                    }
                }
                let still_in_use = self.worktree_editorconfig_state.values().any(|s| {
                    s.external_config_paths
                        .as_ref()
                        .map_or(false, |p| p.contains(abs_path))
                });
                if !still_in_use {
                    self.external_configs.remove(abs_path);
                    self.external_config_watchers.remove(abs_path);
                }
            }
            (LocalSettingsPath::InWorktree(rel_path), Some(content)) => {
                let state = self
                    .worktree_editorconfig_state
                    .entry(worktree_id)
                    .or_default();
                match state.internal_configs.entry(rel_path.clone()) {
                    btree_map::Entry::Vacant(v) => match content.parse() {
                        Ok(parsed) => {
                            v.insert(EditorconfigData {
                                content: content.to_owned(),
                                parsed: Some(parsed),
                            });
                        }
                        Err(e) => {
                            v.insert(EditorconfigData {
                                content: content.to_owned(),
                                parsed: None,
                            });
                            return Err(InvalidSettingsError::Editorconfig {
                                message: e.to_string(),
                                path: LocalSettingsPath::InWorktree(
                                    rel_path.join(RelPath::unix(EDITORCONFIG_NAME).unwrap()),
                                ),
                            });
                        }
                    },
                    btree_map::Entry::Occupied(mut o) => {
                        if o.get().content != content {
                            match content.parse() {
                                Ok(parsed) => {
                                    o.insert(EditorconfigData {
                                        content: content.to_owned(),
                                        parsed: Some(parsed),
                                    });
                                }
                                Err(e) => {
                                    o.insert(EditorconfigData {
                                        content: content.to_owned(),
                                        parsed: None,
                                    });
                                    return Err(InvalidSettingsError::Editorconfig {
                                        message: e.to_string(),
                                        path: LocalSettingsPath::InWorktree(
                                            rel_path
                                                .join(RelPath::unix(EDITORCONFIG_NAME).unwrap()),
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }
            }
            (LocalSettingsPath::OutsideWorktree(abs_path), Some(content)) => {
                let state = self
                    .worktree_editorconfig_state
                    .entry(worktree_id)
                    .or_default();

                match &mut state.external_config_paths {
                    None => {
                        state.external_config_paths = Some(BTreeSet::from([abs_path.clone()]));
                    }
                    Some(paths) => {
                        paths.insert(abs_path.clone());
                    }
                }

                let should_update = self
                    .external_configs
                    .get(abs_path)
                    .map_or(true, |entry| entry.content != content);

                if should_update {
                    let parsed = match content.parse::<Editorconfig>() {
                        Ok(parsed) => Some(parsed),
                        Err(e) => {
                            self.external_configs.insert(
                                abs_path.clone(),
                                EditorconfigData {
                                    content: content.to_owned(),
                                    parsed: None,
                                },
                            );
                            return Err(InvalidSettingsError::Editorconfig {
                                message: e.to_string(),
                                path: LocalSettingsPath::OutsideWorktree(
                                    abs_path.join(EDITORCONFIG_NAME).into(),
                                ),
                            });
                        }
                    };

                    self.external_configs.insert(
                        abs_path.clone(),
                        EditorconfigData {
                            content: content.to_owned(),
                            parsed,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn remove_worktree(&mut self, root_id: WorktreeId) {
        let Some(removed) = self.worktree_editorconfig_state.remove(&root_id) else {
            return;
        };

        let paths_in_use: HashSet<_> = self
            .worktree_editorconfig_state
            .values()
            .flat_map(|w| w.external_config_paths.iter().flatten())
            .collect();

        for path in removed.external_config_paths.iter().flatten() {
            if !paths_in_use.contains(path) {
                self.external_configs.remove(path);
                self.external_config_watchers.remove(path);
            }
        }
    }

    fn internal_configs(
        &self,
        root_id: WorktreeId,
    ) -> impl '_ + Iterator<Item = (&RelPath, &str, Option<&Editorconfig>)> {
        self.worktree_editorconfig_state
            .get(&root_id)
            .into_iter()
            .flat_map(|state| {
                state.internal_configs.iter().map(|(path, data)| {
                    (path.as_ref(), data.content.as_str(), data.parsed.as_ref())
                })
            })
    }

    fn external_configs(
        &self,
        worktree_id: WorktreeId,
    ) -> impl '_ + Iterator<Item = (&Path, &str, Option<&Editorconfig>)> {
        self.worktree_editorconfig_state
            .get(&worktree_id)
            .into_iter()
            .flat_map(|state| {
                state
                    .external_config_paths
                    .iter()
                    .flatten()
                    .filter_map(|path| {
                        self.external_configs.get(path).map(|entry| {
                            (path.as_ref(), entry.content.as_str(), entry.parsed.as_ref())
                        })
                    })
            })
    }

    pub fn local_editorconfig_settings(
        &self,
        worktree_id: WorktreeId,
    ) -> impl '_ + Iterator<Item = (LocalSettingsPath, &str, Option<&Editorconfig>)> {
        let external = self
            .external_configs(worktree_id)
            .map(|(path, content, parsed)| {
                (
                    LocalSettingsPath::OutsideWorktree(path.into()),
                    content,
                    parsed,
                )
            });
        let internal = self
            .internal_configs(worktree_id)
            .map(|(path, content, parsed)| {
                (LocalSettingsPath::InWorktree(path.into()), content, parsed)
            });
        external.chain(internal)
    }

    pub fn load_external_configs(
        &mut self,
        worktree_id: WorktreeId,
        worktree_path: Arc<Path>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) {
        let state = self
            .worktree_editorconfig_state
            .entry(worktree_id)
            .or_default();

        if state.external_config_paths.is_some() {
            return;
        }

        let task = cx.spawn(async move |this, cx| {
            let external_config_paths =
                Self::discover_external_config_paths(worktree_path, &fs).await;

            this.update(cx, |this, cx| {
                let state = this
                    .worktree_editorconfig_state
                    .entry(worktree_id)
                    .or_default();

                state.external_config_paths = Some(external_config_paths.iter().cloned().collect());

                for dir_path in external_config_paths {
                    if this.external_config_watchers.contains_key(&dir_path) {
                        continue;
                    }

                    let watcher_task =
                        Self::watch_external_config(fs.clone(), dir_path.clone(), cx);
                    this.external_config_watchers.insert(dir_path, watcher_task);
                }
            })
            .ok();
        });

        self.worktree_editorconfig_state
            .entry(worktree_id)
            .or_default()
            .external_configs_loading_task = Some(task);
    }

    async fn discover_external_config_paths(
        worktree_path: Arc<Path>,
        fs: &Arc<dyn Fs>,
    ) -> Vec<Arc<Path>> {
        let mut paths = Vec::new();
        let mut current = worktree_path.parent().map(|p| p.to_path_buf());
        while let Some(dir) = current {
            let dir_path: Arc<Path> = Arc::from(dir.as_path());
            let editorconfig_path = dir.join(EDITORCONFIG_NAME);
            if let Ok(content) = fs.load(&editorconfig_path).await {
                paths.push(dir_path);
                if let Ok(parsed) = content.parse::<Editorconfig>() {
                    if parsed.is_root {
                        break;
                    }
                }
            }
            current = dir.parent().map(|p| p.to_path_buf());
        }
        paths
    }

    fn watch_external_config(
        fs: Arc<dyn Fs>,
        dir_path: Arc<Path>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let config_path = dir_path.join(EDITORCONFIG_NAME);
        let mut config_rx = watch_config_file(cx.background_executor(), fs, config_path);

        cx.spawn(async move |this, cx| {
            while let Some(content) = config_rx.next().await {
                let content = Some(content).filter(|c| !c.is_empty());
                let dir_path = dir_path.clone();

                this.update(cx, |this, cx| {
                    let worktree_ids: Vec<WorktreeId> =
                        this.worktree_editorconfig_state
                            .iter()
                            .filter_map(|(worktree_id, state)| {
                                state.external_config_paths.as_ref().and_then(|paths| {
                                    paths.contains(&dir_path).then_some(*worktree_id)
                                })
                            })
                            .collect();
                    cx.emit(EditorconfigEvent {
                        path: LocalSettingsPath::OutsideWorktree(dir_path),
                        content,
                        worktree_ids,
                    });
                })
                .ok();
            }
        })
    }

    pub fn properties(
        &self,
        for_worktree: WorktreeId,
        for_path: &RelPath,
    ) -> Option<EditorconfigProperties> {
        let mut properties = EditorconfigProperties::new();
        let state = self.worktree_editorconfig_state.get(&for_worktree);
        let empty_path: Arc<RelPath> = RelPath::empty().into();
        let internal_root_config_is_root = state
            .and_then(|state| state.internal_configs.get(&empty_path))
            .and_then(|data| data.parsed.as_ref())
            .is_some_and(|ec| ec.is_root);

        if !internal_root_config_is_root {
            for (_, _, parsed_editorconfig) in self.external_configs(for_worktree) {
                if let Some(parsed_editorconfig) = parsed_editorconfig {
                    if parsed_editorconfig.is_root {
                        properties = EditorconfigProperties::new();
                    }
                    for section in &parsed_editorconfig.sections {
                        section
                            .apply_to(&mut properties, for_path.as_std_path())
                            .log_err()?;
                    }
                }
            }
        }

        for (directory_with_config, _, parsed_editorconfig) in self.internal_configs(for_worktree) {
            if !for_path.starts_with(directory_with_config) {
                properties.use_fallbacks();
                return Some(properties);
            }
            let parsed_editorconfig = parsed_editorconfig?;
            if parsed_editorconfig.is_root {
                properties = EditorconfigProperties::new();
            }
            for section in &parsed_editorconfig.sections {
                section
                    .apply_to(&mut properties, for_path.as_std_path())
                    .log_err()?;
            }
        }

        properties.use_fallbacks();
        Some(properties)
    }
}
