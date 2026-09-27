//! How a user turn's attachments ride inside its text, both ways.
//!
//! The daemon stores one text per message. What the user typed, the
//! documents they pasted (inlined, so a model without file tools has them),
//! and the paths of files they dropped (so the file tools can open them) all
//! travel in it. [`compose`] builds that text at send time; [`parse`] takes it
//! apart again when a chat is resumed, so the resumed bubble shows what the
//! user typed with its attachment chips, not the scaffolding around it (#45).
//!
//! Both halves live here so they cannot drift. An inlined document is closed
//! by an explicit end line, which is what makes it separable from the text
//! after it; turns sent before that existed are parsed best-effort.

use serde::{Deserialize, Serialize};

/// Heads the list of dropped file paths.
const FILES_HEADER: &str = "Files provided by the user:";

/// Appended to a turn sent by "Regenerate": the same question, asked again.
/// Recognized on replay, where it is not shown as a new user bubble.
pub const REGENERATE_NOTE: &str =
    "\n\n(Please reconsider your previous answer above and provide an improved response.)";

/// A document pasted or dropped into a turn and sent inline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineDocument {
    pub label: String,
    pub content: String,
}

/// An attachment as the chat shows it: a chip with a name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttachmentChip {
    pub name: String,
    /// `file`, `document` or `image`.
    pub kind: &'static str,
}

/// A stored user turn, taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTurn {
    /// What the user typed.
    pub text: String,
    pub attachments: Vec<AttachmentChip>,
    /// Sent by "Regenerate" rather than typed.
    pub regenerate: bool,
}

fn doc_open(label: &str) -> String {
    format!("--- {label} ---")
}

fn doc_close(label: &str) -> String {
    format!("--- end of {label} ---")
}

/// The text sent for a turn: inlined documents, then dropped file paths, then
/// what the user typed.
pub fn compose(text: &str, documents: &[InlineDocument], file_paths: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for doc in documents {
        parts.push(format!(
            "{}\n{}\n{}",
            doc_open(&doc.label),
            doc.content,
            doc_close(&doc.label)
        ));
    }
    if !file_paths.is_empty() {
        let list: Vec<String> = file_paths.iter().map(|p| format!("- {p}")).collect();
        parts.push(format!("{FILES_HEADER}\n{}", list.join("\n")));
    }
    let text = text.trim();
    if !text.is_empty() {
        parts.push(text.to_string());
    }
    parts.join("\n\n")
}

/// Take a stored user turn apart. Never fails: anything it cannot recognize
/// stays in the text, which is where it would have been shown anyway.
pub fn parse(stored: &str) -> ParsedTurn {
    let (mut rest, regenerate) = match stored.strip_suffix(REGENERATE_NOTE) {
        Some(r) => (r, true),
        None => (stored, false),
    };
    let mut attachments = Vec::new();

    // Inlined documents, each `--- label ---` ... `--- end of label ---`.
    while let Some((label, after_open)) = leading_doc_open(rest) {
        let close = format!("\n{}", doc_close(label));
        match after_open.find(&close) {
            Some(end) => {
                attachments.push(AttachmentChip {
                    name: label.to_string(),
                    kind: "document",
                });
                rest = after_open[end + close.len()..].trim_start_matches('\n');
            }
            None => {
                // An older turn: no end line. The documents run up to the file
                // list if there is one, otherwise up to the last paragraph,
                // which is taken to be what the user typed.
                let boundary = rest
                    .find(&format!("\n\n{FILES_HEADER}\n"))
                    .or_else(|| rest.rfind("\n\n"));
                let Some(boundary) = boundary else { break };
                let docs = &rest[..boundary];
                for line in docs.lines() {
                    if let Some(label) = legacy_doc_label(line) {
                        attachments.push(AttachmentChip {
                            name: label.to_string(),
                            kind: "document",
                        });
                    }
                }
                rest = rest[boundary..].trim_start_matches('\n');
                break;
            }
        }
    }

    // Dropped file paths.
    if let Some(after) = rest.strip_prefix(FILES_HEADER) {
        let after = after.strip_prefix('\n').unwrap_or(after);
        let (list, tail) = match after.find("\n\n") {
            Some(i) => (&after[..i], &after[i + 2..]),
            None => (after, ""),
        };
        let paths: Vec<&str> = list.lines().filter_map(|l| l.strip_prefix("- ")).collect();
        if !paths.is_empty() {
            for path in paths {
                let name = path
                    .rsplit(['/', '\\'])
                    .next()
                    .filter(|n| !n.is_empty())
                    .unwrap_or(path);
                attachments.push(AttachmentChip {
                    name: name.to_string(),
                    kind: "file",
                });
            }
            rest = tail;
        }
    }

    ParsedTurn {
        text: rest.trim().to_string(),
        attachments,
        regenerate,
    }
}

/// `(label, text after the opening line)` when `s` starts with a document.
fn leading_doc_open(s: &str) -> Option<(&str, &str)> {
    let (first, after) = s.split_once('\n')?;
    let label = legacy_doc_label(first)?;
    Some((label, after))
}

/// The label of a `--- label ---` line (not an end line).
fn legacy_doc_label(line: &str) -> Option<&str> {
    let label = line.strip_prefix("--- ")?.strip_suffix(" ---")?;
    (!label.is_empty() && !label.starts_with("end of ")).then_some(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(label: &str, content: &str) -> InlineDocument {
        InlineDocument {
            label: label.into(),
            content: content.into(),
        }
    }

    #[test]
    fn a_composed_turn_parses_back_to_what_was_typed() {
        let docs = [
            doc("notes.md", "line one\n\nline two"),
            doc("Pasted text", "x"),
        ];
        let paths = ["C:/work/report.pdf".to_string()];
        let sent = compose("What changed?", &docs, &paths);
        let parsed = parse(&sent);
        assert_eq!(parsed.text, "What changed?");
        assert!(!parsed.regenerate);
        let names: Vec<&str> = parsed.attachments.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["notes.md", "Pasted text", "report.pdf"]);
    }

    #[test]
    fn a_document_containing_blank_lines_and_fake_headers_stays_whole() {
        let sent = compose("Summarize", &[doc("a.txt", "--- b ---\n\nstill a")], &[]);
        let parsed = parse(&sent);
        assert_eq!(parsed.text, "Summarize");
        assert_eq!(parsed.attachments.len(), 1);
    }

    #[test]
    fn a_plain_turn_is_untouched() {
        let parsed = parse("just a question\n\nwith two paragraphs");
        assert_eq!(parsed.text, "just a question\n\nwith two paragraphs");
        assert!(parsed.attachments.is_empty());
    }

    #[test]
    fn a_regenerated_turn_is_recognized() {
        let parsed = parse(&format!("Why?{REGENERATE_NOTE}"));
        assert!(parsed.regenerate);
        assert_eq!(parsed.text, "Why?");
    }

    /// Turns sent before documents had an end line.
    #[test]
    fn an_older_turn_is_parsed_best_effort() {
        let old = "--- notes.md ---\nbody\n\nFiles provided by the user:\n- /f/a.txt\n\nDo it";
        let parsed = parse(old);
        assert_eq!(parsed.text, "Do it");
        assert_eq!(parsed.attachments.len(), 2);

        let old = "--- notes.md ---\nbody\n\nDo it";
        let parsed = parse(old);
        assert_eq!(parsed.text, "Do it");
        assert_eq!(parsed.attachments[0].name, "notes.md");
    }

    #[test]
    fn files_only() {
        let parsed = parse("Files provided by the user:\n- C:\\x\\y.docx\n\nOpen it");
        assert_eq!(parsed.text, "Open it");
        assert_eq!(parsed.attachments[0].name, "y.docx");
    }
}
