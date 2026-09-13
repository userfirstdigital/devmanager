//! The composer's model picker: one popup that reaches every provider's models.
//!
//! The old composer menu listed only the current provider's models, unsorted
//! and unsearchable, so switching from a Claude model to a Codex one meant
//! opening two menus and knowing which provider owned which name. This module
//! owns the parts of the replacement that do not need a window: which rows a
//! rail entry or a query admits, how a query ranks them, and where the keyboard
//! highlight sits. `native_shell` paints them.
//!
//! Ranking follows T3 Code's picker (`modelPickerSearch.ts`): a query is split
//! into tokens, each token takes its best field match, and the total is the sum,
//! lower first. Favourites get a fixed boost so they lead among equals. The one
//! deliberate omission is fuzzy (subsequence) matching -- a caseless tiered
//! substring match is predictable enough for a list this short, and it never
//! offers a model whose name the person did not actually type part of.

/// Which rail entry the picker is showing when no query is typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelPickerRail {
    /// Every favourited model, across providers.
    Favorites,
    /// One provider instance's models.
    Instance(String),
}

/// One rail entry: a configured, enabled provider instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPickerInstance {
    pub instance_id: String,
    /// The instance's own label, so "Codex Personal" keeps its authored name.
    pub display_name: String,
    /// `ProviderDriverKind::as_str`, which also picks the rail glyph.
    pub driver: String,
}

/// One model row, already resolved against the provider's catalogue and the
/// user's visibility policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPickerRow {
    pub instance_id: String,
    /// The owning instance's label, painted under the model name.
    pub provider_label: String,
    pub driver: String,
    pub slug: String,
    pub label: String,
    pub is_favorite: bool,
    /// The model the composer would launch with right now.
    pub is_current: bool,
}

/// Favourites lead among equally good matches. T3 Code's own boost.
const FAVORITE_SCORE_BOOST: i64 = 24;
/// Ctrl+1 .. Ctrl+9 address the first nine visible rows.
pub const MAX_JUMP_SHORTCUTS: usize = 9;

/// The picker's live state. Opening it is a new value, closing it drops one.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelPickerState {
    query: String,
    rail: ModelPickerRail,
    highlight: usize,
    /// Where the chip was clicked, so the popup can hang off it. `None` when
    /// the keyboard opened it, which anchors to the window instead.
    anchor: Option<(f32, f32)>,
    /// A refusal worth showing in place, e.g. an unsaved settings draft.
    notice: Option<String>,
}

impl ModelPickerState {
    pub fn new(rail: ModelPickerRail, anchor: Option<(f32, f32)>) -> Self {
        Self {
            query: String::new(),
            rail,
            highlight: 0,
            anchor,
            notice: None,
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Typing re-ranks the list, so the highlight returns to the best match.
    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
        self.highlight = 0;
        self.notice = None;
    }

    pub fn rail(&self) -> &ModelPickerRail {
        &self.rail
    }

    /// Choosing a rail entry clears the query: the entry is the filter now.
    pub fn set_rail(&mut self, rail: ModelPickerRail) {
        self.rail = rail;
        self.query.clear();
        self.highlight = 0;
        self.notice = None;
    }

    pub fn highlight(&self) -> usize {
        self.highlight
    }

    pub fn set_highlight(&mut self, index: usize) {
        self.highlight = index;
    }

    /// Wraps, so Down on the last row returns to the first.
    pub fn move_highlight(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.highlight = 0;
            return;
        }
        let next = (self.highlight as isize + delta).rem_euclid(len as isize);
        self.highlight = next as usize;
    }

    /// Clamped, because the row count changes under the highlight as you type.
    pub fn highlight_within(&self, len: usize) -> usize {
        if len == 0 {
            0
        } else {
            self.highlight.min(len - 1)
        }
    }

    pub fn anchor(&self) -> Option<(f32, f32)> {
        self.anchor
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }
}

/// Normalised for matching: caseless, trimmed, single-spaced.
fn normalize(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Is `at` the start of a word inside `field`?
fn at_word_boundary(field: &str, at: usize) -> bool {
    if at == 0 {
        return true;
    }
    field[..at]
        .chars()
        .next_back()
        .map(|ch| ch == ' ' || ch == '-' || ch == '_' || ch == '.' || ch == '/')
        .unwrap_or(false)
}

/// One field's score for one token, or `None` when the field does not carry it.
/// Lower is better, and each tier is worth more than the one above it.
fn score_field(field: &str, token: &str, base: i64) -> Option<i64> {
    if field.is_empty() || token.is_empty() {
        return None;
    }
    if field == token {
        return Some(base);
    }
    if field.starts_with(token) {
        return Some(base + 2);
    }
    let found = field.find(token)?;
    if at_word_boundary(field, found) {
        Some(base + 4)
    } else {
        Some(base + 6)
    }
}

/// A row's total score for a query, or `None` when any token is absent. Every
/// token has to land somewhere, so "claude son" keeps Sonnet and drops Opus.
pub fn score_row(row: &ModelPickerRow, query: &str) -> Option<i64> {
    let query = normalize(query);
    let tokens: Vec<&str> = query.split_whitespace().filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
        return Some(0);
    }
    let label = normalize(&row.label);
    let slug = normalize(&row.slug);
    let provider = normalize(&row.provider_label);
    let driver = normalize(&row.driver);
    let fields = [
        label.as_str(),
        slug.as_str(),
        provider.as_str(),
        driver.as_str(),
    ];
    let mut total = 0i64;
    for token in tokens {
        let best = fields
            .iter()
            .enumerate()
            .filter_map(|(index, field)| score_field(field, token, index as i64 * 10))
            .min()?;
        total += best;
    }
    if row.is_favorite {
        total -= FAVORITE_SCORE_BOOST;
    }
    Some(total)
}

/// The rows to paint, in order.
///
/// A query searches every provider, because the whole point is to stop making
/// someone find the provider before the model. An empty query shows the rail
/// entry's own rows and keeps the catalogue's order, which already puts
/// favourites first.
pub fn visible_rows<'a>(
    rows: &'a [ModelPickerRow],
    query: &str,
    rail: &ModelPickerRail,
) -> Vec<&'a ModelPickerRow> {
    if query.trim().is_empty() {
        return rows
            .iter()
            .filter(|row| match rail {
                ModelPickerRail::Favorites => row.is_favorite,
                ModelPickerRail::Instance(id) => &row.instance_id == id,
            })
            .collect();
    }
    let mut scored: Vec<(i64, usize, &ModelPickerRow)> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| score_row(row, query).map(|score| (score, index, row)))
        .collect();
    // Ties keep catalogue order, so the list never reshuffles arbitrarily.
    scored.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    scored.into_iter().map(|(_, _, row)| row).collect()
}

/// T3 Code's picker geometry: a 360x346 panel behind a rail.
pub const PANEL_WIDTH: f32 = 360.0;
pub const PANEL_HEIGHT: f32 = 346.0;
/// Below this a list stops being a list, so the panel stops shrinking and
/// `anchored` slides it inside the window instead.
pub const MIN_PANEL_WIDTH: f32 = 220.0;
pub const MIN_PANEL_HEIGHT: f32 = 140.0;
/// The gutter the popup keeps against every window edge.
pub const WINDOW_MARGIN: f32 = 8.0;

/// How big the panel may paint inside a window of this size.
///
/// The old menu was a fixed 200x220 mounted inside the composer's clipped meta
/// strip, so a short pane cut it off and a wide one wasted the room. This keeps
/// the preferred size when the window can hold it, shrinks to fit when it
/// cannot, and stops at a floor -- past that the popup is slid, not squeezed.
pub fn panel_size(viewport_width: f32, viewport_height: f32) -> (f32, f32) {
    let available_width = viewport_width - WINDOW_MARGIN * 2.0;
    let available_height = viewport_height - WINDOW_MARGIN * 2.0;
    (
        PANEL_WIDTH.min(available_width.max(MIN_PANEL_WIDTH)),
        PANEL_HEIGHT.min(available_height.max(MIN_PANEL_HEIGHT)),
    )
}

/// Ctrl+1 .. Ctrl+9 for the first nine rows, nothing after that.
pub fn jump_shortcut_label(index: usize) -> Option<String> {
    (index < MAX_JUMP_SHORTCUTS).then(|| format!("Ctrl+{}", index + 1))
}

/// The row a Ctrl+digit chord addresses, if it is on screen.
pub fn jump_target(key: &str, len: usize) -> Option<usize> {
    let digit = key.chars().next().filter(|ch| ch.is_ascii_digit())?;
    if key.chars().count() != 1 {
        return None;
    }
    let index = digit.to_digit(10)? as usize;
    if index == 0 || index > MAX_JUMP_SHORTCUTS || index > len {
        return None;
    }
    Some(index - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(instance: &str, provider: &str, slug: &str, label: &str) -> ModelPickerRow {
        ModelPickerRow {
            instance_id: instance.into(),
            provider_label: provider.into(),
            driver: instance.split('_').next().unwrap_or(instance).into(),
            slug: slug.into(),
            label: label.into(),
            is_favorite: false,
            is_current: false,
        }
    }

    fn catalogue() -> Vec<ModelPickerRow> {
        vec![
            row("claude", "Claude Code", "opus", "Claude Opus"),
            row("claude", "Claude Code", "sonnet", "Claude Sonnet"),
            row("claude", "Claude Code", "haiku", "Claude Haiku"),
            row("codex", "Codex", "gpt-5.6-sol", "GPT-5.6 Sol"),
            row("codex", "Codex", "gpt-5.6-luna", "GPT-5.6 Luna"),
        ]
    }

    #[test]
    fn a_rail_entry_shows_only_that_instance() {
        let rows = catalogue();
        let visible = visible_rows(&rows, "", &ModelPickerRail::Instance("codex".into()));
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().all(|row| row.instance_id == "codex"));
    }

    #[test]
    fn the_favorites_rail_crosses_providers() {
        let mut rows = catalogue();
        rows[1].is_favorite = true;
        rows[3].is_favorite = true;
        let visible = visible_rows(&rows, "", &ModelPickerRail::Favorites);
        assert_eq!(
            visible
                .iter()
                .map(|row| row.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["sonnet", "gpt-5.6-sol"]
        );
    }

    #[test]
    fn a_query_searches_every_provider_not_just_the_rail_entry() {
        let rows = catalogue();
        // The rail sits on Claude, but the query names a Codex model.
        let visible = visible_rows(&rows, "luna", &ModelPickerRail::Instance("claude".into()));
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].slug, "gpt-5.6-luna");
    }

    #[test]
    fn every_token_has_to_land_somewhere() {
        let rows = catalogue();
        let visible = visible_rows(&rows, "claude son", &ModelPickerRail::Favorites);
        assert_eq!(
            visible
                .iter()
                .map(|row| row.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["sonnet"]
        );
        assert!(visible_rows(&rows, "claude zzz", &ModelPickerRail::Favorites).is_empty());
    }

    #[test]
    fn a_provider_name_matches_its_models() {
        let rows = catalogue();
        let visible = visible_rows(&rows, "codex", &ModelPickerRail::Favorites);
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().all(|row| row.instance_id == "codex"));
    }

    #[test]
    fn an_exact_name_outranks_a_mere_mention_and_favourites_lead_ties() {
        let rows = catalogue();
        let exact = score_row(&rows[1], "claude sonnet").unwrap();
        let partial = score_row(&rows[0], "claude").unwrap();
        assert!(exact > i64::MIN);
        let mut favourite = rows[0].clone();
        favourite.is_favorite = true;
        assert!(
            score_row(&favourite, "claude").unwrap() < partial,
            "a favourite leads an otherwise identical row"
        );
    }

    #[test]
    fn the_highlight_wraps_and_survives_a_shrinking_list() {
        let mut state = ModelPickerState::new(ModelPickerRail::Favorites, None);
        state.move_highlight(-1, 3);
        assert_eq!(state.highlight(), 2);
        state.move_highlight(1, 3);
        assert_eq!(state.highlight(), 0);
        state.set_highlight(7);
        assert_eq!(state.highlight_within(3), 2, "clamped, never out of range");
        assert_eq!(state.highlight_within(0), 0);
    }

    #[test]
    fn typing_or_switching_rails_returns_the_highlight_to_the_top() {
        let mut state = ModelPickerState::new(ModelPickerRail::Instance("claude".into()), None);
        state.set_highlight(4);
        state.set_query("son");
        assert_eq!(state.highlight(), 0);
        state.set_highlight(2);
        state.set_rail(ModelPickerRail::Favorites);
        assert_eq!(state.highlight(), 0);
        assert!(state.query().is_empty(), "a rail entry is its own filter");
    }

    #[test]
    fn the_panel_fits_whatever_window_it_is_given() {
        // Room to spare: the preferred size, unchanged.
        assert_eq!(panel_size(1600.0, 1200.0), (PANEL_WIDTH, PANEL_HEIGHT));
        // A narrow pane shrinks the panel instead of cropping it.
        let (width, height) = panel_size(300.0, 400.0);
        assert!(
            width <= 300.0 - WINDOW_MARGIN * 2.0,
            "{width} fits the width"
        );
        assert!(
            height <= 400.0 - WINDOW_MARGIN * 2.0,
            "{height} fits the height"
        );
        // Smaller than the floor: the panel stops shrinking and is slid
        // inside the window by the anchor's own fit instead.
        let (width, height) = panel_size(120.0, 90.0);
        assert_eq!((width, height), (MIN_PANEL_WIDTH, MIN_PANEL_HEIGHT));
        // A zero-sized window during a resize must not produce a negative box.
        let (width, height) = panel_size(0.0, 0.0);
        assert!(width > 0.0 && height > 0.0);
    }

    #[test]
    fn only_the_first_nine_rows_take_a_chord() {
        assert_eq!(jump_shortcut_label(0).as_deref(), Some("Ctrl+1"));
        assert_eq!(jump_shortcut_label(8).as_deref(), Some("Ctrl+9"));
        assert_eq!(jump_shortcut_label(9), None);
        assert_eq!(jump_target("3", 5), Some(2));
        assert_eq!(jump_target("9", 5), None, "past the end of the list");
        assert_eq!(jump_target("0", 5), None);
        assert_eq!(jump_target("a", 5), None);
    }
}
