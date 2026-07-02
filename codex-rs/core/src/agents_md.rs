//! AGENTS.md discovery and user instruction assembly.
//!
//! Project-level documentation is primarily stored in files named `AGENTS.md`.
//! Additional fallback filenames can be configured via `project_doc_fallback_filenames`.
//! We include the concatenation of all files found along the path from the
//! project root to the current working directory, plus any task-focused paths
//! mentioned by the user, as follows:
//!
//! 1.  Determine the project root by walking upwards from the current working
//!     directory until a configured `project_root_markers` entry is found.
//!     When `project_root_markers` is unset, the default marker list is used
//!     (`.git`). If no marker is found, only the current working directory is
//!     considered. An empty marker list disables parent traversal.
//! 2.  Collect every `AGENTS.md` found from the project root down to the
//!     current working directory (inclusive).
//! 3.  For mentioned files or directories under the same project root, collect
//!     their root-to-path `AGENTS.md` chain as well.
//! 4.  We do **not** walk past the project root.

use crate::config::Config;
use crate::context::UserInstructions as ContextUserInstructions;
use crate::environment_selection::TurnEnvironmentSnapshot;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStackOrdering;
use codex_config::default_project_root_markers;
use codex_config::merge_toml_values;
use codex_config::project_root_markers_from_config;
use codex_exec_server::ExecutorFileSystem;
use codex_extension_api::UserInstructions;
use codex_file_system::FindUpErrorPolicy;
use codex_file_system::find_nearest_ancestor_with_markers;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::collections::HashSet;
use std::io;
use toml::Value as TomlValue;
use tracing::error;

/// Default filename scanned for AGENTS.md instructions.
pub const DEFAULT_AGENTS_MD_FILENAME: &str = "AGENTS.md";
/// Preferred local override for AGENTS.md instructions.
pub const LOCAL_AGENTS_MD_FILENAME: &str = "AGENTS.override.md";

/// When both user and project AGENTS.md docs are present, they will be
/// concatenated with the following separator.
const AGENTS_MD_SEPARATOR: &str = "\n\n--- project-doc ---\n\n";
const AGENTS_MD_IMPORT_MAX_DEPTH: usize = 4;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct AgentsMdFocusPath {
    pub(crate) environment_id: String,
    pub(crate) path: PathUri,
}

/// Loads project AGENTS.md content and combines it with host-provided user
/// instructions.
#[cfg(test)]
pub(crate) async fn load_project_instructions(
    config: &Config,
    user_instructions: Option<UserInstructions>,
    environments: &TurnEnvironmentSnapshot,
) -> Option<LoadedAgentsMd> {
    load_project_instructions_with_focus_paths(
        config,
        user_instructions,
        environments,
        /*focus_paths*/ &[],
    )
    .await
}

pub(crate) async fn load_project_instructions_with_focus_paths(
    config: &Config,
    user_instructions: Option<UserInstructions>,
    environments: &TurnEnvironmentSnapshot,
    focus_paths: &[AgentsMdFocusPath],
) -> Option<LoadedAgentsMd> {
    let mut loaded = LoadedAgentsMd::from_user_instructions(user_instructions);
    for turn_environment in &environments.turn_environments {
        let filesystem = turn_environment.environment.get_filesystem();
        let environment_focus_paths = focus_paths
            .iter()
            .filter(|focus_path| focus_path.environment_id == turn_environment.environment_id)
            .map(|focus_path| focus_path.path.clone())
            .collect::<Vec<_>>();
        match read_agents_md(
            config,
            filesystem.as_ref(),
            &turn_environment.environment_id,
            turn_environment.cwd(),
            &environment_focus_paths,
        )
        .await
        {
            Ok(Some(docs)) => loaded.entries.extend(docs.entries),
            Ok(None) => {}
            Err(e) => {
                error!(
                    environment_id = turn_environment.environment_id,
                    "error trying to find AGENTS.md docs: {e:#}"
                );
            }
        }
    }

    (!loaded.is_empty()).then_some(loaded)
}

/// Attempt to locate and load AGENTS.md documentation.
///
/// On success returns `Ok(Some(loaded))` where `loaded` contains every
/// discovered doc. If no documentation file is found the function returns
/// `Ok(None)`. Unexpected I/O failures bubble up as `Err` so callers can
/// decide how to handle them.
async fn read_agents_md(
    config: &Config,
    fs: &dyn ExecutorFileSystem,
    environment_id: &str,
    cwd: &PathUri,
    focus_paths: &[PathUri],
) -> io::Result<Option<LoadedAgentsMd>> {
    let max_total = config.project_doc_max_bytes;

    if max_total == 0 {
        return Ok(None);
    }

    let discovery = discover_agents_md(config, cwd, fs).await?;

    let mut remaining: u64 = max_total as u64;
    let mut loaded = LoadedAgentsMd::default();

    let read_context = AgentsMdReadContext {
        fs,
        environment_id,
        cwd,
        import_root: &discovery.project_root,
    };
    let mut visited = HashSet::new();
    for path in discovery.paths {
        read_agents_md_path_with_imports(
            &read_context,
            path,
            &mut remaining,
            &mut loaded,
            &mut visited,
        )
        .await?;
    }
    for focus_path in focus_paths {
        if remaining == 0 {
            break;
        }
        let Some(focus_dir) = agents_md_focus_dir(fs, focus_path).await? else {
            continue;
        };
        if !focus_dir.starts_with(&discovery.project_root) {
            continue;
        }
        let focus_discovery = discover_agents_md(config, &focus_dir, fs).await?;
        if focus_discovery.project_root != discovery.project_root {
            continue;
        }
        for path in focus_discovery.paths {
            read_agents_md_path_with_imports(
                &read_context,
                path,
                &mut remaining,
                &mut loaded,
                &mut visited,
            )
            .await?;
        }
    }

    if loaded.is_empty() {
        Ok(None)
    } else {
        Ok(Some(loaded))
    }
}

struct PendingAgentsMdPath {
    path: PathUri,
    import_depth: usize,
}

struct AgentsMdReadContext<'a> {
    fs: &'a dyn ExecutorFileSystem,
    environment_id: &'a str,
    cwd: &'a PathUri,
    import_root: &'a PathUri,
}

async fn read_agents_md_path_with_imports(
    context: &AgentsMdReadContext<'_>,
    path: PathUri,
    remaining: &mut u64,
    loaded: &mut LoadedAgentsMd,
    visited: &mut HashSet<PathUri>,
) -> io::Result<()> {
    let mut pending = vec![PendingAgentsMdPath {
        path,
        import_depth: 0,
    }];

    while let Some(PendingAgentsMdPath { path, import_depth }) = pending.pop() {
        if *remaining == 0 {
            break;
        }
        if !visited.insert(path.clone()) {
            continue;
        }
        let Some(text) = read_agents_md_file(context.fs, &path, remaining).await? else {
            continue;
        };
        let imports = if import_depth < AGENTS_MD_IMPORT_MAX_DEPTH {
            extract_agents_md_imports(&text)
        } else {
            Vec::new()
        };
        loaded.entries.push(InstructionEntry {
            contents: text,
            provenance: InstructionProvenance::Project {
                source_path: path.clone(),
                environment_id: context.environment_id.to_string(),
                cwd: context.cwd.clone(),
            },
        });
        for imported in imports.into_iter().rev() {
            let Some(import_path) =
                resolve_agents_md_import_path(&path, context.import_root, &imported)
            else {
                continue;
            };
            pending.push(PendingAgentsMdPath {
                path: import_path,
                import_depth: import_depth + 1,
            });
        }
    }

    Ok(())
}

async fn read_agents_md_file(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    remaining: &mut u64,
) -> io::Result<Option<String>> {
    match fs.get_metadata(path, /*sandbox*/ None).await {
        Ok(metadata) if metadata.is_file => {}
        Ok(_) => return Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    }

    let mut data = match fs.read_file(path, /*sandbox*/ None).await {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let size = data.len() as u64;
    if size > *remaining {
        data.truncate(*remaining as usize);
    }

    if size > *remaining {
        tracing::warn!(
            path = %path,
            remaining_bytes = remaining,
            "project doc exceeds remaining budget; truncating"
        );
    }

    *remaining = remaining.saturating_sub(data.len() as u64);
    let text = String::from_utf8_lossy(&data).to_string();
    if text.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

fn resolve_agents_md_import_path(
    source_path: &PathUri,
    import_root: &PathUri,
    import_path: &str,
) -> Option<PathUri> {
    let base_dir = source_path.parent().unwrap_or_else(|| import_root.clone());
    let resolved = match base_dir.join(import_path) {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::warn!(
                source_path = %source_path,
                import_path,
                "invalid AGENTS.md import path: {err}"
            );
            return None;
        }
    };
    if !resolved.starts_with(import_root) {
        tracing::warn!(
            source_path = %source_path,
            import_path,
            resolved_path = %resolved,
            import_root = %import_root,
            "ignoring AGENTS.md import outside project doc root"
        );
        return None;
    }
    Some(resolved)
}

fn extract_agents_md_imports(contents: &str) -> Vec<String> {
    let mut imports = Vec::new();
    let mut in_fenced_block = false;
    for line in contents.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fenced_block = !in_fenced_block;
            continue;
        }
        if in_fenced_block {
            continue;
        }
        extract_agents_md_imports_from_line(line, &mut imports);
    }
    imports
}

fn extract_agents_md_imports_from_line(line: &str, imports: &mut Vec<String>) {
    let mut in_inline_code = false;
    let mut chars = line.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        if character == '`' {
            in_inline_code = !in_inline_code;
            continue;
        }
        if character != '@' || in_inline_code {
            continue;
        }
        let previous = line[..index].chars().next_back();
        if !is_agents_md_import_prefix(previous) {
            continue;
        }
        let start = index + character.len_utf8();
        let mut end = start;
        while let Some((next_index, next_character)) = chars.peek().copied() {
            if !is_agents_md_import_path_char(next_character) {
                break;
            }
            end = next_index + next_character.len_utf8();
            chars.next();
        }
        if let Some(import) = normalize_agents_md_import_candidate(&line[start..end]) {
            imports.push(import);
        }
    }
}

fn is_agents_md_import_prefix(previous: Option<char>) -> bool {
    previous.is_none_or(|character| {
        character.is_whitespace() || matches!(character, '(' | '[' | '<' | '"' | '\'')
    })
}

fn is_agents_md_import_path_char(character: char) -> bool {
    !character.is_whitespace()
        && !matches!(
            character,
            '`' | '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '#'
        )
}

fn normalize_agents_md_import_candidate(candidate: &str) -> Option<String> {
    let candidate = candidate.trim_end_matches(['.', ',', ';', ':']);
    if candidate.is_empty()
        || candidate.contains(':')
        || (!candidate.contains('/') && !candidate.contains('\\') && !candidate.contains('.'))
    {
        return None;
    }
    Some(candidate.to_string())
}

struct AgentsMdDiscovery {
    project_root: PathUri,
    paths: Vec<PathUri>,
}

/// Discovers AGENTS.md files from the project root to the current working
/// directory, inclusive. Symlinks are allowed.
#[cfg(test)]
async fn agents_md_paths(
    config: &Config,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
) -> io::Result<Vec<PathUri>> {
    Ok(discover_agents_md(config, cwd, fs).await?.paths)
}

async fn discover_agents_md(
    config: &Config,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
) -> io::Result<AgentsMdDiscovery> {
    let dir = cwd.clone();
    let mut merged = TomlValue::Table(toml::map::Map::new());
    for layer in config.config_layer_stack.get_layers(
        ConfigLayerStackOrdering::LowestPrecedenceFirst,
        /*include_disabled*/ false,
    ) {
        if matches!(layer.name, ConfigLayerSource::Project { .. }) {
            continue;
        }
        merge_toml_values(&mut merged, &layer.config);
    }
    let project_root_markers = match project_root_markers_from_config(&merged) {
        Ok(Some(markers)) => markers,
        Ok(None) => default_project_root_markers(),
        Err(err) => {
            tracing::warn!("invalid project_root_markers: {err}");
            default_project_root_markers()
        }
    };
    let project_root = find_nearest_ancestor_with_markers(
        fs,
        &dir,
        project_root_markers,
        FindUpErrorPolicy::Propagate,
        /*sandbox*/ None,
    )
    .await?;
    let (project_root, search_dirs) = if let Some(root) = project_root {
        let mut dirs = Vec::new();
        let mut cursor = dir.clone();
        loop {
            dirs.push(cursor.clone());
            if cursor == root {
                break;
            }
            let Some(parent) = cursor.parent() else {
                break;
            };
            cursor = parent;
        }
        dirs.reverse();
        (root, dirs)
    } else {
        (dir.clone(), vec![dir])
    };

    let mut found = Vec::new();
    let candidate_filenames = candidate_filenames(config);
    for directory in search_dirs {
        for name in &candidate_filenames {
            let candidate = directory
                .join(name)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
            match fs.get_metadata(&candidate, /*sandbox*/ None).await {
                Ok(metadata) if metadata.is_file => {
                    found.push(candidate);
                    break;
                }
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
    }
    Ok(AgentsMdDiscovery {
        project_root,
        paths: found,
    })
}

async fn agents_md_focus_dir(
    fs: &dyn ExecutorFileSystem,
    focus_path: &PathUri,
) -> io::Result<Option<PathUri>> {
    match fs.get_metadata(focus_path, /*sandbox*/ None).await {
        Ok(metadata) if metadata.is_directory => Ok(Some(focus_path.clone())),
        Ok(metadata) if metadata.is_file => Ok(focus_path.parent()),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let Some(parent) = focus_path.parent() else {
                return Ok(None);
            };
            match fs.get_metadata(&parent, /*sandbox*/ None).await {
                Ok(metadata) if metadata.is_directory => Ok(Some(parent)),
                Ok(_) => Ok(None),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err),
    }
}

fn candidate_filenames(config: &Config) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::with_capacity(2 + config.project_doc_fallback_filenames.len());
    names.push(LOCAL_AGENTS_MD_FILENAME);
    names.push(DEFAULT_AGENTS_MD_FILENAME);
    for candidate in &config.project_doc_fallback_filenames {
        let candidate = candidate.as_str();
        if candidate.is_empty() {
            continue;
        }
        if !names.contains(&candidate) {
            names.push(candidate);
        }
    }
    names
}

/// Model-visible instructions loaded from AGENTS.md files and internal
/// guidance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadedAgentsMd {
    /// Host-provided user instructions.
    user_instructions: Option<UserInstructions>,

    /// Ordered instructions and their provenance.
    entries: Vec<InstructionEntry>,
}

impl LoadedAgentsMd {
    /// Creates loaded instructions containing one user-level AGENTS.md entry.
    pub fn new_user(contents: String, path: AbsolutePathBuf) -> Self {
        if contents.trim().is_empty() {
            return Self::default();
        }
        Self {
            user_instructions: Some(UserInstructions {
                text: contents,
                source: path,
            }),
            entries: Vec::new(),
        }
    }

    fn from_user_instructions(user_instructions: Option<UserInstructions>) -> Self {
        Self {
            user_instructions: user_instructions
                .filter(|instructions| !instructions.text.trim().is_empty()),
            entries: Vec::new(),
        }
    }

    /// Creates source-less user instructions for tests.
    ///
    /// This cannot be gated with `#[cfg(test)]` because integration tests
    /// compile `codex-core` as a normal dependency without that configuration.
    pub fn from_text_for_testing(contents: impl Into<String>) -> Self {
        let contents = contents.into();
        if contents.trim().is_empty() {
            return Self::default();
        }
        Self {
            user_instructions: None,
            entries: vec![InstructionEntry {
                contents,
                provenance: InstructionProvenance::Internal,
            }],
        }
    }

    fn is_empty(&self) -> bool {
        self.user_instructions.is_none()
            && self
                .entries
                .iter()
                .all(|entry| entry.contents.trim().is_empty())
    }

    /// Returns the concatenated model-visible instruction text.
    pub fn text(&self) -> String {
        if self.has_multiple_project_environments() {
            self.environment_labeled_text()
        } else {
            self.legacy_text()
        }
    }

    fn legacy_text(&self) -> String {
        let mut output = String::new();
        let mut has_previous = false;
        let mut previous_was_project = false;
        if let Some(instructions) = &self.user_instructions {
            output.push_str(&instructions.text);
            has_previous = true;
        }
        for entry in &self.entries {
            let is_project = matches!(&entry.provenance, InstructionProvenance::Project { .. });
            if has_previous {
                // The project-doc marker tells the model where workspace-scoped
                // instructions begin, so it is only needed on the transition
                // from user or internal instructions to project instructions.
                let separator = if is_project && !previous_was_project {
                    AGENTS_MD_SEPARATOR
                } else {
                    "\n\n"
                };
                output.push_str(separator);
            }
            output.push_str(&entry.contents);
            has_previous = true;
            previous_was_project = is_project;
        }
        output
    }

    fn environment_labeled_text(&self) -> String {
        let mut output = String::new();
        let mut has_previous = false;
        let mut previous_environment: Option<(&str, &PathUri)> = None;
        if let Some(instructions) = &self.user_instructions {
            output.push_str(&instructions.text);
            has_previous = true;
        }
        for entry in &self.entries {
            match &entry.provenance {
                InstructionProvenance::Project {
                    environment_id,
                    cwd,
                    ..
                } => {
                    if has_previous {
                        output.push_str("\n\n");
                    }
                    // One environment can contribute several hierarchical AGENTS.md files from
                    // its project root through its cwd. Label that environment once for the
                    // complete group rather than repeating the label before every file.
                    let environment = (environment_id.as_str(), cwd);
                    if previous_environment != Some(environment) {
                        output.push_str(&format!(
                            "for `{}` with root {}\n\n",
                            environment_id,
                            cwd.inferred_native_path_string()
                        ));
                    }
                    output.push_str(&entry.contents);
                    previous_environment = Some(environment);
                }
                InstructionProvenance::Internal => {
                    if has_previous {
                        output.push_str("\n\n");
                    }
                    output.push_str(&entry.contents);
                    previous_environment = None;
                }
            }
            has_previous = true;
        }
        output
    }

    pub(crate) fn contextual_user_fragment(&self) -> ContextUserInstructions {
        // One contributing project environment retains the legacy cwd wrapper. With two or more,
        // the body labels every contributing environment itself, so the outer cwd is omitted.
        let directory = if self.has_multiple_project_environments() {
            None
        } else {
            self.single_project_cwd()
                .map(PathUri::inferred_native_path_string)
        };
        ContextUserInstructions {
            directory,
            text: self.text(),
        }
    }

    /// Returns the AGENTS.md files that supplied instruction entries.
    pub fn sources(&self) -> impl Iterator<Item = PathUri> + '_ {
        self.user_instructions
            .iter()
            .map(|instructions| PathUri::from_abs_path(&instructions.source))
            .chain(
                self.entries
                    .iter()
                    .filter_map(|entry| entry.provenance.path()),
            )
    }

    fn has_multiple_project_environments(&self) -> bool {
        let mut first_environment_id = None;
        self.entries.iter().any(|entry| {
            let InstructionProvenance::Project { environment_id, .. } = &entry.provenance else {
                return false;
            };
            match first_environment_id {
                Some(first_environment_id) => first_environment_id != environment_id,
                None => {
                    first_environment_id = Some(environment_id);
                    false
                }
            }
        })
    }

    fn single_project_cwd(&self) -> Option<&PathUri> {
        self.entries
            .iter()
            .find_map(|entry| match &entry.provenance {
                InstructionProvenance::Project { cwd, .. } => Some(cwd),
                InstructionProvenance::Internal => None,
            })
    }
}

/// One model-visible instruction and its provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InstructionEntry {
    /// Model-visible instruction text.
    contents: String,

    /// Origin of the instruction.
    provenance: InstructionProvenance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InstructionProvenance {
    /// Workspace instructions discovered from project AGENTS.md files.
    Project {
        /// Exact AGENTS.md file, distinct from the environment's selected cwd.
        source_path: PathUri,
        environment_id: String,
        cwd: PathUri,
    },

    /// Instructions without a file source, including internally defined guidance.
    Internal,
}

impl InstructionProvenance {
    fn path(&self) -> Option<PathUri> {
        match self {
            Self::Project { source_path, .. } => Some(source_path.clone()),
            Self::Internal => None,
        }
    }
}

#[cfg(test)]
#[path = "agents_md_tests.rs"]
mod tests;
