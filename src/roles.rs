use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::types::{RoleApplyMode, RoleSpec};

#[derive(Debug, Default)]
pub struct RoleLibrary {
    roles: Vec<RoleSpec>,
}

impl RoleLibrary {
    pub fn load(role_dirs: &[PathBuf]) -> Result<Self> {
        let mut roles = Vec::new();
        for dir in role_dirs {
            if !dir.exists() {
                continue;
            }
            for entry in fs::read_dir(dir)
                .with_context(|| format!("failed to read roles directory {}", dir.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                    continue;
                }
                roles.push(load_role(&path)?);
            }
        }
        roles.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));
        Ok(Self { roles })
    }

    pub fn all(&self) -> &[RoleSpec] {
        &self.roles
    }

    pub fn get_by_name(&self, name: &str) -> Option<RoleSpec> {
        self.roles
            .iter()
            .find(|role| role.name.eq_ignore_ascii_case(name))
            .cloned()
    }

    pub fn get_by_path(&self, path: &Path) -> Option<RoleSpec> {
        self.roles.iter().find(|role| role.path == path).cloned()
    }

    pub fn get_index(&self, index: usize) -> Option<RoleSpec> {
        self.roles.get(index).cloned()
    }
}

#[derive(Debug, Deserialize)]
struct FrontMatter {
    name: Option<String>,
    description: Option<String>,
    extra_args: Option<Vec<String>>,
    apply_mode: Option<RoleApplyMode>,
}

fn load_role(path: &Path) -> Result<RoleSpec> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read role file {}", path.display()))?;
    let (front_matter, body) = split_front_matter(&raw)?;

    let fallback_name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| anyhow!("failed to determine role name for {}", path.display()))?
        .replace(['_', '-'], " ");

    Ok(RoleSpec {
        name: front_matter
            .as_ref()
            .and_then(|meta| meta.name.clone())
            .unwrap_or(fallback_name),
        description: front_matter
            .as_ref()
            .and_then(|meta| meta.description.clone()),
        body,
        default_extra_args: front_matter
            .as_ref()
            .and_then(|meta| meta.extra_args.clone())
            .unwrap_or_default(),
        apply_mode: front_matter
            .and_then(|meta| meta.apply_mode)
            .unwrap_or_default(),
        path: path.to_path_buf(),
    })
}

fn split_front_matter(raw: &str) -> Result<(Option<FrontMatter>, String)> {
    if !raw.starts_with("---\n") && !raw.starts_with("---\r\n") {
        return Ok((None, raw.trim().to_string()));
    }

    let mut lines = raw.lines();
    let first = lines.next().unwrap_or_default();
    if first.trim() != "---" {
        return Ok((None, raw.trim().to_string()));
    }

    let mut front = Vec::new();
    let mut body_lines = Vec::new();
    let mut in_front_matter = true;
    for line in lines {
        if in_front_matter && line.trim() == "---" {
            in_front_matter = false;
            continue;
        }
        if in_front_matter {
            front.push(line);
        } else {
            body_lines.push(line);
        }
    }

    if in_front_matter {
        return Ok((None, raw.trim().to_string()));
    }

    let meta = serde_yaml::from_str::<FrontMatter>(&front.join("\n"))
        .context("failed to parse role front matter")?;
    Ok((Some(meta), body_lines.join("\n").trim().to_string()))
}

pub fn write_sample_roles(target_dir: &Path, force: bool) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(target_dir)
        .with_context(|| format!("failed to create roles directory {}", target_dir.display()))?;

    let samples = sample_roles();
    let mut written = Vec::new();
    for (file_name, contents) in samples {
        let path = target_dir.join(file_name);
        if path.exists() && !force {
            continue;
        }
        fs::write(&path, contents)
            .with_context(|| format!("failed to write sample role {}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

fn sample_roles() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        (
            "reviewer.md",
            r#"---
name: Reviewer
description: Bug-focused code reviewer that prioritizes regressions, edge cases, and missing tests.
apply_mode: prepend
---
You are a senior code reviewer.

Primary goals:
- Find correctness bugs, regressions, risky assumptions, and security issues.
- Prefer concrete findings over broad summaries.
- Call out missing tests when behavior could regress.

Output expectations:
- Lead with the highest-severity issues first.
- Reference files and behavior precisely.
- Keep the tone direct and technical.
"#,
        ),
        (
            "planner.md",
            r#"---
name: Planner
description: Splits larger work into tractable steps and keeps execution orderly.
apply_mode: system
---
You are an engineering planner.

Rules:
- Break work into a small number of concrete steps.
- Surface dependencies, sequencing risks, and missing information.
- Prefer actionable next steps over abstract discussion.
- Keep plans concise enough to execute immediately.
"#,
        ),
        (
            "shipper.md",
            r#"---
name: Shipper
description: Pragmatic implementation role focused on finishing working code with minimal churn.
apply_mode: prepend
---
You are an implementation-focused software engineer.

Rules:
- Make the smallest defensible change that solves the problem.
- Preserve existing patterns unless they are clearly broken.
- Verify behavior before declaring work complete.
- Avoid speculative refactors.
"#,
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::split_front_matter;

    #[test]
    fn parses_front_matter() {
        let source = r#"---
name: Reviewer
apply_mode: prepend
---
Role body
"#;

        let (meta, body) = split_front_matter(source).expect("front matter parse should succeed");
        let meta = meta.expect("meta should exist");
        assert_eq!(meta.name.as_deref(), Some("Reviewer"));
        assert_eq!(meta.apply_mode, Some(crate::types::RoleApplyMode::Prepend));
        assert_eq!(body, "Role body");
    }
}
