//! Workspace paths mentioned at a word boundary in the composer.
use super::super::{carbon::Choice, input::Editor, workspace::FileListing};
use std::path::{Component, Path};

pub(crate) struct Mention<'a> {
    pub begin: usize,
    pub end: usize,
    pub query: &'a str,
    pub directory: String,
    pub prefix: String,
}

// A backslash quotes the next character in a filename, including whitespace.
fn token_end(text: &str) -> usize {
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch.is_whitespace() {
            return index;
        }
    }
    text.len()
}

fn decode(path: &str) -> String {
    let mut decoded = String::new();
    let mut chars = path.chars();
    while let Some(ch) = chars.next() {
        decoded.push(if ch == '\\' {
            chars.next().unwrap_or(ch)
        } else {
            ch
        });
    }
    decoded
}

fn tokens(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut cursor = 0;
    std::iter::from_fn(move || {
        while cursor < text.len() {
            let start = cursor + text[cursor..].find('@')?;
            cursor = start + 1;
            if start != 0 && !text[..start].ends_with(char::is_whitespace) {
                continue;
            }
            cursor += token_end(&text[cursor..]);
            return Some((start, &text[start..cursor]));
        }
        None
    })
}

fn workspace_path(path: &str) -> bool {
    !path.chars().any(char::is_control)
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
}

impl<'a> Mention<'a> {
    #[cfg(test)]
    pub fn at(editor: &'a Editor) -> Option<Self> {
        Self::at_with_skills(editor, &[])
    }

    pub(in crate::frontend) fn at_with_skills(
        editor: &'a Editor,
        skills: &[super::SkillCommand],
    ) -> Option<Self> {
        if editor.pasting()
            || editor.overflow
            || !editor.bytes.get(..editor.cursor)?.contains(&b'@')
        {
            return None;
        }
        let text = editor.text().ok()?;
        if !super::accepts_file_mentions(text, skills) {
            return None;
        }
        let (begin, token) = tokens(text).find(|(begin, token)| {
            *begin < editor.cursor && editor.cursor <= begin + token.len()
        })?;
        let query = text.get(begin..editor.cursor)?;
        let path = decode(&query[1..]);
        if !workspace_path(&path) {
            return None;
        }
        let (directory, prefix) = path.rsplit_once('/').unwrap_or(("", &path));
        Some(Self {
            begin,
            end: begin + token.len(),
            query,
            directory: directory.to_owned(),
            prefix: prefix.to_owned(),
        })
    }

    pub fn choices(&self, listing: &FileListing) -> Vec<Choice> {
        listing
            .entries
            .iter()
            .filter_map(|entry| {
                let name = entry.path.file_name()?.to_str()?;
                if !name.starts_with(&self.prefix) || name.chars().any(char::is_control) {
                    return None;
                }
                Some(Choice {
                    label: if entry.directory {
                        format!("{name}/")
                    } else {
                        name.to_owned()
                    },
                    detail: String::new(),
                })
            })
            .collect()
    }
}

pub(super) fn insert(query: &str, label: &str) -> String {
    let directory = query
        .rsplit_once('/')
        .map_or("@", |(dir, _)| &query[..dir.len() + 1]);
    let mut path = directory.to_owned();
    for ch in label.chars() {
        if ch == '\\' || ch.is_whitespace() {
            path.push('\\');
        }
        path.push(ch);
    }
    path
}

pub(crate) fn image_paths(text: &str, max_images: usize) -> Vec<String> {
    let mut files = Vec::new();
    for (_, token) in tokens(text) {
        let path = decode(&token[1..]);
        if workspace_path(&path)
            && Path::new(&path)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| {
                    ["png", "jpg", "jpeg"]
                        .iter()
                        .any(|known| e.eq_ignore_ascii_case(known))
                })
            && !files.contains(&path)
        {
            files.push(path);
            if files.len() > max_images {
                break;
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_references_handle_spaces_duplicates_and_literal_at_signs() {
        let text = r"Ignore user@example.png and \@literal.png, compare @photos/my\ photo.PNG @photos/my\ photo.PNG @../outside.jpg @a.jpeg";
        assert_eq!(image_paths(text, 4), ["photos/my photo.PNG", "a.jpeg"]);
        assert_eq!(
            image_paths("@1.jpg @2.jpg @3.jpg @4.jpg", 1),
            ["1.jpg", "2.jpg"]
        );
    }

    #[test]
    fn completion_only_changes_the_active_token() {
        let mut editor = Editor::default();
        let draft = "См. @src/старое в отчёте";
        assert!(editor.replace(0, 0, draft.as_bytes()));
        editor.cursor = "См. @src/ст".len();
        let token = Mention::at(&editor).unwrap();
        assert_eq!(token.directory, "src");
        assert_eq!(token.prefix, "ст");
        let value = insert(token.query, "новый файл.txt");
        assert!(editor.replace(token.begin, token.end, value.as_bytes()));
        assert_eq!(editor.text().unwrap(), r"См. @src/новый\ файл.txt в отчёте");
    }
}
