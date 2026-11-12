//! Turn a notification body into Pango markup we can always show.
//!
//! The spec allows a small subset (`b`, `i`, `u`, `a href`, `img alt`), but
//! senders send much more: Chrome adds `<a href>` around the origin because
//! we advertise `body-hyperlinks`, mail and calendar apps send `<br>`, `<p>`,
//! `&nbsp;` and friends. Pango rejects all of that, so the old fallback
//! escaped the whole body and showed the raw HTML.
//!
//! Like gnome-shell we keep `b`, `i` and `u` (plus their HTML cousins), but
//! instead of showing other tags as text we convert them: line breaks and
//! block ends become new lines, list items get a bullet, `img` becomes its
//! alt text, links become their text, and everything else is dropped.
//! Entities are decoded and the result re-escaped, so the output is always
//! valid markup.

/// Convert `body` into valid Pango markup.
pub fn body_to_markup(body: &str) -> String {
    let mut out = Out::default();
    let mut rest = body;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('<') {
            if let Some(r) = comment_or_decl(after) {
                rest = r;
                continue;
            }
            if let Some((tag, r)) = parse_tag(after) {
                rest = out.tag(&tag, r);
                continue;
            }
            out.text("<");
            rest = after;
        } else if let Some(after) = rest.strip_prefix('&') {
            match entity(after) {
                Some((s, r)) => {
                    out.text(&s);
                    rest = r;
                }
                None => {
                    out.text("&");
                    rest = after;
                }
            }
        } else {
            let end = rest.find(['<', '&']).unwrap_or(rest.len());
            out.text(&rest[..end]);
            rest = &rest[end..];
        }
    }
    out.finish()
}

/// `<!-- … -->`, `<!DOCTYPE …>` or `<?xml …?>`: skip it.
fn comment_or_decl(after: &str) -> Option<&str> {
    if let Some(r) = after.strip_prefix("!--") {
        return Some(r.find("-->").map_or("", |i| &r[i + 3..]));
    }
    if after.starts_with('!') || after.starts_with('?') {
        return after.find('>').map(|i| &after[i + 1..]);
    }
    None
}

struct Tag {
    name: String,
    closing: bool,
    self_closing: bool,
    attrs: Vec<(String, String)>,
}

impl Tag {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Parse a tag after its `<`. Returns `None` when this does not look like a
/// tag at all (`1 < 2`), so the `<` is shown as text.
fn parse_tag(s: &str) -> Option<(Tag, &str)> {
    let (closing, s) = match s.strip_prefix('/') {
        Some(r) => (true, r),
        None => (false, s),
    };
    if !s.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let name_end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == ':'))
        .unwrap_or(s.len());
    let name = s[..name_end].to_ascii_lowercase();
    let mut r = &s[name_end..];
    // Right after the name must come the end, whitespace or `/`.
    if !(r.is_empty() || r.starts_with(['>', '/']) || r.starts_with(char::is_whitespace)) {
        return None;
    }
    let mut attrs = Vec::new();
    let mut self_closing = false;
    loop {
        r = r.trim_start();
        if let Some(after) = r.strip_prefix('>') {
            return Some((
                Tag {
                    name,
                    closing,
                    self_closing,
                    attrs,
                },
                after,
            ));
        }
        if let Some(after) = r.strip_prefix('/') {
            self_closing = true;
            r = after;
            continue;
        }
        if r.is_empty() || r.starts_with('<') {
            // Never closed: not a tag.
            return None;
        }
        let key_end = r
            .find(|c: char| c.is_whitespace() || c == '=' || c == '>' || c == '/')
            .unwrap_or(r.len());
        if key_end == 0 {
            // Stray character such as a quote; skip it.
            r = &r[r.chars().next().map_or(1, char::len_utf8)..];
            continue;
        }
        let key = r[..key_end].to_ascii_lowercase();
        r = r[key_end..].trim_start();
        let mut value = String::new();
        if let Some(after) = r.strip_prefix('=') {
            r = after.trim_start();
            if let Some(q) = r.chars().next().filter(|c| *c == '"' || *c == '\'') {
                let inner = &r[1..];
                let end = inner.find(q)?;
                value = decode_entities(&inner[..end]);
                r = &inner[end + 1..];
            } else {
                let end = r
                    .find(|c: char| c.is_whitespace() || c == '>')
                    .unwrap_or(r.len());
                value = decode_entities(&r[..end]);
                r = &r[end..];
            }
        }
        attrs.push((key, value));
    }
}

/// Decode an entity after its `&`. Returns the text and what follows `;`.
fn entity(s: &str) -> Option<(String, &str)> {
    let semi = s.find(';').filter(|i| *i <= 32)?;
    let name = &s[..semi];
    let rest = &s[semi + 1..];
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        let c = char::from_u32(code).filter(|c| *c != '\0')?;
        return Some((c.to_string(), rest));
    }
    let c = match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "ensp" => "\u{2002}",
        "emsp" => "\u{2003}",
        "thinsp" => "\u{2009}",
        "zwnj" => "\u{200c}",
        "zwj" => "\u{200d}",
        "shy" => "\u{ad}",
        "ndash" => "–",
        "mdash" => "—",
        "hellip" => "…",
        "lsquo" => "‘",
        "rsquo" => "’",
        "sbquo" => "‚",
        "ldquo" => "“",
        "rdquo" => "”",
        "bdquo" => "„",
        "laquo" => "«",
        "raquo" => "»",
        "bull" => "•",
        "middot" => "·",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "deg" => "°",
        "plusmn" => "±",
        "times" => "×",
        "divide" => "÷",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "cent" => "¢",
        "sect" => "§",
        "para" => "¶",
        "larr" => "←",
        "rarr" => "→",
        "uarr" => "↑",
        "darr" => "↓",
        "harr" => "↔",
        "check" => "✓",
        _ => return None,
    };
    Some((c.to_string(), rest))
}

fn decode_entities(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        match entity(&rest[i + 1..]) {
            Some((t, r)) => {
                out.push_str(&t);
                rest = r;
            }
            None => {
                out.push('&');
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Tags we keep, mapped to their Pango name.
fn style(name: &str) -> Option<&'static str> {
    match name {
        "b" | "strong" => Some("b"),
        "i" | "em" | "cite" | "var" => Some("i"),
        "u" | "ins" => Some("u"),
        "s" | "strike" | "del" => Some("s"),
        "tt" | "code" | "kbd" | "samp" => Some("tt"),
        "sub" => Some("sub"),
        "sup" => Some("sup"),
        _ => None,
    }
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "li"
            | "ul"
            | "ol"
            | "dl"
            | "dt"
            | "dd"
            | "tr"
            | "table"
            | "blockquote"
            | "pre"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "hr"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "address"
            | "center"
    )
}

#[derive(Default)]
struct Out {
    buf: String,
    /// Pango tags currently open, innermost last.
    open: Vec<&'static str>,
    /// New lines owed before the next text (block ends, `<br>`).
    pending_newlines: usize,
    /// Inside `<script>`/`<style>`/`<head>`: content is dropped.
    skip: Option<String>,
    /// Whether any text has been written yet.
    started: bool,
}

impl Out {
    fn text(&mut self, s: &str) {
        if self.skip.is_some() || s.is_empty() {
            return;
        }
        // Pango rejects most control characters.
        let cleaned: String;
        let s = if s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
            cleaned = s
                .chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                .collect();
            if cleaned.is_empty() {
                return;
            }
            cleaned.as_str()
        } else {
            s
        };
        if self.pending_newlines > 0 {
            if s.trim().is_empty() {
                // Whitespace between blocks: drop it.
                return;
            }
            if self.started {
                for _ in 0..self.pending_newlines.min(2) {
                    self.buf.push('\n');
                }
            }
            self.pending_newlines = 0;
            self.buf.push_str(&glib::markup_escape_text(s.trim_start()));
        } else {
            self.buf.push_str(&glib::markup_escape_text(s));
        }
        self.started = true;
    }

    fn newline(&mut self, n: usize) {
        self.pending_newlines = self.pending_newlines.max(n);
    }

    /// Handle a tag; returns the input to continue from.
    fn tag<'a>(&mut self, tag: &Tag, rest: &'a str) -> &'a str {
        if let Some(skip) = &self.skip {
            if tag.closing && *skip == tag.name {
                self.skip = None;
            }
            return rest;
        }
        let name = tag.name.as_str();
        if matches!(name, "script" | "style" | "head" | "title") {
            if !tag.closing && !tag.self_closing {
                self.skip = Some(tag.name.clone());
            }
            return rest;
        }
        if let Some(p) = style(name) {
            if tag.closing {
                if let Some(pos) = self.open.iter().rposition(|t| *t == p) {
                    // Close through to it, then reopen the inner ones.
                    let inner: Vec<_> = self.open.drain(pos..).collect();
                    for t in inner.iter().rev() {
                        self.buf.push_str(&format!("</{t}>"));
                    }
                    for t in &inner[1..] {
                        self.buf.push_str(&format!("<{t}>"));
                        self.open.push(t);
                    }
                }
            } else if !tag.self_closing {
                self.flush_newlines();
                self.buf.push_str(&format!("<{p}>"));
                self.open.push(p);
            }
            return rest;
        }
        match name {
            "br" => self.newline_now(),
            "img" if !tag.closing => {
                if let Some(alt) = tag.attr("alt").map(str::trim).filter(|a| !a.is_empty()) {
                    let alt = alt.to_string();
                    self.text(&alt);
                }
            }
            "li" if !tag.closing => {
                self.newline(1);
                self.flush_newlines();
                self.text("• ");
            }
            "td" | "th" if tag.closing => self.text(" "),
            _ if is_block(name) => {
                let n = if matches!(name, "p" | "blockquote" | "table" | "ul" | "ol") {
                    2
                } else {
                    1
                };
                self.newline(n);
            }
            // a, span, font, and anything unknown: keep only the content.
            _ => {}
        }
        rest
    }

    /// `<br>`: each one counts, even between blocks.
    fn newline_now(&mut self) {
        if self.started {
            if self.pending_newlines > 0 {
                self.pending_newlines += 1;
            } else {
                self.pending_newlines = 1;
            }
        }
    }

    /// Write owed new lines before opening a tag, so `<p><b>x` breaks
    /// before the `<b>`.
    fn flush_newlines(&mut self) {
        if self.pending_newlines > 0 && self.started {
            for _ in 0..self.pending_newlines.min(2) {
                self.buf.push('\n');
            }
        }
        self.pending_newlines = 0;
    }

    fn finish(mut self) -> String {
        for t in self.open.drain(..).rev() {
            self.buf.push_str(&format!("</{t}>"));
        }
        // Drop empty style pairs and trailing whitespace.
        let mut s = self.buf;
        loop {
            let before = s.len();
            for t in ["b", "i", "u", "s", "tt", "sub", "sup"] {
                s = s.replace(&format!("<{t}></{t}>"), "");
            }
            if s.len() == before {
                break;
            }
        }
        let trimmed = s.trim_end();
        let trimmed = trimmed.trim_start_matches(['\n', ' ', '\t']);
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(s: &str) -> String {
        let out = body_to_markup(s);
        assert!(
            gtk4::pango::parse_markup(&out, '\0').is_ok(),
            "invalid markup {out:?} from {s:?}"
        );
        out
    }

    #[test]
    fn keeps_the_spec_subset() {
        assert_eq!(valid("<b>bold</b> text"), "<b>bold</b> text");
        assert_eq!(valid("<i>a</i> <u>b</u>"), "<i>a</i> <u>b</u>");
        assert_eq!(valid("<strong>x</strong><em>y</em>"), "<b>x</b><i>y</i>");
        assert_eq!(valid("<B>Loud</B>"), "<b>Loud</b>");
    }

    #[test]
    fn plain_text_is_escaped() {
        assert_eq!(valid("1 < 2 & 3 > 0"), "1 &lt; 2 &amp; 3 &gt; 0");
        assert_eq!(valid("a<b"), "a&lt;b");
        assert_eq!(valid("Tom & Jerry"), "Tom &amp; Jerry");
        assert_eq!(valid("x &foo; y"), "x &amp;foo; y");
    }

    #[test]
    fn chrome_style_link_and_escaped_text() {
        // What Chrome sends for a Gmail notification.
        let body = "<a href=\"https://mail.google.com/\">mail.google.com</a>\n\n\
                    Re: lunch &amp; such\nDon&#39;t forget!";
        assert_eq!(
            valid(body),
            "mail.google.com\n\nRe: lunch &amp; such\nDon&apos;t forget!"
        );
    }

    #[test]
    fn images_become_alt_text() {
        assert_eq!(valid("a <img src=\"x.png\"/> b"), "a  b");
        assert_eq!(
            valid("hi <img src=\"x.png\" alt=\"🙂\"> there"),
            "hi 🙂 there"
        );
    }

    #[test]
    fn html_layout_becomes_lines() {
        assert_eq!(valid("one<br>two<br/>three"), "one\ntwo\nthree");
        assert_eq!(
            valid("<p>Meeting</p><p>Room&nbsp;4</p>"),
            "Meeting\n\nRoom\u{a0}4"
        );
        assert_eq!(
            valid("<div>When</div><div>10:00 &ndash; 11:00</div>"),
            "When\n10:00 – 11:00"
        );
        assert_eq!(valid("<ul><li>a</li><li>b</li></ul>"), "• a\n• b");
        assert_eq!(
            valid("<p>Hi <b>Ken</b>,</p>\n  <p>See <span style=\"color:red\">you</span></p>"),
            "Hi <b>Ken</b>,\n\nSee you"
        );
    }

    #[test]
    fn junk_is_dropped() {
        assert_eq!(
            valid("<html><head><style>p{}</style></head><body>x</body></html>"),
            "x"
        );
        assert_eq!(valid("a<!-- note -->b"), "ab");
        assert_eq!(valid("<font color=red>red</font>"), "red");
        assert_eq!(valid("<script>alert(1)</script>ok"), "ok");
    }

    #[test]
    fn unbalanced_tags_are_fixed() {
        assert_eq!(valid("<b>open"), "<b>open</b>");
        assert_eq!(valid("close</b> only"), "close only");
        assert_eq!(valid("<b><i>x</b>y</i>"), "<b><i>x</i></b><i>y</i>");
        assert_eq!(valid("<b></b>"), "");
    }

    #[test]
    fn odd_input_stays_valid() {
        for s in [
            "<",
            "&",
            "<a href=\"x",
            "<a href='x'>y",
            "&#0;",
            "&#xFFFFFFFF;",
            "<<b>>",
            "</>",
            "<img alt=\"<b>\">",
            "<p",
            "\u{1}ctrl",
        ] {
            valid(s);
        }
    }
}
