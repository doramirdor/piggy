//! Unified line diff for the advice surface.
//!
//! The app renders a diff; it does not compute one. Shipping structured rows
//! rather than a patch string keeps a diff algorithm out of the app bundle, and
//! keeps the line numbers the UI prints identical to the ones the CLI would
//! write for the same edit.
//!
//! Deliberately not a dependency: the only diffs Piggy shows are one CLAUDE.md
//! against its deterministic cleanup, which is a deletion-only edit today and a
//! whole-file rewrite once the advisor drafts one. A line LCS covers both.
//!
//! It lives in the core rather than in the app because two surfaces need the
//! same answer: the app's `advice_diff` command, and `piggy advise --json
//! --diff`, which is what generates the dev fixture the app is designed
//! against. Two implementations would be two sets of line numbers.

/// Context lines kept on each side of a change.
pub const CONTEXT: usize = 3;

/// Most diff lines returned. Past this the surface says so rather than
/// rendering a file the reader cannot scroll.
pub const MAX_LINES: usize = 400;

/// Largest LCS table this module will build, in cells. A CLAUDE.md is hundreds
/// of lines, so the real cost is a rounding error; the cap exists because the
/// table is quadratic and the input is a file a user can make as large as they
/// like. Past it, [`unified`] emits the honest coarse answer (the whole
/// differing middle out, the whole new middle in) rather than allocating a
/// gigabyte to make it prettier.
const MAX_LCS_CELLS: usize = 4_000_000;

/// What one line of the diff is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Unchanged, shown for context.
    Ctx,
    /// Present only after the edit.
    Add,
    /// Present only before the edit.
    Del,
}

impl Op {
    /// Stable machine name for a wire payload.
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Ctx => "ctx",
            Op::Add => "add",
            Op::Del => "del",
        }
    }
}

/// One rendered line: what happened to it, its text, and its number on each
/// side. A number is `None` on the side the line does not exist on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub op: Op,
    pub text: String,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
}

/// A run of changed lines with its context, headed the standard way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// `@@ -12,7 +12,3 @@`.
    pub header: String,
    pub lines: Vec<Line>,
}

/// The whole comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub hunks: Vec<Hunk>,
    /// Lines added across the *whole* edit, not just the shown hunks: the
    /// disclosure summary quotes this, and a truncated view must not understate
    /// what applying would do.
    pub added: usize,
    /// Lines removed across the whole edit.
    pub removed: usize,
    /// True when [`MAX_LINES`] cut the rendered hunks short.
    pub truncated: bool,
}

impl Diff {
    /// Whether the two texts differ at all.
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }
}

/// Split into lines the way a file reads, not the way a parser would like it.
///
/// `\r` stays on the line: a CRLF file has to render as it is stored, and apply
/// writes the engine's bytes regardless of what this shows. A trailing newline
/// terminates the last line rather than starting an empty one, so a file and the
/// same file with its final newline intact do not differ by a phantom row.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        out.pop();
    }
    out
}

/// Unified diff of `before` against `after`.
pub fn unified(before: &str, after: &str) -> Diff {
    let old = split_lines(before);
    let new = split_lines(after);
    let script = script(&old, &new);

    let added = script.iter().filter(|l| l.op == Op::Add).count();
    let removed = script.iter().filter(|l| l.op == Op::Del).count();
    if added == 0 && removed == 0 {
        return Diff::default();
    }

    let (hunks, truncated) = hunks(script);
    Diff {
        hunks,
        added,
        removed,
        truncated,
    }
}

/// Every line of both texts in order, tagged. The full script, before it is cut
/// into hunks.
fn script(old: &[&str], new: &[&str]) -> Vec<Line> {
    // Common head and tail first. For the deletion-only edit that is today's
    // whole use case this reduces the quadratic middle to the handful of lines
    // that actually moved.
    let head = old
        .iter()
        .zip(new.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();

    let mut out = Vec::with_capacity(old.len() + new.len());
    let mut old_no = 0u32;
    let mut new_no = 0u32;
    let push = |out: &mut Vec<Line>, op: Op, text: &str, old_no: &mut u32, new_no: &mut u32| {
        let (o, n) = match op {
            Op::Ctx => {
                *old_no += 1;
                *new_no += 1;
                (Some(*old_no), Some(*new_no))
            }
            Op::Del => {
                *old_no += 1;
                (Some(*old_no), None)
            }
            Op::Add => {
                *new_no += 1;
                (None, Some(*new_no))
            }
        };
        out.push(Line {
            op,
            text: text.to_string(),
            old_no: o,
            new_no: n,
        });
    };

    for line in &old[..head] {
        push(&mut out, Op::Ctx, line, &mut old_no, &mut new_no);
    }

    let old_mid = &old[head..old.len() - tail];
    let new_mid = &new[head..new.len() - tail];
    if old_mid.len().saturating_mul(new_mid.len()) > MAX_LCS_CELLS {
        // Coarse but true: everything that was there is out, everything that is
        // there now is in. Deletions first, so the pair reads as a replacement.
        for line in old_mid {
            push(&mut out, Op::Del, line, &mut old_no, &mut new_no);
        }
        for line in new_mid {
            push(&mut out, Op::Add, line, &mut old_no, &mut new_no);
        }
    } else {
        for (op, text) in lcs_script(old_mid, new_mid) {
            push(&mut out, op, text, &mut old_no, &mut new_no);
        }
    }

    for line in &old[old.len() - tail..] {
        push(&mut out, Op::Ctx, line, &mut old_no, &mut new_no);
    }
    out
}

/// Longest-common-subsequence walk over the differing middle.
fn lcs_script<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<(Op, &'a str)> {
    let (n, m) = (old.len(), new.len());
    if n == 0 {
        return new.iter().map(|l| (Op::Add, *l)).collect();
    }
    if m == 0 {
        return old.iter().map(|l| (Op::Del, *l)).collect();
    }

    // table[i][j] = LCS length of old[i..] and new[j..].
    let mut table = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[at(i, j)] = if old[i] == new[j] {
                table[at(i + 1, j + 1)] + 1
            } else {
                table[at(i + 1, j)].max(table[at(i, j + 1)])
            };
        }
    }

    let mut out = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old[i] == new[j] {
            out.push((Op::Ctx, old[i]));
            i += 1;
            j += 1;
        } else if table[at(i + 1, j)] >= table[at(i, j + 1)] {
            // Deletions before additions at a tie, so a replaced block reads
            // "this went out, this came in" rather than interleaved.
            out.push((Op::Del, old[i]));
            i += 1;
        } else {
            out.push((Op::Add, new[j]));
            j += 1;
        }
    }
    while i < n {
        out.push((Op::Del, old[i]));
        i += 1;
    }
    while j < m {
        out.push((Op::Add, new[j]));
        j += 1;
    }
    out
}

/// Cut the script into hunks: every changed line with [`CONTEXT`] lines of
/// company on each side, runs that overlap merged.
fn hunks(script: Vec<Line>) -> (Vec<Hunk>, bool) {
    let changed: Vec<usize> = script
        .iter()
        .enumerate()
        .filter(|(_, l)| l.op != Op::Ctx)
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return (Vec::new(), false);
    }

    // Merge the context windows into ranges.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &i in &changed {
        let lo = i.saturating_sub(CONTEXT);
        let hi = (i + CONTEXT + 1).min(script.len());
        match ranges.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => ranges.push((lo, hi)),
        }
    }

    let mut out = Vec::new();
    let mut emitted = 0usize;
    let mut truncated = false;
    for (lo, hi) in ranges {
        if emitted >= MAX_LINES {
            truncated = true;
            break;
        }
        let room = MAX_LINES - emitted;
        let end = hi.min(lo + room);
        if end < hi {
            truncated = true;
        }
        let lines: Vec<Line> = script[lo..end].to_vec();
        if lines.is_empty() {
            truncated = true;
            break;
        }
        emitted += lines.len();
        out.push(Hunk {
            header: header(&lines),
            lines,
        });
    }
    (out, truncated)
}

/// `@@ -old_start,old_count +new_start,new_count @@`.
///
/// A side with no lines in the hunk gets start 0, which is what unified diff
/// does for a pure insertion or deletion at a file boundary.
fn header(lines: &[Line]) -> String {
    let old_start = lines.iter().find_map(|l| l.old_no).unwrap_or(0);
    let new_start = lines.iter().find_map(|l| l.new_no).unwrap_or(0);
    let old_count = lines.iter().filter(|l| l.old_no.is_some()).count();
    let new_count = lines.iter().filter(|l| l.new_no.is_some()).count();
    format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[&str]) -> String {
        format!("{}\n", lines.join("\n"))
    }

    /// The shape every ClaudemdFix has: lines come out, nothing goes in. A diff
    /// that invents an added line for a pure deletion tells the reader Piggy is
    /// about to write prose it is not writing.
    #[test]
    fn a_deletion_only_edit_produces_no_added_lines() {
        let before = text(&["a", "b", "c", "d"]);
        let after = text(&["a", "c", "d"]);
        let d = unified(&before, &after);
        assert_eq!(d.added, 0);
        assert_eq!(d.removed, 1);
        assert!(d.hunks.iter().flat_map(|h| &h.lines).all(|l| l.op != Op::Add));
    }

    #[test]
    fn line_numbers_track_both_sides_across_a_hunk() {
        let before = text(&["a", "b", "c", "d", "e"]);
        let after = text(&["a", "c", "d", "e"]);
        let d = unified(&before, &after);
        let lines = &d.hunks[0].lines;
        // "a" is line 1 on both sides; "b" is old line 2 with no new number;
        // "c" is old 3 and new 2 - the sides have diverged by one from here on.
        assert_eq!((lines[0].old_no, lines[0].new_no), (Some(1), Some(1)));
        assert_eq!((lines[1].old_no, lines[1].new_no), (Some(2), None));
        assert_eq!((lines[2].old_no, lines[2].new_no), (Some(3), Some(2)));
    }

    #[test]
    fn context_is_capped_at_three_lines_each_side() {
        let before: Vec<&str> = vec![
            "1", "2", "3", "4", "5", "6", "7", "8", "gone", "9", "10", "11", "12", "13", "14",
        ];
        let after: Vec<&str> = before.iter().filter(|l| **l != "gone").copied().collect();
        let d = unified(&text(&before), &text(&after));
        assert_eq!(d.hunks.len(), 1);
        let lines = &d.hunks[0].lines;
        // Three context, the deletion, three context.
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[3].op, Op::Del);
        assert_eq!(lines[3].text, "gone");
    }

    #[test]
    fn two_changes_far_apart_produce_two_hunks() {
        let before = text(&[
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p",
        ]);
        let after = text(&["b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o"]);
        let d = unified(&before, &after);
        assert_eq!(d.hunks.len(), 2);
        assert_eq!(d.removed, 2);
    }

    #[test]
    fn an_unchanged_file_produces_no_hunks() {
        let same = text(&["a", "b", "c"]);
        let d = unified(&same, &same);
        assert!(d.hunks.is_empty());
        assert!(d.is_empty());
        assert!(!d.truncated);
    }

    /// A file bigger than the view is a real case (a 4,000-line CLAUDE.md is a
    /// thing people have). The sheet says it is showing part of the edit rather
    /// than pretending the rest is not there.
    #[test]
    fn a_diff_past_the_cap_reports_truncated() {
        let before: Vec<String> = (0..1200).map(|i| format!("line {i}")).collect();
        let after: Vec<String> = before.iter().filter(|l| !l.ends_with('7')).cloned().collect();
        let bt = format!("{}\n", before.join("\n"));
        let at = format!("{}\n", after.join("\n"));
        let d = unified(&bt, &at);
        assert!(d.truncated);
        let shown: usize = d.hunks.iter().map(|h| h.lines.len()).sum();
        assert!(shown <= MAX_LINES, "{shown} lines shown");
        // The counts describe the whole edit, not the shown part: 120 lines end
        // in 7 across 0..1200.
        assert_eq!(d.removed, 120);
    }

    /// A CRLF file must render as it is stored. Normalising the `\r` away here
    /// would show the reader lines that are not the lines on disk, and apply
    /// writes the engine's bytes either way.
    #[test]
    fn a_carriage_return_is_displayed_not_normalised_away() {
        let before = "a\r\nb\r\nc\r\n";
        let after = "a\r\nc\r\n";
        let d = unified(before, after);
        let del = d
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .find(|l| l.op == Op::Del)
            .expect("a deleted line");
        assert_eq!(del.text, "b\r");
        let ctx = &d.hunks[0].lines[0];
        assert_eq!(ctx.text, "a\r");
    }

    #[test]
    fn the_header_names_both_sides() {
        let before = text(&["a", "b", "c", "d"]);
        let after = text(&["a", "c", "d"]);
        let d = unified(&before, &after);
        assert_eq!(d.hunks[0].header, "@@ -1,4 +1,3 @@");
    }

    /// An empty file has no lines at all, not one empty line: otherwise creating
    /// content in an empty file reads as a modification of a line nobody wrote.
    #[test]
    fn an_empty_file_has_no_lines() {
        let d = unified("", "hello\n");
        assert_eq!(d.added, 1);
        assert_eq!(d.removed, 0);
    }

    /// The trailing newline is a terminator. Without this, every file whose last
    /// line is unchanged still showed a phantom empty row at the bottom.
    #[test]
    fn a_trailing_newline_does_not_become_an_empty_line() {
        let d = unified("a\nb\n", "a\n");
        assert_eq!(d.removed, 1);
        assert_eq!(d.added, 0);
    }
}
