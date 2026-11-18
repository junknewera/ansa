//! Bounded, read-only line comparison for acknowledged file edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Context,
    Add,
    Remove,
    Hunk,
    Notice,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: Kind,
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub text: String,
}
impl Row {
    pub fn horizontal_limit(&self, width: usize) -> usize {
        if matches!(self.kind, Kind::Hunk | Kind::Notice) {
            return 0;
        }
        let gutter = super::cells::width(&self.gutter());
        let code = super::workspace::display_text(&self.code());
        super::cells::width(&code).saturating_sub(width.saturating_sub(gutter).max(1))
    }

    pub fn gutter(&self) -> String {
        format!(
            "{:>4} {:>4} {} ",
            self.old.map(|n| n.to_string()).unwrap_or_default(),
            self.new.map(|n| n.to_string()).unwrap_or_default(),
            match self.kind {
                Kind::Add => "+",
                Kind::Remove => "-",
                _ => " ",
            }
        )
    }
    pub fn display(&self) -> String {
        if matches!(self.kind, Kind::Hunk | Kind::Notice) {
            return self.text.clone();
        }
        format!("{}{}", self.gutter(), self.code())
    }
    pub fn code(&self) -> String {
        self.text.replace('\r', "^M")
    }
    pub fn role(&self) -> &'static str {
        match self.kind {
            Kind::Add => "tool_diff_add",
            Kind::Remove => "tool_diff_remove",
            Kind::Hunk => "tool_diff_hunk",
            Kind::Notice => "tool_diff_notice",
            Kind::Context => "tool_diff_context",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub rows: Vec<Row>,
    pub added: usize,
    pub removed: usize,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditView {
    pub path: String,
    pub diff: Diff,
}

pub(super) fn patch_rows(text: &str) -> Vec<Row> {
    let mut old = 0;
    let mut new = 0;
    text.lines()
        .map(|line| {
            if let Some(ranges) = line
                .strip_prefix("@@ -")
                .and_then(|rest| rest.split_once(" +"))
            {
                old = ranges
                    .0
                    .split(',')
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                new = ranges
                    .1
                    .split([',', ' '])
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                return Row {
                    kind: Kind::Hunk,
                    old: None,
                    new: None,
                    text: line.into(),
                };
            }
            if line.starts_with("diff --git ")
                || line.starts_with("--- ")
                || line.starts_with("+++ ")
                || line.starts_with("index ")
            {
                return Row {
                    kind: Kind::Notice,
                    old: None,
                    new: None,
                    text: line.into(),
                };
            }
            let (kind, before, after) = if line.starts_with('-') && old > 0 {
                let number = old;
                old += 1;
                (Kind::Remove, Some(number), None)
            } else if line.starts_with('+') && new > 0 {
                let number = new;
                new += 1;
                (Kind::Add, None, Some(number))
            } else if line.starts_with(' ') && old > 0 && new > 0 {
                let numbers = (old, new);
                old += 1;
                new += 1;
                (Kind::Context, Some(numbers.0), Some(numbers.1))
            } else {
                return Row {
                    kind: Kind::Notice,
                    old: None,
                    new: None,
                    text: line.into(),
                };
            };
            Row {
                kind,
                old: before,
                new: after,
                text: line[1..].into(),
            }
        })
        .collect()
}

fn frontier(layer: &[isize], distance: usize, diagonal: isize) -> isize {
    let at = diagonal + distance as isize;
    if at < 0 {
        return -1;
    }
    layer.get(at as usize).copied().unwrap_or(-1)
}
fn compare<'a>(before: &[&'a str], after: &[&'a str]) -> Option<Vec<(Kind, &'a str)>> {
    let n = before.len() as isize;
    let m = after.len() as isize;
    let maximum = (n + m) as usize;
    let origin = maximum + 1;
    let mut v = vec![0isize; maximum * 2 + 3];
    let mut trace = Vec::new();
    let mut work = 0usize;
    for d in 0..=maximum {
        // Trace and CPU are both bounded even for unrelated newline-heavy files.
        if d.saturating_mul(d) > 300_000 {
            return None;
        }
        let distance = d as isize;
        let mut complete = false;
        for k in (-distance..=distance).step_by(2) {
            work += 1;
            if work > 1_000_000 {
                return None;
            }
            let index = (origin as isize + k) as usize;
            let mut x = if k == -distance || (k != distance && v[index - 1] < v[index + 1]) {
                v[index + 1]
            } else {
                v[index - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && x >= 0 && y >= 0 && before[x as usize] == after[y as usize] {
                work += 1;
                if work > 1_000_000 {
                    return None;
                }
                x += 1;
                y += 1;
            }
            v[index] = x;
            if x >= n && y >= m {
                complete = true;
                break;
            }
        }
        trace.push(v[origin - d..=origin + d].to_vec());
        if complete {
            let (mut x, mut y) = (n, m);
            let mut result = Vec::with_capacity(maximum);
            for depth in (1..=d).rev() {
                let k = x - y;
                let previous = &trace[depth - 1];
                let distance = depth as isize;
                let previous_k = if k == -distance
                    || (k != distance
                        && frontier(previous, depth - 1, k - 1)
                            < frontier(previous, depth - 1, k + 1))
                {
                    k + 1
                } else {
                    k - 1
                };
                let previous_x = frontier(previous, depth - 1, previous_k);
                let previous_y = previous_x - previous_k;
                while x > previous_x && y > previous_y {
                    x -= 1;
                    y -= 1;
                    result.push((Kind::Context, before[x as usize]));
                }
                if x == previous_x {
                    y -= 1;
                    result.push((Kind::Add, after[y as usize]));
                } else {
                    x -= 1;
                    result.push((Kind::Remove, before[x as usize]));
                }
            }
            while x > 0 && y > 0 {
                x -= 1;
                y -= 1;
                result.push((Kind::Context, before[x as usize]));
            }
            result.reverse();
            return Some(result);
        }
    }
    None
}
pub fn between(before: &str, after: &str) -> Option<Diff> {
    if before.len() > 64 * 1024 || after.len() > 64 * 1024 {
        return None;
    }
    let old: Vec<_> = before.split_inclusive('\n').collect();
    let new: Vec<_> = after.split_inclusive('\n').collect();
    let edits = compare(&old, &new)?;
    let mut numbered = Vec::with_capacity(edits.len());
    let (mut old_line, mut new_line) = (1, 1);
    let (mut added, mut removed) = (0, 0);
    for (kind, text) in edits {
        let (old, new) = match kind {
            Kind::Add => {
                added += 1;
                let n = new_line;
                new_line += 1;
                (None, Some(n))
            }
            Kind::Remove => {
                removed += 1;
                let n = old_line;
                old_line += 1;
                (Some(n), None)
            }
            _ => {
                let at = (Some(old_line), Some(new_line));
                old_line += 1;
                new_line += 1;
                at
            }
        };
        numbered.push(Row {
            kind,
            old,
            new,
            text: text.strip_suffix('\n').unwrap_or(text).into(),
        });
        if !text.ends_with('\n') && kind != Kind::Context {
            numbered.push(Row {
                kind: Kind::Notice,
                old: None,
                new: None,
                text: "\\ Нет переноса строки в конце файла".into(),
            });
        }
    }
    let mut intervals: Vec<(usize, usize)> = Vec::new();
    for (index, row) in numbered
        .iter()
        .enumerate()
        .filter(|(_, row)| row.kind != Kind::Context)
    {
        let _ = row;
        let start = index.saturating_sub(3);
        let end = (index + 4).min(numbered.len());
        if let Some(last) = intervals.last_mut().filter(|last| start <= last.1) {
            last.1 = end;
        } else {
            intervals.push((start, end));
        }
    }
    let mut rows = Vec::new();
    for (start, end) in intervals {
        let part = &numbered[start..end];
        let old_start = part.iter().find_map(|row| row.old).unwrap_or_else(|| {
            part.iter()
                .find_map(|row| row.new)
                .unwrap_or(1)
                .saturating_sub(1)
        });
        let new_start = part.iter().find_map(|row| row.new).unwrap_or_else(|| {
            part.iter()
                .find_map(|row| row.old)
                .unwrap_or(1)
                .saturating_sub(1)
        });
        rows.push(Row {
            kind: Kind::Hunk,
            old: None,
            new: None,
            text: format!(
                "@@ -{},{} +{},{} @@",
                old_start,
                part.iter().filter(|r| r.old.is_some()).count(),
                new_start,
                part.iter().filter(|r| r.new.is_some()).count()
            ),
        });
        rows.extend_from_slice(part);
    }
    Some(Diff {
        rows,
        added,
        removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edit_script_reconstructs_both_inputs_and_preserves_separate_hunks() {
        let old = "first\nA\n1\n2\n3\n4\n5\n6\n7\n8\n9\nB\nlast\n";
        let new = old.replace("A\n", "X\n").replace("B\n", "Y\n");
        let before: Vec<_> = old.split_inclusive('\n').collect();
        let after: Vec<_> = new.split_inclusive('\n').collect();
        let script = compare(&before, &after).unwrap();
        assert_eq!(
            script
                .iter()
                .filter(|(k, _)| *k != Kind::Add)
                .map(|(_, t)| *t)
                .collect::<String>(),
            old
        );
        assert_eq!(
            script
                .iter()
                .filter(|(k, _)| *k != Kind::Remove)
                .map(|(_, t)| *t)
                .collect::<String>(),
            new
        );
        let diff = between(old, &new).unwrap();
        assert_eq!((diff.added, diff.removed), (2, 2));
        assert_eq!(diff.rows.iter().filter(|r| r.kind == Kind::Hunk).count(), 2);
        for (a, b) in [
            ("", "new\n"),
            ("old\n", ""),
            ("a\n", "a"),
            ("Ж界😀\n", "Ж界🌍\n"),
            ("a\na\nb\n", "a\nb\nb\n"),
        ] {
            let aa: Vec<_> = a.split_inclusive('\n').collect();
            let bb: Vec<_> = b.split_inclusive('\n').collect();
            let edit = compare(&aa, &bb).unwrap();
            assert_eq!(
                edit.iter()
                    .filter(|(k, _)| *k != Kind::Add)
                    .map(|(_, t)| *t)
                    .collect::<String>(),
                a
            );
            assert_eq!(
                edit.iter()
                    .filter(|(k, _)| *k != Kind::Remove)
                    .map(|(_, t)| *t)
                    .collect::<String>(),
                b
            );
        }
        assert!(
            between("a\n", "a")
                .unwrap()
                .rows
                .iter()
                .any(|r| r.kind == Kind::Notice)
        );
    }
    #[test]
    fn unrelated_large_edits_hit_the_comparison_budget() {
        assert!(between(&"a\n".repeat(2000), &"b\n".repeat(2000)).is_none());
        assert!(between("same", "same").unwrap().rows.is_empty());
        for (before, after) in [("x\r", "x"), ("x\r\n", "x\n")] {
            let diff = between(before, after).unwrap();
            let removed = diff
                .rows
                .iter()
                .find(|row| row.kind == Kind::Remove)
                .unwrap();
            let added = diff.rows.iter().find(|row| row.kind == Kind::Add).unwrap();
            assert!(removed.text.ends_with('\r') && removed.display().contains("^M"));
            assert!(!added.display().contains("^M"));
        }
    }
    #[test]
    fn repeated_lines_match_an_independent_lcs_oracle() {
        let words: Vec<String> = (0..5)
            .flat_map(|len| {
                (0..(1usize << len)).map(move |bits| {
                    (0..len)
                        .map(|at| if bits & (1 << at) == 0 { "a\n" } else { "b\n" })
                        .collect()
                })
            })
            .collect();
        for a in &words {
            for b in &words {
                let aa: Vec<_> = a.split_inclusive('\n').collect();
                let bb: Vec<_> = b.split_inclusive('\n').collect();
                let script = compare(&aa, &bb).unwrap();
                assert_eq!(
                    script
                        .iter()
                        .filter(|(k, _)| *k != Kind::Add)
                        .map(|(_, t)| *t)
                        .collect::<String>(),
                    *a
                );
                assert_eq!(
                    script
                        .iter()
                        .filter(|(k, _)| *k != Kind::Remove)
                        .map(|(_, t)| *t)
                        .collect::<String>(),
                    *b
                );
                let mut lcs = vec![vec![0; bb.len() + 1]; aa.len() + 1];
                for x in 0..aa.len() {
                    for y in 0..bb.len() {
                        lcs[x + 1][y + 1] = if aa[x] == bb[y] {
                            lcs[x][y] + 1
                        } else {
                            lcs[x][y + 1].max(lcs[x + 1][y])
                        };
                    }
                }
                assert_eq!(
                    script.iter().filter(|(k, _)| *k != Kind::Context).count(),
                    aa.len() + bb.len() - 2 * lcs[aa.len()][bb.len()]
                );
            }
        }
    }
}
