//! Typed, triggerable rules for Link context injection.
//!
//! Rules are markdown files with a small frontmatter header, loaded from
//! `<cwd>/.codex/rules/` (project) and `<codex_home>/rules/` (user) at thread
//! start. The frontmatter decides the rule's context class instead of dumping
//! every rule into the prompt:
//!
//! - `always_apply: true` — body injected every turn.
//! - `globs: src/**/*.rs, tests/**` — body injected once a file touched this
//!   session matches; until then only the index line is visible.
//! - `description: ...` only — listed in a bounded index so the model can
//!   read the file when the description becomes relevant.
//! - bare file — index line with path only (manual reference).
//!
//! The rendered block is bounded: rule bodies are middle-truncated with a
//! pointer to the source file, and the whole fragment has a hard token cap.

use std::path::Path;
use std::path::PathBuf;

use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use globset::GlobBuilder;
use globset::GlobSet;
use globset::GlobSetBuilder;

const RULES_START: &str = "<codex_link_rules>";
const RULES_END: &str = "</codex_link_rules>";
/// Per-rule body budget; oversized bodies keep head and tail plus a pointer.
const MAX_RULE_BODY_CHARS: usize = 2_000;
/// Hard cap for the whole rendered fragment.
const MAX_RULES_FRAGMENT_TOKENS: usize = 1_500;
const MAX_RULES_PER_SOURCE_DIR: usize = 64;

#[derive(Clone, Debug, Default)]
pub(crate) struct LinkRule {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) description: Option<String>,
    pub(crate) globs: Vec<String>,
    pub(crate) always_apply: bool,
    pub(crate) body: String,
}

impl LinkRule {
    fn matcher(&self) -> Option<GlobSet> {
        if self.globs.is_empty() {
            return None;
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in &self.globs {
            match GlobBuilder::new(pattern).literal_separator(false).build() {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(err) => {
                    tracing::warn!(
                        "link rule {}: invalid glob `{pattern}`: {err}",
                        self.path.display()
                    );
                }
            }
        }
        builder.build().ok()
    }

    fn index_line(&self) -> String {
        match self.description.as_deref() {
            Some(description) => format!(
                "- {} — {} ({})",
                self.name,
                description,
                self.path.display()
            ),
            None => format!("- {} ({})", self.name, self.path.display()),
        }
    }
}

/// Rules loaded for one thread, with the project root used to relativize
/// touched paths for glob matching.
#[derive(Clone, Debug, Default)]
pub(crate) struct LinkRules {
    pub(crate) project_root: PathBuf,
    pub(crate) rules: Vec<LinkRule>,
}

impl LinkRules {
    /// Loads project rules (`<cwd>/.codex/rules/`) and user rules
    /// (`<codex_home>/rules/`), project first so its entries render first.
    /// Best-effort: unreadable files or directories are skipped with a log.
    pub(crate) fn load(project_root: &Path, codex_home: &Path) -> Self {
        let mut rules = Vec::new();
        for dir in [
            project_root.join(".codex").join("rules"),
            codex_home.join("rules"),
        ] {
            rules.extend(load_rules_dir(&dir));
        }
        Self {
            project_root: project_root.to_path_buf(),
            rules,
        }
    }

    /// Renders the bounded rules fragment for the current turn, or `None`
    /// when no rules exist. `files_touched` selects which glob-scoped rules
    /// have activated this session.
    pub(crate) fn render(&self, files_touched: &[String]) -> Option<String> {
        if self.rules.is_empty() {
            return None;
        }

        let relative_touched: Vec<String> = files_touched
            .iter()
            .map(|file| {
                Path::new(file)
                    .strip_prefix(&self.project_root)
                    .map(|relative| relative.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| file.clone())
            })
            .collect();

        let mut active_sections = Vec::new();
        let mut index_lines = Vec::new();
        for rule in &self.rules {
            let matched_file = rule.matcher().and_then(|matcher| {
                relative_touched
                    .iter()
                    .find(|file| matcher.is_match(file))
                    .cloned()
            });
            if rule.always_apply || matched_file.is_some() {
                let reason = match matched_file {
                    Some(file) => format!("path-scoped, matched {file}"),
                    None => "always".to_string(),
                };
                let body = truncate_rule_body(&rule.body, &rule.path);
                active_sections.push(format!(
                    "[{reason}] {} ({}):\n{body}",
                    rule.name,
                    rule.path.display()
                ));
            } else {
                index_lines.push(rule.index_line());
            }
        }

        let mut sections = Vec::new();
        sections.push(format!(
            "{RULES_START}\nProject and user rules. Active rules below are binding for this turn; indexed rules list what else exists — read the file when its description matches your task."
        ));
        sections.extend(active_sections);
        if !index_lines.is_empty() {
            sections.push(format!("Available rules:\n{}", index_lines.join("\n")));
        }
        sections.push(RULES_END.to_string());

        let rendered = sections.join("\n\n");
        Some(truncate_text(
            &rendered,
            TruncationPolicy::Tokens(MAX_RULES_FRAGMENT_TOKENS),
        ))
    }
}

fn truncate_rule_body(body: &str, path: &Path) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= MAX_RULE_BODY_CHARS {
        return sanitize_rule_text(trimmed);
    }
    let truncated = truncate_text(trimmed, TruncationPolicy::Bytes(MAX_RULE_BODY_CHARS));
    sanitize_rule_text(&format!(
        "{truncated}\n(rule truncated; read {} for the full text)",
        path.display()
    ))
}

/// A rule body must not be able to fake the fragment boundary.
fn sanitize_rule_text(text: &str) -> String {
    text.replace(RULES_START, "<rules>")
        .replace(RULES_END, "</rules>")
}

fn load_rules_dir(dir: &Path) -> Vec<LinkRule> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("failed to read rules directory {}: {err}", dir.display());
            }
            return Vec::new();
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    paths.sort();
    paths.truncate(MAX_RULES_PER_SOURCE_DIR);

    paths
        .into_iter()
        .filter_map(|path| match std::fs::read_to_string(&path) {
            Ok(contents) => Some(parse_rule_file(&path, &contents)),
            Err(err) => {
                tracing::warn!("failed to read rule file {}: {err}", path.display());
                None
            }
        })
        .collect()
}

/// Parses a rule file: optional `---`-delimited frontmatter with
/// `description`, `globs` (comma-separated), and `always_apply` keys,
/// followed by the markdown body.
fn parse_rule_file(path: &Path, contents: &str) -> LinkRule {
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rule".to_string());
    let mut rule = LinkRule {
        name,
        path: path.to_path_buf(),
        ..LinkRule::default()
    };

    let mut lines = contents.lines();
    let Some(first) = lines.next() else {
        return rule;
    };
    if first.trim() != "---" {
        rule.body = contents.to_string();
        return rule;
    }

    let mut body_lines = Vec::new();
    let mut in_frontmatter = true;
    for line in lines {
        if in_frontmatter {
            if line.trim() == "---" {
                in_frontmatter = false;
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "description"
                    if !value.is_empty() => {
                        rule.description = Some(value.to_string());
                    }
                "globs" => {
                    rule.globs = value
                        .split(',')
                        .map(str::trim)
                        .filter(|pattern| !pattern.is_empty())
                        .map(str::to_string)
                        .collect();
                }
                // Accept both snake_case and Cursor-style camelCase.
                "always_apply" | "alwaysApply" => {
                    rule.always_apply = value.eq_ignore_ascii_case("true");
                }
                _ => {}
            }
        } else {
            body_lines.push(line);
        }
    }
    if in_frontmatter {
        // Unterminated frontmatter: treat the whole file as body so the rule
        // is never silently dropped.
        rule.body = contents.to_string();
        rule.description = None;
        rule.globs = Vec::new();
        rule.always_apply = false;
        return rule;
    }
    rule.body = body_lines.join("\n").trim().to_string();
    rule
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
