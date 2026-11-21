//! The writing critique behind `/critique`.
//!
//! The critique is run by the chat's own model, which acts as the editor: it
//! reads the draft, agrees a brief with the user, has the lenses applied
//! (each by a fresh delegate when the app can arm a panel of hosts, else by
//! itself in turn), triages what they found, has the top fixes sketched, and
//! writes the editorial memo. This crate holds no state and calls nothing. It
//! is the knowledge and the instructions:
//!
//! - the **guides** ([`guides`]): markdown files, one for the core lenses,
//!   one per genre (its anatomy, its own lenses, and its settings for the
//!   core ones), one for fact-checking and one for sketches. The app writes
//!   them where the model and its delegates can read them, or inlines them
//!   when it cannot ([`GuideSource`]).
//! - the **options** a `/critique` command line carries ([`parse_options`]).
//! - the **playbook** ([`playbook`]), the brief the app adds to the turn;
//!   [`playbook_on`] words it for another [`Surface`] (a button instead of
//!   the slash command, plus any output format the app reads from the run).
//!
//! Author context (voice anchors, habits to watch) is optional input: an app
//! with a behavioural-memory engine may fill it from there, one without
//! leaves it empty or takes it from its settings.

use std::fmt::Write as _;

/// A guide file: its file name and its markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guide {
    pub file: &'static str,
    pub content: &'static str,
}

pub const CORE: Guide = Guide {
    file: "core.md",
    content: include_str!("../packs/core.md"),
};
pub const FACT_CHECK: Guide = Guide {
    file: "fact-check.md",
    content: include_str!("../packs/fact-check.md"),
};
pub const SKETCHES: Guide = Guide {
    file: "sketches.md",
    content: include_str!("../packs/sketches.md"),
};

/// Every guide file, in the order an inlined playbook lists them.
pub fn guides() -> Vec<Guide> {
    let mut all = vec![CORE];
    all.extend(Genre::ALL.iter().filter_map(|g| g.pack()));
    all.push(FACT_CHECK);
    all.push(SKETCHES);
    all
}

/// The guides an inlined playbook carries: all of them while the genre is
/// open, else only the ones that genre can need.
fn inline_guides(genre: Genre) -> Vec<Guide> {
    if genre == Genre::Auto {
        return guides();
    }
    let mut out = vec![CORE];
    out.extend(genre.pack());
    out.push(FACT_CHECK);
    out.push(SKETCHES);
    out
}

/// The kind of writing being critiqued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Genre {
    /// The editor decides from the draft and asks the user to confirm.
    Auto,
    OpEd,
    Essay,
    Academic,
    Memo,
    Proposal,
    /// None of the packs: the core lenses and a generic anatomy.
    Other,
}

impl Genre {
    /// The genres the editor can choose between.
    pub const ALL: [Genre; 6] = [
        Genre::OpEd,
        Genre::Essay,
        Genre::Academic,
        Genre::Memo,
        Genre::Proposal,
        Genre::Other,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            Genre::Auto => "auto",
            Genre::OpEd => "op-ed",
            Genre::Essay => "essay",
            Genre::Academic => "academic",
            Genre::Memo => "memo",
            Genre::Proposal => "proposal",
            Genre::Other => "other",
        }
    }

    fn parse(s: &str) -> Option<Genre> {
        Some(match s {
            "auto" => Genre::Auto,
            "op-ed" | "oped" | "opinion" | "op_ed" => Genre::OpEd,
            "essay" | "essayistic" => Genre::Essay,
            "academic" | "article" | "paper" | "journal" => Genre::Academic,
            "memo" | "email" | "briefing" => Genre::Memo,
            "proposal" | "grant" | "application" => Genre::Proposal,
            "other" | "general" => Genre::Other,
            _ => return None,
        })
    }

    /// One line for the editor's menu.
    pub fn summary(self) -> &'static str {
        match self {
            Genre::Auto => "decide from the draft",
            Genre::OpEd => {
                "an argument for a general reader, 600–1,200 words, tied to the news \
                 (lede, thesis, stakes, argument, \"to be sure\", kicker)"
            }
            Genre::Essay => {
                "long-form inquiry that tests an idea against reality and turns \
                 (essayistic op-eds and academic essays too)"
            }
            Genre::Academic => {
                "a contribution to a scholarly conversation, empirical or interpretive \
                 (abstract, CARS introduction, positioning, method, analysis, discussion)"
            }
            Genre::Memo => {
                "writing a busy reader must act on: memo, work email, briefing, update \
                 (Writing for Busy Readers)"
            }
            Genre::Proposal => "a grant, fellowship or project proposal judged by reviewers",
            Genre::Other => {
                "anything else (report, blog post, newsletter, talk): core lenses and a \
                 generic anatomy"
            }
        }
    }

    /// The genre's guide. `Other` lives in the core guide.
    pub fn pack(self) -> Option<Guide> {
        let (file, content) = match self {
            Genre::OpEd => ("op-ed.md", include_str!("../packs/op-ed.md")),
            Genre::Essay => ("essay.md", include_str!("../packs/essay.md")),
            Genre::Academic => ("academic.md", include_str!("../packs/academic.md")),
            Genre::Memo => ("memo.md", include_str!("../packs/memo.md")),
            Genre::Proposal => ("proposal.md", include_str!("../packs/proposal.md")),
            Genre::Auto | Genre::Other => return None,
        };
        Some(Guide { file, content })
    }
}

/// How far along the draft is, which decides what is worth critiquing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The editor judges it from the draft.
    Auto,
    /// Structure and argument only.
    Early,
    /// Argument, structure, reader and altitude.
    Revising,
    /// Everything, the prose lens included.
    Polish,
}

impl Stage {
    pub fn slug(self) -> &'static str {
        match self {
            Stage::Auto => "auto",
            Stage::Early => "early",
            Stage::Revising => "revising",
            Stage::Polish => "polish",
        }
    }

    fn parse(s: &str) -> Option<Stage> {
        Some(match s {
            "auto" => Stage::Auto,
            "early" | "rough" => Stage::Early,
            "revising" | "revision" | "revise" | "middle" => Stage::Revising,
            "polish" | "final" | "late" | "polishing" => Stage::Polish,
            _ => return None,
        })
    }
}

/// What a `/critique` command line asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub genre: Genre,
    pub stage: Stage,
    /// Add the fact-check stage. Off unless asked for.
    pub fact_check: bool,
    /// Sketch the high-priority fixes. On unless switched off.
    pub sketches: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            genre: Genre::Auto,
            stage: Stage::Auto,
            fact_check: false,
            sketches: true,
        }
    }
}

/// Read the options at the start of a `/critique` command's text (without the
/// command itself): `genre=op-ed`, `stage=polish`, `factcheck`,
/// `sketches=off`, or a bare genre or stage name (`memo`, `polish`). Reading
/// stops at the first word that is not an option; the rest is the user's
/// instructions, which the model reads as they are.
pub fn parse_options(text: &str) -> Options {
    let mut opts = Options::default();
    for word in text.split_whitespace() {
        let word = word.trim_end_matches([',', ';']).to_ascii_lowercase();
        let (key, value) = match word.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (word.clone(), None),
        };
        let on = |v: &Option<String>| {
            !matches!(
                v.as_deref(),
                Some("off" | "no" | "false" | "0" | "none" | "skip")
            )
        };
        match (key.as_str(), &value) {
            ("genre" | "type" | "kind", Some(v)) => match Genre::parse(v) {
                Some(g) => opts.genre = g,
                None => break,
            },
            ("stage", Some(v)) => match Stage::parse(v) {
                Some(s) => opts.stage = s,
                None => break,
            },
            ("factcheck" | "fact-check" | "facts" | "fact_check", v) => opts.fact_check = on(v),
            ("sketches" | "sketch" | "variants", v) => opts.sketches = on(v),
            ("nosketches" | "no-sketches", None) => opts.sketches = false,
            (bare, None) => {
                if let Some(g) = Genre::parse(bare).filter(|g| *g != Genre::Auto) {
                    opts.genre = g;
                } else if let Some(s) = Stage::parse(bare).filter(|s| *s != Stage::Auto) {
                    opts.stage = s;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    opts
}

/// A host the editor may put a delegate on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// What the `host` argument of `call_specialist` takes.
    pub id: String,
    /// How the editor tells hosts apart: name, provider / model, capability.
    pub label: String,
}

/// Who applies the lenses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delegation {
    /// Fresh delegates of the `critic` specialist, on these hosts (at least
    /// one), armed for the turn by the app.
    Panel(Vec<Host>),
    /// The editor applies every lens itself.
    Solo,
}

/// Where the guides are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuideSource {
    /// Written to this folder, under their [`Guide::file`] names.
    Folder(String),
    /// Not on disk: the playbook carries them.
    Inline,
}

/// The name of the specialist a panel's delegates run as.
pub const CRITIC: &str = "critic";

/// What the playbook is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybookInput<'a> {
    pub options: Options,
    pub delegation: Delegation,
    pub guides: GuideSource,
    /// Paths or URLs of the draft. Empty: the draft is in the user's message,
    /// or the editor must ask for it.
    pub drafts: &'a [String],
    /// Optional calibration from the author's own writing.
    pub author_context: Option<&'a str>,
    /// Where the finished memo should be saved, when the app has a folder for
    /// outputs.
    pub outputs_dir: Option<&'a str>,
}

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

fn guide_ref(source: &GuideSource, file: &str) -> String {
    match source {
        GuideSource::Folder(dir) => {
            let sep = if dir.contains('\\') && !dir.contains('/') {
                '\\'
            } else {
                '/'
            };
            format!("`{}{sep}{file}`", dir.trim_end_matches(['/', '\\']))
        }
        GuideSource::Inline => format!("the guide `{file}` below"),
    }
}

/// Where the critique runs: how the user started it, how they confirm the
/// brief, and anything the app adds to the run. Kitty's chat is
/// [`SLASH_COMMAND`]; an app with buttons (Bacon's partner pane) words its
/// own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surface<'a> {
    /// Completes "a writing critique the user started …" (no full stop).
    pub started: &'a str,
    /// The line the brief ends with: how the user runs the critique.
    pub go_line: &'a str,
    /// Extra instructions for the run (an output format the app reads, say),
    /// placed after the memo's shape.
    pub run_extra: Option<&'a str>,
}

/// The `/critique` command in a chat: what [`playbook`] uses.
pub const SLASH_COMMAND: Surface<'static> = Surface {
    started: "with /critique",
    go_line: "Reply **go** to run the critique, or correct anything above.",
    run_extra: None,
};

/// The intro after its first sentence (which names the [`Surface`]).
const INTRO_TAIL: &str = "\
You read the draft, agree a brief with the user, have the critique's lenses applied, decide what \
matters most, and write the editorial memo. You diagnose and direct; you never rewrite the draft as \
a whole.";

const BRIEF_TURN_HEAD: &str = "\
## This turn: the brief

1. Read the whole draft.
2. Read the core guide and, once you have decided the genre, its pack. If the genre is `auto`, \
choose from the menu below.
3. Reply with the brief in exactly this shape, then stop:

**Genre:** the genre (and, if you inferred it, why, in one line)
**Reader:** who exactly will read this, what they already know, and what they want from it
**Purpose:** what the piece must make that reader think or do
**Stage:** early (structure and argument only), revising (argument, structure, reader) or polish \
(everything, the prose lens included)
**Preservation lock:**
- Core claim: the central thesis
- Sharpest angle: what cuts against conventional wisdom
- Load-bearing evidence: the one or two strongest cases, data or sources
- Anchor: a recurring image, scene or motif, or \"none yet\"
- Voice: two or three adjectives, and one verbatim sentence worth protecting
**Lenses:** the lenses you will run, given the genre and stage
**Fact-check:** on or off
**Sketches:** on or off

End with: \"";

/// What follows the [`Surface::go_line`] in the brief turn.
const BRIEF_TURN_TAIL: &str = "\" Do not begin the critique in this turn. If there is no draft to \
read, ask the user for it instead.";

const LENS_RULES: &str = "\
Which lenses run:
- Always: the map, then the genre's anatomy (its pack's Anatomy section, or the generic anatomy \
in the core guide for \"other\"), the pack's own lens, argument and adversary, multivalence, \
reader and altitude, and seams and ending.
- At the polish stage only: prose.
- At the early stage, skip seams and ending and the prose lens, and look past sentence-level \
problems everywhere.";

const TRIAGE: &str = "\
### Triage

Sort everything the lenses found into four buckets. Choose; do not list everything.

1. **What's strong (protect these):** three or four points, from the preservation lock and the \
lenses, each with the verbatim line or passage to protect.
2. **High-priority fixes:** at most three, the moves that would most change the piece — \
structural moves before sentence-level ones, and none from the prose lens at the early stage. \
For each: a type, `MECHANICAL_REWRITE` (a cut, a compression, an altitude shift, a reordering, \
ending surgery) or `HUMAN_SLOT` (it needs the author's own experience, implication or \
uncertainty); the target span (P#–P#, one or two paragraphs); the issue, in two sentences \
grounded in quotes; and a one-sentence directive for how to rework that span.
3. **Nice-to-haves:** two or three secondary improvements, two sentences each.
4. **If you have time:** one or two small items.

When two reports of the same lens disagree, do not average them: put the sharper one where it \
belongs and keep the other's distinct angle, labelled as a second reading. A wrong fact found by \
the fact-check goes first among the high-priority fixes and does not count against the three.";

const MEMO: &str = "\
### The memo

Write the memo as your reply, addressed to the author as a senior editor would write it. Never \
mention lenses, delegates, steps or the guides. Use this shape:

# Editorial memo: <title of the piece>

## What's strong (protect these)
## High-priority fixes
### 1. <actionable title>
**The issue:** …
> **Original (P#–P#):** the span, verbatim
then the sketches, each labelled, or for a fix you did not sketch, the directive.
(repeat for each fix)
## Fact-check (only if it ran)
A table: claim, location, verdict, source, and what the source actually says.
## Nice-to-haves
## If you have time";

/// The brief the app adds to a `/critique` turn.
pub fn playbook(input: &PlaybookInput) -> String {
    playbook_on(input, &SLASH_COMMAND)
}

/// The brief for a critique started on `surface`.
pub fn playbook_on(input: &PlaybookInput, surface: &Surface) -> String {
    let opts = input.options;
    let g = |file: &str| guide_ref(&input.guides, file);
    let mut out = String::new();
    let _ = write!(
        out,
        "You are the editor for a writing critique the user started {}. {INTRO_TAIL}",
        surface.started
    );
    out.push_str("\n\n");

    let _ = writeln!(
        out,
        "Options from the user's command: genre={}, stage={}, fact-check={}, sketches={}. \
         The user may change any of them when they answer the brief.\n",
        opts.genre.slug(),
        opts.stage.slug(),
        on_off(opts.fact_check),
        on_off(opts.sketches),
    );

    out.push_str("The draft: ");
    if input.drafts.is_empty() {
        out.push_str(
            "in the user's message or its attachments. If there is none, ask the user for it.\n",
        );
    } else {
        out.push('\n');
        for d in input.drafts {
            let _ = writeln!(out, "- {d}");
        }
    }

    out.push_str("\nGuides:\n");
    let _ = writeln!(
        out,
        "- core lenses, and the generic anatomy: {}",
        g(CORE.file)
    );
    for genre in Genre::ALL {
        let file = genre.pack().map_or(CORE.file, |p| p.file);
        let _ = writeln!(out, "- {}: {} — {}", genre.slug(), genre.summary(), g(file));
    }
    let _ = writeln!(out, "- fact-checking: {}", g(FACT_CHECK.file));
    let _ = writeln!(out, "- sketches: {}", g(SKETCHES.file));

    out.push('\n');
    out.push_str(BRIEF_TURN_HEAD);
    out.push_str(surface.go_line);
    out.push_str(BRIEF_TURN_TAIL);
    out.push_str("\n\n## Next turn: the run\n\n");
    out.push_str(
        "When the user answers the brief, apply their corrections and run the critique in that \
         same turn, without asking again unless their reply is a question. Work through these \
         steps in order.\n\n",
    );
    out.push_str(LENS_RULES);
    out.push_str("\n\n");

    match &input.delegation {
        Delegation::Panel(hosts) => {
            let _ = write!(
                out,
                "Delegation: each lens is applied by a fresh `{CRITIC}` delegate, so it sees \
                 only what it needs. Call `call_specialist` with specialist \"{CRITIC}\" and a \
                 `host` from the panel below; put every independent call in the same step, then \
                 collect the reports with `await_specialists`. Every request contains, in this \
                 order: (a) the task, named exactly as the guide names it (\"lens: map\", \
                 \"lens: argument and adversary\", \"anatomy: op-ed\", \"lens: verify, claims \
                 C1–C5\", \"sketch: voice, fix 2\"); (b) the confirmed brief, in full; (c) the map, \
                 once it exists; (d) for a sketch, the fix and its target span verbatim. Pass the \
                 draft's paths or URLs and the guide files the task needs in `refs`. If a \
                 delegate fails, apply that lens yourself.\n\n"
            );
            out.push_str("Panel (host — name — provider / model — capability, price):\n");
            for h in hosts {
                let _ = writeln!(out, "- {} — {}", h.id, h.label);
            }
            out.push('\n');
            if hosts.len() >= 2 {
                out.push_str(
                    "Put the lenses on the more capable hosts. Run the argument and adversary \
                     lens twice, on two different hosts, and keep both reports. Give each fix's \
                     two sketches two different hosts.\n\n",
                );
            }
            out.push_str(
                "1. **Map.** One delegate: \"lens: map\". Wait for it.\n\
                 2. **Lenses.** One delegate per lens, all at once, each given the map.\n",
            );
            if opts.fact_check {
                out.push_str(
                    "3. **Fact-check.** Find the claims yourself (\"Lens: claims\" in the \
                     fact-checking guide), then verify them in batches of about five, one \
                     delegate per batch, all at once.\n",
                );
            } else {
                out.push_str(
                    "3. **Fact-check** only if the user turned it on in reply to the brief: then \
                     find the claims yourself and verify them in batches of about five, one \
                     delegate per batch.\n",
                );
            }
            out.push_str("4. **Triage**, yourself (below).\n");
            out.push_str(
                "5. **Sketches**, unless switched off: for each high-priority fix, two \
                 delegates at once, one under the structural mandate and one under the voice \
                 mandate of the sketches guide.\n",
            );
            out.push_str("6. **Memo**, yourself (below).\n\n");
        }
        Delegation::Solo => {
            out.push_str(
                "Delegation: none is available, so apply each lens yourself, one at a time and \
                 in order. Before each, re-read only that lens's questions. Keep your working \
                 notes terse; the memo is what the user reads.\n\n\
                 1. **Map.**\n\
                 2. **Lenses**, one after another, each against the map.\n",
            );
            if opts.fact_check {
                out.push_str(
                    "3. **Fact-check:** find the claims, then verify each with web search.\n",
                );
            } else {
                out.push_str(
                    "3. **Fact-check** only if the user turned it on in reply to the brief.\n",
                );
            }
            out.push_str(
                "4. **Triage** (below).\n\
                 5. **Sketches**, unless switched off: for each high-priority fix, one sketch \
                 under each mandate of the sketches guide.\n\
                 6. **Memo** (below).\n\n",
            );
        }
    }

    out.push_str(TRIAGE);
    out.push_str("\n\n");
    out.push_str(MEMO);
    out.push('\n');
    if let Some(extra) = surface.run_extra.map(str::trim).filter(|e| !e.is_empty()) {
        out.push('\n');
        out.push_str(extra);
        out.push('\n');
    }
    if let Some(dir) = input.outputs_dir {
        let _ = writeln!(
            out,
            "\nIf you can write files, also save the memo as a markdown file in `{dir}`, named \
             after the piece (`critique - <title>.md`)."
        );
    }

    if let Some(ctx) = input
        .author_context
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        out.push_str(
            "\n## Author context\n\nWhat the author has said about their own writing. Use it \
             to weigh findings (habits to watch get extra scrutiny; boundaries they have set are \
             respected) and, in sketches, only to calibrate cadence and restraint. Its examples \
             come from other pieces: never reference their topics or reuse their phrasing. Pass \
             it to every sketch delegate.\n\n<author_context>\n",
        );
        out.push_str(ctx);
        out.push_str("\n</author_context>\n");
    }

    if input.guides == GuideSource::Inline {
        out.push_str("\n## The guides\n");
        for guide in inline_guides(opts.genre) {
            let _ = write!(
                out,
                "\n<guide file=\"{}\">\n{}\n</guide>\n",
                guide.file,
                guide.content.trim_end()
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_read_until_the_first_ordinary_word() {
        let o = parse_options("genre=op-ed stage=polish factcheck sketches=off please be harsh");
        assert_eq!(o.genre, Genre::OpEd);
        assert_eq!(o.stage, Stage::Polish);
        assert!(o.fact_check);
        assert!(!o.sketches);

        let o = parse_options("memo, early: this goes to the dean genre=essay");
        assert_eq!(o.genre, Genre::Memo);
        assert_eq!(
            o.stage,
            Stage::Auto,
            "\"early:\" is not an option word, so reading stopped"
        );

        let o = parse_options("This is my op-ed draft");
        assert_eq!(o, Options::default(), "prose never sets an option");
        assert_eq!(parse_options(""), Options::default());
        assert!(!parse_options("factcheck=off").fact_check);
        assert!(!parse_options("nosketches").sketches);
        assert_eq!(parse_options("genre=sonnet polish").genre, Genre::Auto);
    }

    #[test]
    fn every_genre_has_a_guide() {
        for g in Genre::ALL {
            let pack = g.pack().unwrap_or(CORE);
            assert!(pack.content.starts_with("# Critique guide"), "{g:?}");
            assert!(pack.content.contains("Anatomy") || pack.content.contains("anatomy"));
        }
        let files: Vec<_> = guides().iter().map(|g| g.file).collect();
        assert_eq!(
            files,
            [
                "core.md",
                "op-ed.md",
                "essay.md",
                "academic.md",
                "memo.md",
                "proposal.md",
                "fact-check.md",
                "sketches.md"
            ]
        );
    }

    fn input<'a>(
        delegation: Delegation,
        guides: GuideSource,
        drafts: &'a [String],
    ) -> PlaybookInput<'a> {
        PlaybookInput {
            options: Options::default(),
            delegation,
            guides,
            drafts,
            author_context: None,
            outputs_dir: None,
        }
    }

    #[test]
    fn a_panel_playbook_names_the_hosts_and_points_at_the_guide_files() {
        let drafts = vec!["C:\\chat\\critique\\draft.md".to_string()];
        let hosts = vec![
            Host {
                id: "card-a".into(),
                label: "A — OpenRouter / a — capability 80, $$".into(),
            },
            Host {
                id: "card-b".into(),
                label: "B — OpenRouter / b — capability unknown, price unknown".into(),
            },
        ];
        let text = playbook(&input(
            Delegation::Panel(hosts),
            GuideSource::Folder("C:\\chat\\critique\\guides".into()),
            &drafts,
        ));
        assert!(text.contains("- card-a — A — OpenRouter / a — capability 80, $$"));
        assert!(text.contains("`C:\\chat\\critique\\guides\\memo.md`"));
        assert!(text.contains("- C:\\chat\\critique\\draft.md"));
        assert!(text.contains("two different hosts"));
        assert!(text.contains("specialist \"critic\""));
        assert!(!text.contains("<guide file="), "files are not inlined");
        assert!(text.contains("genre=auto, stage=auto, fact-check=off, sketches=on"));
    }

    #[test]
    fn a_solo_playbook_inlines_the_guides_and_the_author_context() {
        let mut i = input(Delegation::Solo, GuideSource::Inline, &[]);
        i.options.fact_check = true;
        i.author_context = Some("Habit to watch: the institutional \"we\".");
        i.outputs_dir = Some("C:/chat/outputs");
        let text = playbook(&i);
        assert!(!text.contains("call_specialist"));
        assert!(text.contains("verify each with web search"));
        assert!(text.contains("<guide file=\"op-ed.md\">"));
        assert!(text.contains("<guide file=\"sketches.md\">"));
        assert!(text.contains("<author_context>\nHabit to watch"));
        assert!(text.contains("`C:/chat/outputs`"));
        assert!(text.contains("ask the user for it"));

        i.options.genre = Genre::Memo;
        let text = playbook(&i);
        assert!(text.contains("<guide file=\"memo.md\">"));
        assert!(
            !text.contains("<guide file=\"op-ed.md\">"),
            "a known genre inlines its own pack only"
        );
    }

    #[test]
    fn a_surface_words_the_start_and_the_go_line_and_adds_to_the_run() {
        let i = input(Delegation::Solo, GuideSource::Inline, &[]);
        let slash = playbook(&i);
        assert_eq!(slash, playbook_on(&i, &SLASH_COMMAND));
        assert!(slash.contains("the user started with /critique. You read the draft"));
        assert!(slash.contains(
            "End with: \"Reply **go** to run the critique, or correct anything above.\" Do not \
             begin the critique in this turn."
        ));

        let button = Surface {
            started: "with the Critique button",
            go_line: "Press **Run critique** to run it.",
            run_extra: Some("  EXTRA RULES  "),
        };
        let text = playbook_on(&i, &button);
        assert!(text.contains("the user started with the Critique button. You read"));
        assert!(text.contains("End with: \"Press **Run critique** to run it.\" Do not begin"));
        assert!(!text.contains("/critique"));
        assert!(!text.contains("Reply **go**"));
        let memo = text.find("## If you have time").unwrap();
        let extra = text.find("\nEXTRA RULES\n").unwrap();
        assert!(memo < extra, "the app's run rules follow the memo's shape");
        assert!(!slash.contains("EXTRA RULES"));
    }

    #[test]
    fn blank_author_context_adds_nothing() {
        let mut i = input(Delegation::Solo, GuideSource::Folder("/g".into()), &[]);
        i.author_context = Some("   ");
        assert!(!playbook(&i).contains("Author context"));
    }
}
