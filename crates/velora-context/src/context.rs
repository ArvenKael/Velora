use std::fmt;

/// One part of what the model is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    title: String,
    body: String,
}

impl Section {
    /// Builds a section.
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Section {
            title: title.into(),
            body: body.into(),
        }
    }

    /// What this part is.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// What it says.
    pub fn body(&self) -> &str {
        &self.body
    }
}

/// What the model is shown, assembled from what the system holds.
///
/// Deterministic: the same beliefs, the same history and the same task produce
/// the same context, character for character. That matters because a thought is
/// something history has to be able to explain afterwards, and a context
/// assembled differently each time explains nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    sections: Vec<Section>,
    omitted: Vec<String>,
    truncated: bool,
    /// The cap the render must honour, or zero for none.
    ceiling: usize,
}

impl Context {
    pub(crate) fn new(
        sections: Vec<Section>,
        omitted: Vec<String>,
        truncated: bool,
        ceiling: usize,
    ) -> Self {
        Context {
            sections,
            omitted,
            truncated,
            ceiling,
        }
    }

    /// The parts, in the order the model reads them.
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// Parts that did not fit, by title.
    ///
    /// A thought taken without part of the picture should be explainable as
    /// such, rather than looking like a thought taken with all of it.
    pub fn omitted(&self) -> &[String] {
        &self.omitted
    }

    /// Whether anything was cut or left out.
    pub fn was_truncated(&self) -> bool {
        self.truncated
    }

    /// How long the rendered context is.
    pub fn char_count(&self) -> usize {
        self.render().chars().count()
    }

    /// Whether there is nothing to show.
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// The context as the text a model is given.
    pub fn render(&self) -> String {
        let mut out = String::new();

        for section in &self.sections {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("## ");
            out.push_str(&section.title);
            out.push('\n');
            out.push_str(&section.body);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }

        if !self.omitted.is_empty() {
            // Bounded for the same reason everything else is: this note is part
            // of the context too, and a budget that is quietly overrun is not a
            // budget.
            let shown = self.omitted.len().min(4);
            let mut titles = self.omitted[..shown].join(", ");
            if self.omitted.len() > shown {
                titles.push_str(&format!(", and {} more", self.omitted.len() - shown));
            }

            let mut footer = format!("## Left out for length\n{titles}\n");
            if self.ceiling > 0 {
                let room = self.ceiling.saturating_sub(out.chars().count() + 1);
                footer = fit(&footer, room).0;
            }

            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&footer);
        }

        out
    }
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// Shortens text to fit, reporting whether it had to.
pub(crate) fn fit(text: &str, room: usize) -> (String, bool) {
    if text.chars().count() <= room {
        return (text.to_owned(), false);
    }
    if room == 0 {
        return (String::new(), true);
    }

    let head: String = text.chars().take(room.saturating_sub(1)).collect();
    (format!("{head}…"), true)
}
