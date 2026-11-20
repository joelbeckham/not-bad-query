use std::{
    fs::{self, File},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

const LIMIT: usize = 64 * 1024;
const MANIFEST: &str = "SKILL.md";

#[derive(Deserialize, Serialize)]
pub(super) struct SkillMetadata {
    name: String,
    description: String,
}

#[derive(Clone, Deserialize)]
pub struct Skill {
    pub name: String,
    pub markdown: String,
    #[serde(skip)]
    pub original: Option<String>,
}

impl Skill {
    /// Path relative to the project's skills directory.
    pub fn relative_path(&self) -> String {
        format!("{}/{MANIFEST}", self.name)
    }

    pub(super) fn review(&mut self, root: &Path, original: Option<String>) -> Result<()> {
        let path = self.proposal_path(root)?;
        ensure!(
            !path.try_exists()? || original.is_some(),
            "Ask the assistant to read the existing SKILL.md before editing it."
        );
        self.original = original;
        Ok(())
    }

    pub fn accept(&self, root: &Path) -> Result<()> {
        let path = self.proposal_path(root)?;
        let skills = skills_dir(root)?;
        create_directory(&skills)?;
        create_directory(&skills.join(&self.name))?;
        nbq_core::files::save_script(&path, &self.markdown, self.original.as_deref())?;
        Ok(())
    }

    fn proposal_path(&self, root: &Path) -> Result<PathBuf> {
        ensure!(self.markdown.len() <= LIMIT, "Skill exceeds 64 KiB.");
        parse_metadata(&self.name, &self.markdown).context("Invalid proposed SKILL.md")?;
        markdown_path(&skills_dir(root)?, &self.relative_path())
    }
}

pub(super) fn discover(root: &Path) -> Result<Vec<SkillMetadata>> {
    let dir = skills_dir(root)?;
    if !dir.try_exists()? {
        return Ok(Vec::new());
    }
    let mut skills = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let Ok(name) = entry?.file_name().into_string() else {
            continue;
        };
        if !valid_name(&name) {
            continue;
        }
        let path = format!("{name}/{MANIFEST}");
        if let Ok(metadata) = markdown_path(&dir, &path)
            .and_then(|path| read_markdown(&path))
            .and_then(|markdown| parse_metadata(&name, &markdown))
        {
            skills.push(metadata);
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

pub(super) fn read(root: &Path, skills: &[SkillMetadata], path: &str) -> Result<String> {
    let (name, _) = path
        .split_once('/')
        .context("Use a path like skill-name/SKILL.md.")?;
    ensure!(
        skills.iter().any(|skill| skill.name == name),
        "Unknown skill."
    );
    let content = read_markdown(&markdown_path(&skills_dir(root)?, path)?)?;
    if path == format!("{name}/{MANIFEST}") {
        parse_metadata(name, &content)?;
    }
    Ok(content)
}

fn parse_metadata(name: &str, markdown: &str) -> Result<SkillMetadata> {
    ensure!(valid_name(name), "Invalid skill directory name.");
    let mut lines = markdown
        .trim_start_matches('\u{feff}')
        .split_inclusive('\n');
    ensure!(
        lines
            .next()
            .is_some_and(|line| line.trim_end_matches(['\r', '\n']) == "---"),
        "SKILL.md must start with YAML front matter."
    );
    let mut yaml = String::new();
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let metadata: SkillMetadata =
                serde_saphyr::from_str(&yaml).context("Invalid skill front matter.")?;
            ensure!(
                metadata.name == name,
                "Skill name must match its directory."
            );
            ensure!(
                !metadata.description.trim().is_empty()
                    && metadata.description.chars().count() <= 1024,
                "Skill description must contain 1–1024 characters."
            );
            return Ok(metadata);
        }
        yaml.push_str(line);
    }
    anyhow::bail!("SKILL.md front matter needs a closing --- line.")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name == name.to_lowercase()
        && name.chars().all(|c| c == '-' || c.is_alphanumeric())
}

fn skills_dir(root: &Path) -> Result<PathBuf> {
    let dir = root.canonicalize()?.join("skills");
    check_path(&dir, true)?;
    Ok(dir)
}

/// Paths sent to the model use forward slashes on all platforms.
fn markdown_path(dir: &Path, relative: &str) -> Result<PathBuf> {
    ensure!(
        relative
            .split_once('/')
            .is_some_and(|(name, _)| valid_name(name))
            && relative.ends_with(".md"),
        "Use a Markdown path within a skill."
    );
    let mut path = dir.to_path_buf();
    let mut parts = relative.split('/').peekable();
    while let Some(part) = parts.next() {
        ensure!(
            !part.is_empty()
                && !part.ends_with(['.', ' '])
                && !part
                    .chars()
                    .any(|c| c.is_control() || "\\:<>\"|?*".contains(c)),
            "Invalid skill path."
        );
        path.push(part);
        check_path(&path, parts.peek().is_some())?;
    }
    Ok(path)
}

/// Prevent a skill folder or reference from redirecting outside the project.
fn check_path(path: &Path, directory: bool) -> Result<bool> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        ensure!(
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "Linked skill paths are unsupported."
        );
    }
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "Invalid skill path."
    );
    Ok(true)
}

fn create_directory(path: &Path) -> Result<()> {
    if !check_path(path, true)? {
        match fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                check_path(path, true)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn read_markdown(path: &Path) -> Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(LIMIT as u64 + 1)
        .read_to_string(&mut text)?;
    ensure!(text.len() <= LIMIT, "Skill document exceeds 64 KiB.");
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_skill_writes_only_the_reviewed_revision() {
        let root = tempfile::tempdir().unwrap();
        let mut skill = Skill {
            name: "revenue".into(),
            markdown:
                "---\nname: revenue\ndescription: Use for net revenue.\n---\nExclude refunds."
                    .into(),
            original: None,
        };
        skill.review(root.path(), None).unwrap();
        assert!(!root.path().join("skills").exists());
        skill.accept(root.path()).unwrap();
        assert!(skill.review(root.path(), None).is_err());
        assert!(skill.accept(root.path()).is_err());
        let old = skill.markdown.clone();
        skill.markdown.push_str("\nExclude taxes.");
        skill.review(root.path(), Some(old)).unwrap();
        skill.accept(root.path()).unwrap();
        assert!(skill.accept(root.path()).is_err());
        assert_eq!(
            fs::read_to_string(root.path().join("skills/revenue/SKILL.md")).unwrap(),
            skill.markdown
        );
        skill.name = "other".into();
        assert!(skill.accept(root.path()).is_err());
        assert!(!root.path().join("skills/other").exists());
    }

    #[test]
    fn read_paths_stay_inside_the_named_skill() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("skills/revenue");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(MANIFEST),
            "---\nname: revenue\ndescription: Use for net revenue.\n---",
        )
        .unwrap();
        let skills = discover(root.path()).unwrap();
        for path in [
            "revenue/../revenue/SKILL.md",
            "revenue//SKILL.md",
            "revenue/..\\revenue/SKILL.md",
            "revenue/C:/SKILL.md",
            "revenue/references/notes.txt",
            "unknown/SKILL.md",
        ] {
            assert!(read(root.path(), &skills, path).is_err(), "{path}");
        }
    }
}
