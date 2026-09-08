//! Markdown to WhatsApp formatting and chunking.
//!
//! Agent bodies are Markdown; WhatsApp has its own flavour (`*bold*`,
//! `_italic_`, `~strike~`, triple-backtick code). Rendering maps between the
//! two, then chunks at 3500 characters, splitting on paragraph, then line,
//! then a hard boundary, with `(1/3)` markers. A fenced block is never split
//! mid-fence: close it, mark the chunk, and reopen in the next.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// Maximum characters per outbound message.
pub const CHUNK_MAX_CHARS: usize = 3500;

/// Render Markdown to WhatsApp's formatting flavour: `**bold**` becomes
/// `*bold*`, `*italic*` becomes `_italic_`, `~~strike~~` becomes `~strike~`,
/// code becomes triple backticks, headings become a bold line, list items are
/// prefixed with `- ` nested two spaces per level, links become
/// `text (url)`, images become `[image: alt] (url)`, and tables become a
/// preformatted block. Paragraphs are separated by a blank line; a soft break
/// becomes a newline.
pub fn render(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    let mut renderer = Renderer::default();
    for event in Parser::new_ext(markdown, options) {
        renderer.event(event);
    }
    renderer.finish()
}

/// Split `text` into chunks of at most `max` characters ([`CHUNK_MAX_CHARS`]
/// is the default a caller wants), splitting on paragraph boundaries first,
/// then line boundaries, then a hard character boundary so multibyte text is
/// never torn. When more than one chunk results, each carries a trailing
/// `(n/total)` marker.
///
/// A fenced code block is never left open: a chunk that would end inside a
/// fence is closed with a fence line and the next chunk reopens the fence,
/// and a chunk that starts inside a fence is prefixed with a reopening fence
/// line. Those few added fence characters, plus the marker, may push a chunk
/// a handful of characters past `max`; the alternative, a mid-fence split,
/// would corrupt the code for the reader.
pub fn chunk(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for paragraph in split_paragraphs(text) {
        if fits_paragraph(&current, paragraph, max) {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(paragraph);
            continue;
        }
        if !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if paragraph.chars().count() <= max {
            current.push_str(paragraph);
        } else {
            // A single paragraph over the limit splits on lines, then on a
            // hard character boundary, and never packs back into its
            // neighbours.
            chunks.extend(pack_lines(paragraph, max));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    add_markers(repair_fences(chunks))
}

/// Split on blank lines, keeping fenced code blocks (which may themselves
/// contain blank lines) inside a single paragraph.
fn split_paragraphs(text: &str) -> Vec<&str> {
    let mut paragraphs = Vec::new();
    let mut start = 0;
    let mut offset = 0;
    let mut in_fence = false;
    for line in text.split('\n') {
        if line == "```" {
            in_fence = !in_fence;
        }
        if line.is_empty() && !in_fence {
            // The slice ends where the blank line starts; drop the newline
            // that terminated the last content line so paragraphs rejoin
            // cleanly with a single blank line.
            let paragraph = text[start..offset].trim_end_matches('\n');
            if !paragraph.is_empty() {
                paragraphs.push(paragraph);
            }
            start = offset + 1;
        }
        offset += line.len() + 1;
    }
    let last = text.get(start..).unwrap_or("");
    if !last.is_empty() {
        paragraphs.push(last);
    }
    paragraphs
}

fn fits_paragraph(current: &str, piece: &str, max: usize) -> bool {
    let join = if current.is_empty() { 0 } else { 2 };
    current.chars().count() + join + piece.chars().count() <= max
}

fn fits_line(current: &str, piece: &str, max: usize) -> bool {
    let join = usize::from(!current.is_empty());
    current.chars().count() + join + piece.chars().count() <= max
}

/// Pack the lines of an oversized paragraph into chunks, hard-splitting any
/// single line that alone exceeds `max`.
fn pack_lines(paragraph: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in paragraph.split('\n') {
        for piece in split_line(line, max) {
            if fits_line(&current, &piece, max) {
                if !current.is_empty() {
                    current.push('\n');
                }
                current.push_str(&piece);
            } else {
                if !current.is_empty() {
                    chunks.push(std::mem::take(&mut current));
                }
                current = piece;
            }
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Hard-split a single line at `max` characters, always on a char boundary.
fn split_line(line: &str, max: usize) -> Vec<String> {
    if line.chars().count() <= max {
        return vec![line.to_string()];
    }
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut count = 0;
    for (index, _) in line.char_indices() {
        if count == max {
            pieces.push(line[start..index].to_string());
            start = index;
            count = 0;
        }
        count += 1;
    }
    pieces.push(line[start..].to_string());
    pieces
}

/// Close any chunk that ends inside a fence and reopen the fence in the next
/// chunk, so no chunk leaves a code fence open.
fn repair_fences(chunks: Vec<String>) -> Vec<String> {
    let mut repaired = Vec::with_capacity(chunks.len());
    let mut open = false;
    for chunk in chunks {
        let mut chunk = chunk;
        if open {
            chunk.insert_str(0, "```\n");
        }
        let toggles = chunk.lines().filter(|line| *line == "```").count();
        if toggles % 2 == 1 {
            while chunk.ends_with('\n') {
                chunk.pop();
            }
            chunk.push_str("\n```");
            open = true;
        } else {
            open = false;
        }
        repaired.push(chunk);
    }
    repaired
}

/// Append a `(n/total)` marker to every chunk when there is more than one.
fn add_markers(mut chunks: Vec<String>) -> Vec<String> {
    let total = chunks.len();
    if total > 1 {
        for (index, chunk) in chunks.iter_mut().enumerate() {
            chunk.push_str(&format!("\n({}/{})", index + 1, total));
        }
    }
    chunks
}

#[derive(Default)]
struct Renderer {
    out: String,
    /// Newlines owed to the next block, so paragraphs are separated by a
    /// blank line without trailing blank lines at the end of the output.
    pending: usize,
    list_depth: usize,
    /// Cells already written in the current table row.
    cell: usize,
    links: Vec<String>,
    images: Vec<String>,
    /// Depth of block-level constructs whose contents are dropped.
    skip: usize,
}

impl Renderer {
    fn flush(&mut self) {
        if self.pending > 0 && !self.out.is_empty() {
            self.out.push_str(&"\n".repeat(self.pending));
        }
        self.pending = 0;
    }

    fn text(&mut self, text: &str) {
        self.flush();
        for c in text.chars() {
            // WhatsApp interprets these as formatting; a stray one from the
            // source would corrupt the message, so escape it.
            if matches!(c, '*' | '_' | '~' | '`') {
                self.out.push('\\');
            }
            self.out.push(c);
        }
    }

    fn finish(mut self) -> String {
        while self.out.ends_with('\n') {
            self.out.pop();
        }
        self.out
    }

    fn event(&mut self, event: Event<'_>) {
        if self.skip > 0 {
            match event {
                Event::Start(_) => self.skip += 1,
                Event::End(_) => self.skip -= 1,
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(Tag::Paragraph) => self.flush(),
            Event::End(TagEnd::Paragraph) => {
                self.pending = if self.list_depth > 0 { 1 } else { 2 };
            }
            Event::Start(Tag::Heading { .. }) => {
                self.flush();
                self.out.push('*');
            }
            Event::End(TagEnd::Heading(_)) => {
                self.out.push('*');
                self.pending = 2;
            }
            Event::Start(Tag::BlockQuote(_)) => self.flush(),
            Event::End(TagEnd::BlockQuote(_)) => self.pending = 2,
            Event::Start(Tag::CodeBlock(_)) => {
                self.flush();
                self.out.push_str("```\n");
            }
            Event::End(TagEnd::CodeBlock) => {
                while self.out.ends_with('\n') {
                    self.out.pop();
                }
                self.out.push_str("\n```");
                self.pending = 2;
            }
            Event::Code(code) => {
                self.flush();
                self.out.push_str("```");
                self.out
                    .extend(code.chars().map(|c| if c == '\n' { ' ' } else { c }));
                self.out.push_str("```");
            }
            Event::Start(Tag::List(_)) => self.list_depth += 1,
            Event::End(TagEnd::List(_)) => {
                self.list_depth -= 1;
                if self.list_depth == 0 {
                    self.pending = 2;
                }
            }
            Event::Start(Tag::Item) => {
                self.flush();
                if !self.out.is_empty() && !self.out.ends_with('\n') {
                    self.out.push('\n');
                }
                self.out
                    .push_str(&"  ".repeat(self.list_depth.saturating_sub(1)));
                self.out.push_str("- ");
            }
            Event::End(TagEnd::Item) => self.pending = 1,
            Event::Start(Tag::Table(_)) => {
                self.flush();
                self.out.push_str("```\n");
            }
            Event::Start(Tag::TableHead) => self.cell = 0,
            Event::Start(Tag::TableRow) => {
                self.cell = 0;
                if !self.out.is_empty() && !self.out.ends_with('\n') {
                    self.out.push('\n');
                }
            }
            Event::Start(Tag::TableCell) => {
                if self.cell > 0 {
                    self.out.push_str(" | ");
                }
                self.cell += 1;
            }
            Event::End(TagEnd::TableHead) => {
                self.out.push('\n');
                let separator = vec!["---"; self.cell].join(" | ");
                self.out.push_str(&separator);
                self.out.push('\n');
            }
            Event::End(TagEnd::TableRow) => self.out.push('\n'),
            Event::End(TagEnd::Table) => {
                while self.out.ends_with('\n') {
                    self.out.pop();
                }
                self.out.push_str("\n```");
                self.pending = 2;
            }
            Event::Start(Tag::Emphasis) => self.out.push('_'),
            Event::End(TagEnd::Emphasis) => self.out.push('_'),
            Event::Start(Tag::Strong) => self.out.push('*'),
            Event::End(TagEnd::Strong) => self.out.push('*'),
            Event::Start(Tag::Strikethrough) => self.out.push('~'),
            Event::End(TagEnd::Strikethrough) => self.out.push('~'),
            Event::Start(Tag::Link { dest_url, .. }) => self.links.push(dest_url.to_string()),
            Event::End(TagEnd::Link) => {
                if let Some(url) = self.links.pop() {
                    self.out.push_str(" (");
                    self.out.push_str(&url);
                    self.out.push(')');
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                self.images.push(dest_url.to_string());
                self.out.push_str("[image: ");
            }
            Event::End(TagEnd::Image) => {
                self.out.push(']');
                if let Some(url) = self.images.pop() {
                    self.out.push_str(" (");
                    self.out.push_str(&url);
                    self.out.push(')');
                }
            }
            Event::Text(text) => self.text(&text),
            Event::SoftBreak | Event::HardBreak => {
                self.flush();
                self.out.push('\n');
            }
            Event::Rule => {
                self.flush();
                self.out.push_str("---");
                self.pending = 2;
            }
            Event::FootnoteReference(name) => {
                self.flush();
                self.out.push('[');
                self.out.push_str(&name);
                self.out.push(']');
            }
            Event::InlineMath(math) => self.text(&math),
            Event::DisplayMath(math) => {
                self.flush();
                self.out.push_str(&math);
                self.pending = 2;
            }
            Event::Start(Tag::FootnoteDefinition(_) | Tag::MetadataBlock(_)) => self.skip += 1,
            Event::End(TagEnd::FootnoteDefinition | TagEnd::MetadataBlock(_)) => self.pending = 2,
            Event::End(TagEnd::TableCell) => {}
            Event::Start(
                Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition,
            ) => self.flush(),
            Event::End(
                TagEnd::DefinitionList
                | TagEnd::DefinitionListTitle
                | TagEnd::DefinitionListDefinition,
            ) => self.pending = 2,
            // Raw HTML has no WhatsApp equivalent; its contents arrive as
            // Html events and are dropped.
            Event::Start(Tag::HtmlBlock) => self.flush(),
            Event::End(TagEnd::HtmlBlock) => self.pending = 2,
            Event::Html(_) | Event::InlineHtml(_) | Event::TaskListMarker(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip a trailing `(n/total)` marker, returning the chunk body.
    fn without_marker(chunk: &str) -> &str {
        match chunk.rsplit_once('\n') {
            Some((body, tail)) if tail.starts_with('(') && tail.ends_with(')') => body,
            _ => chunk,
        }
    }

    /// Parse a trailing `(n/total)` marker.
    fn marker(chunk: &str) -> Option<(usize, usize)> {
        let tail = chunk.rsplit_once('\n')?.1;
        let inner = tail.strip_prefix('(')?.strip_suffix(')')?;
        let (n, total) = inner.split_once('/')?;
        Some((n.parse().ok()?, total.parse().ok()?))
    }

    #[test]
    fn bold_italic_and_strike_map_to_whatsapp_markers() {
        assert_eq!(render("**bold**"), "*bold*");
        assert_eq!(render("*italic*"), "_italic_");
        assert_eq!(render("~~strike~~"), "~strike~");
    }

    #[test]
    fn code_maps_to_triple_backticks() {
        assert_eq!(render("`one`"), "```one```");
        // Inline code stays on one line.
        assert_eq!(render("`a\nb`"), "```a b```");
        assert_eq!(render("```\nfn main() {}\n```"), "```\nfn main() {}\n```");
        // The fence info string is dropped: WhatsApp shows it verbatim.
        assert_eq!(
            render("```rust\nfn main() {}\n```"),
            "```\nfn main() {}\n```"
        );
    }

    #[test]
    fn heading_becomes_a_bold_line() {
        assert_eq!(render("## Notes"), "*Notes*");
    }

    #[test]
    fn links_and_images_render_the_url_in_parens() {
        assert_eq!(
            render("[docs](https://example.com)"),
            "docs (https://example.com)"
        );
        assert_eq!(
            render("![logo](https://example.com/x.png)"),
            "[image: logo] (https://example.com/x.png)"
        );
    }

    #[test]
    fn soft_break_newline_paragraph_blank_line() {
        assert_eq!(render("a\nb"), "a\nb");
        assert_eq!(render("a\n\nb"), "a\n\nb");
    }

    #[test]
    fn literal_markers_are_escaped() {
        assert_eq!(render(r"\*not bold\*"), r"\*not bold\*");
    }

    #[test]
    fn nested_lists_indent_two_spaces_per_level() {
        let markdown = "- top\n  - child\n    - grandchild\n- second";
        assert_eq!(
            render(markdown),
            "- top\n  - child\n    - grandchild\n- second"
        );
    }

    #[test]
    fn table_renders_as_a_preformatted_block() {
        let markdown = "| name | value |\n|---|---|\n| a | 1 |\n| b | 2 |";
        assert_eq!(
            render(markdown),
            "```\nname | value\n--- | ---\na | 1\nb | 2\n```"
        );
    }

    #[test]
    fn a_full_document_renders_to_whatsapp_flavour() {
        let markdown = "## Deploy notes\n\nShip **fast** with *care* and ~~no fear~~, run `make deploy`.\n\n- app\n  - worker\n\nRead [the runbook](https://example.com/rb) or see ![graph](https://example.com/g.png).\n\n| svc | status |\n|---|---|\n| api | up |\n| db | down |";
        let expected = "*Deploy notes*\n\nShip *fast* with _care_ and ~no fear~, run ```make deploy```.\n\n- app\n  - worker\n\nRead the runbook (https://example.com/rb) or see [image: graph] (https://example.com/g.png).\n\n```\nsvc | status\n--- | ---\napi | up\ndb | down\n```";
        assert_eq!(render(markdown), expected);
    }

    #[test]
    fn short_text_is_a_single_chunk_without_marker() {
        assert_eq!(chunk("hello", CHUNK_MAX_CHARS), vec!["hello".to_string()]);
        assert_eq!(chunk("", CHUNK_MAX_CHARS), vec![String::new()]);
    }

    #[test]
    fn short_paragraphs_rejoin_with_a_single_blank_line() {
        // Regression: non-final paragraphs kept the newline that preceded
        // the blank line, so rejoining produced one newline too many.
        assert_eq!(
            chunk("first\n\nsecond\n\nthird", CHUNK_MAX_CHARS),
            vec!["first\n\nsecond\n\nthird".to_string()]
        );
        assert_eq!(
            chunk("one-line options\n\n1. a\n2. b", 20),
            vec![
                "one-line options\n(1/2)".to_string(),
                "1. a\n2. b\n(2/2)".to_string()
            ]
        );
    }

    #[test]
    fn chunk_markers_round_trip_the_total() {
        let text = (1..=10)
            .map(|i| format!("paragraph {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk(&text, 40);
        assert!(chunks.len() > 1);
        let total = chunks.len();
        for (index, c) in chunks.iter().enumerate() {
            assert_eq!(marker(c), Some((index + 1, total)));
        }
    }

    #[test]
    fn one_very_long_line_splits_on_char_boundaries() {
        // Multibyte on purpose: a torn 'ü' would panic on slice or fail the
        // all-'ü' assertion.
        let body = "ü".repeat(20_000);
        let chunks = chunk(&render(&body), CHUNK_MAX_CHARS);
        assert_eq!(chunks.len(), 6);
        for (index, c) in chunks.iter().enumerate() {
            let body = without_marker(c);
            let expect = if index < 5 {
                CHUNK_MAX_CHARS
            } else {
                20_000 - 5 * CHUNK_MAX_CHARS
            };
            assert_eq!(body.chars().count(), expect);
            assert!(body.chars().all(|c| c == 'ü'));
            assert_eq!(marker(c), Some((index + 1, 6)));
        }
    }

    #[test]
    fn a_fenced_block_straddling_a_boundary_is_closed_and_reopened() {
        let markdown =
            "```\n".to_string() + &"let x = 1; // padding padding padding\n".repeat(20) + "```";
        let rendered = render(&markdown);
        let chunks = chunk(&rendered, 120);
        assert!(chunks.len() > 2, "expected several chunks, got {chunks:?}");

        let total = chunks.len();
        for (index, c) in chunks.iter().enumerate() {
            // Only fence markers and the marker line may push past `max`.
            assert!(
                c.chars().count() <= 120 + 20,
                "chunk {index} overran the limit: {c:?}"
            );
            assert_eq!(marker(c), Some((index + 1, total)));
            let fences = without_marker(c).lines().filter(|l| *l == "```").count();
            if index < total - 1 {
                // No chunk but the last may leave a fence open.
                assert_eq!(fences % 2, 0, "chunk {index} leaves a fence open: {c:?}");
                assert!(
                    without_marker(c).ends_with("```"),
                    "mid-fence chunk {index} must be closed: {c:?}"
                );
            } else {
                assert_eq!(fences, 2, "the final chunk holds the whole fence: {c:?}");
            }
        }
        // Reopening happened somewhere past the first chunk.
        assert!(
            chunks
                .iter()
                .skip(1)
                .any(|c| without_marker(c).starts_with("```\n")),
            "a straddled fence must reopen in the next chunk: {chunks:?}"
        );
    }
}
