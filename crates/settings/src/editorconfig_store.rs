use anyhow::{Context as _, Result};
use collections::{BTreeMap, BTreeSet, HashMap, HashSet, btree_map};
use ec4rs::{ConfigParser, PropertiesSource, Section};
use fs::Fs;
use futures::StreamExt;
use gpui::{AsyncApp, Context, EventEmitter, Task, WeakEntity};
use paths::EDITORCONFIG_NAME;
use smallvec::SmallVec;
use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
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

struct ExternalEditorconfigData {
    config: EditorconfigData,
    _watch_task: Option<Task<()>>,
}

#[derive(Clone, Debug)]
pub enum EditorconfigEvent {
    InternalConfigChanged {
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
    },
    ExternalConfigChanged {
        worktree_id: WorktreeId,
        abs_path: Arc<Path>,
    },
}

#[derive(Default)]
pub struct EditorconfigStore {
    /// External editorconfig files shared across multiple worktrees
    external_configs: BTreeMap<Arc<Path>, ExternalEditorconfigData>,
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
    pub(crate) fn set_config(
        &mut self,
        worktree_id: WorktreeId,
        path: LocalSettingsPath,
        content: Option<&str>,
        cx: &mut Context<Self>,
    ) -> std::result::Result<(), InvalidSettingsError> {
        match (&path, content) {
            (LocalSettingsPath::InWorktree(rel_path), None) => {
                if let Some(state) = self.worktree_editorconfig_state.get_mut(&worktree_id) {
                    state.internal_configs.remove(rel_path);
                }
            }
            (LocalSettingsPath::OutsideWorktree(abs_path), None) => {
                self.external_configs.remove(abs_path);
                if let Some(state) = self.worktree_editorconfig_state.get_mut(&worktree_id) {
                    if let Some(paths) = &mut state.external_config_paths {
                        paths.remove(abs_path);
                    }
                }
                // do something about this
                cx.emit(EditorconfigEvent::ExternalConfigChanged {
                    worktree_id,
                    abs_path: abs_path.clone(),
                });
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
                let should_update = self
                    .external_configs
                    .get(abs_path)
                    .map_or(true, |entry| entry.config.content != content);

                if should_update {
                    let parsed = match content.parse::<Editorconfig>() {
                        Ok(parsed) => Some(parsed),
                        Err(e) => {
                            self.external_configs.insert(
                                abs_path.clone(),
                                ExternalEditorconfigData {
                                    config: EditorconfigData {
                                        content: content.to_owned(),
                                        parsed: None,
                                    },
                                    _watch_task: None,
                                },
                            );
                            let state = self
                                .worktree_editorconfig_state
                                .entry(worktree_id)
                                .or_default();
                            let paths = state
                                .external_config_paths
                                .get_or_insert_with(BTreeSet::new);
                            paths.insert(abs_path.clone());

                            cx.emit(EditorconfigEvent::ExternalConfigChanged {
                                worktree_id,
                                abs_path: abs_path.clone(),
                            });

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
                        ExternalEditorconfigData {
                            config: EditorconfigData {
                                content: content.to_owned(),
                                parsed,
                            },
                            _watch_task: None,
                        },
                    );

                    let state = self
                        .worktree_editorconfig_state
                        .entry(worktree_id)
                        .or_default();
                    let paths = state
                        .external_config_paths
                        .get_or_insert_with(BTreeSet::new);
                    paths.insert(abs_path.clone());
                }

                cx.emit(EditorconfigEvent::ExternalConfigChanged {
                    worktree_id,
                    abs_path: abs_path.clone(),
                });
            }
        }
        Ok(())
    }

    pub(crate) fn remove_worktree(&mut self, root_id: WorktreeId) {
        if let Some(removed) = self.worktree_editorconfig_state.remove(&root_id) {
            let paths_in_use: HashSet<_> = self
                .worktree_editorconfig_state
                .values()
                .flat_map(|w| w.external_config_paths.iter().flatten())
                .collect();
            for path in removed.external_config_paths.iter().flatten() {
                if !paths_in_use.contains(path) {
                    self.external_configs.remove(path);
                }
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
                            (
                                path.as_ref(),
                                entry.config.content.as_str(),
                                entry.config.parsed.as_ref(),
                            )
                        })
                    })
            })
    }

    pub fn get_configs(
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
        let Some(state) = self.worktree_editorconfig_state.get(&worktree_id) else {
            return;
        };

        // We don't currently watch for newly added or removed external editorconfig files, but only use
        // the external editorconfig files discovered when the worktree was added
        if state.external_config_paths.is_some() {
            return;
        }

        // We only traverse up the directory tree when there exists some internal config for the worktree,
        // since we are not sure if the project needs editorconfig lookup or not.
        if state.internal_configs.is_empty() {
            return;
        }

        let task = cx.spawn(async move |this, cx| {
            let external_configs =
                Self::reload_external_config_chain(&this, worktree_path, &fs, &cx).await;
            this.update(cx, |this, cx| {
                let state = this
                    .worktree_editorconfig_state
                    .entry(worktree_id)
                    .or_default();
                state.external_config_paths = Some(
                    external_configs
                        .iter()
                        .map(|(path, _)| path.clone())
                        .collect(),
                );
                for (dir_path, config_data) in external_configs {
                    if this.external_configs.contains_key(&dir_path) {
                        continue;
                    }
                    let editorconfig_path = dir_path.join(EDITORCONFIG_NAME);
                    let watcher_task = Self::watch_external_config(
                        fs.clone(),
                        worktree_id,
                        dir_path.clone(),
                        editorconfig_path,
                        cx,
                    );
                    let config = config_data.unwrap_or_default();
                    this.external_configs.insert(
                        dir_path,
                        ExternalEditorconfigData {
                            config,
                            _watch_task: Some(watcher_task),
                        },
                    );
                }
            })
            .ok();
        });

        self.worktree_editorconfig_state
            .entry(worktree_id)
            .or_default()
            .external_configs_loading_task = Some(task);
    }

    async fn reload_external_config_chain(
        this: &WeakEntity<Self>,
        worktree_path: Arc<Path>,
        fs: &Arc<dyn Fs>,
        cx: &AsyncApp,
    ) -> Vec<(Arc<Path>, Option<EditorconfigData>)> {
        let cached_configs: HashMap<Arc<Path>, Option<Editorconfig>> = this
            .read_with(cx, |this, _| {
                this.external_configs
                    .iter()
                    .map(|(path, entry)| (path.clone(), entry.config.parsed.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let mut external_configs = Vec::new();
        let mut current = worktree_path.parent().map(|p| p.to_path_buf());

        while let Some(dir) = current {
            let dir_path: Arc<Path> = Arc::from(dir.as_path());
            if let Some(cached) = cached_configs.get(&dir_path) {
                let is_root = cached.as_ref().is_some_and(|c| c.is_root);
                external_configs.push((dir_path, None));
                if is_root {
                    break;
                }
            } else {
                let editorconfig_path = dir.join(EDITORCONFIG_NAME);
                if let Ok(content) = fs.load(&editorconfig_path).await {
                    match content.parse::<Editorconfig>() {
                        Ok(parsed) => {
                            let is_root = parsed.is_root;
                            external_configs.push((
                                dir_path,
                                Some(EditorconfigData {
                                    content,
                                    parsed: Some(parsed),
                                }),
                            ));
                            if is_root {
                                break;
                            }
                        }
                        Err(err) => {
                            log::warn!(
                                "Failed to parse external editorconfig at {:?}: {}",
                                editorconfig_path,
                                err
                            );
                            external_configs.push((
                                dir_path,
                                Some(EditorconfigData {
                                    content,
                                    parsed: None,
                                }),
                            ));
                        }
                    }
                }
            }
            current = dir.parent().map(|p| p.to_path_buf());
        }

        external_configs
    }

    fn watch_external_config(
        fs: Arc<dyn Fs>,
        worktree_id: WorktreeId,
        dir_path: Arc<Path>,
        editorconfig_path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let mut config_rx =
            watch_config_file(cx.background_executor(), fs, editorconfig_path.clone());

        cx.spawn(async move |this, cx| {
            while let Some(content) = config_rx.next().await {
                let parsed = if content.is_empty() {
                    None
                } else {
                    match content.parse::<Editorconfig>() {
                        Ok(parsed) => Some(parsed),
                        Err(err) => {
                            log::warn!(
                                "Failed to parse external editorconfig at {:?}: {}",
                                editorconfig_path,
                                err
                            );
                            None
                        }
                    }
                };

                let dir_path = dir_path.clone();
                this.update(cx, |this, cx| {
                    if let Some(entry) = this.external_configs.get_mut(&dir_path) {
                        entry.config.content = content.clone();
                        entry.config.parsed = parsed;
                    }
                    cx.emit(EditorconfigEvent::ExternalConfigChanged {
                        worktree_id,
                        abs_path: dir_path,
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
