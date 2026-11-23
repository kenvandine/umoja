//! Markdown answers drawn as GTK widgets.
//!
//! pulldown-cmark parses the text; each block becomes its own widget
//! (paragraph and heading labels, list rows with a hanging bullet, a code
//! box with a Copy button, a grid for tables, a quote with an accent bar)
//! and inline marks become Pango markup inside those labels. Everything is
//! styled from CSS (`.md-*` classes), so it follows the shell's dark,
//! translucent look and the accent colour.

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use umoja_common::i18n;

/// What a link click does (open the URI, close the card...).
pub type LinkHandler = std::rc::Rc<dyn Fn(&str)>;

pub fn render(markdown: &str, on_link: LinkHandler) -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.add_css_class("md");
    let mut b = Builder {
        stack: vec![root.clone()],
        inline: String::new(),
        code: None,
        lists: Vec::new(),
        table: None,
        bullet: None,
        on_link,
    };
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for ev in Parser::new_ext(markdown, opts) {
        b.event(ev);
    }
    b.flush(&[]);
    root
}

struct Table {
    grid: gtk::Grid,
    aligns: Vec<Alignment>,
    row: i32,
    col: i32,
    head: bool,
}

struct Builder {
    /// Containers, innermost last.
    stack: Vec<gtk::Box>,
    /// Pango markup of the block being read.
    inline: String,
    code: Option<(String, String)>,
    /// Next number of each open list (None: bullets).
    lists: Vec<Option<u64>>,
    table: Option<Table>,
    /// The marker of the list item being read (a task item swaps it for a box).
    bullet: Option<gtk::Label>,
    on_link: LinkHandler,
}

impl Builder {
    fn parent(&self) -> &gtk::Box {
        self.stack.last().expect("root")
    }

    fn label(&self, markup: &str, classes: &[&str]) -> gtk::Label {
        let l = gtk::Label::builder()
            .use_markup(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .selectable(true)
            .hexpand(true)
            .build();
        l.set_markup(markup);
        for c in classes {
            l.add_css_class(c);
        }
        let on_link = self.on_link.clone();
        l.connect_activate_link(move |_, uri| {
            on_link(uri);
            glib::Propagation::Stop
        });
        // A selectable label selects all its text when it gets focus.
        l.set_focus_on_click(false);
        l
    }

    /// Ends the text read so far as one label.
    fn flush(&mut self, classes: &[&str]) {
        let text = std::mem::take(&mut self.inline);
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let l = self.label(text, classes);
        self.parent().append(&l);
    }

    fn push_box(&mut self, class: &str, spacing: i32) {
        let bx = gtk::Box::new(gtk::Orientation::Vertical, spacing);
        bx.add_css_class(class);
        self.parent().append(&bx);
        self.stack.push(bx);
    }

    fn event(&mut self, ev: Event) {
        if let Some((_, code)) = self.code.as_mut() {
            match ev {
                Event::Text(t) => return code.push_str(&t),
                Event::End(TagEnd::CodeBlock) => {}
                _ => return,
            }
        }
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.inline.push_str(&glib::markup_escape_text(&t)),
            Event::Code(t) => {
                self.inline
                    .push_str("<span font_family=\"monospace\" background=\"#ffffff1f\">\u{2009}");
                self.inline.push_str(&glib::markup_escape_text(&t));
                self.inline.push_str("\u{2009}</span>");
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                self.inline.push_str(&glib::markup_escape_text(&t))
            }
            Event::SoftBreak => self.inline.push(' '),
            Event::HardBreak => self.inline.push('\n'),
            Event::Rule => {
                self.flush(&[]);
                let s = gtk::Separator::new(gtk::Orientation::Horizontal);
                s.add_css_class("md-rule");
                self.parent().append(&s);
            }
            Event::TaskListMarker(done) => match self.bullet.as_ref() {
                Some(b) => b.set_label(if done { "☑" } else { "☐" }),
                None => self.inline.push_str(if done { "☑ " } else { "☐ " }),
            },
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph | Tag::Heading { .. } => self.flush(&[]),
            Tag::BlockQuote(_) => {
                self.flush(&[]);
                self.push_box("md-quote", 6);
            }
            Tag::CodeBlock(kind) => {
                self.flush(&[]);
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.flush(&[]);
                self.lists.push(start);
                self.push_box("md-list", 4);
            }
            Tag::Item => {
                self.flush(&[]);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}.", *n - 1)
                    }
                    _ => "•".to_owned(),
                };
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let bullet = gtk::Label::builder()
                    .label(marker)
                    .valign(gtk::Align::Start)
                    .build();
                bullet.add_css_class("md-bullet");
                let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
                content.set_hexpand(true);
                row.append(&bullet);
                row.append(&content);
                self.parent().append(&row);
                self.bullet = Some(bullet);
                self.stack.push(content);
            }
            Tag::Table(aligns) => {
                self.flush(&[]);
                let grid = gtk::Grid::builder()
                    .column_spacing(0)
                    .row_spacing(0)
                    .build();
                grid.add_css_class("md-table");
                // Wide tables scroll sideways instead of widening the card.
                let scroll = gtk::ScrolledWindow::builder()
                    .child(&grid)
                    .vscrollbar_policy(gtk::PolicyType::Never)
                    .propagate_natural_height(true)
                    .build();
                self.parent().append(&scroll);
                self.table = Some(Table {
                    grid,
                    aligns,
                    row: 0,
                    col: 0,
                    head: false,
                });
            }
            Tag::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.head = true;
                }
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => self.inline.push_str("<i>"),
            Tag::Strong => self.inline.push_str("<b>"),
            Tag::Strikethrough => self.inline.push_str("<s>"),
            Tag::Link { dest_url, .. } => {
                self.inline.push_str(&format!(
                    "<a href=\"{}\">",
                    glib::markup_escape_text(&dest_url)
                ));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(&[]),
            TagEnd::Heading(level) => {
                let class = match level {
                    HeadingLevel::H1 => "md-h1",
                    HeadingLevel::H2 => "md-h2",
                    _ => "md-h3",
                };
                self.flush(&[class]);
            }
            TagEnd::BlockQuote(_) | TagEnd::Item => {
                self.flush(&[]);
                self.stack.pop();
            }
            TagEnd::List(_) => {
                self.flush(&[]);
                self.lists.pop();
                self.stack.pop();
            }
            TagEnd::CodeBlock => {
                let (lang, code) = self.code.take().unwrap_or_default();
                self.code_block(&lang, code.trim_end());
            }
            TagEnd::TableCell => {
                let markup = std::mem::take(&mut self.inline);
                let Some(t) = self.table.as_ref() else { return };
                let (head, row, col) = (t.head, t.row, t.col);
                let align = t
                    .aligns
                    .get(col as usize)
                    .copied()
                    .unwrap_or(Alignment::None);
                let mut classes = vec!["md-cell"];
                if head {
                    classes.push("md-head");
                }
                if row % 2 == 0 && !head {
                    classes.push("md-even");
                }
                let l = self.label(markup.trim(), &classes);
                l.set_wrap(false);
                l.set_xalign(match align {
                    Alignment::Right => 1.0,
                    Alignment::Center => 0.5,
                    _ => 0.0,
                });
                let t = self.table.as_mut().expect("table");
                t.grid.attach(&l, col, row, 1, 1);
                t.col += 1;
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.head = false;
                    t.row += 1;
                    t.col = 0;
                }
            }
            TagEnd::Table => self.table = None,
            TagEnd::Emphasis => self.inline.push_str("</i>"),
            TagEnd::Strong => self.inline.push_str("</b>"),
            TagEnd::Strikethrough => self.inline.push_str("</s>"),
            TagEnd::Link => self.inline.push_str("</a>"),
            _ => {}
        }
    }

    fn code_block(&self, lang: &str, code: &str) {
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("md-code");
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        head.add_css_class("md-code-head");
        let name = gtk::Label::builder()
            .label(lang)
            .xalign(0.0)
            .hexpand(true)
            .build();
        name.add_css_class("dim-label");
        let copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text(i18n::umoja("Copy"))
            .css_classes(["flat", "circular"])
            .build();
        let text = code.to_owned();
        copy.connect_clicked(move |b| b.clipboard().set_text(&text));
        head.append(&name);
        head.append(&copy);
        let body = gtk::Label::builder()
            .label(code)
            .xalign(0.0)
            .selectable(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        body.add_css_class("md-code-body");
        body.set_focus_on_click(false);
        frame.append(&head);
        frame.append(&body);
        self.parent().append(&frame);
    }
}
