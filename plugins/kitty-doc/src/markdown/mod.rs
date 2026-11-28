//! Document → markdown: the shared pieces the Word, OpenDocument and
//! PowerPoint converters build on — escaping, inline formatting runs, and
//! block rendering (headings, paragraphs, nested lists, quotes, code, GFM
//! tables, footnotes).
//!
//! The output is GFM plus `[^n]` footnotes. Emphasis is `**bold**`,
//! `_italic_` (`*italic*` when the run sits inside a word, where an
//! underscore would not open emphasis), `~~strike~~`, `` `code` ``. Text
//! that would otherwise read as markup — a line starting `# ` or `1. `, a
//! literal `*` — is backslash-escaped, so the document reads as written.
//!
//! Additive: nothing in the MCP tools uses this module.

pub mod xml;

/// Characters escaped wherever they appear in prose.
fn needs_escape(c: char, next: Option<char>) -> bool {
    match c {
        '\\' | '`' | '*' | '_' | '[' | ']' => true,
        // `~~` opens strikethrough; one tilde is plain.
        '~' => next == Some('~'),
        // `<tag`, `</`, `<!--` and `<http…>` read as HTML or autolinks.
        '<' => matches!(next, Some(n) if n.is_ascii_alphabetic() || n == '/' || n == '!'),
        _ => false,
    }
}

/// Bare web addresses are left untouched: an escape inside one would become
/// part of the address.
fn url_len(s: &str) -> usize {
    let lower = s.get(..12).unwrap_or(s).to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("www."))
    {
        return 0;
    }
    s.char_indices()
        .find(|(_, c)| c.is_whitespace() || matches!(c, '<' | '>' | '"'))
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Escape inline markup characters in plain text. Web addresses pass
/// through unchanged.
pub fn escape_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut i = 0;
    let bytes_len = text.len();
    let mut prev_alnum = false;
    while i < bytes_len {
        let rest = &text[i..];
        if !prev_alnum {
            let n = url_len(rest);
            if n > 0 {
                out.push_str(&rest[..n]);
                i += n;
                prev_alnum = false;
                continue;
            }
        }
        let mut chars = rest.chars();
        let c = chars.next().unwrap_or(' ');
        let next = chars.next();
        // `&name;` and `&#123;` would be read as entities.
        if c == '&' {
            let entity = rest[1..]
                .find(';')
                .map(|end| {
                    let body = &rest[1..1 + end];
                    !body.is_empty()
                        && body.len() <= 32
                        && body
                            .trim_start_matches('#')
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric())
                })
                .unwrap_or(false);
            if entity {
                out.push('\\');
            }
        } else if needs_escape(c, next) {
            out.push('\\');
        }
        out.push(c);
        prev_alnum = c.is_alphanumeric();
        i += c.len_utf8();
    }
    out
}

/// Escape what would turn the start of a line into block markup: headings,
/// quotes, list markers, setext underlines, and indentation that would make a
/// code block. `line` should already have had `escape_inline` applied.
pub fn escape_line_start(line: &str) -> String {
    let trimmed = line.trim_start();
    // Four spaces of indentation would start an indented code block.
    let line = if line.len() - trimmed.len() >= 4 {
        trimmed
    } else {
        line
    };
    let t = line.trim_start();
    let lead = &line[..line.len() - t.len()];
    let first = t.chars().next();
    let after = |n: usize| t[n..].chars().next();
    let space_or_end = |c: Option<char>| matches!(c, None | Some(' ') | Some('\t'));
    let escaped = match first {
        Some('#') => {
            let hashes = t.chars().take_while(|&c| c == '#').count();
            hashes <= 6 && space_or_end(after(hashes))
        }
        Some('>') => true,
        Some('-') | Some('+') | Some('*') => {
            space_or_end(after(1))
                // `---` / `===` alone are rules or setext underlines.
                || t.chars().all(|c| c == '-' || c == ' ')
        }
        Some('=') => t.chars().all(|c| c == '=' || c == ' '),
        Some('|') => false,
        Some(c) if c.is_ascii_digit() => {
            let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
            digits <= 9
                && matches!(after(digits), Some('.') | Some(')'))
                && space_or_end(t[digits + 1..].chars().next())
        }
        _ => false,
    };
    if !escaped {
        return line.to_string();
    }
    match first {
        Some(c) if c.is_ascii_digit() => {
            let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
            format!("{lead}{}\\{}", &t[..digits], &t[digits..])
        }
        _ => format!("{lead}\\{t}"),
    }
}

/// Escape a whole paragraph of plain text: inline markup everywhere, block
/// markup at the start of each line.
pub fn escape_text(text: &str) -> String {
    escape_inline(text)
        .split('\n')
        .map(escape_line_start)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Formatting carried by one run of text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Format {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
}

/// One piece of a paragraph.
#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    /// Plain text (escaped when rendered) with its formatting and link.
    Text {
        text: String,
        format: Format,
        link: Option<String>,
    },
    /// Markdown written as is (a footnote reference, an image placeholder).
    Raw(String),
}

impl Inline {
    pub fn text(text: impl Into<String>, format: Format, link: Option<String>) -> Self {
        Inline::Text {
            text: text.into(),
            format,
            link,
        }
    }
}

/// A placeholder for a picture, with its description when it has one.
pub fn image_placeholder(description: Option<&str>) -> Inline {
    let desc = description
        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|d| !d.is_empty());
    match desc {
        Some(d) => Inline::Raw(format!("_\\[Image: {}\\]_", escape_inline(&d))),
        None => Inline::Raw("_\\[Image\\]_".to_string()),
    }
}

fn wrap(text: &str, format: Format, before: Option<char>, after: Option<char>) -> String {
    if format.code {
        let ticks = if text.contains('`') { "`` " } else { "`" };
        let close = if text.contains('`') { " ``" } else { "`" };
        return format!("{ticks}{text}{close}");
    }
    let mut s = escape_inline(text);
    if format.strike {
        s = format!("~~{s}~~");
    }
    if format.italic {
        // An underscore inside a word does not open emphasis; an asterisk does.
        let in_word =
            before.is_some_and(char::is_alphanumeric) || after.is_some_and(char::is_alphanumeric);
        s = if in_word {
            format!("*{s}*")
        } else {
            format!("_{s}_")
        };
    }
    if format.bold {
        s = format!("**{s}**");
    }
    s
}

/// Render a paragraph's inline pieces. Adjacent runs with the same formatting
/// and link merge first, so `**a****b**` never appears; spaces at a run's
/// edges move outside its markers, where emphasis can open and close.
pub fn render_inline(pieces: &[Inline]) -> String {
    // Merge.
    let mut merged: Vec<Inline> = Vec::new();
    for p in pieces {
        match (merged.last_mut(), p) {
            (
                Some(Inline::Text {
                    text: a,
                    format: fa,
                    link: la,
                }),
                Inline::Text {
                    text: b,
                    format: fb,
                    link: lb,
                },
            ) if (fa == fb || b.trim().is_empty() && !fa.code) && la == lb => a.push_str(b),
            (Some(Inline::Raw(a)), Inline::Raw(b)) => a.push_str(b),
            _ => {
                if let Inline::Text { text, .. } = p {
                    if text.is_empty() {
                        continue;
                    }
                }
                merged.push(p.clone());
            }
        }
    }

    // Group consecutive pieces under the same link.
    let mut out = String::new();
    let mut i = 0;
    while i < merged.len() {
        let link = match &merged[i] {
            Inline::Text { link, .. } => link.clone(),
            Inline::Raw(_) => None,
        };
        let mut j = i + 1;
        if link.is_some() {
            while j < merged.len() {
                match &merged[j] {
                    Inline::Text { link: l, .. } if *l == link => j += 1,
                    _ => break,
                }
            }
        }
        let mut label = String::new();
        for piece in &merged[i..j] {
            match piece {
                Inline::Raw(r) => label.push_str(r),
                Inline::Text { text, format, .. } => {
                    if *format == Format::default() {
                        label.push_str(&escape_inline(text));
                        continue;
                    }
                    let core = text.trim();
                    if core.is_empty() {
                        label.push_str(&escape_inline(text));
                        continue;
                    }
                    let lead = &text[..text.len() - text.trim_start().len()];
                    let trail = &text[text.trim_end().len()..];
                    let before = if lead.is_empty() {
                        label.chars().last().or_else(|| out.chars().last())
                    } else {
                        Some(' ')
                    };
                    label.push_str(lead);
                    label.push_str(&wrap(core, *format, before, trail.chars().next()));
                    label.push_str(trail);
                }
            }
        }
        match link {
            Some(url) if !label.trim().is_empty() => {
                let lead = &label[..label.len() - label.trim_start().len()];
                let trail = &label[label.trim_end().len()..];
                out.push_str(lead);
                out.push_str(&format!(
                    "[{}]({})",
                    label.trim(),
                    url.replace(' ', "%20").replace(')', "%29")
                ));
                out.push_str(trail);
            }
            _ => out.push_str(&label),
        }
        i = j;
    }
    out
}

/// One block of a converted document.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading {
        level: u8,
        text: String,
    },
    Paragraph(String),
    /// `level` is 0 for a top-level item. `number` is `None` for a bullet.
    ListItem {
        level: usize,
        number: Option<u32>,
        text: String,
    },
    Quote(String),
    /// Literal text (rendered in a fence, never escaped).
    Code(String),
    /// Rows of already-rendered inline cell text; the first row is the header.
    Table(Vec<Vec<String>>),
    /// Markdown passed through as is.
    Raw(String),
}

/// A GFM pipe table. Ragged rows are padded; pipes and line breaks inside a
/// cell are made safe; an empty table renders as nothing.
pub fn render_table(rows: &[Vec<String>]) -> String {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return String::new();
    }
    let cell = |s: &str| -> String {
        let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
        // A pipe already escaped stays as it is; a bare one is escaped.
        let mut out = String::with_capacity(one_line.len());
        let mut prev = '\0';
        for c in one_line.chars() {
            if c == '|' && prev != '\\' {
                out.push('\\');
            }
            out.push(c);
            prev = c;
        }
        out
    };
    let line = |row: &[String]| -> String {
        let mut cells: Vec<String> = row.iter().map(|c| cell(c)).collect();
        cells.resize(width, String::new());
        format!("| {} |", cells.join(" | "))
    };
    let mut out = vec![line(&rows[0])];
    out.push(format!("|{}", " --- |".repeat(width)));
    for row in &rows[1..] {
        out.push(line(row));
    }
    out.join("\n")
}

/// Render blocks as markdown. Consecutive list items form one list (nested by
/// level, each level indented to the text column of the item above);
/// consecutive quote and code blocks merge.
pub fn render_blocks(blocks: &[Block]) -> String {
    let mut parts: Vec<String> = Vec::new();
    // Marker widths of the open list levels.
    let mut list_cols: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < blocks.len() {
        match &blocks[i] {
            Block::ListItem { .. } => {
                let mut lines: Vec<String> = Vec::new();
                list_cols.clear();
                while let Some(Block::ListItem {
                    level,
                    number,
                    text,
                }) = blocks.get(i)
                {
                    // A level can only deepen one step at a time.
                    let level = (*level).min(list_cols.len());
                    list_cols.truncate(level);
                    let indent: usize = list_cols.iter().sum();
                    let marker = match number {
                        Some(n) => format!("{n}."),
                        None => "-".to_string(),
                    };
                    list_cols.push(marker.len() + 1);
                    let pad = " ".repeat(indent);
                    let cont = " ".repeat(indent + marker.len() + 1);
                    let body = text.replace('\n', &format!("\n{cont}"));
                    lines.push(format!("{pad}{marker} {body}"));
                    i += 1;
                }
                parts.push(lines.join("\n"));
                continue;
            }
            Block::Quote(_) => {
                let mut lines: Vec<String> = Vec::new();
                while let Some(Block::Quote(text)) = blocks.get(i) {
                    if !lines.is_empty() {
                        lines.push(">".to_string());
                    }
                    for l in text.split('\n') {
                        lines.push(format!("> {l}"));
                    }
                    i += 1;
                }
                parts.push(lines.join("\n"));
                continue;
            }
            Block::Code(_) => {
                let mut body: Vec<&str> = Vec::new();
                while let Some(Block::Code(text)) = blocks.get(i) {
                    body.push(text);
                    i += 1;
                }
                let joined = body.join("\n");
                let fence = if joined.contains("```") {
                    "~~~~"
                } else {
                    "```"
                };
                parts.push(format!("{fence}\n{joined}\n{fence}"));
                continue;
            }
            Block::Heading { level, text } => {
                let level = (*level).clamp(1, 6) as usize;
                let text = text.replace('\n', " ");
                if !text.trim().is_empty() {
                    parts.push(format!("{} {}", "#".repeat(level), text.trim()));
                }
            }
            Block::Paragraph(text) => {
                if !text.trim().is_empty() {
                    parts.push(text.trim().to_string());
                }
            }
            Block::Table(rows) => {
                let t = render_table(rows);
                if !t.is_empty() {
                    parts.push(t);
                }
            }
            Block::Raw(text) => {
                if !text.trim().is_empty() {
                    parts.push(text.trim_end().to_string());
                }
            }
        }
        i += 1;
    }
    parts.join("\n\n")
}

/// A table cell's paragraphs as one line: a line break inside a paragraph,
/// or a paragraph that does not end a sentence, becomes ` / ` (pipe tables
/// have no line breaks); sentences run on with a space.
pub fn join_cell(parts: Vec<String>) -> String {
    let mut out = String::new();
    for part in parts {
        let one = part
            .split('\n')
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" / ");
        if one.is_empty() {
            continue;
        }
        if !out.is_empty() {
            let sentence = out.ends_with(['.', '!', '?', ':', ';', ',']);
            out.push_str(if sentence { " " } else { " / " });
        }
        out.push_str(&one);
    }
    out
}

/// Footnote definitions in reference order: `[^1]: text`.
pub fn render_footnotes(notes: &[(String, String)]) -> String {
    notes
        .iter()
        .map(|(label, text)| {
            let body = text.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("[^{label}]: {body}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_markup_is_escaped_but_urls_are_not() {
        assert_eq!(escape_inline("2*3*4 and a_b"), "2\\*3\\*4 and a\\_b");
        assert_eq!(
            escape_inline("see https://x.org/a_b_c now"),
            "see https://x.org/a_b_c now"
        );
        assert_eq!(escape_inline("<div> & &amp;"), "\\<div> & \\&amp;");
        assert_eq!(escape_inline("a ~ b ~~c"), "a ~ b \\~~c");
    }

    #[test]
    fn block_starts_are_escaped() {
        assert_eq!(escape_line_start("# not a heading"), "\\# not a heading");
        assert_eq!(escape_line_start("1. not a list"), "1\\. not a list");
        assert_eq!(escape_line_start("- dash"), "\\- dash");
        assert_eq!(escape_line_start("---"), "\\---");
        assert_eq!(escape_line_start("> q"), "\\> q");
        assert_eq!(escape_line_start("        indented"), "indented");
        assert_eq!(escape_line_start("1999 was a year"), "1999 was a year");
        assert_eq!(escape_line_start("#hashtag"), "#hashtag");
    }

    #[test]
    fn runs_merge_and_spaces_move_outside_markers() {
        let bold = Format {
            bold: true,
            ..Format::default()
        };
        let pieces = vec![
            Inline::text("A ", Format::default(), None),
            Inline::text("bold ", bold, None),
            Inline::text("word", bold, None),
            Inline::text(" here.", Format::default(), None),
        ];
        assert_eq!(render_inline(&pieces), "A **bold word** here.");
    }

    #[test]
    fn italic_inside_a_word_uses_asterisks() {
        let it = Format {
            italic: true,
            ..Format::default()
        };
        let pieces = vec![
            Inline::text("un", Format::default(), None),
            Inline::text("believ", it, None),
            Inline::text("able and ", Format::default(), None),
            Inline::text("this", it, None),
        ];
        assert_eq!(render_inline(&pieces), "un*believ*able and _this_");
    }

    #[test]
    fn links_wrap_their_formatted_label() {
        let pieces = vec![
            Inline::text("Read ", Format::default(), None),
            Inline::text(
                "the docs",
                Format::default(),
                Some("https://x.org/a b".into()),
            ),
            Inline::text(".", Format::default(), None),
        ];
        assert_eq!(
            render_inline(&pieces),
            "Read [the docs](https://x.org/a%20b)."
        );
    }

    #[test]
    fn lists_nest_to_the_text_column_above() {
        let blocks = vec![
            Block::ListItem {
                level: 0,
                number: Some(1),
                text: "one".into(),
            },
            Block::ListItem {
                level: 1,
                number: None,
                text: "sub".into(),
            },
            Block::ListItem {
                level: 2,
                number: None,
                text: "subsub".into(),
            },
            Block::ListItem {
                level: 0,
                number: Some(2),
                text: "two".into(),
            },
            Block::Paragraph("After.".into()),
        ];
        assert_eq!(
            render_blocks(&blocks),
            "1. one\n   - sub\n     - subsub\n2. two\n\nAfter."
        );
    }

    #[test]
    fn cell_lines_join_with_slashes_and_sentences_with_spaces() {
        assert_eq!(
            join_cell(vec![
                "Attention\nNotice what holds focus.".into(),
                "Then more.".into()
            ]),
            "Attention / Notice what holds focus. Then more."
        );
    }

    #[test]
    fn tables_pad_rows_and_escape_pipes() {
        let rows = vec![
            vec!["A".to_string(), "B".to_string()],
            vec!["x|y".to_string()],
        ];
        assert_eq!(
            render_table(&rows),
            "| A | B |\n| --- | --- |\n| x\\|y |  |"
        );
    }
}
