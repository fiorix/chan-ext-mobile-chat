//! Markdown rendered without raw HTML, remote images, or executable URLs.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

fn safe_link(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    !url.chars().any(|c| c.is_control())
        && (lower.starts_with("https://")
            || lower.starts_with("http://")
            || lower.starts_with("mailto:"))
}

pub(crate) fn render(body: &str) -> String {
    let mut image_depth = 0;
    let events = Parser::new_ext(body, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES)
        .filter_map(|event| match event {
            Event::Html(text) | Event::InlineHtml(text) => Some(Event::Text(text)),
            Event::Start(Tag::Image { .. }) => {
                image_depth += 1;
                None
            }
            Event::End(TagEnd::Image) => {
                image_depth -= 1;
                None
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => Some(Event::Start(Tag::Link {
                link_type,
                dest_url: if safe_link(&dest_url) {
                    dest_url
                } else {
                    CowStr::from("#")
                },
                title,
                id,
            })),
            other if image_depth == 0 => Some(other),
            _ => None,
        });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_markdown_without_executable_markup_or_image_fetches() {
        let html = render(
            "**Hello**\n\n<script>alert(1)</script>\n\n[x](javascript:alert%281%29)\n\n![x](https://tracker.invalid/pixel)",
        );
        assert!(html.contains("<strong>Hello</strong>"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("javascript:"));
        assert!(!html.contains("<img"));
        assert!(render("[docs](https://example.com)").contains("href=\"https://example.com\""));
    }
}
