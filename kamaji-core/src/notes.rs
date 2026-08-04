use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use crate::error::NoteError;
use crate::okf;
use crate::prompt::IngestResult;

/// Writes `result` as an Obsidian-compatible markdown note with YAML
/// frontmatter under `<repo_root>/notes/<category>/`. Returns the path
/// written, relative to `repo_root`, for use in git commands.
pub fn write_note(
    repo_root: &Path,
    date: NaiveDate,
    result: &IngestResult,
) -> Result<PathBuf, NoteError> {
    let category_dir = repo_root.join("notes").join(&result.category);
    std::fs::create_dir_all(&category_dir).map_err(|source| NoteError::CreateDir {
        path: category_dir.clone(),
        source,
    })?;

    let date_str = date.format("%Y-%m-%d").to_string();
    let path = unique_path(&category_dir, &date_str, &result.slug);

    let contents = render_markdown(date_str.as_str(), result);
    std::fs::write(&path, contents).map_err(|source| NoteError::Write {
        path: path.clone(),
        source,
    })?;

    let relative = path.strip_prefix(repo_root).unwrap_or(&path).to_path_buf();
    Ok(relative)
}

/// Starting-point categories offered to the ingest prompt before any notes
/// exist, so day-one messages land in a topic (e.g. "agentic-ai") instead of
/// Claude minting an arbitrary first folder with nothing to anchor on -- or,
/// worse, converging on a broad format label like "tech-news". Not a fixed
/// taxonomy: `categories_for_prompt` unions these with whatever folders
/// already exist on disk, and the prompt still lets Claude name a new
/// category when none of these fit.
const SEED_CATEGORIES: &[&str] = &[
    "agentic-ai",
    "coding-best-practices",
    "mlops",
    "machine-learning",
    "llm-inference",
    "llm-theory",
];

/// Categories to hand to the ingest prompt as reuse candidates: existing
/// `notes/` folders plus [`SEED_CATEGORIES`], deduped and sorted.
pub fn categories_for_prompt(repo_root: &Path) -> Vec<String> {
    let mut categories = list_categories(repo_root);
    for seed in SEED_CATEGORIES {
        if !categories.iter().any(|c| c == seed) {
            categories.push((*seed).to_string());
        }
    }
    categories.sort();
    categories
}

/// Enumerates existing category folder names under `<repo_root>/notes/`, so
/// they can be interpolated into the ingest prompt and Claude can reuse one
/// instead of always minting a new folder. Gathered here (rather than
/// discovered by Claude via tool calls) because the state is trivial for
/// Rust to collect up front -- see TODO.md. Missing `notes/` (first run) or
/// any other read error just yields no categories; enumeration is a prompt
/// hint, not something a job should fail over.
pub fn list_categories(repo_root: &Path) -> Vec<String> {
    let notes_dir = repo_root.join("notes");
    let Ok(entries) = std::fs::read_dir(&notes_dir) else {
        return Vec::new();
    };

    let mut categories: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    categories.sort();
    categories
}

/// Appends a numeric suffix if `<date>-<slug>.md` already exists, so two
/// notes on the same day with the same slug don't clobber each other.
fn unique_path(notes_dir: &Path, date_str: &str, slug: &str) -> PathBuf {
    let base = notes_dir.join(format!("{date_str}-{slug}.md"));
    if !base.exists() {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = notes_dir.join(format!("{date_str}-{slug}-{n}.md"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Renders OKF-conformant frontmatter (see `okf.rs`). The OKF core fields
/// (`type`, `title`, `description`, `resource`, `tags`, `timestamp`) come
/// first; `category` and `importance` ride along as producer-defined custom
/// fields, which OKF consumers must preserve.
fn render_markdown(date_str: &str, result: &IngestResult) -> String {
    let mut fm = String::new();
    fm.push_str("---\n");
    fm.push_str(&format!("type: {}\n", okf::TYPE_NOTE));
    fm.push_str(&format!("title: {}\n", okf::yaml_quote(&result.title)));
    fm.push_str(&format!(
        "description: {}\n",
        okf::yaml_quote(&okf::description_from_summary(&result.summary))
    ));
    if let Some(source) = &result.source_url {
        // OKF `resource`: the canonical URI for the underlying asset. The
        // ingest contract's `source_url` is exactly that.
        fm.push_str(&format!("resource: {}\n", okf::yaml_quote(source)));
    }
    let tags = result
        .tags
        .iter()
        .map(|t| okf::yaml_quote(t))
        .collect::<Vec<_>>()
        .join(", ");
    fm.push_str(&format!("tags: [{tags}]\n"));
    // OKF `timestamp` is ISO 8601. Ingest notes carry a date only (no clock
    // time), so anchor them at midnight UTC.
    fm.push_str(&format!("timestamp: {date_str}T00:00:00Z\n"));
    // Custom fields, kept alongside the OKF core:
    fm.push_str(&format!(
        "category: {}\n",
        okf::yaml_quote(&result.category)
    ));
    fm.push_str(&format!("importance: {}\n", result.importance));
    fm.push_str("---\n\n");
    fm.push_str(result.summary.trim());
    fm.push('\n');
    fm
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> IngestResult {
        IngestResult {
            title: "A Title".to_string(),
            summary: "A short summary.".to_string(),
            importance: 4,
            tags: vec!["rust".to_string(), "async".to_string()],
            source_url: Some("https://example.com".to_string()),
            slug: "a-title".to_string(),
            category: "programming".to_string(),
        }
    }

    #[test]
    fn writes_note_with_expected_filename_and_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();
        let rel = write_note(dir.path(), date, &sample()).unwrap();
        assert_eq!(
            rel,
            PathBuf::from("notes/programming/2026-07-09-a-title.md")
        );

        let contents = std::fs::read_to_string(dir.path().join(&rel)).unwrap();
        assert!(contents.starts_with("---\n"));
        assert!(contents.contains("type: note\n"));
        assert!(contents.contains("title: \"A Title\"\n"));
        assert!(contents.contains("description: \"A short summary.\"\n"));
        assert!(contents.contains("timestamp: 2026-07-09T00:00:00Z\n"));
        assert!(contents.contains("category: \"programming\"\n"));
        assert!(contents.contains("resource: \"https://example.com\"\n"));
        assert!(contents.contains("importance: 4\n"));
        assert!(contents.contains("tags: [\"rust\", \"async\"]\n"));
        assert!(contents.contains("A short summary."));
    }

    #[test]
    fn omits_resource_when_no_source_url() {
        let dir = tempfile::tempdir().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();
        let mut result = sample();
        result.source_url = None;
        let rel = write_note(dir.path(), date, &result).unwrap();
        let contents = std::fs::read_to_string(dir.path().join(&rel)).unwrap();
        assert!(!contents.contains("resource:"));
    }

    #[test]
    fn dedupes_same_day_same_slug_filenames() {
        let dir = tempfile::tempdir().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();
        let first = write_note(dir.path(), date, &sample()).unwrap();
        let second = write_note(dir.path(), date, &sample()).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            second,
            PathBuf::from("notes/programming/2026-07-09-a-title-2.md")
        );
    }

    #[test]
    fn different_categories_get_separate_folders() {
        let dir = tempfile::tempdir().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();
        let mut other = sample();
        other.category = "cooking".to_string();

        let first = write_note(dir.path(), date, &sample()).unwrap();
        let second = write_note(dir.path(), date, &other).unwrap();

        assert_eq!(
            first,
            PathBuf::from("notes/programming/2026-07-09-a-title.md")
        );
        assert_eq!(second, PathBuf::from("notes/cooking/2026-07-09-a-title.md"));
    }

    #[test]
    fn list_categories_returns_empty_when_notes_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_categories(dir.path()).is_empty());
    }

    #[test]
    fn list_categories_returns_sorted_folder_names() {
        let dir = tempfile::tempdir().unwrap();
        write_note(
            dir.path(),
            NaiveDate::from_ymd_opt(2026, 7, 9).unwrap(),
            &sample(),
        )
        .unwrap();
        let mut other = sample();
        other.category = "cooking".to_string();
        write_note(
            dir.path(),
            NaiveDate::from_ymd_opt(2026, 7, 9).unwrap(),
            &other,
        )
        .unwrap();

        assert_eq!(
            list_categories(dir.path()),
            vec!["cooking".to_string(), "programming".to_string()]
        );
    }

    #[test]
    fn categories_for_prompt_returns_seeds_when_notes_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(categories_for_prompt(dir.path()), {
            let mut seeds: Vec<String> = SEED_CATEGORIES.iter().map(|s| s.to_string()).collect();
            seeds.sort();
            seeds
        });
    }

    #[test]
    fn categories_for_prompt_dedupes_folder_matching_a_seed() {
        let dir = tempfile::tempdir().unwrap();
        let mut result = sample();
        result.category = "mlops".to_string();
        write_note(
            dir.path(),
            NaiveDate::from_ymd_opt(2026, 7, 9).unwrap(),
            &result,
        )
        .unwrap();

        let categories = categories_for_prompt(dir.path());
        assert_eq!(
            categories.iter().filter(|c| c.as_str() == "mlops").count(),
            1
        );
        assert!(categories.contains(&"agentic-ai".to_string()));
    }
}
