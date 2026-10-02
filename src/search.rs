//! Vim-style text search (`/`, `n`, `N`, Ctrl+F) over pdfium's page text.
//!
//! Each page's text is indexed once into normalized strings (whitespace runs
//! collapsed, plus a lowercased copy) with a byte -> char map. Queries run a
//! SIMD substring search (`memchr::memmem`) over those, and only pages whose
//! text arrived since the last query are scanned on later frames.

use std::collections::{BTreeMap, HashMap};

use eframe::egui::{self, Key, Rect, pos2};

/// Id of the search field.
pub const SEARCH_FIELD: &str = "search-field";
use memchr::memmem;

use crate::annot::model::Pt;
use crate::app::App;
use crate::pdf::worker::TextChar;

/// Normalized text of one page.
struct PageIndex {
    /// Original case, whitespace runs collapsed to one space.
    exact: String,
    lower: String,
    /// For each byte of `exact` / `lower`: index of the source char in the page's chars.
    exact_map: Vec<u32>,
    lower_map: Vec<u32>,
}

impl PageIndex {
    fn new(chars: &[TextChar]) -> Self {
        let mut idx = PageIndex { exact: String::new(), lower: String::new(), exact_map: Vec::new(), lower_map: Vec::new() };
        let mut last_space = true;
        for (i, c) in chars.iter().enumerate() {
            let ch = if c.ch.is_whitespace() || c.ch.is_control() { ' ' } else { c.ch };
            if ch == ' ' && last_space {
                continue;
            }
            last_space = ch == ' ';
            let before = idx.exact.len();
            idx.exact.push(ch);
            idx.exact_map.extend(std::iter::repeat_n(i as u32, idx.exact.len() - before));
            let before = idx.lower.len();
            idx.lower.extend(ch.to_lowercase());
            idx.lower_map.extend(std::iter::repeat_n(i as u32, idx.lower.len() - before));
        }
        idx
    }
}

/// A prepared query: normalized needle plus case mode.
pub struct Query {
    needle: String,
    /// Smartcase: an uppercase letter in the query makes it case-sensitive.
    exact: bool,
}

impl Query {
    pub fn new(q: &str) -> Option<Query> {
        let needle = q.split_whitespace().collect::<Vec<_>>().join(" ");
        if needle.is_empty() {
            return None;
        }
        let exact = needle.chars().any(char::is_uppercase);
        let needle = if exact { needle } else { needle.to_lowercase() };
        Some(Query { needle, exact })
    }

    fn find(&self, finder: &memmem::Finder, idx: &PageIndex, chars: &[TextChar]) -> Vec<Vec<[f32; 4]>> {
        let (hay, map) = if self.exact { (&idx.exact, &idx.exact_map) } else { (&idx.lower, &idx.lower_map) };
        let len = self.needle.len();
        finder
            .find_iter(hay.as_bytes())
            .map(|start| {
                let (first, last) = (map[start] as usize, map[start + len - 1] as usize);
                line_rects(&chars[first..=last])
            })
            .collect()
    }
}

#[cfg(test)]
/// Finds occurrences of `query` in a page (for tests).
pub fn find_in_page(chars: &[TextChar], query: &str) -> Vec<Vec<[f32; 4]>> {
    let Some(q) = Query::new(query) else { return Vec::new() };
    let finder = memmem::Finder::new(q.needle.as_bytes());
    q.find(&finder, &PageIndex::new(chars), chars)
}

/// Union of character boxes, split where the text wraps to a new line.
fn line_rects(chars: &[TextChar]) -> Vec<[f32; 4]> {
    let mut rects: Vec<[f32; 4]> = Vec::new();
    for c in chars.iter().filter(|c| c.rect[2] > c.rect[0]) {
        let r = c.rect;
        match rects.last_mut() {
            Some(l) if (r[1] + r[3]) / 2.0 > l[1] && (r[1] + r[3]) / 2.0 < l[3] => {
                *l = [l[0].min(r[0]), l[1].min(r[1]), l[2].max(r[2]), l[3].max(r[3])];
            }
            _ => rects.push(r),
        }
    }
    rects
}

#[derive(Default)]
pub struct Search {
    pub open: bool,
    pub query: String,
    focus: bool,
    /// Matches per page (user-space rects per line), pages in order.
    by_page: BTreeMap<usize, Vec<Vec<[f32; 4]>>>,
    /// Flattened (page, index within page) in document order.
    pub order: Vec<(usize, usize)>,
    pub current: Option<usize>,
    indexes: HashMap<usize, PageIndex>,
    /// Query the matches are for, and how many pages have been scanned for it.
    searched_query: String,
    searched_pages: usize,
}

impl Search {
    /// Forgets results and page text (pages were reordered), keeping the query.
    pub fn reset_pages(&mut self) {
        *self = Search { open: self.open, query: std::mem::take(&mut self.query), ..Default::default() };
    }

    pub fn total(&self) -> usize {
        self.order.len()
    }

    pub(crate) fn current_rects(&self) -> Option<(usize, &Vec<[f32; 4]>)> {
        let (page, i) = *self.order.get(self.current?)?;
        Some((page, &self.by_page[&page][i]))
    }
}

impl App {
    /// `/`: start a new search. Ctrl+F: reopen, keeping the last query.
    pub fn open_search(&mut self, fresh: bool) {
        self.search.open = true;
        self.search.focus = true;
        if fresh {
            self.search.query.clear();
        }
    }

    pub fn close_search(&mut self) {
        let indexes = std::mem::take(&mut self.search.indexes);
        self.search = crate::search::Search { indexes, query: std::mem::take(&mut self.search.query), ..Default::default() };
    }

    /// Indexes newly extracted pages and scans the ones not yet searched for this query.
    fn update_matches(&mut self) {
        let s = &mut self.search;
        let query_changed = s.query != s.searched_query;
        if query_changed {
            s.searched_query = s.query.clone();
            s.by_page.clear();
            s.searched_pages = usize::MAX; // force a full scan below
        }
        for (&p, chars) in &self.text_chars {
            s.indexes.entry(p).or_insert_with(|| PageIndex::new(chars));
        }
        if !query_changed && s.searched_pages == s.indexes.len() {
            return;
        }
        let Some(q) = Query::new(&s.query) else {
            s.by_page.clear();
            s.order.clear();
            s.current = None;
            s.searched_pages = s.indexes.len();
            return;
        };
        let finder = memmem::Finder::new(q.needle.as_bytes());
        let mut changed = false;
        for (&p, idx) in &s.indexes {
            if s.by_page.contains_key(&p) {
                continue;
            }
            let found = q.find(&finder, idx, &self.text_chars[&p]);
            changed |= !found.is_empty();
            s.by_page.insert(p, found);
        }
        s.searched_pages = s.indexes.len();
        if changed || query_changed {
            let prev = s.current.and_then(|c| s.order.get(c).copied());
            s.order = s.by_page.iter().flat_map(|(&p, m)| (0..m.len()).map(move |i| (p, i))).collect();
            s.current = prev.and_then(|m| s.order.iter().position(|&o| o == m));
            if query_changed || s.current.is_none() {
                // Incsearch: jump to the first match at or after the current page.
                let here = self.view.current_page;
                s.current = s.order.iter().position(|&(p, _)| p >= here).or((!s.order.is_empty()).then_some(0));
                self.reveal_current_match();
            }
        }
    }

    /// `n` / `N`: next or previous match, wrapping around like Vim.
    pub fn search_step(&mut self, ctx: &egui::Context, step: i64) {
        let total = self.search.total();
        if total == 0 {
            if !self.search.query.is_empty() {
                let msg = format!("Pattern not found: {}", self.search.query);
                self.set_status(ctx, msg, true);
            }
            return;
        }
        let cur = self.search.current.map_or(if step > 0 { -1 } else { 0 }, |c| c as i64);
        let next = cur + step;
        if next >= total as i64 {
            self.set_status(ctx, "Search hit BOTTOM, continuing at TOP", false);
        } else if next < 0 {
            self.set_status(ctx, "Search hit TOP, continuing at BOTTOM", false);
        }
        self.search.current = Some(next.rem_euclid(total as i64) as usize);
        self.reveal_current_match();
    }

    pub fn search_bar(&mut self, ui: &mut egui::Ui) {
        if !self.search.open {
            return;
        }
        let n_pages = self.doc.as_ref().map_or(0, |d| d.pages.len());
        // Text extraction is lazy; ask for every page while searching. The worker
        // always renders pages before extracting text, so this never stalls the view.
        for p in 0..n_pages {
            self.request_text(p);
        }
        use crate::ui::chrome::icon_button;
        use egui_phosphor::regular as ph;
        ui.label(egui::RichText::new(ph::MAGNIFYING_GLASS).size(13.0).weak());
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.search.query)
                .id(egui::Id::new(SEARCH_FIELD))
                .desired_width(160.0)
                .hint_text("Search  (/ then Enter, n / N)"),
        );
        if std::mem::take(&mut self.search.focus) {
            r.request_focus();
            // Select the previous query, so typing replaces it.
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), r.id) {
                let all = egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(self.search.query.chars().count()),
                );
                state.cursor.set_char_range(Some(all));
                state.store(ui.ctx(), r.id);
            }
        }
        self.update_matches();
        let (enter, shift, escape) =
            ui.input(|i| (i.key_pressed(Key::Enter), i.modifiers.shift, i.key_pressed(Key::Escape)));
        let ctx = ui.ctx().clone();
        if r.lost_focus() && enter {
            // Like Vim: Enter confirms the search and returns to normal mode; n / N step.
            if shift {
                self.search_step(&ctx, -1);
            }
        }
        let total = self.search.total();
        if icon_button(ui, ph::CARET_UP, "Previous (N)", total > 0, false, 22.0).clicked() {
            self.search_step(&ctx, -1);
        }
        if icon_button(ui, ph::CARET_DOWN, "Next (n)", total > 0, false, 22.0).clicked() {
            self.search_step(&ctx, 1);
        }
        let loaded = self.text_chars.len();
        if total > 0 {
            let cur = self.search.current.map_or(0, |c| c + 1);
            ui.label(format!("{cur}/{total}"));
        } else if !self.search.query.trim().is_empty() {
            let note = if loaded < n_pages { "searching…" } else { "no matches" };
            ui.label(egui::RichText::new(note).weak());
        }
        if loaded < n_pages {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
        }
        if icon_button(ui, ph::X, "Close (Esc)", true, false, 22.0).clicked() || escape {
            self.close_search();
        }
    }

    fn reveal_current_match(&mut self) {
        if let Some((page, rects)) = self.search.current_rects()
            && let Some(r) = rects.first()
        {
            self.view.reveal = Some((page, Pt::new(r[0], r[3])));
        }
    }

    /// Paints match highlights on a page (screen space).
    pub fn paint_search(&self, painter: &egui::Painter, page: usize, to_screen: &crate::annot::geometry::Affine) {
        if !self.search.open {
            return;
        }
        let Some(matches) = self.search.by_page.get(&page) else { return };
        let current = self.search.current.and_then(|c| self.search.order.get(c)).copied();
        // Blue with an outline, so matches don't look like the yellow highlights
        // saved in the document; the current one is stronger.
        let accent = crate::ui::theme::ACCENT;
        for (i, rects) in matches.iter().enumerate() {
            let (fill, stroke) = if current == Some((page, i)) {
                (accent.gamma_multiply(0.35), egui::Stroke::new(2.0, accent))
            } else {
                (accent.gamma_multiply(0.12), egui::Stroke::new(1.0, accent.gamma_multiply(0.8)))
            };
            for r in rects {
                let a = to_screen.apply(Pt::new(r[0], r[1]));
                let b = to_screen.apply(Pt::new(r[2], r[3]));
                let rect = Rect::from_two_pos(pos2(a.x, a.y), pos2(b.x, b.y)).expand(1.5);
                painter.rect_filled(rect, 2.0, fill);
                painter.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Outside);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str, y: f32) -> Vec<TextChar> {
        s.chars()
            .enumerate()
            .map(|(i, ch)| TextChar { ch, rect: [i as f32 * 6.0, y, i as f32 * 6.0 + 5.0, y + 10.0] })
            .collect()
    }

    #[test]
    fn lowercase_query_ignores_case() {
        let c = chars("Hello world, hello again", 0.0);
        let m = find_in_page(&c, "hello");
        assert_eq!(m.len(), 2);
        assert_eq!(m[1][0][0], 13.0 * 6.0);
    }

    #[test]
    fn uppercase_query_is_case_sensitive() {
        let c = chars("Hello world, hello again", 0.0);
        assert_eq!(find_in_page(&c, "Hello").len(), 1);
    }

    #[test]
    fn match_across_lines_gives_one_rect_per_line() {
        let mut c = chars("end of", 100.0);
        c.push(TextChar { ch: '\r', rect: [0.0; 4] });
        c.push(TextChar { ch: '\n', rect: [0.0; 4] });
        c.extend(chars("line", 80.0));
        // pdfium emits CR LF at line breaks; a space in the query matches them.
        let m = find_in_page(&c, "of line");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].len(), 2);
    }

    #[test]
    fn non_ascii_text_maps_back_to_the_right_chars() {
        // 'İ' lowercases to two chars; 'é' is two UTF-8 bytes.
        let c = chars("İstanbul café Café", 0.0);
        let m = find_in_page(&c, "café");
        assert_eq!(m.len(), 2);
        assert_eq!(m[1][0][0], 14.0 * 6.0);
        assert_eq!(m[1][0][2], 17.0 * 6.0 + 5.0);
    }

    #[test]
    fn large_document_search_is_fast() {
        let page: Vec<TextChar> = chars(&"The quick brown fox jumps over the lazy dog. ".repeat(80), 0.0);
        let idx = PageIndex::new(&page);
        let q = Query::new("lazy dog").unwrap();
        let finder = memmem::Finder::new(q.needle.as_bytes());
        let start = std::time::Instant::now();
        let mut n = 0;
        for _ in 0..1000 {
            // 1000 pages of ~3600 chars each.
            n += q.find(&finder, &idx, &page).len();
        }
        assert_eq!(n, 80_000);
        assert!(start.elapsed().as_millis() < 500, "took {:?}", start.elapsed());
    }
}
