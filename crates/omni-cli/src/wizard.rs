//! Interactive prompts for `omni-sync enroll` and `omni-sync add`.
//!
//! Every function here is called only when stdin is a terminal; the
//! non-interactive paths in `main.rs` never prompt, so scripts and CI keep
//! working with flags. The filesystem walk behind path completion is small and
//! synchronous, matching the rest of the CLI.

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use inquire::autocompletion::{Autocomplete, Replacement};
use inquire::{Confirm, CustomUserError, Select, Text};
use omni_netproto::FolderDto;

/// Ask which server folder to watch. The list comes from the server.
pub fn select_folder(folders: &[FolderDto]) -> Result<String> {
    if folders.is_empty() {
        bail!("the server has no folders yet — create one on the dashboard first");
    }
    let items: Vec<String> = folders
        .iter()
        .map(|folder| format!("{}  ({})", folder.name, folder.id))
        .collect();
    let choice = Select::new("Which folder do you want to watch?", items)
        .with_help_message("↑/↓ to move, enter to pick")
        .prompt()?;
    // The id is the parenthesised tail.
    folders
        .iter()
        .find(|folder| choice.ends_with(&format!("({})", folder.id)))
        .map(|folder| folder.id.clone())
        .ok_or_else(|| anyhow!("could not match the selection to a folder"))
}

/// Prompt for a line of text, with an optional default shown and used on an
/// empty answer.
pub fn prompt(message: &str, default: Option<&str>) -> Result<String> {
    let mut prompt = Text::new(message);
    if let Some(default) = default {
        prompt = prompt.with_default(default);
    }
    Ok(prompt.prompt()?.trim().to_string())
}

/// Prompt for a local directory, with filesystem path completion.
pub fn prompt_path(message: &str) -> Result<PathBuf> {
    let value = Text::new(message)
        .with_autocomplete(PathCompleter)
        .with_help_message("tab completes directories; ~ is your home")
        .prompt()?;
    Ok(expand_tilde(&value))
}

/// A yes/no answer.
pub fn confirm(message: &str, default: bool) -> Result<bool> {
    Ok(Confirm::new(message).with_default(default).prompt()?)
}

/// Expand a leading `~` (only `~` and `~/…`) to the user's home directory.
pub fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return home_dir();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(input)
}

/// The user's home directory, from the usual environment variables.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Completes a partially typed path against the filesystem, directories only.
#[derive(Clone)]
struct PathCompleter;

impl Autocomplete for PathCompleter {
    fn get_suggestions(&mut self, input: &str) -> Result<Vec<String>, CustomUserError> {
        Ok(suggest_dirs(input))
    }

    fn get_completion(
        &mut self,
        input: &str,
        highlighted: Option<String>,
    ) -> Result<Replacement, CustomUserError> {
        if let Some(suggestion) = highlighted {
            return Ok(Some(suggestion));
        }
        let suggestions = suggest_dirs(input);
        if suggestions.len() == 1 {
            return Ok(Some(suggestions[0].clone()));
        }
        match longest_common_prefix(&suggestions) {
            Some(prefix) if prefix.len() > input.len() => Ok(Some(prefix)),
            _ => Ok(None),
        }
    }
}

/// Directory suggestions for the partially typed `input`, each with a trailing
/// slash so the next tab continues from inside it.
fn suggest_dirs(input: &str) -> Vec<String> {
    let (display_dir, read_dir, prefix) = split_input(input);
    let entries = match std::fs::read_dir(&read_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) {
            continue;
        }
        // Hidden directories only appear once the user types the dot.
        if name.starts_with('.') && !prefix.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if is_dir {
            out.push(format!("{display_dir}{name}/"));
        }
    }
    out.sort();
    out
}

/// Split `input` into the directory to *show* (as the user typed it), the real
/// directory to read (`~` expanded), and the partial final component.
fn split_input(input: &str) -> (String, PathBuf, String) {
    if input.is_empty() {
        return (String::new(), PathBuf::from("."), String::new());
    }
    if input.ends_with('/') {
        return (input.to_string(), expand_tilde(input), String::new());
    }
    match input.rfind('/') {
        Some(index) => {
            let display = input[..=index].to_string();
            let prefix = input[index + 1..].to_string();
            (display.clone(), expand_tilde(&display), prefix)
        }
        None => (String::new(), PathBuf::from("."), input.to_string()),
    }
}

fn longest_common_prefix(values: &[String]) -> Option<String> {
    let mut iter = values.iter();
    let mut prefix = iter.next()?.clone();
    for value in iter {
        while !value.starts_with(&prefix) {
            prefix.pop();
            if prefix.is_empty() {
                return None;
            }
        }
    }
    Some(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expands_only_for_the_home_forms() {
        let home = home_dir();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/Docs"), home.join("Docs"));
        // `~user` and a mid-string tilde are left alone.
        assert_eq!(expand_tilde("~root/Docs"), PathBuf::from("~root/Docs"));
        assert_eq!(expand_tilde("/tmp/~x"), PathBuf::from("/tmp/~x"));
    }

    #[test]
    fn input_splits_into_dir_and_prefix() {
        let (display, read, prefix) = split_input("/home/me/Pro");
        assert_eq!(display, "/home/me/");
        assert_eq!(read, PathBuf::from("/home/me/"));
        assert_eq!(prefix, "Pro");

        let (display, _, prefix) = split_input("/home/me/");
        assert_eq!(display, "/home/me/");
        assert_eq!(prefix, "");

        let (display, read, prefix) = split_input("src");
        assert_eq!(display, "");
        assert_eq!(read, PathBuf::from("."));
        assert_eq!(prefix, "src");
    }

    #[test]
    fn common_prefix_of_siblings_is_their_shared_stem() {
        let values = vec!["/a/Projects/".into(), "/a/Proj/".into()];
        assert_eq!(longest_common_prefix(&values).as_deref(), Some("/a/Proj"));
        assert_eq!(longest_common_prefix(&[]), None);
    }

    #[test]
    fn suggestions_are_directories_only_and_matched_by_prefix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("alpha")).unwrap();
        std::fs::create_dir(dir.path().join("alpine")).unwrap();
        std::fs::create_dir(dir.path().join("beta")).unwrap();
        std::fs::write(dir.path().join("afile"), b"x").unwrap();

        let input = format!("{}/al", dir.path().display());
        let suggestions = suggest_dirs(&input);
        assert_eq!(suggestions.len(), 2);
        assert!(suggestions.iter().all(|s| s.ends_with('/')));
        assert!(suggestions.iter().any(|s| s.ends_with("alpha/")));
        assert!(suggestions.iter().any(|s| s.ends_with("alpine/")));
    }
}
