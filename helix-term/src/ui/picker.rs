mod handlers;
mod query;

use crate::{
    alt,
    compositor::{self, Component, Compositor, Context, Event, EventResult},
    ctrl, key, shift,
    ui::{
        self,
        diff_view::{
            compute_diff_lines_from_hunks, compute_word_diff, get_line_highlights, DiffLine,
            should_pair_lines, WordSegment,
        },
        document::{render_document, LinePos, TextRenderer},
        picker::query::PickerQuery,
        text_decorations::DecorationManager,
        EditorView,
    },
};
use futures_util::future::BoxFuture;
use helix_event::AsyncHook;
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Nucleo};
use thiserror::Error;
use tokio::sync::mpsc::Sender;
use tui::{
    buffer::Buffer as Surface,
    layout::Constraint,
    text::{Span, Spans},
    widgets::{Block, BorderType, Cell, Row, Table},
};

use tui::widgets::Widget;

use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashMap,
    io::Read,
    path::Path,
    sync::{
        atomic::{self, AtomicUsize},
        Arc,
    },
};

use crate::ui::{Prompt, PromptEvent};
use helix_core::{
    char_idx_at_visual_offset, fuzzy::MATCHER, movement::Direction,
    text_annotations::TextAnnotations, unicode::segmentation::UnicodeSegmentation, Position,
    Rope,
};
use helix_view::{
    editor::Action,
    graphics::{CursorKind, Margin, Modifier, Rect},
    theme::Style,
    view::ViewPosition,
    Document, DocumentId, Editor,
};
use helix_vcs::Hunk;

use self::handlers::{DynamicQueryChange, DynamicQueryHandler, PreviewHighlightHandler};

pub const ID: &str = "picker";

pub const MIN_AREA_WIDTH_FOR_PREVIEW: u16 = 72;
/// Biggest file size to preview in bytes
pub const MAX_FILE_SIZE_FOR_PREVIEW: u64 = 10 * 1024 * 1024;

#[derive(PartialEq, Eq, Hash)]
pub enum PathOrId<'a> {
    Id(DocumentId),
    Path(&'a Path),
}

impl<'a> From<&'a Path> for PathOrId<'a> {
    fn from(path: &'a Path) -> Self {
        Self::Path(path)
    }
}

impl From<DocumentId> for PathOrId<'_> {
    fn from(v: DocumentId) -> Self {
        Self::Id(v)
    }
}

type FileCallback<T> = Box<dyn for<'a> Fn(&'a Editor, &'a T) -> Option<FileLocation<'a>>>;

/// Callback for custom preview content. Returns a CachedPreview directly.
type CustomPreviewCallback<T> = Box<dyn for<'a> Fn(&'a Editor, &'a T) -> Option<CachedPreview>>;

/// File path and range of lines (used to align and highlight lines)
pub type FileLocation<'a> = (PathOrId<'a>, Option<(usize, usize)>);

/// A file entry in a file list preview (for git commit/stash previews)
#[derive(Clone, Debug)]
pub struct FileListEntry {
    /// Status code: M (modified), A (added), D (deleted), R (renamed), C (copied), etc.
    pub status: String,
    /// File path
    pub path: String,
    /// Number of lines added (0 for deleted files)
    pub additions: usize,
    /// Number of lines deleted (0 for new files)
    pub deletions: usize,
}

pub enum CachedPreview {
    Document(Box<Document>),
    Directory(Vec<(String, bool)>),
    Binary,
    LargeFile,
    NotFound,
    /// Custom text content with optional styling (content, is_diff)
    /// When is_diff is true, the content will be rendered with diff highlighting
    CustomText { content: String, is_diff: bool },
    /// Diff content with full DiffView rendering (syntax highlighting, word-level diff, etc.)
    Diff {
        /// Original content (from git HEAD/index)
        diff_base: helix_core::Rope,
        /// Current content (working copy)
        doc: helix_core::Rope,
        /// Diff hunks
        hunks: Vec<helix_vcs::Hunk>,
        /// File path for syntax highlighting
        file_path: std::path::PathBuf,
        /// File name for display
        file_name: String,
    },
    /// A list of files with status and line stats (for git commit/stash previews)
    /// Rendered with color-coded status indicators
    /// Optional commit info header: hash, author, date, subject
    FileList {
        files: Vec<FileListEntry>,
        /// Commit hash (short form)
        commit_hash: Option<String>,
        /// Author name
        author: Option<String>,
        /// Date string (e.g., "2024-01-15")
        date: Option<String>,
        /// Relative date (e.g., "2 days ago")
        relative_date: Option<String>,
        /// Commit subject
        subject: Option<String>,
    },
}

// We don't store this enum in the cache so as to avoid lifetime constraints
// from borrowing a document already opened in the editor.
pub enum Preview<'picker, 'editor> {
    Cached(&'picker CachedPreview),
    EditorDocument(&'editor Document),
}

impl Preview<'_, '_> {
    fn document(&self) -> Option<&Document> {
        match self {
            Preview::EditorDocument(doc) => Some(doc),
            Preview::Cached(CachedPreview::Document(doc)) => Some(doc),
            _ => None,
        }
    }

    fn dir_content(&self) -> Option<&Vec<(String, bool)>> {
        match self {
            Preview::Cached(CachedPreview::Directory(dir_content)) => Some(dir_content),
            _ => None,
        }
    }

    /// Alternate text to show for the preview.
    fn placeholder(&self) -> &str {
        match *self {
            Self::EditorDocument(_) => "<Invalid file location>",
            Self::Cached(preview) => match preview {
                CachedPreview::Document(_) => "<Invalid file location>",
                CachedPreview::Directory(_) => "<Invalid directory location>",
                CachedPreview::Binary => "<Binary file>",
                CachedPreview::LargeFile => "<File too large to preview>",
                CachedPreview::NotFound => "<File not found>",
                CachedPreview::CustomText { .. } => "<No content>",
                CachedPreview::Diff { .. } => "<No diff>",
                CachedPreview::FileList { .. } => "<No files>",
            },
        }
    }
}

fn inject_nucleo_item<T, D>(
    injector: &nucleo::Injector<T>,
    columns: &[Column<T, D>],
    item: T,
    editor_data: &D,
) {
    injector.push(item, |item, dst| {
        for (column, text) in columns.iter().filter(|column| column.filter).zip(dst) {
            *text = column.format_text(item, editor_data).into()
        }
    });
}

pub struct Injector<T, D> {
    dst: nucleo::Injector<T>,
    columns: Arc<[Column<T, D>]>,
    editor_data: Arc<D>,
    version: usize,
    picker_version: Arc<AtomicUsize>,
    /// A marker that requests a redraw when the injector drops.
    /// This marker causes the "running" indicator to disappear when a background job
    /// providing items is finished and drops. This could be wrapped in an [Arc] to ensure
    /// that the redraw is only requested when all Injectors drop for a Picker (which removes
    /// the "running" indicator) but the redraw handle is debounced so this is unnecessary.
    _redraw: helix_event::RequestRedrawOnDrop,
}

impl<I, D> Clone for Injector<I, D> {
    fn clone(&self) -> Self {
        Injector {
            dst: self.dst.clone(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version,
            picker_version: self.picker_version.clone(),
            _redraw: helix_event::RequestRedrawOnDrop,
        }
    }
}

#[derive(Error, Debug)]
#[error("picker has been shut down")]
pub struct InjectorShutdown;

impl<T, D> Injector<T, D> {
    pub fn push(&self, item: T) -> Result<(), InjectorShutdown> {
        if self.version != self.picker_version.load(atomic::Ordering::Relaxed) {
            return Err(InjectorShutdown);
        }

        inject_nucleo_item(&self.dst, &self.columns, item, &self.editor_data);
        Ok(())
    }
}

type ColumnFormatFn<T, D> = for<'a> fn(&'a T, &'a D) -> Cell<'a>;

pub struct Column<T, D> {
    name: Arc<str>,
    format: ColumnFormatFn<T, D>,
    /// Whether the column should be passed to nucleo for matching and filtering.
    /// `DynamicPicker` uses this so that the dynamic column (for example regex in
    /// global search) is not used for filtering twice.
    filter: bool,
    hidden: bool,
}

impl<T, D> Column<T, D> {
    pub fn new(name: impl Into<Arc<str>>, format: ColumnFormatFn<T, D>) -> Self {
        Self {
            name: name.into(),
            format,
            filter: true,
            hidden: false,
        }
    }

    /// A column which does not display any contents
    pub fn hidden(name: impl Into<Arc<str>>) -> Self {
        let format = |_: &T, _: &D| unreachable!();

        Self {
            name: name.into(),
            format,
            filter: false,
            hidden: true,
        }
    }

    pub fn without_filtering(mut self) -> Self {
        self.filter = false;
        self
    }

    fn format<'a>(&self, item: &'a T, data: &'a D) -> Cell<'a> {
        (self.format)(item, data)
    }

    fn format_text<'a>(&self, item: &'a T, data: &'a D) -> Cow<'a, str> {
        let text: String = self.format(item, data).content.into();
        text.into()
    }
}

/// Returns a new list of options to replace the contents of the picker
/// when called with the current picker query,
type DynQueryCallback<T, D> =
    fn(&str, &mut Editor, Arc<D>, &Injector<T, D>) -> BoxFuture<'static, anyhow::Result<()>>;

pub struct Picker<T: 'static + Send + Sync, D: 'static> {
    columns: Arc<[Column<T, D>]>,
    primary_column: usize,
    editor_data: Arc<D>,
    version: Arc<AtomicUsize>,
    matcher: Nucleo<T>,

    /// Current height of the completions box
    completion_height: u16,

    cursor: u32,
    prompt: Prompt,
    query: PickerQuery,

    /// Whether to show the preview panel (default true)
    show_preview: bool,
    /// Constraints for tabular formatting
    widths: Vec<Constraint>,

    callback_fn: PickerCallback<T>,
    default_action: Action,

    pub truncate_start: bool,
    /// Caches paths to documents
    preview_cache: HashMap<Arc<Path>, CachedPreview>,
    read_buffer: Vec<u8>,
    /// Given an item in the picker, return the file path and line number to display.
    file_fn: Option<FileCallback<T>>,
    /// Custom preview callback that returns CachedPreview directly.
    /// Takes precedence over file_fn if set.
    custom_preview_fn: Option<CustomPreviewCallback<T>>,
    /// An event handler for syntax highlighting the currently previewed file.
    preview_highlight_handler: Sender<Arc<Path>>,
    dynamic_query_handler: Option<Sender<DynamicQueryChange>>,
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> Picker<T, D> {
    pub fn stream(
        columns: impl IntoIterator<Item = Column<T, D>>,
        editor_data: D,
    ) -> (Nucleo<T>, Injector<T, D>) {
        let columns: Arc<[_]> = columns.into_iter().collect();
        let matcher_columns = columns.iter().filter(|col| col.filter).count() as u32;
        assert!(matcher_columns > 0);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new(helix_event::request_redraw),
            None,
            matcher_columns,
        );
        let streamer = Injector {
            dst: matcher.injector(),
            columns,
            editor_data: Arc::new(editor_data),
            version: 0,
            picker_version: Arc::new(AtomicUsize::new(0)),
            _redraw: helix_event::RequestRedrawOnDrop,
        };
        (matcher, streamer)
    }

    pub fn new<C, O, F>(
        columns: C,
        primary_column: usize,
        options: O,
        editor_data: D,
        callback_fn: F,
    ) -> Self
    where
        C: IntoIterator<Item = Column<T, D>>,
        O: IntoIterator<Item = T>,
        F: Fn(&mut Context, &T, Action) + 'static,
    {
        let columns: Arc<[_]> = columns.into_iter().collect();
        let matcher_columns = columns
            .iter()
            .filter(|col: &&Column<T, D>| col.filter)
            .count() as u32;
        assert!(matcher_columns > 0);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new(helix_event::request_redraw),
            None,
            matcher_columns,
        );
        let injector = matcher.injector();
        for item in options {
            inject_nucleo_item(&injector, &columns, item, &editor_data);
        }
        Self::with(
            matcher,
            columns,
            primary_column,
            Arc::new(editor_data),
            Arc::new(AtomicUsize::new(0)),
            callback_fn,
        )
    }

    pub fn with_stream(
        matcher: Nucleo<T>,
        primary_column: usize,
        injector: Injector<T, D>,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        Self::with(
            matcher,
            injector.columns,
            primary_column,
            injector.editor_data,
            injector.picker_version,
            callback_fn,
        )
    }

    fn with(
        matcher: Nucleo<T>,
        columns: Arc<[Column<T, D>]>,
        default_column: usize,
        editor_data: Arc<D>,
        version: Arc<AtomicUsize>,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        assert!(!columns.is_empty());

        let prompt = Prompt::new(
            "".into(),
            None,
            ui::completers::none,
            |_editor: &mut Context, _pattern: &str, _event: PromptEvent| {},
        );

        let widths = columns
            .iter()
            .map(|column| Constraint::Length(column.name.chars().count() as u16))
            .collect();

        let query = PickerQuery::new(columns.iter().map(|col| &col.name).cloned(), default_column);

        Self {
            columns,
            primary_column: default_column,
            matcher,
            editor_data,
            version,
            cursor: 0,
            prompt,
            query,
            truncate_start: true,
            show_preview: true,
            callback_fn: Box::new(callback_fn),
            default_action: Action::Replace,
            completion_height: 0,
            widths,
            preview_cache: HashMap::new(),
            read_buffer: Vec::with_capacity(1024),
            file_fn: None,
            custom_preview_fn: None,
            preview_highlight_handler: PreviewHighlightHandler::<T, D>::default().spawn(),
            dynamic_query_handler: None,
        }
    }

    pub fn injector(&self) -> Injector<T, D> {
        Injector {
            dst: self.matcher.injector(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version.load(atomic::Ordering::Relaxed),
            picker_version: self.version.clone(),
            _redraw: helix_event::RequestRedrawOnDrop,
        }
    }

    /// Clear all items from the picker. This increments the version
    /// and restarts the matcher, invalidating any existing injectors.
    pub fn clear(&mut self) {
        self.version.fetch_add(1, atomic::Ordering::Relaxed);
        self.matcher.restart(false);
    }

    pub fn truncate_start(mut self, truncate_start: bool) -> Self {
        self.truncate_start = truncate_start;
        self
    }

    pub fn with_preview(
        mut self,
        preview_fn: impl for<'a> Fn(&'a Editor, &'a T) -> Option<FileLocation<'a>> + 'static,
    ) -> Self {
        self.file_fn = Some(Box::new(preview_fn));
        // assumption: if we have a preview we are matching paths... If this is ever
        // not true this could be a separate builder function
        self.matcher.update_config(Config::DEFAULT.match_paths());
        self
    }

    /// Set a custom preview callback that returns CachedPreview directly.
    /// This takes precedence over with_preview if both are set.
    /// The callback receives the editor and the current item, and returns
    /// an Optional CachedPreview. Use CachedPreview::CustomText for diff content.
    pub fn with_custom_preview(
        mut self,
        preview_fn: impl for<'a> Fn(&'a Editor, &'a T) -> Option<CachedPreview> + 'static,
    ) -> Self {
        self.custom_preview_fn = Some(Box::new(preview_fn));
        self.show_preview = true;
        self
    }

    pub fn with_history_register(mut self, history_register: Option<char>) -> Self {
        self.prompt.with_history_register(history_register);
        self
    }

    pub fn with_initial_cursor(mut self, cursor: u32) -> Self {
        self.cursor = cursor;
        self
    }

    pub fn with_dynamic_query(
        mut self,
        callback: DynQueryCallback<T, D>,
        debounce_ms: Option<u64>,
    ) -> Self {
        let handler = DynamicQueryHandler::new(callback, debounce_ms).spawn();
        let event = DynamicQueryChange {
            query: self.primary_query(),
            // Treat the initial query as a paste.
            is_paste: true,
        };
        helix_event::send_blocking(&handler, event);
        self.dynamic_query_handler = Some(handler);
        self
    }

    pub fn with_default_action(mut self, action: Action) -> Self {
        self.default_action = action;
        self
    }

    /// Move the cursor by a number of lines, either down (`Forward`) or up (`Backward`)
    pub fn move_by(&mut self, amount: u32, direction: Direction) {
        let len = self.matcher.snapshot().matched_item_count();

        if len == 0 {
            // No results, can't move.
            return;
        }

        match direction {
            Direction::Forward => {
                self.cursor = self.cursor.saturating_add(amount) % len;
            }
            Direction::Backward => {
                self.cursor = self.cursor.saturating_add(len).saturating_sub(amount) % len;
            }
        }
    }

    /// Move the cursor down by exactly one page. After the last page comes the first page.
    pub fn page_up(&mut self) {
        self.move_by(self.completion_height as u32, Direction::Backward);
    }

    /// Move the cursor up by exactly one page. After the first page comes the last page.
    pub fn page_down(&mut self) {
        self.move_by(self.completion_height as u32, Direction::Forward);
    }

    /// Move the cursor to the first entry
    pub fn to_start(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the last entry
    pub fn to_end(&mut self) {
        self.cursor = self
            .matcher
            .snapshot()
            .matched_item_count()
            .saturating_sub(1);
    }

    pub fn selection(&self) -> Option<&T> {
        self.matcher
            .snapshot()
            .get_matched_item(self.cursor)
            .map(|item| item.data)
    }

    /// Force the matcher to process pending items. Useful for testing.
    #[cfg(test)]
    pub fn tick(&mut self) {
        self.matcher.tick(100);
    }

    fn primary_query(&self) -> Arc<str> {
        self.query
            .get(&self.columns[self.primary_column].name)
            .cloned()
            .unwrap_or_else(|| "".into())
    }

    fn header_height(&self) -> u16 {
        if self.columns.len() > 1 {
            1
        } else {
            0
        }
    }

    pub fn toggle_preview(&mut self) {
        self.show_preview = !self.show_preview;
    }

    fn prompt_handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if let EventResult::Consumed(_) = self.prompt.handle_event(event, cx) {
            self.handle_prompt_change(matches!(event, Event::Paste(_)));
        }
        EventResult::Consumed(None)
    }

    fn handle_prompt_change(&mut self, is_paste: bool) {
        // TODO: better track how the pattern has changed
        let line = self.prompt.line();
        let old_query = self.query.parse(line);
        if self.query == old_query {
            return;
        }
        // If the query has meaningfully changed, reset the cursor to the top of the results.
        self.cursor = 0;
        // Have nucleo reparse each changed column.
        for (i, column) in self
            .columns
            .iter()
            .filter(|column| column.filter)
            .enumerate()
        {
            let pattern = self
                .query
                .get(&column.name)
                .map(|f| &**f)
                .unwrap_or_default();
            let old_pattern = old_query
                .get(&column.name)
                .map(|f| &**f)
                .unwrap_or_default();
            // Fastlane: most columns will remain unchanged after each edit.
            if pattern == old_pattern {
                continue;
            }
            let is_append = pattern.starts_with(old_pattern);
            self.matcher.pattern.reparse(
                i,
                pattern,
                CaseMatching::Smart,
                Normalization::Smart,
                is_append,
            );
        }
        // If this is a dynamic picker, notify the query hook that the primary
        // query might have been updated.
        if let Some(handler) = &self.dynamic_query_handler {
            let event = DynamicQueryChange {
                query: self.primary_query(),
                is_paste,
            };
            helix_event::send_blocking(handler, event);
        }
    }

    /// Get (cached) preview for the currently selected item. If a document corresponding
    /// to the path is already open in the editor, it is used instead.
    fn get_preview<'picker, 'editor>(
        &'picker mut self,
        editor: &'editor Editor,
    ) -> Option<(Preview<'picker, 'editor>, Option<(usize, usize)>)> {
        let current = self.selection()?;

        // Check for custom preview callback first
        if let Some(custom_fn) = &self.custom_preview_fn {
            if let Some(preview) = custom_fn(editor, current) {
                // Store in cache temporarily for rendering
                // Use a special key that won't conflict with real paths
                let cache_key: Arc<Path> = Arc::from(Path::new("__custom_preview__"));
                self.preview_cache.insert(cache_key.clone(), preview);
                return Some((Preview::Cached(&self.preview_cache[&cache_key]), None));
            }
        }

        let (path_or_id, range) = (self.file_fn.as_ref()?)(editor, current)?;

        match path_or_id {
            PathOrId::Path(path) => {
                if let Some(doc) = editor.document_by_path(path) {
                    return Some((Preview::EditorDocument(doc), range));
                }

                if self.preview_cache.contains_key(path) {
                    // NOTE: we use `HashMap::get_key_value` here instead of indexing so we can
                    // retrieve the `Arc<Path>` key. The `path` in scope here is a `&Path` and
                    // we can cheaply clone the key for the preview highlight handler.
                    let (path, preview) = self.preview_cache.get_key_value(path).unwrap();
                    if matches!(preview, CachedPreview::Document(doc) if doc.syntax().is_none()) {
                        helix_event::send_blocking(&self.preview_highlight_handler, path.clone());
                    }
                    return Some((Preview::Cached(preview), range));
                }

                let path: Arc<Path> = path.into();
                let preview = std::fs::metadata(&path)
                    .and_then(|metadata| {
                        if metadata.is_dir() {
                            let files = super::directory_content(&path, editor)?;
                            let file_names: Vec<_> = files
                                .iter()
                                .filter_map(|(file_path, is_dir)| {
                                    let name = file_path
                                        .strip_prefix(&path)
                                        .map(|p| Some(p.as_os_str()))
                                        .unwrap_or_else(|_| file_path.file_name())?
                                        .to_string_lossy();
                                    if *is_dir {
                                        Some((format!("{}/", name), true))
                                    } else {
                                        Some((name.into_owned(), false))
                                    }
                                })
                                .collect();
                            Ok(CachedPreview::Directory(file_names))
                        } else if metadata.is_file() {
                            if metadata.len() > MAX_FILE_SIZE_FOR_PREVIEW {
                                return Ok(CachedPreview::LargeFile);
                            }
                            let content_type = std::fs::File::open(&path).and_then(|file| {
                                // Read up to 1kb to detect the content type
                                let n = file.take(1024).read_to_end(&mut self.read_buffer)?;
                                let content_type =
                                    content_inspector::inspect(&self.read_buffer[..n]);
                                self.read_buffer.clear();
                                Ok(content_type)
                            })?;
                            if content_type.is_binary() {
                                return Ok(CachedPreview::Binary);
                            }
                            let mut doc = Document::open(
                                &path,
                                None,
                                false,
                                editor.config.clone(),
                                editor.syn_loader.clone(),
                            )
                            .or(Err(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "Cannot open document",
                            )))?;
                            let loader = editor.syn_loader.load();
                            if let Some(language_config) = doc.detect_language_config(&loader) {
                                doc.language = Some(language_config);
                                // Asynchronously highlight the new document
                                helix_event::send_blocking(
                                    &self.preview_highlight_handler,
                                    path.clone(),
                                );
                            }
                            Ok(CachedPreview::Document(Box::new(doc)))
                        } else {
                            Err(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "Neither a dir, nor a file",
                            ))
                        }
                    })
                    .unwrap_or(CachedPreview::NotFound);
                self.preview_cache.insert(path.clone(), preview);
                Some((Preview::Cached(&self.preview_cache[&path]), range))
            }
            PathOrId::Id(id) => {
                let doc = editor.documents.get(&id).unwrap();
                Some((Preview::EditorDocument(doc), range))
            }
        }
    }

    fn render_picker(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let status = self.matcher.tick(10);
        let snapshot = self.matcher.snapshot();
        if status.changed {
            self.cursor = self
                .cursor
                .min(snapshot.matched_item_count().saturating_sub(1))
        }

        let text_style = cx.editor.theme.get("ui.text");
        let selected = cx.editor.theme.get("ui.text.focus");
        let highlight_style = cx.editor.theme.get("special").add_modifier(Modifier::BOLD);

        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        surface.clear_with(area, background);

        const BLOCK: Block<'_> = Block::bordered();

        // calculate the inner area inside the box
        let inner = BLOCK.inner(area);

        BLOCK.render(area, surface);

        // -- Render the input bar:

        let count = format!(
            "{}{}/{}",
            if status.running || self.matcher.active_injectors() > 0 {
                "(running) "
            } else {
                ""
            },
            snapshot.matched_item_count(),
            snapshot.item_count(),
        );

        let area = inner.clip_left(1).with_height(1);
        let line_area = area.clip_right(count.len() as u16 + 1);

        // render the prompt first since it will clear its background
        self.prompt.render(line_area, surface, cx);

        surface.set_stringn(
            (area.x + area.width).saturating_sub(count.len() as u16 + 1),
            area.y,
            &count,
            (count.len()).min(area.width as usize),
            text_style,
        );

        // -- Separator
        let sep_style = cx.editor.theme.get("ui.background.separator");
        let borders = BorderType::line_symbols(BorderType::Plain);
        for x in inner.left()..inner.right() {
            if let Some(cell) = surface.get_mut(x, inner.y + 1) {
                cell.set_symbol(borders.horizontal).set_style(sep_style);
            }
        }

        // -- Render the contents:
        // subtract area of prompt from top
        let inner = inner.clip_top(2);
        let rows = inner.height.saturating_sub(self.header_height()) as u32;
        let offset = self.cursor - (self.cursor % std::cmp::max(1, rows));
        let cursor = self.cursor.saturating_sub(offset);
        let end = offset
            .saturating_add(rows)
            .min(snapshot.matched_item_count());
        let mut indices = Vec::new();
        let mut matcher = MATCHER.lock();
        matcher.config = Config::DEFAULT;
        if self.file_fn.is_some() || self.custom_preview_fn.is_some() {
            matcher.config.set_match_paths()
        }

        let options = snapshot.matched_items(offset..end).map(|item| {
            let mut widths = self.widths.iter_mut();
            let mut matcher_index = 0;

            Row::new(self.columns.iter().map(|column| {
                if column.hidden {
                    return Cell::default();
                }

                let Some(Constraint::Length(max_width)) = widths.next() else {
                    unreachable!();
                };
                let mut cell = column.format(item.data, &self.editor_data);
                let width = if column.filter {
                    snapshot.pattern().column_pattern(matcher_index).indices(
                        item.matcher_columns[matcher_index].slice(..),
                        &mut matcher,
                        &mut indices,
                    );
                    indices.sort_unstable();
                    indices.dedup();
                    let mut indices = indices.drain(..);
                    let mut next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                    let mut span_list = Vec::new();
                    let mut current_span = String::new();
                    let mut current_style = Style::default();
                    let mut grapheme_idx = 0u32;
                    let mut width = 0;

                    let spans: &[Span] =
                        cell.content.lines.first().map_or(&[], |it| it.0.as_slice());
                    for span in spans {
                        // this looks like a bug on first glance, we are iterating
                        // graphemes but treating them as char indices. The reason that
                        // this is correct is that nucleo will only ever consider the first char
                        // of a grapheme (and discard the rest of the grapheme) so the indices
                        // returned by nucleo are essentially grapheme indecies
                        for grapheme in span.content.graphemes(true) {
                            let style = if grapheme_idx == next_highlight_idx {
                                next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                                span.style.patch(highlight_style)
                            } else {
                                span.style
                            };
                            if style != current_style {
                                if !current_span.is_empty() {
                                    span_list.push(Span::styled(current_span, current_style))
                                }
                                current_span = String::new();
                                current_style = style;
                            }
                            current_span.push_str(grapheme);
                            grapheme_idx += 1;
                        }
                        width += span.width();
                    }

                    span_list.push(Span::styled(current_span, current_style));
                    cell = Cell::from(Spans::from(span_list));
                    matcher_index += 1;
                    width
                } else {
                    cell.content
                        .lines
                        .first()
                        .map(|line| line.width())
                        .unwrap_or_default()
                };

                if width as u16 > *max_width {
                    *max_width = width as u16;
                }

                cell
            }))
        });

        let mut table = Table::new(options)
            .style(text_style)
            .highlight_style(selected)
            .highlight_symbol(" > ")
            .column_spacing(1)
            .widths(&self.widths);

        // -- Header
        if self.columns.len() > 1 {
            let active_column = self.query.active_column(self.prompt.position());
            let header_style = cx.editor.theme.get("ui.picker.header");
            let header_column_style = cx.editor.theme.get("ui.picker.header.column");

            table = table.header(
                Row::new(self.columns.iter().map(|column| {
                    if column.hidden {
                        Cell::default()
                    } else {
                        let style =
                            if active_column.is_some_and(|name| Arc::ptr_eq(name, &column.name)) {
                                cx.editor.theme.get("ui.picker.header.column.active")
                            } else {
                                header_column_style
                            };

                        Cell::from(Span::styled(Cow::from(&*column.name), style))
                    }
                }))
                .style(header_style),
            );
        }

        use tui::widgets::TableState;

        table.render_table(
            inner,
            surface,
            &mut TableState {
                offset: 0,
                selected: Some(cursor as usize),
            },
            self.truncate_start,
        );
    }

    /// Render custom text content in the preview pane.
    /// If is_diff is true, applies diff-specific styling with proper unified diff parsing:
    /// - File headers (---, +++, diff --git): magenta
    /// - Hunk headers (@@): cyan
    /// - Additions (+): green
    /// - Deletions (-): red
    /// - Context lines: dimmed
    fn render_custom_text_preview_static(
        inner: &Rect,
        surface: &mut Surface,
        cx: &mut Context,
        content: &str,
        is_diff: bool,
    ) {
        use helix_view::graphics::Modifier;

        let text = cx.editor.theme.get("ui.text");
        let diff_plus = cx.editor.theme.get("diff.plus");
        let diff_minus = cx.editor.theme.get("diff.minus");
        let diff_delta = cx.editor.theme.get("diff.delta");

        // Style for file headers and diff metadata (magenta/purple)
        // Try diff.delta.moved first, fall back to diff.delta with italic modifier
        let diff_file_header = {
            let moved_style = cx.editor.theme.get("diff.delta.moved");
            if moved_style.fg.is_some() || moved_style.bg.is_some() {
                moved_style
            } else {
                // Fallback: use diff.delta with italic modifier to distinguish from hunk headers
                diff_delta.add_modifier(Modifier::ITALIC)
            }
        };

        // Dimmed style for context lines and metadata
        let diff_context = {
            let style = cx.editor.theme.get("diff.delta");
            if style.fg.is_none() {
                // Fallback to dimmed text
                text.add_modifier(Modifier::DIM)
            } else {
                style
            }
        };

        // Render each line with appropriate styling
        for (i, line) in content.lines().take(inner.height as usize).enumerate() {
            let style = if is_diff {
                // Use the categorize_diff_line function for consistent styling
                match categorize_diff_line(line) {
                    DiffLineCategory::DiffHeader => diff_file_header,
                    DiffLineCategory::IndexLine => diff_context,
                    DiffLineCategory::FileHeader => diff_file_header,
                    DiffLineCategory::HunkHeader => diff_delta,
                    DiffLineCategory::Addition => diff_plus,
                    DiffLineCategory::Deletion => diff_minus,
                    DiffLineCategory::BinaryNotice => diff_context,
                    DiffLineCategory::FileModeChange => diff_context,
                    DiffLineCategory::RenameMetadata => diff_file_header,
                    DiffLineCategory::SimilarityIndex => diff_context,
                    DiffLineCategory::Context => diff_context,
                    DiffLineCategory::Unknown => text,
                }
            } else {
                text
            };

            surface.set_stringn(
                inner.x,
                inner.y + i as u16,
                line,
                inner.width as usize,
                style,
            );
        }
    }

    /// Render a file list preview with color-coded status indicators.
    /// Format: "M  path/to/file.rs  +45 -12"
    /// Colors: M=yellow, A=green, D=red, R=cyan, stats=dimmed
    /// Optional commit info header shows: hash, author, date, subject
    fn render_file_list_preview(
        inner: &Rect,
        surface: &mut Surface,
        cx: &mut Context,
        files: &[FileListEntry],
        commit_hash: Option<&str>,
        author: Option<&str>,
        date: Option<&str>,
        relative_date: Option<&str>,
        subject: Option<&str>,
    ) {
        use helix_view::graphics::Modifier;

        let text = cx.editor.theme.get("ui.text");
        let diff_plus = cx.editor.theme.get("diff.plus");
        let diff_minus = cx.editor.theme.get("diff.minus");
        let diff_delta = cx.editor.theme.get("diff.delta");

        // Calculate lines used by commit header
        let mut header_lines: u16 = 0;
        let has_commit_info = commit_hash.is_some()
            || author.is_some()
            || date.is_some()
            || subject.is_some();

        // Render commit info header if present
        if has_commit_info {
            let dimmed = text.add_modifier(Modifier::DIM);
            let yellow = cx.editor.theme.get("string"); // Use string style for yellow-ish color
            let cyan = cx.editor.theme.get("info"); // Use info style for cyan-ish color

            let mut y = inner.y;
            let mut x = inner.x;

            // Line 1: commit <hash>
            if let Some(hash) = commit_hash {
                surface.set_stringn(x, y, "commit ", inner.width as usize, dimmed);
                x += 7;
                let hash_display = if hash.len() > 7 { &hash[..7] } else { hash };
                surface.set_stringn(x, y, hash_display, inner.width.saturating_sub(7) as usize, yellow);
                y += 1;
                header_lines += 1;
                x = inner.x;
            }

            // Line 2: Author: <name>
            if let Some(author_name) = author {
                surface.set_stringn(x, y, "Author: ", inner.width as usize, dimmed);
                x += 8;
                surface.set_stringn(x, y, author_name, inner.width.saturating_sub(8) as usize, text);
                y += 1;
                header_lines += 1;
                x = inner.x;
            }

            // Line 3: Date: <date> (<relative>)
            if let Some(d) = date {
                surface.set_stringn(x, y, "Date:   ", inner.width as usize, dimmed);
                x += 8;
                let date_display = if let Some(rel) = relative_date {
                    format!("{} ({})", d, rel)
                } else {
                    d.to_string()
                };
                surface.set_stringn(x, y, &date_display, inner.width.saturating_sub(8) as usize, cyan);
                y += 1;
                header_lines += 1;
                x = inner.x;
            }

            // Line 4: <subject> (indented)
            if let Some(subj) = subject {
                let indented = format!("    {}", subj);
                surface.set_stringn(x, y, &indented, inner.width as usize, text);
                y += 1;
                header_lines += 1;
            }

            // Line 5: separator line
            let separator = "─".repeat(inner.width as usize);
            surface.set_stringn(inner.x, y, &separator, inner.width as usize, dimmed);
            header_lines += 1;
        }

        // Status colors based on git status codes
        // M (modified) - yellow/delta
        // A (added) - green/plus
        // D (deleted) - red/minus
        // R (renamed) - cyan (use delta with different shade)
        // C (copied) - cyan
        // ? (untracked) - dimmed
        let style_modified = diff_delta;
        let style_added = diff_plus;
        let style_deleted = diff_minus;
        let style_renamed = {
            // Try to use a cyan-like color for renames
            let style = cx.editor.theme.get("ui.virtual.whitespace");
            if style.fg.is_some() {
                style
            } else {
                diff_delta
            }
        };
        let style_stats = text.add_modifier(Modifier::DIM);

        // Calculate available height for files
        let available_height = inner.height.saturating_sub(header_lines) as usize;

        // Render each file entry
        let start_y = inner.y + header_lines;
        for (i, entry) in files.iter().take(available_height).enumerate() {
            let y = start_y + i as u16;
            let mut x = inner.x;

            // Determine status style based on the first character of status
            let status_char = entry.status.chars().next().unwrap_or(' ');
            let status_style = match status_char {
                'M' => style_modified,
                'A' => style_added,
                'D' => style_deleted,
                'R' | 'C' => style_renamed,
                _ => text,
            };

            // Render status (e.g., "M ", "A ", "D ", "R ")
            let status_str = if entry.status.len() == 1 {
                format!("{} ", entry.status)
            } else {
                format!("{} ", entry.status)
            };
            surface.set_stringn(x, y, &status_str, inner.width as usize, status_style);
            x += status_str.len() as u16;

            // Calculate remaining width for path and stats
            let remaining_width = inner.width.saturating_sub(x - inner.x);

            // Render path
            let path_display = if entry.path.len() > remaining_width as usize {
                // Truncate path if too long, showing the end (filename is more important)
                let truncated = format!("...{}", &entry.path[entry.path.len().saturating_sub(remaining_width as usize - 3)..]);
                truncated
            } else {
                entry.path.clone()
            };

            let path_width = path_display.len().min(remaining_width as usize);
            surface.set_stringn(x, y, &path_display, path_width, text);
            x += path_width as u16;

            // Render stats if there's room and we have them
            let stats = if entry.additions > 0 || entry.deletions > 0 {
                let add_str = if entry.additions > 0 {
                    format!("+{}", entry.additions)
                } else {
                    String::new()
                };
                let del_str = if entry.deletions > 0 {
                    format!("-{}", entry.deletions)
                } else {
                    String::new()
                };
                if !add_str.is_empty() && !del_str.is_empty() {
                    format!("  {} {}", add_str, del_str)
                } else if !add_str.is_empty() {
                    format!("  {}", add_str)
                } else if !del_str.is_empty() {
                    format!("  {}", del_str)
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            if !stats.is_empty() {
                let remaining = inner.width.saturating_sub(x - inner.x);
                if remaining > stats.len() as u16 {
                    surface.set_stringn(x, y, &stats, remaining as usize, style_stats);
                }
            }
        }

        // Show file count at the bottom if there are more files than displayed
        if files.len() > available_height {
            let remaining = files.len() - available_height;
            let msg = format!("... and {} more file(s)", remaining);
            let y = inner.y + inner.height - 1;
            let style = text.add_modifier(Modifier::DIM);
            surface.set_stringn(inner.x, y, &msg, inner.width as usize, style);
        }
    }

    /// Render diff content with syntax highlighting and word-level diff.
    /// This is a simplified version of DiffView's rendering for the preview pane.
    fn render_diff_preview(
        inner: &Rect,
        surface: &mut Surface,
        cx: &mut Context,
        diff_base: &Rope,
        doc: &Rope,
        hunks: &[Hunk],
        file_path: &Path,
        file_name: &str,
    ) {
        use helix_core::syntax::Syntax;
        use helix_core::unicode::width::UnicodeWidthStr;
        use helix_view::graphics::{Modifier, UnderlineStyle};

        // Get styles from theme
        let style_plus = cx.editor.theme.get("diff.plus");
        let style_minus = cx.editor.theme.get("diff.minus");
        let style_delta = cx.editor.theme.get("diff.delta");
        let style_header = cx.editor.theme.get("ui.popup.info");

        // Add background colors if theme doesn't provide them
        let style_plus = if style_plus.bg.is_none() {
            style_plus.patch(helix_view::graphics::Style {
                bg: Some(helix_view::graphics::Color::Rgb(40, 80, 40)),
                ..Default::default()
            })
        } else {
            style_plus
        };

        let style_minus = if style_minus.bg.is_none() {
            style_minus.patch(helix_view::graphics::Style {
                bg: Some(helix_view::graphics::Color::Rgb(80, 40, 40)),
                ..Default::default()
            })
        } else {
            style_minus
        };

        // Context line style
        let style_context = {
            let theme_style = cx.editor.theme.get("diff.delta");
            if theme_style.fg.is_none() && theme_style.bg.is_none() {
                helix_view::graphics::Style {
                    fg: Some(helix_view::graphics::Color::Rgb(108, 108, 108)),
                    ..Default::default()
                }
            } else {
                theme_style
            }
        };

        // Word emphasis styles
        let style_minus_emph = style_minus.patch(helix_view::graphics::Style {
            bg: Some(helix_view::graphics::Color::Rgb(140, 40, 40)),
            underline_style: Some(UnderlineStyle::Line),
            add_modifier: Modifier::BOLD | style_minus.add_modifier,
            ..Default::default()
        });
        let style_plus_emph = style_plus.patch(helix_view::graphics::Style {
            bg: Some(helix_view::graphics::Color::Rgb(40, 140, 40)),
            underline_style: Some(UnderlineStyle::Line),
            add_modifier: Modifier::BOLD | style_plus.add_modifier,
            ..Default::default()
        });

        // Get syntax loader and theme
        let loader = cx.editor.syn_loader.load();
        let theme = &cx.editor.theme;

        // Initialize syntax for both doc and diff_base
        let doc_syntax: Option<Arc<Syntax>> = {
            let doc_slice = doc.slice(..);
            loader.language_for_filename(file_path).and_then(|lang| {
                Syntax::new(doc_slice, lang, &loader).ok().map(Arc::new)
            })
        };

        let base_syntax: Option<Arc<Syntax>> = {
            let base_slice = diff_base.slice(..);
            loader.language_for_filename(file_path).and_then(|lang| {
                Syntax::new(base_slice, lang, &loader).ok().map(Arc::new)
            })
        };

        let doc_syntax_ref = doc_syntax.as_ref().map(|arc| arc.as_ref());
        let base_syntax_ref = base_syntax.as_ref().map(|arc| arc.as_ref());

        // Compute diff lines using shared function
        let (diff_lines, hunk_boundaries) = compute_diff_lines_from_hunks(diff_base, doc, hunks);

        // Caches for word diffs and syntax highlights
        let word_diff_cache: RefCell<HashMap<usize, Vec<WordSegment>>> = RefCell::new(HashMap::new());
        let syntax_highlight_cache: RefCell<HashMap<usize, Vec<(usize, usize, helix_view::graphics::Style)>>> = 
            RefCell::new(HashMap::new());

        // Calculate visible range (account for title bar taking 1 line)
        let visible_start = 0;
        let visible_end = (inner.height as usize).saturating_sub(1).min(diff_lines.len());

        // Prepare word diffs for VISIBLE deletion/addition pairs ONLY
        // Use hunk boundaries for correct pairing (deletions and additions are paired within the same hunk)
        {
            let mut word_cache = word_diff_cache.borrow_mut();

            for hunk in &hunk_boundaries {
                // Only process hunks that overlap with the visible range
                if hunk.end < visible_start || hunk.start > visible_end {
                    continue;
                }

                // Collect all deletion and addition line indices within this hunk
                let mut deletion_indices: Vec<usize> = Vec::new();
                let mut addition_indices: Vec<usize> = Vec::new();

                for line_index in hunk.start..hunk.end {
                    match diff_lines.get(line_index) {
                        Some(DiffLine::Deletion { .. }) => deletion_indices.push(line_index),
                        Some(DiffLine::Addition { .. }) => addition_indices.push(line_index),
                        _ => {}
                    }
                }

                // Pair deletions with additions by index within this hunk
                for (del_idx, add_idx) in deletion_indices.iter().zip(addition_indices.iter()) {
                    // Only process if at least one of the lines is in the visible range
                    if *del_idx < visible_start && *add_idx < visible_start {
                        continue;
                    }
                    if *del_idx > visible_end && *add_idx > visible_end {
                        continue;
                    }

                    if let (
                        Some(DiffLine::Deletion { content: old_content, .. }),
                        Some(DiffLine::Addition { content: new_content, .. }),
                    ) = (diff_lines.get(*del_idx), diff_lines.get(*add_idx))
                    {
                        if should_pair_lines(old_content, new_content) {
                            let (old_segments, new_segments) = compute_word_diff(old_content, new_content);
                            word_cache.insert(*del_idx, old_segments);
                            word_cache.insert(*add_idx, new_segments);
                        }
                    }
                }
            }
        }

        // Prepare syntax highlights for visible lines only
        {
            let mut highlight_cache = syntax_highlight_cache.borrow_mut();
            for line_index in visible_start..visible_end {
                if let Some(diff_line) = diff_lines.get(line_index) {
                    let highlights = get_line_highlights(
                        diff_line,
                        doc,
                        diff_base,
                        doc_syntax_ref,
                        base_syntax_ref,
                        &loader,
                        theme,
                    );
                    highlight_cache.insert(line_index, highlights);
                }
            }
        }

        // Calculate stats for title
        let mut added: usize = 0;
        let mut removed: usize = 0;
        for hunk in hunks {
            added += (hunk.after.end.saturating_sub(hunk.after.start)) as usize;
            removed += (hunk.before.end.saturating_sub(hunk.before.start)) as usize;
        }

        // Render title bar
        let title = format!(" {}: +{} -{} ", file_name, added, removed);
        surface.set_stringn(inner.x, inner.y, title, inner.width as usize, style_header);

        // Render diff lines
        let mut y = inner.y + 1; // Start after title
        let max_y = inner.y + inner.height;

        for (line_index, diff_line) in diff_lines.iter().enumerate() {
            if y >= max_y {
                break;
            }

            // Get syntax highlights for this line
            let line_highlights = syntax_highlight_cache
                .borrow()
                .get(&line_index)
                .cloned()
                .unwrap_or_default();

            let spans = match diff_line {
                DiffLine::HunkHeader { text, .. } => {
                    // Simplified hunk header - just show the text
                    vec![Span::styled(text.clone(), style_delta)]
                }
                DiffLine::Context { base_line, doc_line, content } => {
                    let base_num = base_line
                        .map(|n| format!("{:>4}", n))
                        .unwrap_or_else(|| "    ".to_string());
                    let doc_num = doc_line
                        .map(|n| format!("{:>4}", n))
                        .unwrap_or_else(|| "    ".to_string());

                    let content_str = content.as_str();
                    let mut content_spans = Vec::new();

                    if line_highlights.is_empty() {
                        content_spans.push(Span::styled(content_str, style_context));
                    } else {
                        let mut last_end = 0;
                        for (byte_start, byte_end, segment_style) in &line_highlights {
                            let start = (*byte_start).min(content_str.len());
                            let end = (*byte_end).min(content_str.len());

                            if start > last_end {
                                let gap = &content_str[last_end..start];
                                if !gap.is_empty() {
                                    content_spans.push(Span::styled(gap, style_context));
                                }
                            }

                            if end > start {
                                let segment = &content_str[start..end];
                                if !segment.is_empty() {
                                    let mut patched_style = style_context.patch(*segment_style);
                                    if style_context.bg.is_some() {
                                        patched_style.bg = style_context.bg;
                                    }
                                    content_spans.push(Span::styled(segment, patched_style));
                                }
                            }

                            last_end = end;
                        }

                        if last_end < content_str.len() {
                            let trailing = &content_str[last_end..];
                            if !trailing.is_empty() {
                                content_spans.push(Span::styled(trailing, style_context));
                            }
                        }
                    }

                    let mut all_spans = vec![
                        Span::styled(base_num, style_context),
                        Span::styled(" ", style_context),
                        Span::styled(doc_num, style_context),
                        Span::styled("  │", style_context),
                        Span::styled(" ", style_context),
                    ];
                    all_spans.extend(content_spans);
                    all_spans
                }
                DiffLine::Deletion { base_line, content } => {
                    let line_num_str = format!("{:>4}", base_line);
                    let content_str = content.as_str();
                    let mut content_spans = Vec::new();

                    if let Some(word_segments) = word_diff_cache.borrow().get(&line_index) {
                        let word_segments = word_segments.clone();
                        if word_segments.is_empty() {
                            content_spans.push(Span::styled(content_str, style_minus));
                        } else {
                            let mut byte_offset = 0;
                            for segment in &word_segments {
                                let segment_text = &segment.text;
                                let segment_len = segment_text.len();
                                let base_style = if segment.is_emph { style_minus_emph } else { style_minus };

                                if line_highlights.is_empty() {
                                    content_spans.push(Span::styled(segment_text.clone(), base_style));
                                } else {
                                    let seg_start = byte_offset;
                                    let seg_end = byte_offset + segment_len;
                                    let mut last_pos = 0;

                                    for (hl_start, hl_end, hl_style) in &line_highlights {
                                        let start = (*hl_start).max(seg_start).min(seg_end) - seg_start;
                                        let end = (*hl_end).max(seg_start).min(seg_end) - seg_start;

                                        if start > last_pos && start < segment_len {
                                            let gap = &segment_text[last_pos..start];
                                            if !gap.is_empty() {
                                                content_spans.push(Span::styled(gap.to_string(), base_style));
                                            }
                                        }

                                        if end > start && start < segment_len {
                                            let text = &segment_text[start..end.min(segment_len)];
                                            if !text.is_empty() {
                                                let mut patched = base_style.patch(*hl_style);
                                                if base_style.bg.is_some() {
                                                    patched.bg = base_style.bg;
                                                }
                                                content_spans.push(Span::styled(text.to_string(), patched));
                                            }
                                        }
                                        last_pos = end.min(segment_len);
                                    }

                                    if last_pos < segment_len {
                                        let trailing = &segment_text[last_pos..];
                                        if !trailing.is_empty() {
                                            content_spans.push(Span::styled(trailing.to_string(), base_style));
                                        }
                                    }
                                }
                                byte_offset += segment_len;
                            }
                        }
                    } else if line_highlights.is_empty() {
                        content_spans.push(Span::styled(content_str, style_minus));
                    } else {
                        let mut last_end = 0;
                        for (byte_start, byte_end, segment_style) in &line_highlights {
                            let start = (*byte_start).min(content_str.len());
                            let end = (*byte_end).min(content_str.len());

                            if start > last_end {
                                let gap = &content_str[last_end..start];
                                if !gap.is_empty() {
                                    content_spans.push(Span::styled(gap, style_minus));
                                }
                            }

                            if end > start {
                                let segment = &content_str[start..end];
                                if !segment.is_empty() {
                                    let mut patched_style = style_minus.patch(*segment_style);
                                    if style_minus.bg.is_some() {
                                        patched_style.bg = style_minus.bg;
                                    }
                                    content_spans.push(Span::styled(segment, patched_style));
                                }
                            }
                            last_end = end;
                        }

                        if last_end < content_str.len() {
                            let trailing = &content_str[last_end..];
                            if !trailing.is_empty() {
                                content_spans.push(Span::styled(trailing, style_minus));
                            }
                        }
                    }

                    let mut all_spans = vec![
                        Span::styled("     ", style_minus),
                        Span::styled(line_num_str, style_minus),
                        Span::styled("-", style_minus),
                        Span::styled(" │", style_minus),
                        Span::styled(" ", style_minus),
                    ];
                    all_spans.extend(content_spans);
                    all_spans
                }
                DiffLine::Addition { doc_line, content } => {
                    let line_num_str = format!("{:>4}", doc_line);
                    let content_str = content.as_str();
                    let mut content_spans = Vec::new();

                    if let Some(word_segments) = word_diff_cache.borrow().get(&line_index) {
                        let word_segments = word_segments.clone();
                        if word_segments.is_empty() {
                            content_spans.push(Span::styled(content_str, style_plus));
                        } else {
                            let mut byte_offset = 0;
                            for segment in &word_segments {
                                let segment_text = &segment.text;
                                let segment_len = segment_text.len();
                                let base_style = if segment.is_emph { style_plus_emph } else { style_plus };

                                if line_highlights.is_empty() {
                                    content_spans.push(Span::styled(segment_text.clone(), base_style));
                                } else {
                                    let seg_start = byte_offset;
                                    let seg_end = byte_offset + segment_len;
                                    let mut last_pos = 0;

                                    for (hl_start, hl_end, hl_style) in &line_highlights {
                                        let start = (*hl_start).max(seg_start).min(seg_end) - seg_start;
                                        let end = (*hl_end).max(seg_start).min(seg_end) - seg_start;

                                        if start > last_pos && start < segment_len {
                                            let gap = &segment_text[last_pos..start];
                                            if !gap.is_empty() {
                                                content_spans.push(Span::styled(gap.to_string(), base_style));
                                            }
                                        }

                                        if end > start && start < segment_len {
                                            let text = &segment_text[start..end.min(segment_len)];
                                            if !text.is_empty() {
                                                let mut patched = base_style.patch(*hl_style);
                                                if base_style.bg.is_some() {
                                                    patched.bg = base_style.bg;
                                                }
                                                content_spans.push(Span::styled(text.to_string(), patched));
                                            }
                                        }
                                        last_pos = end.min(segment_len);
                                    }

                                    if last_pos < segment_len {
                                        let trailing = &segment_text[last_pos..];
                                        if !trailing.is_empty() {
                                            content_spans.push(Span::styled(trailing.to_string(), base_style));
                                        }
                                    }
                                }
                                byte_offset += segment_len;
                            }
                        }
                    } else if line_highlights.is_empty() {
                        content_spans.push(Span::styled(content_str, style_plus));
                    } else {
                        let mut last_end = 0;
                        for (byte_start, byte_end, segment_style) in &line_highlights {
                            let start = (*byte_start).min(content_str.len());
                            let end = (*byte_end).min(content_str.len());

                            if start > last_end {
                                let gap = &content_str[last_end..start];
                                if !gap.is_empty() {
                                    content_spans.push(Span::styled(gap, style_plus));
                                }
                            }

                            if end > start {
                                let segment = &content_str[start..end];
                                if !segment.is_empty() {
                                    let mut patched_style = style_plus.patch(*segment_style);
                                    if style_plus.bg.is_some() {
                                        patched_style.bg = style_plus.bg;
                                    }
                                    content_spans.push(Span::styled(segment, patched_style));
                                }
                            }
                            last_end = end;
                        }

                        if last_end < content_str.len() {
                            let trailing = &content_str[last_end..];
                            if !trailing.is_empty() {
                                content_spans.push(Span::styled(trailing, style_plus));
                            }
                        }
                    }

                    let mut all_spans = vec![
                        Span::styled("     ", style_plus),
                        Span::styled(line_num_str, style_plus),
                        Span::styled("+", style_plus),
                        Span::styled(" │", style_plus),
                        Span::styled(" ", style_plus),
                    ];
                    all_spans.extend(content_spans);
                    all_spans
                }
            };

            // Render the line
            let mut x_pos = inner.x;
            for span in &spans {
                if x_pos >= inner.x + inner.width {
                    break;
                }
                let remaining_width = (inner.x + inner.width - x_pos) as usize;
                let content_len = span.content.width().min(remaining_width);

                if content_len > 0 {
                    surface.set_stringn(x_pos, y, &span.content, content_len, span.style);
                }
                x_pos += span.content.width() as u16;
            }

            y += 1;
        }
    }

    fn render_preview(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        let text = cx.editor.theme.get("ui.text");
        let directory = cx.editor.theme.get("ui.text.directory");
        surface.clear_with(area, background);

        const BLOCK: Block<'_> = Block::bordered();

        // calculate the inner area inside the box
        let inner = BLOCK.inner(area);
        // 1 column gap on either side
        let margin = Margin::horizontal(1);
        let inner = inner.inner(margin);
        BLOCK.render(area, surface);

        // Check for custom preview content first (before calling get_preview)
        // This avoids borrow checker issues with get_preview
        if let Some(custom_fn) = &self.custom_preview_fn {
            if let Some(current) = self.selection() {
                if let Some(preview) = custom_fn(cx.editor, current) {
                    match preview {
                        CachedPreview::Diff { diff_base, doc, hunks, file_path, file_name } => {
                            Self::render_diff_preview(
                                &inner, surface, cx,
                                &diff_base, &doc, &hunks, &file_path, &file_name,
                            );
                            return;
                        }
                        CachedPreview::CustomText { content, is_diff } => {
                            Self::render_custom_text_preview_static(&inner, surface, cx, &content, is_diff);
                            return;
                        }
                        CachedPreview::FileList {
                            files,
                            commit_hash,
                            author,
                            date,
                            relative_date,
                            subject,
                        } => {
                            Self::render_file_list_preview(
                                &inner,
                                surface,
                                cx,
                                &files,
                                commit_hash.as_deref(),
                                author.as_deref(),
                                date.as_deref(),
                                relative_date.as_deref(),
                                subject.as_deref(),
                            );
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }

        if let Some((preview, range)) = self.get_preview(cx.editor) {
            let doc = match preview.document() {
                Some(doc)
                    if range.is_none_or(|(start, end)| {
                        start <= end && end <= doc.text().len_lines()
                    }) =>
                {
                    doc
                }
                _ => {
                    if let Some(dir_content) = preview.dir_content() {
                        for (i, (path, is_dir)) in
                            dir_content.iter().take(inner.height as usize).enumerate()
                        {
                            let style = if *is_dir { directory } else { text };
                            surface.set_stringn(
                                inner.x,
                                inner.y + i as u16,
                                path,
                                inner.width as usize,
                                style,
                            );
                        }
                        return;
                    }

                    let alt_text = preview.placeholder();
                    let x = inner.x + inner.width.saturating_sub(alt_text.len() as u16) / 2;
                    let y = inner.y + inner.height / 2;
                    surface.set_stringn(x, y, alt_text, inner.width as usize, text);
                    return;
                }
            };

            let mut offset = ViewPosition::default();
            if let Some((start_line, end_line)) = range {
                let height = end_line - start_line;
                let text = doc.text().slice(..);
                let start = text.line_to_char(start_line);
                let middle = text.line_to_char(start_line + height / 2);
                if height < inner.height as usize {
                    let text_fmt = doc.text_format(inner.width, None);
                    let annotations = TextAnnotations::default();
                    (offset.anchor, offset.vertical_offset) = char_idx_at_visual_offset(
                        text,
                        middle,
                        // align to middle
                        -(inner.height as isize / 2),
                        0,
                        &text_fmt,
                        &annotations,
                    );
                    if start < offset.anchor {
                        offset.anchor = start;
                        offset.vertical_offset = 0;
                    }
                } else {
                    offset.anchor = start;
                }
            }

            let loader = cx.editor.syn_loader.load();
            let config = cx.editor.config();

            let syntax_highlighter =
                EditorView::doc_syntax_highlighter(doc, offset.anchor, area.height, &loader);
            let mut overlay_highlights = Vec::new();
            if doc
                .language_config()
                .and_then(|config| config.rainbow_brackets)
                .unwrap_or(config.rainbow_brackets)
            {
                if let Some(overlay) = EditorView::doc_rainbow_highlights(
                    doc,
                    offset.anchor,
                    area.height,
                    &cx.editor.theme,
                    &loader,
                ) {
                    overlay_highlights.push(overlay);
                }
            }

            EditorView::doc_diagnostics_highlights_into(
                doc,
                &cx.editor.theme,
                &mut overlay_highlights,
            );

            let mut decorations = DecorationManager::default();

            if let Some((start, end)) = range {
                let style = cx
                    .editor
                    .theme
                    .try_get("ui.highlight")
                    .unwrap_or_else(|| cx.editor.theme.get("ui.selection"));
                let draw_highlight = move |renderer: &mut TextRenderer, pos: LinePos| {
                    if (start..=end).contains(&pos.doc_line) {
                        let area = Rect::new(
                            renderer.viewport.x,
                            pos.visual_line,
                            renderer.viewport.width,
                            1,
                        );
                        renderer.set_style(area, style)
                    }
                };
                decorations.add_decoration(draw_highlight);
            }

            render_document(
                surface,
                inner,
                doc,
                offset,
                // TODO: compute text annotations asynchronously here (like inlay hints)
                &TextAnnotations::default(),
                syntax_highlighter,
                overlay_highlights,
                &cx.editor.theme,
                decorations,
            );
        }
    }
}

impl<I: 'static + Send + Sync, D: 'static + Send + Sync> Component for Picker<I, D> {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // +---------+ +---------+
        // |prompt   | |preview  |
        // +---------+ |         |
        // |picker   | |         |
        // |         | |         |
        // +---------+ +---------+

        let render_preview =
            self.show_preview && (self.file_fn.is_some() || self.custom_preview_fn.is_some()) && area.width > MIN_AREA_WIDTH_FOR_PREVIEW;

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };

        let picker_area = area.with_width(picker_width);
        self.render_picker(picker_area, surface, cx);

        if render_preview {
            let preview_area = area.clip_left(picker_width);
            self.render_preview(preview_area, surface, cx);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        // TODO: keybinds for scrolling preview

        let key_event = match event {
            Event::Key(event) => *event,
            Event::Paste(..) => return self.prompt_handle_event(event, ctx),
            Event::Resize(..) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };

        let close_fn = |picker: &mut Self| {
            // if the picker is very large don't store it as last_picker to avoid
            // excessive memory consumption
            let callback: compositor::Callback =
                if picker.matcher.snapshot().item_count() > 1_000_000 {
                    Box::new(|compositor: &mut Compositor, _ctx| {
                        // remove the layer
                        compositor.pop();
                    })
                } else {
                    // stop streaming in new items in the background, really we should
                    // be restarting the stream somehow once the picker gets
                    // reopened instead (like for an FS crawl) that would also remove the
                    // need for the special case above but that is pretty tricky
                    picker.version.fetch_add(1, atomic::Ordering::Relaxed);
                    Box::new(|compositor: &mut Compositor, _ctx| {
                        // remove the layer
                        compositor.last_picker = compositor.pop();
                    })
                };
            EventResult::Consumed(Some(callback))
        };

        match key_event {
            shift!(Tab) | key!(Up) | ctrl!('p') => {
                self.move_by(1, Direction::Backward);
            }
            key!(Tab) | key!(Down) | ctrl!('n') => {
                self.move_by(1, Direction::Forward);
            }
            key!(PageDown) | ctrl!('d') => {
                self.page_down();
            }
            key!(PageUp) | ctrl!('u') => {
                self.page_up();
            }
            key!(Home) => {
                self.to_start();
            }
            key!(End) => {
                self.to_end();
            }
            key!(Esc) | ctrl!('c') => return close_fn(self),
            alt!(Enter) => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, self.default_action);
                }
            }
            key!(Enter) => {
                // If the prompt has a history completion and is empty, use enter to accept
                // that completion
                if let Some(completion) = self
                    .prompt
                    .first_history_completion(ctx.editor)
                    .filter(|_| self.prompt.line().is_empty())
                {
                    // The percent character is used by the query language and needs to be
                    // escaped with a backslash.
                    let completion = if completion.contains('%') {
                        completion.replace('%', "\\%")
                    } else {
                        completion.into_owned()
                    };
                    self.prompt.set_line(completion, ctx.editor);

                    // Inserting from the history register is a paste.
                    self.handle_prompt_change(true);
                } else {
                    if let Some(option) = self.selection() {
                        (self.callback_fn)(ctx, option, self.default_action);
                    }
                    if let Some(history_register) = self.prompt.history_register() {
                        if let Err(err) = ctx
                            .editor
                            .registers
                            .push(history_register, self.primary_query().to_string())
                        {
                            ctx.editor.set_error(err.to_string());
                        }
                    }
                    return close_fn(self);
                }
            }
            ctrl!('s') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::HorizontalSplit);
                }
                return close_fn(self);
            }
            ctrl!('v') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::VerticalSplit);
                }
                return close_fn(self);
            }
            ctrl!('t') => {
                self.toggle_preview();
            }
            _ => {
                self.prompt_handle_event(event, ctx);
            }
        }

        EventResult::Consumed(None)
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let block = Block::bordered();
        // calculate the inner area inside the box
        let inner = block.inner(area);

        // prompt area
        let render_preview =
            self.show_preview && (self.file_fn.is_some() || self.custom_preview_fn.is_some()) && area.width > MIN_AREA_WIDTH_FOR_PREVIEW;

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };
        let area = inner.clip_left(1).with_height(1).with_width(picker_width);

        self.prompt.cursor(area, editor)
    }

    fn required_size(&mut self, (width, height): (u16, u16)) -> Option<(u16, u16)> {
        self.completion_height = height.saturating_sub(4 + self.header_height());
        Some((width, height))
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}
impl<T: 'static + Send + Sync, D> Drop for Picker<T, D> {
    fn drop(&mut self) {
        // ensure we cancel any ongoing background threads streaming into the picker
        self.version.fetch_add(1, atomic::Ordering::Relaxed);
    }
}

type PickerCallback<T> = Box<dyn Fn(&mut Context, &T, Action)>;

/// Categorizes a diff line for styling purposes.
/// This enum represents the different categories of lines in a unified diff format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineCategory {
    /// "diff --git a/file b/file" header
    DiffHeader,
    /// "index abc123..def456 100644" or "Index: abc123"
    IndexLine,
    /// "--- a/file" or "+++ b/file" file headers
    FileHeader,
    /// "@@ -1,4 +1,5 @@" hunk header
    HunkHeader,
    /// Addition line (starts with '+')
    Addition,
    /// Deletion line (starts with '-')
    Deletion,
    /// Binary file notice
    BinaryNotice,
    /// File mode changes (new file mode, deleted file mode, etc.)
    FileModeChange,
    /// Rename/copy metadata
    RenameMetadata,
    /// Similarity index for renames
    SimilarityIndex,
    /// Context line (starts with space or empty)
    Context,
    /// Unknown line type (use default text style)
    Unknown,
}

/// Determines the category of a diff line for styling purposes.
/// This function is used by `render_custom_text_preview_static` to apply
/// appropriate styles to different types of diff lines.
pub fn categorize_diff_line(line: &str) -> DiffLineCategory {
    if line.starts_with("diff --git ") {
        DiffLineCategory::DiffHeader
    } else if line.starts_with("index ") || line.starts_with("Index: ") {
        DiffLineCategory::IndexLine
    } else if line.starts_with("--- ") || line.starts_with("+++ ") {
        DiffLineCategory::FileHeader
    } else if line.starts_with("@@ ") {
        DiffLineCategory::HunkHeader
    } else if line.starts_with('+') {
        DiffLineCategory::Addition
    } else if line.starts_with('-') {
        DiffLineCategory::Deletion
    } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
        DiffLineCategory::BinaryNotice
    } else if line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
    {
        DiffLineCategory::FileModeChange
    } else if line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("copy from ")
        || line.starts_with("copy to ")
    {
        DiffLineCategory::RenameMetadata
    } else if line.starts_with("similarity index ") || line.starts_with("dissimilarity index ") {
        DiffLineCategory::SimilarityIndex
    } else if line.starts_with(' ') || line.is_empty() {
        DiffLineCategory::Context
    } else {
        DiffLineCategory::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test suite for diff line categorization and styling.
    /// 
    /// These tests verify that the `categorize_diff_line` function correctly
    /// identifies different types of diff lines according to the unified diff format.
    /// The styling is then applied based on these categories in `render_custom_text_preview_static`.

    mod diff_line_categorization {
        use super::*;

        // ========================================
        // Test 1: File headers (`---`, `+++`, `diff --git`)
        // ========================================

        #[test]
        fn test_diff_header_line() {
            // Standard diff header
            assert_eq!(
                categorize_diff_line("diff --git a/src/main.rs b/src/main.rs"),
                DiffLineCategory::DiffHeader
            );
            
            // With different file paths
            assert_eq!(
                categorize_diff_line("diff --git a/old/path.txt b/new/path.txt"),
                DiffLineCategory::DiffHeader
            );
            
            // Should NOT match if not exactly "diff --git "
            assert_ne!(
                categorize_diff_line("diff --cc a/file.txt"),
                DiffLineCategory::DiffHeader
            );
        }

        #[test]
        fn test_file_header_minus() {
            // Standard --- header
            assert_eq!(
                categorize_diff_line("--- a/src/main.rs"),
                DiffLineCategory::FileHeader
            );
            
            // With different paths
            assert_eq!(
                categorize_diff_line("--- a/lib/module.rs"),
                DiffLineCategory::FileHeader
            );
            
            // /dev/null for new files
            assert_eq!(
                categorize_diff_line("--- /dev/null"),
                DiffLineCategory::FileHeader
            );
        }

        #[test]
        fn test_file_header_plus() {
            // Standard +++ header
            assert_eq!(
                categorize_diff_line("+++ b/src/main.rs"),
                DiffLineCategory::FileHeader
            );
            
            // With different paths
            assert_eq!(
                categorize_diff_line("+++ b/lib/module.rs"),
                DiffLineCategory::FileHeader
            );
            
            // /dev/null for deleted files
            assert_eq!(
                categorize_diff_line("+++ /dev/null"),
                DiffLineCategory::FileHeader
            );
        }

        // ========================================
        // Test 2: Hunk headers (`@@`)
        // ========================================

        #[test]
        fn test_hunk_header() {
            // Standard hunk header
            assert_eq!(
                categorize_diff_line("@@ -1,4 +1,5 @@"),
                DiffLineCategory::HunkHeader
            );
            
            // With context
            assert_eq!(
                categorize_diff_line("@@ -10,7 +10,8 @@ fn main() {"),
                DiffLineCategory::HunkHeader
            );
            
            // Single line changes
            assert_eq!(
                categorize_diff_line("@@ -42 +42 @@"),
                DiffLineCategory::HunkHeader
            );
            
            // At start of file
            assert_eq!(
                categorize_diff_line("@@ -0,0 +1,5 @@"),
                DiffLineCategory::HunkHeader
            );
        }

        // ========================================
        // Test 3: Additions (`+`)
        // ========================================

        #[test]
        fn test_addition_lines() {
            // Simple addition
            assert_eq!(
                categorize_diff_line("+new line"),
                DiffLineCategory::Addition
            );
            
            // Empty addition (just the +)
            assert_eq!(
                categorize_diff_line("+"),
                DiffLineCategory::Addition
            );
            
            // Addition with spaces
            assert_eq!(
                categorize_diff_line("+    indented code"),
                DiffLineCategory::Addition
            );
            
            // Addition with special characters
            assert_eq!(
                categorize_diff_line("+// comment with @@ symbols"),
                DiffLineCategory::Addition
            );
            
            // Multi-line string addition
            assert_eq!(
                categorize_diff_line("+    \"multi\""),
                DiffLineCategory::Addition
            );
        }

        #[test]
        fn test_addition_not_confused_with_file_header() {
            // A line starting with ++ should NOT be a file header
            // (file headers are "+++ " with space after)
            assert_eq!(
                categorize_diff_line("++not a file header"),
                DiffLineCategory::Addition
            );
            
            // Single + followed by space is an addition
            assert_eq!(
                categorize_diff_line("+ added line"),
                DiffLineCategory::Addition
            );
        }

        // ========================================
        // Test 4: Deletions (`-`)
        // ========================================

        #[test]
        fn test_deletion_lines() {
            // Simple deletion
            assert_eq!(
                categorize_diff_line("-old line"),
                DiffLineCategory::Deletion
            );
            
            // Empty deletion (just the -)
            assert_eq!(
                categorize_diff_line("-"),
                DiffLineCategory::Deletion
            );
            
            // Deletion with spaces
            assert_eq!(
                categorize_diff_line("-    indented code"),
                DiffLineCategory::Deletion
            );
            
            // Deletion with special characters
            assert_eq!(
                categorize_diff_line("-// comment"),
                DiffLineCategory::Deletion
            );
        }

        #[test]
        fn test_deletion_not_confused_with_file_header() {
            // A line starting with -- should NOT be a file header
            // (file headers are "--- " with space after)
            assert_eq!(
                categorize_diff_line("--not a file header"),
                DiffLineCategory::Deletion
            );
            
            // Single - followed by space is a deletion
            assert_eq!(
                categorize_diff_line("- removed line"),
                DiffLineCategory::Deletion
            );
        }

        // ========================================
        // Test 5: Context lines (space prefix)
        // ========================================

        #[test]
        fn test_context_lines() {
            // Context line with space prefix
            assert_eq!(
                categorize_diff_line(" unchanged line"),
                DiffLineCategory::Context
            );
            
            // Empty line (also context)
            assert_eq!(
                categorize_diff_line(""),
                DiffLineCategory::Context
            );
            
            // Context with indentation
            assert_eq!(
                categorize_diff_line("    indented context"),
                DiffLineCategory::Context
            );
            
            // Context that looks like code
            assert_eq!(
                categorize_diff_line(" fn main() {"),
                DiffLineCategory::Context
            );
        }

        // ========================================
        // Test 6: Metadata lines (index, mode changes)
        // ========================================

        #[test]
        fn test_index_line() {
            // Standard index line
            assert_eq!(
                categorize_diff_line("index abc123..def456 100644"),
                DiffLineCategory::IndexLine
            );
            
            // Index with different mode
            assert_eq!(
                categorize_diff_line("index 1234567..89abcde 100755"),
                DiffLineCategory::IndexLine
            );
            
            // Index: format (SVN style)
            assert_eq!(
                categorize_diff_line("Index: src/main.rs"),
                DiffLineCategory::IndexLine
            );
        }

        #[test]
        fn test_file_mode_changes() {
            // New file
            assert_eq!(
                categorize_diff_line("new file mode 100644"),
                DiffLineCategory::FileModeChange
            );
            
            // Deleted file
            assert_eq!(
                categorize_diff_line("deleted file mode 100644"),
                DiffLineCategory::FileModeChange
            );
            
            // Mode change (old mode)
            assert_eq!(
                categorize_diff_line("old mode 100644"),
                DiffLineCategory::FileModeChange
            );
            
            // Mode change (new mode)
            assert_eq!(
                categorize_diff_line("new mode 100755"),
                DiffLineCategory::FileModeChange
            );
        }

        #[test]
        fn test_binary_notice() {
            // Binary files notice
            assert_eq!(
                categorize_diff_line("Binary files a/image.png and b/image.png differ"),
                DiffLineCategory::BinaryNotice
            );
            
            // GIT binary patch
            assert_eq!(
                categorize_diff_line("GIT binary patch"),
                DiffLineCategory::BinaryNotice
            );
        }

        #[test]
        fn test_rename_metadata() {
            // Rename from
            assert_eq!(
                categorize_diff_line("rename from old_name.rs"),
                DiffLineCategory::RenameMetadata
            );
            
            // Rename to
            assert_eq!(
                categorize_diff_line("rename to new_name.rs"),
                DiffLineCategory::RenameMetadata
            );
            
            // Copy from
            assert_eq!(
                categorize_diff_line("copy from template.rs"),
                DiffLineCategory::RenameMetadata
            );
            
            // Copy to
            assert_eq!(
                categorize_diff_line("copy to new_file.rs"),
                DiffLineCategory::RenameMetadata
            );
        }

        #[test]
        fn test_similarity_index() {
            // Similarity index
            assert_eq!(
                categorize_diff_line("similarity index 87%"),
                DiffLineCategory::SimilarityIndex
            );
            
            // Dissimilarity index
            assert_eq!(
                categorize_diff_line("dissimilarity index 50%"),
                DiffLineCategory::SimilarityIndex
            );
        }

        // ========================================
        // Test 7: Unknown lines
        // ========================================

        #[test]
        fn test_unknown_lines() {
            // Lines that don't match any known pattern
            assert_eq!(
                categorize_diff_line("some random text"),
                DiffLineCategory::Unknown
            );
            
            // Lines starting with other characters
            assert_eq!(
                categorize_diff_line("# comment"),
                DiffLineCategory::Unknown
            );
            
            // Lines that look like code but not in diff format
            assert_eq!(
                categorize_diff_line("fn main() {}"),
                DiffLineCategory::Unknown
            );
        }

        // ========================================
        // Test 8: Edge cases
        // ========================================

        #[test]
        fn test_edge_cases() {
            // Line with only whitespace starting with space (context in diff format)
            assert_eq!(
                categorize_diff_line("   "),
                DiffLineCategory::Context
            );
            
            // Very long line
            let long_line = "+".repeat(1000);
            assert_eq!(
                categorize_diff_line(&format!("+{}", long_line)),
                DiffLineCategory::Addition
            );
            
            // Line with unicode
            assert_eq!(
                categorize_diff_line("+🎉 unicode addition"),
                DiffLineCategory::Addition
            );
            
            // Hunk header with complex context
            assert_eq!(
                categorize_diff_line("@@ -100,7 +100,7 @@ impl<T> SomeStruct<T> where T: Clone + Debug {"),
                DiffLineCategory::HunkHeader
            );
        }

        // ========================================
        // Test 9: Complete diff example
        // ========================================

        #[test]
        fn test_complete_diff_example() {
            // A realistic diff example with all line types
            let diff_lines = [
                ("diff --git a/src/main.rs b/src/main.rs", DiffLineCategory::DiffHeader),
                ("index abc123..def456 100644", DiffLineCategory::IndexLine),
                ("--- a/src/main.rs", DiffLineCategory::FileHeader),
                ("+++ b/src/main.rs", DiffLineCategory::FileHeader),
                ("@@ -1,5 +1,6 @@", DiffLineCategory::HunkHeader),
                (" fn main() {", DiffLineCategory::Context),
                ("     println!(\"Hello\");", DiffLineCategory::Context),
                ("-    let x = 1;", DiffLineCategory::Deletion),
                ("+    let x = 2;", DiffLineCategory::Addition),
                ("+    let y = 3;", DiffLineCategory::Addition),
                (" }", DiffLineCategory::Context),
            ];

            for (line, expected) in diff_lines {
                assert_eq!(
                    categorize_diff_line(line),
                    expected,
                    "Failed for line: {:?}",
                    line
                );
            }
        }

        #[test]
        fn test_complete_new_file_diff() {
            // Diff for a new file
            let diff_lines = [
                ("diff --git a/new_file.rs b/new_file.rs", DiffLineCategory::DiffHeader),
                ("new file mode 100644", DiffLineCategory::FileModeChange),
                ("index 0000000..abc1234", DiffLineCategory::IndexLine),
                ("--- /dev/null", DiffLineCategory::FileHeader),
                ("+++ b/new_file.rs", DiffLineCategory::FileHeader),
                ("@@ -0,0 +1,3 @@", DiffLineCategory::HunkHeader),
                ("+fn new_function() {", DiffLineCategory::Addition),
                ("+    println!(\"New\");", DiffLineCategory::Addition),
                ("+}", DiffLineCategory::Addition),
            ];

            for (line, expected) in diff_lines {
                assert_eq!(
                    categorize_diff_line(line),
                    expected,
                    "Failed for line: {:?}",
                    line
                );
            }
        }

        #[test]
        fn test_complete_deleted_file_diff() {
            // Diff for a deleted file
            let diff_lines = [
                ("diff --git a/old_file.rs b/old_file.rs", DiffLineCategory::DiffHeader),
                ("deleted file mode 100644", DiffLineCategory::FileModeChange),
                ("index abc1234..0000000", DiffLineCategory::IndexLine),
                ("--- a/old_file.rs", DiffLineCategory::FileHeader),
                ("+++ /dev/null", DiffLineCategory::FileHeader),
                ("@@ -1,3 +0,0 @@", DiffLineCategory::HunkHeader),
                ("-fn old_function() {", DiffLineCategory::Deletion),
                ("-    println!(\"Old\");", DiffLineCategory::Deletion),
                ("-}", DiffLineCategory::Deletion),
            ];

            for (line, expected) in diff_lines {
                assert_eq!(
                    categorize_diff_line(line),
                    expected,
                    "Failed for line: {:?}",
                    line
                );
            }
        }

        #[test]
        fn test_complete_rename_diff() {
            // Diff for a renamed file
            let diff_lines = [
                ("diff --git a/old_name.rs b/new_name.rs", DiffLineCategory::DiffHeader),
                ("similarity index 100%", DiffLineCategory::SimilarityIndex),
                ("rename from old_name.rs", DiffLineCategory::RenameMetadata),
                ("rename to new_name.rs", DiffLineCategory::RenameMetadata),
            ];

            for (line, expected) in diff_lines {
                assert_eq!(
                    categorize_diff_line(line),
                    expected,
                    "Failed for line: {:?}",
                    line
                );
            }
        }

        #[test]
        fn test_complete_mode_change_diff() {
            // Diff for a mode change (chmod)
            let diff_lines = [
                ("diff --git a/script.sh b/script.sh", DiffLineCategory::DiffHeader),
                ("old mode 100644", DiffLineCategory::FileModeChange),
                ("new mode 100755", DiffLineCategory::FileModeChange),
            ];

            for (line, expected) in diff_lines {
                assert_eq!(
                    categorize_diff_line(line),
                    expected,
                    "Failed for line: {:?}",
                    line
                );
            }
        }
    }
}
