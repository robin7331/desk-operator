//! `desk skill`: the agent skills shipped inside the binary, so they always match this version.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{InstallArgs, SkillCmd};
use crate::error::{DeskError, Result};
use crate::output::{Report, table};

/// Every skill: (name, SKILL.md). The name is also the folder `install` writes it to. A test
/// keeps this in sync with the repo's `skills/` folder.
pub const SKILLS: &[(&str, &str)] = &[
    (
        "desk-operator",
        include_str!("../../skills/desk-operator/SKILL.md"),
    ),
    (
        "desk-add-device",
        include_str!("../../skills/desk-add-device/SKILL.md"),
    ),
];

/// Printed by a bare `desk skill`.
const DEFAULT: &str = "desk-operator";

pub fn run(name: Option<String>, action: Option<SkillCmd>, json: bool) -> Result<Report> {
    match action {
        Some(SkillCmd::List) => Ok(list()),
        Some(SkillCmd::Install(args)) => match target(&args, json)? {
            Some(dir) => install(dir, args.force),
            None => Ok(Report::new(
                &serde_json::json!({ "dir": null, "skills": [] }),
                "nothing installed (later: `desk skill install`)\n".into(),
            )),
        },
        None => show(name.as_deref().unwrap_or(DEFAULT)),
    }
}

/// Where to install: from the flags, or asked on a terminal. `None`: the user chose not to.
fn target(args: &InstallArgs, json: bool) -> Result<Option<PathBuf>> {
    if let Some(dir) = &args.dir {
        return Ok(Some(dir.clone()));
    }
    if args.global {
        return global_dir().map(Some);
    }
    if args.project {
        return project_dir().map(Some);
    }
    let interactive = !json && std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if !interactive {
        return Err(DeskError::Usage(
            "choose where to install: --global (~/.claude/skills, every Claude Code session), \
             --project (./.claude/skills) or --dir <skills folder>"
                .into(),
        ));
    }
    let global = global_dir()?;
    let project = project_dir()?;
    let options = vec![
        format!(
            "Globally ({}): every Claude Code session, wherever you start it, sees the skill \
             descriptions (~{} tokens); the full text loads only when used",
            global.display(),
            description_tokens()
        ),
        format!("This project only ({})", project.display()),
        "Not now".to_string(),
    ];
    let choice = inquire::Select::new("Install desk's agent skills?", options.clone())
        .with_help_message("later: desk skill install --global | --project | --dir <path>")
        .prompt();
    match choice {
        Ok(c) if c == options[0] => Ok(Some(global)),
        Ok(c) if c == options[1] => Ok(Some(project)),
        Ok(_)
        | Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(None),
        Err(e) => Err(DeskError::Internal(format!("prompt failed: {e}"))),
    }
}

/// Roughly what the skill list costs in every session: names and descriptions (~4 chars/token).
fn description_tokens() -> usize {
    let chars: usize = SKILLS
        .iter()
        .map(|(name, text)| name.len() + description(text).len())
        .sum();
    chars.div_ceil(4).div_ceil(10) * 10
}

#[derive(Serialize)]
struct SkillInfo {
    name: &'static str,
    description: String,
}

fn list() -> Report {
    let skills: Vec<SkillInfo> = SKILLS
        .iter()
        .map(|(name, text)| SkillInfo {
            name,
            description: description(text),
        })
        .collect();
    let rows: Vec<Vec<String>> = skills
        .iter()
        .map(|s| vec![s.name.to_string(), first_sentence(&s.description)])
        .collect();
    let mut human = table(&["SKILL", "DESCRIPTION"], &rows);
    human.push_str("\nprint one with `desk skill <name>`; install all with `desk skill install`\n");
    Report::new(&serde_json::json!({ "skills": skills }), human)
}

fn show(name: &str) -> Result<Report> {
    let (name, text) = SKILLS.iter().find(|(n, _)| *n == name).ok_or_else(|| {
        DeskError::Usage(format!(
            "no skill `{name}` (known: {}; see `desk skill list`)",
            names()
        ))
    })?;
    Ok(Report::new(
        &serde_json::json!({ "name": name, "skill": text }),
        text.to_string(),
    ))
}

#[derive(Serialize)]
struct Installed {
    name: &'static str,
    path: PathBuf,
    /// `installed` (new), `updated` (an older version of this skill), `unchanged`, or
    /// `replaced` (something else was there; only with --force)
    action: &'static str,
}

/// Write `<dir>/<name>/SKILL.md` for every skill. An existing file is only replaced when it is
/// this skill (its frontmatter has the same `name`), unless `force`. Nothing is written when
/// any target is refused.
fn install(dir: PathBuf, force: bool) -> Result<Report> {
    let mut plan = Vec::new();
    for (name, text) in SKILLS {
        let path = dir.join(name).join("SKILL.md");
        let action = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "installed",
            Err(e) => {
                return Err(DeskError::Internal(format!(
                    "cannot read {}: {e}",
                    path.display()
                )));
            }
            Ok(old) if old == *text => "unchanged",
            Ok(old) if frontmatter_name(&old) == Some(name) => "updated",
            Ok(_) if force => "replaced",
            Ok(_) => {
                return Err(DeskError::Usage(format!(
                    "{} exists and isn't the desk skill `{name}`; nothing was installed \
                     (pass --force to overwrite it, or install elsewhere)",
                    path.display()
                )));
            }
        };
        plan.push(Installed { name, path, action });
    }
    for (item, (_, text)) in plan.iter().zip(SKILLS) {
        if item.action != "unchanged" {
            write(&item.path, text)?;
        }
    }

    let rows: Vec<Vec<String>> = plan
        .iter()
        .map(|i| {
            vec![
                i.name.to_string(),
                i.action.to_string(),
                i.path.display().to_string(),
            ]
        })
        .collect();
    let mut human = table(&["SKILL", "ACTION", "PATH"], &rows);
    human.push_str("\nrun `desk skill install` again after updating desk\n");
    Ok(Report::new(
        &serde_json::json!({ "dir": dir, "skills": plan }),
        human,
    ))
}

fn write(path: &Path, text: &str) -> Result<()> {
    let fail =
        |e: std::io::Error| DeskError::Internal(format!("cannot write {}: {e}", path.display()));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(fail)?;
    }
    std::fs::write(path, text).map_err(fail)
}

/// Whether `dir` holds any of desk's skills (installed by some version of desk).
pub(super) fn installed_in(dir: &Path) -> bool {
    SKILLS.iter().any(|(name, _)| {
        std::fs::read_to_string(dir.join(name).join("SKILL.md"))
            .is_ok_and(|text| frontmatter_name(&text) == Some(name))
    })
}

/// Claude Code's personal skills folder, `~/.claude/skills`. Other agents: `--dir`.
pub(super) fn global_dir() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().join(".claude").join("skills"))
        .ok_or_else(|| {
            DeskError::Usage("no home directory found; pass --dir <skills folder>".into())
        })
}

/// Claude Code's project skills folder: `.claude/skills` in the current directory.
fn project_dir() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join(".claude").join("skills"))
}

fn names() -> String {
    SKILLS
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A frontmatter field (`key: value` between the leading `---` lines).
fn frontmatter<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let body = text.strip_prefix("---\n")?;
    let end = body.find("\n---")?;
    body[..end].lines().find_map(|l| {
        l.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(':'))
            .map(str::trim)
    })
}

fn frontmatter_name(text: &str) -> Option<&str> {
    frontmatter(text, "name")
}

fn description(text: &str) -> String {
    frontmatter(text, "description")
        .unwrap_or_default()
        .to_string()
}

fn first_sentence(s: &str) -> String {
    match s.find(". ") {
        Some(i) => s[..=i].to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_skill_names_itself() {
        for (name, text) in SKILLS {
            assert_eq!(frontmatter_name(text), Some(*name));
            assert!(!description(text).is_empty(), "{name} has no description");
        }
    }
}
