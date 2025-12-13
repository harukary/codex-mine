use codex_protocol::subagents::SubAgentDefinition;
use codex_protocol::subagents::SubAgentMode;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use tokio::fs;

use crate::config::find_codex_home;
use crate::git_info::resolve_root_git_project_for_trust;

const CONFIG_DIR: &str = ".codex";
const SUBAGENTS_DIR: &str = "subagents";

/// Return the default subagents directory: `$CODEX_HOME/subagents`.
/// If `CODEX_HOME` cannot be resolved, returns `None`.
pub fn default_subagents_dir() -> Option<PathBuf> {
    find_codex_home().ok().map(|home| home.join(SUBAGENTS_DIR))
}

/// Return potential subagent roots in priority order.
///
/// The repository-local `.codex/subagents` is returned first when available,
/// followed by the `$CODEX_HOME/subagents` directory.
pub fn subagent_search_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Some(repo_root) = resolve_root_git_project_for_trust(cwd) {
        roots.push(repo_root.join(CONFIG_DIR).join(SUBAGENTS_DIR));
    }

    if let Some(home_subagents) = default_subagents_dir() {
        roots.push(home_subagents);
    }

    roots
}

/// Discover subagents across all search roots, deduplicating by name.
/// Repository subagents take priority over home subagents when names conflict.
pub async fn discover_subagents(cwd: &Path) -> Vec<SubAgentDefinition> {
    let mut subagents: Vec<SubAgentDefinition> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for root in subagent_search_roots(cwd) {
        for subagent in discover_subagents_in(&root).await {
            if seen.insert(subagent.name.clone()) {
                subagents.push(subagent);
            }
        }
    }

    subagents.sort_by(|a, b| a.name.cmp(&b.name));
    subagents
}

/// Discover subagent files in the given directory, returning entries sorted by name.
/// Non-files are ignored. If the directory does not exist or cannot be read, returns empty.
pub async fn discover_subagents_in(dir: &Path) -> Vec<SubAgentDefinition> {
    let mut out: Vec<SubAgentDefinition> = Vec::new();
    let mut entries = match fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(_) => return out,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let is_file_like = fs::metadata(&path)
            .await
            .map(|m| m.is_file())
            .unwrap_or(false);
        if !is_file_like {
            continue;
        }
        let is_md = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("md"))
            .unwrap_or(false);
        if !is_md {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let content = match fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let (description, tools_allowed, tools_blocked, mode, body) = parse_frontmatter(&content);
        out.push(SubAgentDefinition {
            name,
            path,
            system_prompt: body,
            description,
            tools_allowed,
            tools_blocked,
            mode,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Parse optional YAML-like frontmatter at the beginning of `content`.
/// Supported keys:
/// - `description`: short description shown to the main agent
/// - `tools_allowed`: comma-separated list of allowed tools
/// - `tools_blocked`: comma-separated list of blocked tools
/// - `mode`: `read-only` (default) / `full-auto` / `danger-full-access`
///
/// Returns `(description, tools_allowed, tools_blocked, mode, body_without_frontmatter)`.
fn parse_frontmatter(
    content: &str,
) -> (
    Option<String>,
    Vec<String>,
    Vec<String>,
    Option<SubAgentMode>,
    String,
) {
    let mut segments = content.split_inclusive('\n');
    let Some(first_segment) = segments.next() else {
        return (None, Vec::new(), Vec::new(), None, String::new());
    };
    let first_line = first_segment.trim_end_matches(['\r', '\n']);
    if first_line.trim() != "---" {
        return (None, Vec::new(), Vec::new(), None, content.to_string());
    }

    let mut desc: Option<String> = None;
    let mut allowed: Vec<String> = Vec::new();
    let mut blocked: Vec<String> = Vec::new();
    let mut mode: Option<SubAgentMode> = None;
    let mut frontmatter_closed = false;
    let mut consumed = first_segment.len();

    for segment in segments {
        let line = segment.trim_end_matches(['\r', '\n']);
        let trimmed = line.trim();

        if trimmed == "---" {
            frontmatter_closed = true;
            consumed += segment.len();
            break;
        }

        if trimmed.is_empty() || trimmed.starts_with('#') {
            consumed += segment.len();
            continue;
        }

        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let mut val = v.trim().to_string();
            if val.len() >= 2 {
                let bytes = val.as_bytes();
                let first = bytes[0];
                let last = bytes[bytes.len() - 1];
                if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
                    val = val[1..val.len().saturating_sub(1)].to_string();
                }
            }
            match key.as_str() {
                "description" => desc = Some(val),
                "tools_allowed" | "tools-allowed" => allowed = split_list(&val),
                "tools_blocked" | "tools-blocked" => blocked = split_list(&val),
                "mode" => mode = parse_mode(&val),
                _ => {}
            }
        }

        consumed += segment.len();
    }

    if !frontmatter_closed {
        // Unterminated frontmatter: treat input as-is.
        return (None, Vec::new(), Vec::new(), None, content.to_string());
    }

    let body = if consumed >= content.len() {
        String::new()
    } else {
        content[consumed..].to_string()
    };
    (desc, allowed, blocked, mode, body)
}

fn split_list(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_mode(input: &str) -> Option<SubAgentMode> {
    match input.trim().to_ascii_lowercase().as_str() {
        "read-only" | "readonly" => Some(SubAgentMode::ReadOnly),
        "full-auto" | "full_auto" => Some(SubAgentMode::FullAuto),
        "danger-full-access" | "danger_full_access" | "danger" => {
            Some(SubAgentMode::DangerFullAccess)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serial_test::serial;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    #[tokio::test]
    async fn empty_when_dir_missing() {
        let tmp = tempdir().expect("create TempDir");
        let missing = tmp.path().join("nope");
        let found = discover_subagents_in(&missing).await;
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn discovers_and_sorts_files() {
        let tmp = tempdir().expect("create TempDir");
        let dir = tmp.path();
        fs::write(dir.join("b.md"), b"b").unwrap();
        fs::write(dir.join("a.md"), b"a").unwrap();
        fs::create_dir(dir.join("subdir")).unwrap();
        let found = discover_subagents_in(dir).await;
        let names: Vec<String> = found.into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn skips_non_utf8_files() {
        let tmp = tempdir().expect("create TempDir");
        let dir = tmp.path();
        fs::write(dir.join("good.md"), b"hello").unwrap();
        fs::write(dir.join("bad.md"), vec![0xFF, 0xFE, b'\n']).unwrap();
        let found = discover_subagents_in(dir).await;
        let names: Vec<String> = found.into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["good"]);
    }

    #[tokio::test]
    #[serial]
    async fn discover_subagents_uses_home_and_repo_roots_with_priority() {
        let tmp = tempdir().expect("create TempDir");
        let codex_home = tmp.path().join("home");
        fs::create_dir_all(codex_home.join(SUBAGENTS_DIR)).unwrap();
        let _guard = EnvVarGuard::set("CODEX_HOME", &codex_home);

        let repo_root = tmp.path().join("repo");
        let repo_subagents = repo_root.join(CONFIG_DIR).join(SUBAGENTS_DIR);
        fs::create_dir_all(&repo_subagents).unwrap();

        let status = Command::new("git")
            .args(["init"])
            .current_dir(&repo_root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("initialize git repo");
        assert!(status.success());

        let nested_cwd = repo_root.join("nested");
        fs::create_dir_all(&nested_cwd).unwrap();

        let home_subagents = codex_home.join(SUBAGENTS_DIR);
        fs::write(home_subagents.join("home_only.md"), b"home").unwrap();
        fs::write(home_subagents.join("shared.md"), b"home shared").unwrap();

        fs::write(repo_subagents.join("repo_only.md"), b"repo").unwrap();
        fs::write(repo_subagents.join("shared.md"), b"repo shared").unwrap();

        let subagents = discover_subagents(&nested_cwd).await;
        let names: Vec<String> = subagents.iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["home_only", "repo_only", "shared"]);

        let shared = subagents
            .into_iter()
            .find(|p| p.name == "shared")
            .expect("shared subagent present");
        assert_eq!(shared.system_prompt, "repo shared");
    }

    #[tokio::test]
    async fn parses_frontmatter_and_strips_from_body() {
        let content = "---\nname: ignored\ndescription: \"Sub-agent\"\ntools_allowed: run,read\ntools_blocked: write, net\nmode: full-auto\n---\nBody text";
        let (desc, allowed, blocked, mode, body) = parse_frontmatter(content);
        assert_eq!(desc.as_deref(), Some("Sub-agent"));
        assert_eq!(allowed, vec!["run", "read"]);
        assert_eq!(blocked, vec!["write", "net"]);
        assert_eq!(mode, Some(SubAgentMode::FullAuto));
        assert_eq!(body, "Body text");
    }

    #[test]
    fn parse_frontmatter_preserves_body_newlines() {
        let content =
            "---\r\ndescription: \"Line endings\"\r\n---\r\nFirst line\r\nSecond line\r\n";
        let (_, allowed, blocked, mode, body) = parse_frontmatter(content);
        assert!(allowed.is_empty());
        assert!(blocked.is_empty());
        assert_eq!(mode, None);
        assert_eq!(body, "First line\r\nSecond line\r\n");
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: tests adjust process-scoped environment variables in isolation.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = self.original.take() {
                // SAFETY: tests adjust process-scoped environment variables in isolation.
                unsafe {
                    std::env::set_var(self.key, value);
                }
            } else {
                unsafe {
                    std::env::remove_var(self.key);
                }
            }
        }
    }
}
