//! Self-contained log viewer with level chips + substring filter.
//!
//! The host view creates one `Entity<LogPanel>`, pushes content via
//! [`LogPanel::set_content`] whenever the on-disk log changes, and renders the
//! panel by adding the entity as a child element. All filter state and the
//! `code_editor`-mode input live inside the panel.

use gpui::*;
use gpui_component::Sizable as _;
use gpui_component::button::{Toggle, ToggleVariants as _};
use gpui_component::clipboard::Clipboard;
use gpui_component::input::{Editor, EditorState, Input, InputEvent, InputState};
use rust_i18n::t;

use crate::theme::ThemeExt;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogLevel {
    Error,
    Warning,
    Info,
    Message,
    Debug,
    Other,
}

impl LogLevel {
    fn detect(line: &str) -> Self {
        // BepInEx log format: `[<Level>  : <Source>] message`.
        let s = line.trim_start();
        if s.starts_with("[Error") {
            Self::Error
        } else if s.starts_with("[Warning") {
            Self::Warning
        } else if s.starts_with("[Info") {
            Self::Info
        } else if s.starts_with("[Message") {
            Self::Message
        } else if s.starts_with("[Debug") {
            Self::Debug
        } else {
            Self::Other
        }
    }

    fn label(self) -> String {
        match self {
            Self::Error => t!("log.level.error"),
            Self::Warning => t!("log.level.warning"),
            Self::Info => t!("log.level.info"),
            Self::Message => t!("log.level.message"),
            Self::Debug => t!("log.level.debug"),
            Self::Other => t!("log.level.other"),
        }
        .to_string()
    }

    fn chip_id(self) -> &'static str {
        match self {
            Self::Error => "log-level-Error",
            Self::Warning => "log-level-Warning",
            Self::Info => "log-level-Info",
            Self::Message => "log-level-Message",
            Self::Debug => "log-level-Debug",
            Self::Other => "log-level-Other",
        }
    }
}

const LOG_LEVELS: [LogLevel; 6] = [
    LogLevel::Error,
    LogLevel::Warning,
    LogLevel::Message,
    LogLevel::Info,
    LogLevel::Debug,
    LogLevel::Other,
];

struct LogFilters {
    error: bool,
    warning: bool,
    info: bool,
    message: bool,
    debug: bool,
    other: bool,
}

impl Default for LogFilters {
    fn default() -> Self {
        Self {
            error: true,
            warning: true,
            info: true,
            message: true,
            debug: true,
            other: true,
        }
    }
}

impl LogFilters {
    fn is_enabled(&self, level: LogLevel) -> bool {
        match level {
            LogLevel::Error => self.error,
            LogLevel::Warning => self.warning,
            LogLevel::Info => self.info,
            LogLevel::Message => self.message,
            LogLevel::Debug => self.debug,
            LogLevel::Other => self.other,
        }
    }

    fn toggle(&mut self, level: LogLevel) {
        let slot = match level {
            LogLevel::Error => &mut self.error,
            LogLevel::Warning => &mut self.warning,
            LogLevel::Info => &mut self.info,
            LogLevel::Message => &mut self.message,
            LogLevel::Debug => &mut self.debug,
            LogLevel::Other => &mut self.other,
        };
        *slot = !*slot;
    }
}

pub struct LogPanel {
    filter_input: Entity<InputState>,
    view_input: Entity<EditorState>,
    /// Last string we pushed into `view_input`. We diff against this so we
    /// don't clobber the user's selection on every render tick.
    view_cache: String,
    query: String,
    filters: LogFilters,
    content: String,
}

impl LogPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("log.filter").to_string()));
        cx.subscribe(&filter_input, |this, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.query = state.read(cx).value().to_string();
                cx.notify();
            }
        })
        .detach();

        // Custom `"log"` language is registered at app startup
        // (see `ui::log_language::register`). Highlights the `[Level: …]`
        // prefix per log level.
        let view_input = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("log")
                .line_number(false)
                .folding(false)
        });

        Self {
            filter_input,
            view_input,
            view_cache: String::new(),
            query: String::new(),
            filters: LogFilters::default(),
            content: String::new(),
        }
    }

    pub fn set_content(&mut self, content: String, cx: &mut Context<Self>) {
        if self.content != content {
            self.content = content;
            cx.notify();
        }
    }

    pub fn has_content(&self) -> bool {
        !self.content.is_empty()
    }

    fn toggle_level(&mut self, level: LogLevel, cx: &mut Context<Self>) {
        self.filters.toggle(level);
        cx.notify();
    }
}

impl Render for LogPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        // Cap retained lines so the filter pass stays cheap on huge logs.
        const MAX_LINES: usize = 2000;

        let query = self.query.trim().to_lowercase();
        let total_count = self.content.lines().count();
        let skip = total_count.saturating_sub(MAX_LINES);

        // Filter once, borrowing the log content; no per-line String alloc.
        let kept: Vec<&str> = self
            .content
            .lines()
            .skip(skip)
            .filter(|line| {
                let level = LogLevel::detect(line);
                self.filters.is_enabled(level)
                    && (query.is_empty() || line.to_lowercase().contains(&query))
            })
            .collect();
        let kept_count = kept.len();
        let joined = kept.join("\n");

        if joined != self.view_cache {
            let new_text = joined.clone();
            self.view_input.update(cx, |state, cx| {
                state.set_value(new_text, window, cx);
            });
            self.view_cache = joined;
        }

        let filter_chips: Vec<AnyElement> = LOG_LEVELS
            .iter()
            .copied()
            .map(|level| {
                Toggle::new(level.chip_id())
                    .outline()
                    .xsmall()
                    .label(level.label())
                    .checked(self.filters.is_enabled(level))
                    .on_click(cx.listener(move |this, _: &bool, _window, cx| {
                        this.toggle_level(level, cx);
                    }))
                    .into_any_element()
            })
            .collect();

        let lines_for_copy = self.view_cache.clone();

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("log.latest").to_string()),
                    )
                    .child(div().text_xs().text_color(theme.text_muted).child(
                        t!("log.lines", kept = kept_count, total = total_count).to_string(),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(div().w(px(220.0)).child(Input::new(&self.filter_input)))
                    .children(filter_chips)
                    .child(
                        Clipboard::new("copy-log")
                            .value(lines_for_copy)
                            .tooltip(t!("log.copy").to_string()),
                    ),
            )
            .child(
                div().h(px(320.0)).child(
                    // Read-only: this is a log viewer, and the text is
                    // rewritten from the log file on every refresh — typing
                    // into it would only be undone. `set_value` still applies.
                    Editor::new(&self.view_input)
                        .readonly(true)
                        .font_family("ui-monospace, monospace")
                        .size_full(),
                ),
            )
    }
}
