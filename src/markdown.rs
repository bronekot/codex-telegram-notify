use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// Convert ordinary Markdown to Telegram's HTML subset, limiting visible text.
pub fn render_markdown(input: &str, max_length: usize) -> String {
    if max_length == 0 {
        return String::new();
    }
    let mut document = Document::default();
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(input, options) {
        match event {
            Event::Start(tag) => document.start(tag),
            Event::End(tag) => document.end(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                document.text(&text);
            }
            Event::Code(text) => {
                document.frames.push(Some(Format::Code));
                document.text(&text);
                document.frames.pop();
            }
            Event::SoftBreak | Event::HardBreak => document.text("\n"),
            Event::Rule => {
                document.line_break(2);
                document.text("—");
                document.line_break(2);
            }
            Event::TaskListMarker(checked) => document.text(if checked { "☑ " } else { "☐ " }),
            // These events require parser extensions that are deliberately disabled.
            Event::FootnoteReference(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                document.text(&text);
            }
        }
    }
    document.to_html(max_length.min(4096))
}

/// Escape literal metadata before embedding it in a Markdown template.
pub fn escape_markdown(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '\n' => output.push_str("&#10;"),
            '\r' => output.push_str("&#13;"),
            '\t' => output.push_str("&#9;"),
            character if character.is_ascii_punctuation() => {
                output.push('\\');
                output.push(character);
            }
            character => output.push(character),
        }
    }
    output
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Format {
    Bold,
    Italic,
    Strike,
    Quote,
    Link(String),
    Code,
    Pre(Option<String>),
}

impl Format {
    fn open(&self, output: &mut String) {
        match self {
            Self::Bold => output.push_str("<b>"),
            Self::Italic => output.push_str("<i>"),
            Self::Strike => output.push_str("<s>"),
            Self::Quote => output.push_str("<blockquote>"),
            Self::Link(url) => {
                output.push_str("<a href=\"");
                escape_html(url, output);
                output.push_str("\">");
            }
            Self::Code => output.push_str("<code>"),
            Self::Pre(language) => {
                output.push_str("<pre>");
                if let Some(language) = language {
                    output.push_str("<code class=\"language-");
                    escape_html(language, output);
                    output.push_str("\">");
                }
            }
        }
    }

    fn close(&self, output: &mut String) {
        output.push_str(match self {
            Self::Bold => "</b>",
            Self::Italic => "</i>",
            Self::Strike => "</s>",
            Self::Quote => "</blockquote>",
            Self::Link(_) => "</a>",
            Self::Code => "</code>",
            Self::Pre(Some(_)) => "</code></pre>",
            Self::Pre(None) => "</pre>",
        });
    }
}

struct Run {
    text: String,
    formats: Vec<Format>,
}

#[derive(Default)]
struct Document {
    runs: Vec<Run>,
    frames: Vec<Option<Format>>,
    lists: Vec<Option<u64>>,
    pending_break: usize,
    trailing_newlines: usize,
}

impl Document {
    fn start(&mut self, tag: Tag<'_>) {
        let format = match tag {
            Tag::Heading { .. } => {
                self.line_break(2);
                Some(Format::Bold)
            }
            Tag::Strong => Some(Format::Bold),
            Tag::Emphasis => Some(Format::Italic),
            Tag::Strikethrough => Some(Format::Strike),
            Tag::BlockQuote(_) => {
                self.line_break(2);
                Some(Format::Quote)
            }
            Tag::CodeBlock(kind) => {
                self.line_break(2);
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .filter(|language| {
                            language.chars().all(|character| {
                                character.is_ascii_alphanumeric()
                                    || matches!(character, '_' | '-' | '+' | '.')
                            })
                        })
                        .map(str::to_owned),
                    CodeBlockKind::Indented => None,
                };
                Some(Format::Pre(language))
            }
            Tag::Link { dest_url, .. } => valid_link(&dest_url).map(Format::Link),
            Tag::List(start) => {
                self.line_break(if self.lists.is_empty() { 2 } else { 1 });
                self.lists.push(start);
                None
            }
            Tag::Item => {
                self.line_break(1);
                let indent = "  ".repeat(self.lists.len().saturating_sub(1).min(8));
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number = number.saturating_add(1);
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                self.text(&format!("{indent}{marker}"));
                None
            }
            Tag::HtmlBlock => {
                self.line_break(2);
                None
            }
            _ => None,
        };
        self.frames.push(format);
    }

    fn end(&mut self, tag: TagEnd) {
        self.frames.pop();
        match tag {
            TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock => self.line_break(2),
            TagEnd::Item => self.line_break(1),
            TagEnd::List(_) => {
                self.lists.pop();
                self.line_break(if self.lists.is_empty() { 2 } else { 1 });
            }
            _ => {}
        }
    }

    fn line_break(&mut self, count: usize) {
        self.pending_break = self.pending_break.max(count);
    }

    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if !self.runs.is_empty() && self.pending_break > self.trailing_newlines {
            let separator = "\n".repeat(self.pending_break - self.trailing_newlines);
            self.push(&separator, Vec::new());
        }
        self.pending_break = 0;
        self.push(text, self.formats());
    }

    fn formats(&self) -> Vec<Format> {
        let active: Vec<_> = self.frames.iter().flatten().collect();
        // Telegram code/pre cannot overlap other entities. Quotes and links also
        // cannot nest, so keep the innermost one and split the surrounding runs.
        if let Some(code) = active
            .iter()
            .rev()
            .find(|format| matches!(format, Format::Code | Format::Pre(_)))
        {
            return vec![(**code).clone()];
        }
        let mut formats = Vec::new();
        if let Some(container) = active
            .iter()
            .rev()
            .find(|format| matches!(format, Format::Quote | Format::Link(_)))
        {
            formats.push((**container).clone());
        }
        for format in active {
            if matches!(format, Format::Bold | Format::Italic | Format::Strike)
                && !formats.contains(format)
            {
                formats.push(format.clone());
            }
        }
        formats
    }

    fn push(&mut self, text: &str, formats: Vec<Format>) {
        let newlines = text.chars().rev().take_while(|c| *c == '\n').count();
        self.trailing_newlines = if newlines == text.len() {
            self.trailing_newlines + newlines
        } else {
            newlines
        };
        if let Some(last) = self.runs.last_mut().filter(|run| run.formats == formats) {
            last.text.push_str(text);
        } else {
            self.runs.push(Run {
                text: text.to_owned(),
                formats,
            });
        }
    }

    fn to_html(&self, max_length: usize) -> String {
        let length: usize = self
            .runs
            .iter()
            .map(|run| run.text.encode_utf16().count())
            .sum();
        let truncated = length > max_length;
        let suffix = if max_length > 2 { "\n…" } else { "…" };
        let mut remaining = max_length
            - if truncated {
                suffix.encode_utf16().count()
            } else {
                0
            };
        let mut output = String::new();
        let mut open: &[Format] = &[];
        for run in &self.runs {
            let mut end = 0;
            for (index, character) in run.text.char_indices() {
                if character.len_utf16() > remaining {
                    break;
                }
                remaining -= character.len_utf16();
                end = index + character.len_utf8();
            }
            if end == 0 {
                break;
            }
            let common = open
                .iter()
                .zip(&run.formats)
                .take_while(|(a, b)| a == b)
                .count();
            for format in open[common..].iter().rev() {
                format.close(&mut output);
            }
            for format in &run.formats[common..] {
                format.open(&mut output);
            }
            escape_html(&run.text[..end], &mut output);
            open = &run.formats;
            if end < run.text.len() {
                break;
            }
        }
        for format in open.iter().rev() {
            format.close(&mut output);
        }
        if truncated {
            output.push_str(suffix);
        }
        output
    }
}

fn valid_link(destination: &str) -> Option<String> {
    if destination.chars().any(char::is_control) {
        return None;
    }
    let url = reqwest::Url::parse(destination).ok()?;
    if matches!(url.scheme(), "http" | "https" | "tg") && url.host_str().is_some() {
        Some(url.to_string())
    } else {
        None
    }
}

fn escape_html(text: &str, output: &mut String) {
    for character in text.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            character => output.push(character),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{escape_markdown, render_markdown};

    #[test]
    fn renders_common_markdown_as_telegram_html() {
        let input = "# Итог\n\n**Готово**: *текст* и ~~старое~~, `a < b`.\n\n- один\n- два\n\n3. три\n4. четыре";
        assert_eq!(
            render_markdown(input, 4096),
            "<b>Итог</b>\n\n<b>Готово</b>: <i>текст</i> и <s>старое</s>, <code>a &lt; b</code>.\n\n• один\n• два\n\n3. три\n4. четыре"
        );
    }

    #[test]
    fn escapes_html_and_preserves_unclosed_markdown() {
        assert_eq!(
            render_markdown("**unfinished <b>raw</b> & text", 4096),
            "**unfinished &lt;b&gt;raw&lt;/b&gt; &amp; text"
        );
        assert_eq!(
            render_markdown("```\na < b & c", 4096),
            "<pre>a &lt; b &amp; c</pre>"
        );
    }

    #[test]
    fn metadata_cannot_introduce_markdown_or_html() {
        let literal = "a_[b]*`\\<tag>&quot;\n# title";
        assert_eq!(
            render_markdown(&escape_markdown(literal), 4096),
            "a_[b]*`\\&lt;tag&gt;&amp;quot;\n# title"
        );
    }

    #[test]
    fn links_escape_attributes_and_ignore_local_or_unsupported_destinations() {
        assert_eq!(
            render_markdown("[web](https://example.com/?a=1&b=2) [file](/tmp/a.rs:3) [bad](javascript:alert) [user](tg://user?id=123)", 4096),
            "<a href=\"https://example.com/?a=1&amp;b=2\">web</a> file bad <a href=\"tg://user?id=123\">user</a>"
        );
        assert_eq!(
            render_markdown("[quote](<https://example.com/\"quoted\">)", 4096),
            "<a href=\"https://example.com/%22quoted%22\">quote</a>"
        );
    }

    #[test]
    fn code_and_quotes_do_not_create_forbidden_nested_entities() {
        assert_eq!(
            render_markdown("**before `code` after**", 4096),
            "<b>before </b><code>code</code><b> after</b>"
        );
        assert_eq!(
            render_markdown("> **quote** and [link](https://example.com)\n>\n> > nested\n\n```rust\nlet x = 1;\n```", 4096),
            "<blockquote><b>quote</b> and </blockquote><a href=\"https://example.com/\">link</a>\n\n<blockquote>nested</blockquote>\n\n<pre><code class=\"language-rust\">let x = 1;\n</code></pre>"
        );
    }

    #[test]
    fn truncation_counts_visible_utf16_and_keeps_entities_and_tags_complete() {
        assert_eq!(render_markdown("**<&😀abcd**", 7), "<b>&lt;&amp;😀a</b>\n…");
        assert_eq!(render_markdown("**😀😀**", 3), "\n…");
        assert_eq!(render_markdown("😀", 1), "…");
        assert_eq!(render_markdown("abc", 0), "");
        assert_eq!(render_markdown("**a & b**", 5), "<b>a &amp; b</b>");

        let long = format!("**{}**", "😀".repeat(3000));
        let rendered = render_markdown(&long, 10_000);
        assert!(rendered.starts_with("<b>"));
        assert!(rendered.ends_with("</b>\n…"));
        let visible = rendered.replace("<b>", "").replace("</b>", "");
        assert_eq!(visible.encode_utf16().count(), 4096);
    }

    #[test]
    fn truncation_closes_code_and_links_without_cutting_attributes() {
        assert_eq!(
            render_markdown("```rust\n<&😀abcdef\n```", 7),
            "<pre><code class=\"language-rust\">&lt;&amp;😀a</code></pre>\n…"
        );
        assert_eq!(
            render_markdown("[**abcdef**](https://example.com/?x=1&y=2)", 5),
            "<a href=\"https://example.com/?x=1&amp;y=2\"><b>abc</b></a>\n…"
        );
        assert_eq!(
            render_markdown("[ab](https://example.com) then **cdef**", 5),
            "<a href=\"https://example.com/\">ab</a> \n…"
        );
    }
}
