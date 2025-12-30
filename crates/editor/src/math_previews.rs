use collections::{HashMap, HashSet};
use futures::StreamExt;
use gpui::{
    AppContext, Context, Element, IntoElement, ParentElement, Pixels, SharedString, Size, Styled,
    Task, Window, div, px, size, svg,
};
use inline_preiew::{Backend as RenderBackend, RenderContext};
use language::{LanguageName, MathPreviewBackend, MathPreviewKind};
use language::language_settings::language_settings;
use multi_buffer::{Anchor, MultiBufferOffset, ToOffset};
use regex::Regex;
use std::{
    any::TypeId,
    ops::Range,
    path::Path,
    sync::{Arc, OnceLock, Mutex, Weak},
    time::Duration,
};
use theme::ActiveTheme;
use text::BufferId;

fn shared_render_context() -> Arc<RenderContext> {
    static CONTEXT: OnceLock<Arc<RenderContext>> = OnceLock::new();
    CONTEXT
        .get_or_init(|| Arc::new(RenderContext::new()))
        .clone()
}

fn render_context_for_buffer(buffer_id: Option<BufferId>) -> Arc<RenderContext> {
    static CONTEXTS: OnceLock<Mutex<HashMap<BufferId, Weak<RenderContext>>>> = OnceLock::new();
    if let Some(buffer_id) = buffer_id {
        let mut map = CONTEXTS.get_or_init(Default::default).lock().unwrap();
        if let Some(existing) = map.get(&buffer_id).and_then(|weak| weak.upgrade()) {
            return existing;
        }
        let ctx = Arc::new(RenderContext::new());
        map.insert(buffer_id, Arc::downgrade(&ctx));
        ctx
    } else {
        shared_render_context()
    }
}

use crate::{
    Bias, BlockPlacement, BlockProperties, BlockStyle, Crease, CustomBlockId, DisplayPoint,
    DisplayRow, DisplaySnapshot, Editor, FoldPlaceholder, display_map::ToDisplayPoint,
};

const MATH_PREVIEW_DEBOUNCE_MS: u64 = 150;
const MATH_VISIBLE_PADDING_LINES: u32 = 40;
const MATH_RANGE_SCAN_BYTES: usize = 256;

pub(crate) struct MathPreviews {
    render_context: Arc<RenderContext>,
    refresh_task: Task<()>,
    generation: usize,
    rendered: Vec<MathPreviewEntry>,
    block_ids: HashSet<CustomBlockId>,
    popover: Option<MathPreviewPopover>,
    render_cache: HashMap<u64, RenderedSvg>,
    last_selection_fingerprint: u64,
}

impl MathPreviews {
    pub(crate) fn new() -> Self {
        Self {
            render_context: shared_render_context(),
            refresh_task: Task::ready(()),
            generation: 0,
            rendered: Vec::new(),
            block_ids: HashSet::default(),
            popover: None,
            render_cache: HashMap::default(),
            last_selection_fingerprint: 0,
        }
    }
}

#[derive(Clone, PartialEq)]
pub(crate) struct MathPreviewPopover {
    pub(crate) anchor: Anchor,
    pub(crate) svg_path: SharedString,
    pub(crate) size: Size<Pixels>,
}

#[derive(Clone, PartialEq)]
struct MathPreviewEntry {
    range: Range<MultiBufferOffset>,
    kind: MathPreviewKind,
    _backend: MathPreviewBackend,
    svg_path: SharedString,
    size: Size<Pixels>,
    inline_enabled: bool,
    popover_enabled: bool,
}

struct MathPreviewFold;

struct RenderRequest {
    range: Range<MultiBufferOffset>,
    kind: MathPreviewKind,
    backend: MathPreviewBackend,
    content: String,
    inline_enabled: bool,
    popover_enabled: bool,
}

#[derive(Clone)]
struct RenderedSvg {
    path: SharedString,
    size: Size<Pixels>,
}

#[derive(Clone, Copy)]
struct MathPreviewSettings {
    inline_enabled: bool,
    popover_enabled: bool,
}

fn clip_range_to_len(range: &Range<MultiBufferOffset>, len: usize) -> Option<Range<MultiBufferOffset>> {
    if range.start.0 >= len {
        return None;
    }
    let start = range.start.0;
    let end = range.end.0.min(len);
    (start < end).then(|| MultiBufferOffset(start)..MultiBufferOffset(end))
}

impl Editor {
    pub(crate) fn refresh_math_previews(
        &mut self,
        debounce: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.is_full() {
            self.clear_math_previews(cx);
            return;
        }

        let Some(_) = self.visible_line_count() else {
            return;
        };

        self.math_previews.generation = self.math_previews.generation.wrapping_add(1);
        let generation = self.math_previews.generation;
        let debounce = debounce.then(|| Duration::from_millis(MATH_PREVIEW_DEBOUNCE_MS));
        let render_cache = std::mem::take(&mut self.math_previews.render_cache);

        let default_language_settings = language_settings(None, None, cx);
        let default_preview_settings = MathPreviewSettings {
            inline_enabled: default_language_settings.math_previews_inline,
            popover_enabled: default_language_settings.math_previews_popover,
        };
        let mut preview_settings_by_language = HashMap::default();
        preview_settings_by_language.insert(None, default_preview_settings);
        for (language, settings) in &self.applicable_language_settings {
            preview_settings_by_language.insert(
                language.clone(),
                MathPreviewSettings {
                    inline_enabled: settings.math_previews_inline,
                    popover_enabled: settings.math_previews_popover,
                },
            );
        }
        if !preview_settings_by_language
            .values()
            .any(|settings| settings.inline_enabled || settings.popover_enabled)
        {
            self.clear_math_previews(cx);
            self.math_previews.render_cache = render_cache;
            return;
        }

        self.math_previews.refresh_task = cx.spawn_in(window, async move |editor, cx| {
            let render_cache = render_cache;
            if let Some(debounce) = debounce {
                cx.background_executor().timer(debounce).await;
            }

            let Some((snapshot, visible_range, render_context)) = editor
                .update(cx, |editor, cx| {
                    let display_snapshot = editor.display_snapshot(cx);
                    let Some(range) = visible_math_range(editor, &display_snapshot) else {
                        return None;
                    };
                    let buffer_id = display_snapshot
                        .buffer_snapshot()
                        .buffer_ids_for_range(range.clone())
                        .next();
                    let render_context = render_context_for_buffer(buffer_id);
                    editor.math_previews.render_context = render_context.clone();
                    Some((
                        display_snapshot.buffer_snapshot().clone(),
                        range,
                        render_context,
                    ))
                })
                .ok()
                .flatten()
            else {
                editor
                    .update(cx, |editor, _| {
                        editor.math_previews.render_cache = render_cache;
                    })
                    .ok();
                return;
            };

            let (rendered, render_cache) = cx
                .background_spawn(async move {
                    render_math_previews(
                        &render_context,
                        snapshot,
                        visible_range,
                        preview_settings_by_language,
                        default_preview_settings,
                        render_cache,
                    )
                    .await
                })
                .await;

            editor
                .update_in(cx, |editor, window, cx| {
                    if editor.math_previews.generation != generation {
                        editor.math_previews.render_cache = render_cache;
                        return;
                    }
                    editor.set_math_previews(rendered, window, cx);
                    editor.math_previews.render_cache = render_cache;
                })
                .ok();
        });
    }

    pub(crate) fn update_math_preview_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let display_snapshot = self.display_snapshot(cx);
        let selection_head = self.selections.newest_anchor().head();
        let selection_head_offset = selection_head.to_offset(display_snapshot.buffer_snapshot());
        let selection_fingerprint = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for sel in self
                .selections
                .all::<MultiBufferOffset>(&display_snapshot)
            {
                sel.range().hash(&mut hasher);
            }
            hasher.finish()
        };

        let style = self.style(cx);
        let font_id = window.text_system().resolve_font(&style.text.font());
        let font_size = style.text.font_size.to_pixels(window.rem_size());
        let ascent = window.text_system().ascent(font_id, font_size);
        let descent = -window.text_system().descent(font_id, font_size);
        let text_box_height = (ascent + descent).max(px(1.));

        let mut popover = None;
        for entry in &self.math_previews.rendered {
            if popover.is_none()
                && entry.popover_enabled
                && matches!(entry.kind, MathPreviewKind::Inline)
                && entry.range.contains(&selection_head_offset)
            {
                popover = Some(MathPreviewPopover {
                    anchor: selection_head,
                    svg_path: entry.svg_path.clone(),
                    size: scale_svg_to_height(entry.size, text_box_height),
                });
                break;
            }
        }

        if popover != self.math_previews.popover {
            self.math_previews.popover = popover;
            cx.notify();
        }

        if self.math_previews.rendered.is_empty() {
            return;
        }

        if selection_fingerprint != self.math_previews.last_selection_fingerprint {
            self.math_previews.last_selection_fingerprint = selection_fingerprint;
            let selections = self
                .selections
                .all::<MultiBufferOffset>(&display_snapshot)
                .into_iter()
                .map(|sel| sel.range())
                .collect::<Vec<_>>();
            let mut overlap_ranges = Vec::new();
            let mut block_overlap = false;
            for entry in &self.math_previews.rendered {
                if selections
                    .iter()
                    .any(|sel| ranges_overlap(&entry.range, sel))
                {
                    overlap_ranges.push(entry.range.clone());
                    if matches!(entry.kind, MathPreviewKind::Block) {
                        block_overlap = true;
                    }
                }
            }

            let type_id = TypeId::of::<MathPreviewFold>();
            if !overlap_ranges.is_empty() {
                let buffer_len = display_snapshot.buffer_snapshot().len().0;
                let clipped: Vec<_> = overlap_ranges
                    .into_iter()
                    .filter_map(|range| clip_range_to_len(&range, buffer_len))
                    .collect();
                if !clipped.is_empty() {
                    self.display_map.update(cx, |display_map, cx| {
                        display_map.remove_folds_with_type(clipped, type_id, cx);
                    });
                }
            }
            if block_overlap && !self.math_previews.block_ids.is_empty() {
                let block_ids = std::mem::take(&mut self.math_previews.block_ids);
                self.remove_blocks(block_ids, None, cx);
            }
        }
    }

    pub(crate) fn math_preview_popover(&self) -> Option<MathPreviewPopover> {
        self.math_previews.popover.clone()
    }

    fn set_math_previews(
        &mut self,
        rendered: Vec<MathPreviewEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut rendered = rendered;
        rendered.sort_by_key(|entry| entry.range.start.0);
        self.math_previews
            .rendered
            .sort_by_key(|entry| entry.range.start.0);
        if self.math_previews.rendered == rendered {
            return;
        }

        let ranges_to_remove = self
            .math_previews
            .rendered
            .iter()
            .map(|entry| entry.range.clone())
            .collect();
        self.math_previews.rendered = rendered;
        self.math_previews.last_selection_fingerprint = 0;
        self.rebuild_math_previews(ranges_to_remove, window, cx, true);
    }

    fn rebuild_math_previews(
        &mut self,
        ranges_to_remove: Vec<Range<MultiBufferOffset>>,
        window: &mut Window,
        cx: &mut Context<Self>,
        include_blocks: bool,
    ) {
        let type_id = TypeId::of::<MathPreviewFold>();
        if include_blocks {
            let block_ids = std::mem::take(&mut self.math_previews.block_ids);
            if !block_ids.is_empty() {
                self.remove_blocks(block_ids, None, cx);
            }
        }

        let display_snapshot = self.display_snapshot(cx);
        let selections = self
            .selections
            .all::<MultiBufferOffset>(&display_snapshot);
        let selection_head = self.selections.newest_anchor().head();
        let selection_head_offset = selection_head.to_offset(display_snapshot.buffer_snapshot());
        let style = self.style(cx);
        let line_height = style.text.line_height_in_pixels(window.rem_size());
        let font_id = window.text_system().resolve_font(&style.text.font());
        let font_size = style.text.font_size.to_pixels(window.rem_size());
        let ascent = window.text_system().ascent(font_id, font_size);
        let descent = -window.text_system().descent(font_id, font_size);
        let _text_box_height = (ascent + descent).max(px(1.));
        let inline_target_height = line_height * 1.1;

        let mut popover = None;
        let mut creases = Vec::new();
        let mut blocks = Vec::new();

        // Ensure deterministic order to avoid fold overlaps when ranges are adjacent.
        self.math_previews
            .rendered
            .sort_by_key(|entry| entry.range.start.0);

        for entry in &self.math_previews.rendered {
            // Skip stale ranges that are now outside the buffer.
            let clipped_range =
                clip_range_to_len(&entry.range, display_snapshot.buffer_snapshot().len().0);
            let Some(clipped_range) = clipped_range else {
                continue;
            };
            if popover.is_none()
                && entry.popover_enabled
                && matches!(entry.kind, MathPreviewKind::Inline)
                && clipped_range.contains(&selection_head_offset)
            {
                popover = Some(MathPreviewPopover {
                    anchor: selection_head,
                    svg_path: entry.svg_path.clone(),
                    size: scale_svg_to_height(entry.size, inline_target_height),
                });
            }

            if selections
                .iter()
                .any(|selection| ranges_overlap(&clipped_range, &selection.range()))
            {
                // Hide inline previews while editing inside, but keep block previews visible.
                if matches!(entry.kind, MathPreviewKind::Inline) {
                    continue;
                }
            }

            let start_row = clipped_range
                .start
                .to_display_point(&display_snapshot)
                .row();
            let end_row = clipped_range.end.to_display_point(&display_snapshot).row();
            let span_lines = end_row.0.saturating_sub(start_row.0).saturating_add(1) as u32;

            match entry.kind {
                MathPreviewKind::Inline => {
                    if entry.inline_enabled {
                        let display_size = scale_svg_to_height(entry.size, inline_target_height);
                        let placeholder = math_placeholder(entry, display_size, line_height);
                        creases.push(Crease::simple(clipped_range.clone(), placeholder));
                    }
                }
                MathPreviewKind::Block => {
                    if include_blocks && entry.inline_enabled {
                        let natural_lines =
                            ((entry.size.height / line_height).ceil().max(1.0)) as u32;
                        let lines = natural_lines.max(span_lines).max(1);
                        let display_size =
                            scale_svg_to_height(entry.size, line_height * lines as f32 * 1.05);
                        let height = lines;
                        let start = display_snapshot
                            .buffer_snapshot()
                            .anchor_before(clipped_range.start);
                        let end = display_snapshot
                            .buffer_snapshot()
                            .anchor_after(clipped_range.end);
                        blocks.push(BlockProperties {
                            placement: BlockPlacement::Replace(start..=end),
                            height: Some(height),
                            style: BlockStyle::Flex,
                            render: math_block(entry, display_size),
                            priority: 0,
                        });
                    }
                }
            }
        }

        self.display_map.update(cx, |display_map, cx| {
            if !ranges_to_remove.is_empty() {
                let buffer_len = display_snapshot.buffer_snapshot().len().0;
                let clipped: Vec<_> = ranges_to_remove
                    .into_iter()
                    .filter_map(|range| clip_range_to_len(&range, buffer_len))
                    .collect();
                if !clipped.is_empty() {
                    display_map.remove_folds_with_type(clipped, type_id, cx);
                }
            }
            if !creases.is_empty() {
                let buffer_len = display_snapshot.buffer_snapshot().len().0;
                let clipped: Vec<_> = creases
                    .into_iter()
                    .filter_map(|crease| match crease {
                        Crease::Inline {
                            range,
                            placeholder,
                            render_toggle,
                            render_trailer,
                            metadata,
                        } => {
                            clip_range_to_len(&range, buffer_len).map(|range| Crease::Inline {
                                range,
                                placeholder,
                                render_toggle,
                                render_trailer,
                                metadata,
                            })
                        }
                        Crease::Block { .. } => None,
                    })
                    .collect();
                if !clipped.is_empty() {
                    display_map.fold(clipped, cx);
                }
            }
        });

        if !blocks.is_empty() {
            let block_ids = self.insert_blocks(blocks, None, cx);
            self.math_previews.block_ids = block_ids.into_iter().collect();
        } else if include_blocks {
            self.math_previews.block_ids.clear();
        }

        self.math_previews.popover = popover;
        cx.notify();
    }

    fn clear_math_previews(&mut self, cx: &mut Context<Self>) {
        let ranges_to_remove = self
            .math_previews
            .rendered
            .iter()
            .map(|entry| entry.range.clone())
            .collect::<Vec<_>>();
        let block_ids = std::mem::take(&mut self.math_previews.block_ids);

        if !block_ids.is_empty() {
            self.remove_blocks(block_ids, None, cx);
        }

        if !ranges_to_remove.is_empty() {
            let type_id = TypeId::of::<MathPreviewFold>();
            let buffer_len = self.display_snapshot(cx).buffer_snapshot().len().0;
            self.display_map.update(cx, |display_map, cx| {
                let clipped: Vec<_> = ranges_to_remove
                    .into_iter()
                    .filter_map(|range| clip_range_to_len(&range, buffer_len))
                    .collect();
                if !clipped.is_empty() {
                    display_map.remove_folds_with_type(clipped, type_id, cx);
                }
            });
        }

        self.math_previews.rendered.clear();
        self.math_previews.popover = None;
        cx.notify();
    }
}

fn math_placeholder(
    entry: &MathPreviewEntry,
    display_size: Size<Pixels>,
    line_height: Pixels,
) -> FoldPlaceholder {
    let path = entry.svg_path.clone();
    let size = display_size;
    FoldPlaceholder {
        render: Arc::new(move |_, _, cx| {
            div()
                .flex()
                .items_end()
                .h(line_height)
                .child(
                    svg()
                        .external_path(path.clone())
                        .w(size.width)
                        .h(size.height)
                        .text_color(cx.theme().colors().editor_foreground),
                )
                .into_any()
        }),
        constrain_width: false,
        merge_adjacent: false,
        type_tag: Some(TypeId::of::<MathPreviewFold>()),
    }
}

fn math_block(
    entry: &MathPreviewEntry,
    display_size: Size<Pixels>,
) -> Arc<dyn Send + Sync + Fn(&mut crate::BlockContext) -> gpui::AnyElement> {
    let path = entry.svg_path.clone();
    let size = display_size;
    Arc::new(move |cx| {
        div()
            .pl(cx.anchor_x)
            .child(
                svg()
                    .external_path(path.clone())
                    .w(size.width)
                    .h(size.height)
                    .text_color(cx.theme().colors().editor_foreground),
            )
            .into_any_element()
    })
}

fn ranges_overlap(
    range: &Range<MultiBufferOffset>,
    selection: &Range<MultiBufferOffset>,
) -> bool {
    if selection.start == selection.end {
        range.contains(&selection.start)
    } else {
        range.start < selection.end && selection.start < range.end
    }
}

fn visible_math_range(
    editor: &Editor,
    display_snapshot: &DisplaySnapshot,
) -> Option<Range<MultiBufferOffset>> {
    let visible_lines = editor.visible_line_count()?;
    let scroll_position = editor
        .scroll_manager
        .anchor()
        .scroll_position(display_snapshot);
    let padding = MATH_VISIBLE_PADDING_LINES;
    let start_row = DisplayRow(scroll_position.y.floor().max(0.0) as u32);
    let end_row = DisplayRow((scroll_position.y + visible_lines).ceil() as u32);
    let start_row = DisplayRow(start_row.0.saturating_sub(padding));
    let end_row = DisplayRow(end_row.0.saturating_add(padding));
    let start_point = display_snapshot.clip_point(DisplayPoint::new(start_row, 0), Bias::Left);
    let end_point = display_snapshot.clip_point(DisplayPoint::new(end_row, 0), Bias::Right);
    Some(
        start_point.to_offset(display_snapshot, Bias::Left)
            ..end_point.to_offset(display_snapshot, Bias::Right),
    )
}

async fn render_math_previews(
    render_context: &RenderContext,
    snapshot: multi_buffer::MultiBufferSnapshot,
    visible_range: Range<MultiBufferOffset>,
    preview_settings_by_language: HashMap<Option<LanguageName>, MathPreviewSettings>,
    default_preview_settings: MathPreviewSettings,
    mut render_cache: HashMap<u64, RenderedSvg>,
) -> (Vec<MathPreviewEntry>, HashMap<u64, RenderedSvg>) {
    let mut requests = Vec::new();
    for (range, fragment, buffer_snapshot) in snapshot.math_fragments(visible_range.clone()) {
        let expanded_range = expand_math_range(range.clone(), &snapshot);
        if expanded_range == range {
            continue;
        }
        if !ranges_overlap(&expanded_range, &visible_range) {
            continue;
        }
        let language_name = buffer_snapshot.language_at(fragment.range.start).map(|lang| lang.name());
        let preview_settings = preview_settings_by_language
            .get(&language_name)
            .copied()
            .unwrap_or(default_preview_settings);
        if !preview_settings.inline_enabled && !preview_settings.popover_enabled {
            continue;
        }
        let content = buffer_snapshot
            .text_for_range(fragment.range.clone())
            .collect::<String>();
        if content.trim().is_empty() {
            continue;
        }
        requests.push(RenderRequest {
            range: expanded_range,
            kind: fragment.kind,
            backend: fragment.backend,
            content: wrap_math_fragment(fragment.kind, fragment.backend, &content),
            inline_enabled: preview_settings.inline_enabled,
            popover_enabled: preview_settings.popover_enabled,
        });
    }

    let mut by_backend: HashMap<RenderBackend, Vec<(usize, u64, String)>> = HashMap::default();
    for (idx, request) in requests.iter().enumerate() {
        let key = render_key(request);
        by_backend
            .entry(to_render_backend(request.backend))
            .or_default()
            .push((idx, key, request.content.clone()));
    }

    let mut rendered = vec![None; requests.len()];
    for (backend, entries) in by_backend {
        let mut to_render = Vec::new();
        let mut index_iter = Vec::new();
        for (idx, key, content) in entries {
            if let Some(cached) = render_cache.get(&key) {
                rendered[idx] = Some(cached.clone());
            } else {
                to_render.push(content);
                index_iter.push((idx, key));
            }
        }
        if to_render.is_empty() {
            continue;
        }
        let mut index_iter = index_iter.into_iter();
        let mut stream = render_context.render_batch(backend, to_render);
        while let Some(result) = stream.next().await {
            let Some(idx) = index_iter.next() else {
                break;
            };
            let Ok((path, output)) = result else {
                continue;
            };
            if !output.status.success() || !path.is_file() {
                continue;
            }
            let size = svg_size(&path).unwrap_or_else(default_svg_size);
            let key = idx.1;
            let rendered_svg = RenderedSvg {
                path: path.to_string_lossy().into_owned().into(),
                size,
            };
            render_cache.insert(key, rendered_svg.clone());
            rendered[idx.0] = Some(rendered_svg);
        }
    }

    let mut entries = Vec::new();
    for (request, rendered) in requests.into_iter().zip(rendered) {
        let Some(rendered) = rendered else {
            continue;
        };
        entries.push(MathPreviewEntry {
            range: request.range,
            kind: request.kind,
            _backend: request.backend,
            svg_path: rendered.path,
            size: rendered.size,
            inline_enabled: request.inline_enabled,
            popover_enabled: request.popover_enabled,
        });
    }

    entries.sort_by_key(|entry| entry.range.start.0);

    (entries, render_cache)
}

fn wrap_math_fragment(
    kind: MathPreviewKind,
    backend: MathPreviewBackend,
    content: &str,
) -> String {
    match backend {
        MathPreviewBackend::Latex => match kind {
            MathPreviewKind::Inline => format!("${}$", content),
            MathPreviewKind::Block => format!("\\[\n{}\n\\]", content),
        },
        MathPreviewBackend::Typst => match kind {
            MathPreviewKind::Inline => format!("${}$", content),
            MathPreviewKind::Block => format!("$\n{}\n$", content),
        },
    }
}

fn to_render_backend(backend: MathPreviewBackend) -> RenderBackend {
    match backend {
        MathPreviewBackend::Latex => RenderBackend::LaTeX,
        MathPreviewBackend::Typst => RenderBackend::Typst,
    }
}

fn default_svg_size() -> Size<Pixels> {
    size(px(16.), px(16.))
}

fn svg_size(path: &Path) -> Option<Size<Pixels>> {
    let contents = std::fs::read_to_string(path).ok()?;
    let tag_end = contents.find('>').unwrap_or(contents.len());
    let tag = &contents[..tag_end];
    let width = attribute_value(tag, width_re()).and_then(parse_svg_length);
    let height = attribute_value(tag, height_re()).and_then(parse_svg_length);
    if let (Some(width), Some(height)) = (width, height) {
        return Some(size(width, height));
    }

    let view_box = attribute_value(tag, view_box_re())?;
    let mut parts = view_box
        .split(|c: char| c == ' ' || c == ',')
        .filter(|part| !part.is_empty());
    let _min_x = parts.next()?.parse::<f32>().ok()?;
    let _min_y = parts.next()?.parse::<f32>().ok()?;
    let width = parts.next()?.parse::<f32>().ok()?;
    let height = parts.next()?.parse::<f32>().ok()?;
    Some(size(px(width), px(height)))
}

fn attribute_value<'a>(tag: &'a str, re: &Regex) -> Option<&'a str> {
    let caps = re.captures(tag)?;
    caps.get(1).map(|capture| capture.as_str())
}

fn width_re() -> &'static Regex {
    static WIDTH_RE: OnceLock<Regex> = OnceLock::new();
    WIDTH_RE.get_or_init(|| Regex::new(r#"(?i)\bwidth\s*=\s*["']([^"']+)["']"#).unwrap())
}

fn height_re() -> &'static Regex {
    static HEIGHT_RE: OnceLock<Regex> = OnceLock::new();
    HEIGHT_RE.get_or_init(|| Regex::new(r#"(?i)\bheight\s*=\s*["']([^"']+)["']"#).unwrap())
}

fn view_box_re() -> &'static Regex {
    static VIEWBOX_RE: OnceLock<Regex> = OnceLock::new();
    VIEWBOX_RE.get_or_init(|| Regex::new(r#"(?i)\bviewBox\s*=\s*["']([^"']+)["']"#).unwrap())
}

fn parse_svg_length(value: &str) -> Option<Pixels> {
    let value = value.trim();
    let end = value
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(end);
    let number = number.parse::<f32>().ok()?;
    let px_value = match unit.trim() {
        "" | "px" => number,
        "pt" => number * (4.0 / 3.0),
        "in" => number * 96.0,
        "cm" => number * (96.0 / 2.54),
        "mm" => number * (96.0 / 25.4),
        _ => return None,
    };
    Some(px(px_value))
}

fn expand_math_range(
    range: Range<MultiBufferOffset>,
    snapshot: &multi_buffer::MultiBufferSnapshot,
) -> Range<MultiBufferOffset> {
    let len = snapshot.len().0;
    let mut start = range.start.0.min(len);
    let mut end = range.end.0.min(len);
    if start >= end {
        return MultiBufferOffset(start)..MultiBufferOffset(end);
    }

    let scan_start = start.saturating_sub(MATH_RANGE_SCAN_BYTES);
    let scan_end = (end + MATH_RANGE_SCAN_BYTES).min(len);
    let context: String = snapshot
        .text_for_range(MultiBufferOffset(scan_start)..MultiBufferOffset(scan_end))
        .collect();
    let rel_start = start - scan_start;
    let rel_end = rel_start + (end - start);

    let (prefix_range, suffix_range) = find_delimiters(&context, rel_start, rel_end);
    if prefix_range.is_none() || suffix_range.is_none() {
        return MultiBufferOffset(start)..MultiBufferOffset(end);
    }
    let (prefix_start, _prefix_len) = prefix_range.unwrap();
    let (suffix_start, suffix_len) = suffix_range.unwrap();
    start = scan_start + prefix_start;
    end = (scan_start + suffix_start + suffix_len).min(len);

    MultiBufferOffset(start)..MultiBufferOffset(end)
}

fn find_delimiters(
    context: &str,
    rel_start: usize,
    rel_end: usize,
) -> (Option<(usize, usize)>, Option<(usize, usize)>) {
    let bytes = context.as_bytes();
    let prefix_candidates: &[&[u8]] = &[b"$$", br"\[", br"\(", b"$"];
    let suffix_candidates: &[&[u8]] = &[b"$$", br"\]", br"\)", b"$"];
    let mut prefix = None;
    let mut idx = rel_start.min(bytes.len());
    while idx > 0 && bytes[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }
    for candidate in prefix_candidates {
        if idx >= candidate.len()
            && &bytes[idx - candidate.len()..idx] == *candidate
        {
            prefix = Some((idx - candidate.len(), candidate.len()));
            break;
        }
    }

    let mut suffix = None;
    let mut idx = rel_end.min(bytes.len());
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    for candidate in suffix_candidates {
        if idx + candidate.len() <= bytes.len()
            && &bytes[idx..idx + candidate.len()] == *candidate
        {
            suffix = Some((idx, candidate.len()));
            break;
        }
    }

    (prefix, suffix)
}

pub(crate) fn scale_svg_to_height(svg_size: Size<Pixels>, target_height: Pixels) -> Size<Pixels> {
    if svg_size.height == Pixels::ZERO {
        return svg_size;
    }
    let scale = target_height / svg_size.height;
    size(svg_size.width * scale, svg_size.height * scale)
}

fn render_key(request: &RenderRequest) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_kind(request.kind, &mut hasher);
    hash_backend(request.backend, &mut hasher);
    request.content.hash(&mut hasher);
    hasher.finish()
}

fn hash_kind(kind: MathPreviewKind, hasher: &mut std::collections::hash_map::DefaultHasher) {
    use std::hash::Hash;
    let discriminant = match kind {
        MathPreviewKind::Inline => 0u8,
        MathPreviewKind::Block => 1u8,
    };
    discriminant.hash(hasher);
}

fn hash_backend(
    backend: MathPreviewBackend,
    hasher: &mut std::collections::hash_map::DefaultHasher,
) {
    use std::hash::Hash;
    let discriminant = match backend {
        MathPreviewBackend::Latex => 0u8,
        MathPreviewBackend::Typst => 1u8,
    };
    discriminant.hash(hasher);
}
