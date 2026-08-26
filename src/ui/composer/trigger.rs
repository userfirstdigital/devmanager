//! Trigger and menu derivation for `@` / `$` / `/` composer menus.

use super::segments::{ComposerCursor, PromptDocument, PromptSegment};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    File,
    Skill,
    Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTrigger {
    pub kind: TriggerKind,
    pub query: String,
    pub range: std::ops::Range<usize>,
    pub text_segment_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerSuggestion {
    pub label: String,
    pub insert: PromptSegment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerMenuState {
    pub trigger: ActiveTrigger,
    pub selected_index: usize,
    pub suggestions: Vec<TriggerSuggestion>,
}

impl TriggerMenuState {
    pub fn move_selection(&mut self, delta: isize) {
        let count = self.suggestions.len() as isize;
        if count == 0 {
            self.selected_index = 0;
            return;
        }
        let current = self.selected_index.min(self.suggestions.len() - 1) as isize;
        self.selected_index = ((current + delta).rem_euclid(count)) as usize;
    }

    pub fn selected(&self) -> Option<&TriggerSuggestion> {
        self.suggestions.get(self.selected_index)
    }
}

/// Detect an active trigger immediately before the expanded cursor.
pub fn detect_trigger(document: &PromptDocument, cursor: ComposerCursor) -> Option<ActiveTrigger> {
    let PromptSegment::Text(text) = document.segments.get(cursor.segment_index)? else {
        return None;
    };
    let prefix = &text[..cursor.offset_in_segment.min(text.len())];
    let (kind, start_char) = if let Some(at) = prefix.rfind('@') {
        if token_boundary(prefix, at) {
            (TriggerKind::File, at)
        } else {
            return None;
        }
    } else if let Some(dollar) = prefix.rfind('$') {
        if token_boundary(prefix, dollar) {
            (TriggerKind::Skill, dollar)
        } else {
            return None;
        }
    } else if let Some(slash) = prefix.rfind('/') {
        if token_boundary(prefix, slash) {
            (TriggerKind::Command, slash)
        } else {
            return None;
        }
    } else {
        return None;
    };
    let query = prefix[start_char + 1..].to_string();
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    let segment_start = expanded_offset_before_segment(document, cursor.segment_index);
    Some(ActiveTrigger {
        kind,
        query,
        range: (segment_start + start_char)..(segment_start + cursor.offset_in_segment),
        text_segment_index: cursor.segment_index,
    })
}

fn token_boundary(prefix: &str, index: usize) -> bool {
    index == 0
        || prefix[..index]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_whitespace())
}

fn expanded_offset_before_segment(document: &PromptDocument, segment_index: usize) -> usize {
    document
        .segments
        .iter()
        .take(segment_index)
        .map(|segment| segment.serialize_provider_text().len())
        .sum()
}

pub fn filter_suggestions<'a>(
    kind: TriggerKind,
    query: &str,
    candidates: &'a [TriggerSuggestion],
) -> Vec<&'a TriggerSuggestion> {
    let needle = query.to_ascii_lowercase();
    candidates
        .iter()
        .filter(|suggestion| {
            if suggestion_kind(suggestion) != kind {
                return false;
            }
            needle.is_empty()
                || suggestion.label.to_ascii_lowercase().contains(&needle)
                || suggestion
                    .insert
                    .serialize_provider_text()
                    .to_ascii_lowercase()
                    .contains(&needle)
        })
        .collect()
}

fn suggestion_kind(suggestion: &TriggerSuggestion) -> TriggerKind {
    match suggestion.insert {
        PromptSegment::FileRef { .. } => TriggerKind::File,
        PromptSegment::SkillRef { .. } => TriggerKind::Skill,
        PromptSegment::CommandRef { .. } => TriggerKind::Command,
        PromptSegment::Text(_) => TriggerKind::Command,
    }
}

pub fn apply_suggestion(
    document: &mut PromptDocument,
    trigger: &ActiveTrigger,
    suggestion: &TriggerSuggestion,
) -> bool {
    let Some(PromptSegment::Text(text)) =
        document.segments.get(trigger.text_segment_index).cloned()
    else {
        return false;
    };
    let segment_start = expanded_offset_before_segment(document, trigger.text_segment_index);
    let local = (trigger.range.start.saturating_sub(segment_start))
        ..(trigger.range.end.saturating_sub(segment_start));
    if local.end > text.len() {
        return false;
    }
    document.replace_text_range_with_segment(
        trigger.text_segment_index,
        local,
        suggestion.insert.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_file_skill_and_command_triggers() {
        let document = PromptDocument::from_plain_text(None, "open @src/");
        let cursor =
            ComposerCursor::from_expanded(&document, document.serialize_provider_text().len());
        let trigger = detect_trigger(&document, cursor).expect("file trigger");
        assert_eq!(trigger.kind, TriggerKind::File);
        assert_eq!(trigger.query, "src/");

        let document = PromptDocument::from_plain_text(None, "use $rev");
        let cursor =
            ComposerCursor::from_expanded(&document, document.serialize_provider_text().len());
        assert_eq!(
            detect_trigger(&document, cursor).map(|trigger| trigger.kind),
            Some(TriggerKind::Skill)
        );

        let document = PromptDocument::from_plain_text(None, "/hel");
        let cursor =
            ComposerCursor::from_expanded(&document, document.serialize_provider_text().len());
        assert_eq!(
            detect_trigger(&document, cursor).map(|trigger| trigger.kind),
            Some(TriggerKind::Command)
        );
    }

    #[test]
    fn filters_case_insensitively_and_applies_selection() {
        let candidates = vec![
            TriggerSuggestion {
                label: "mod.rs".into(),
                insert: PromptSegment::FileRef {
                    relative_path: "src/mod.rs".into(),
                    is_directory: false,
                },
            },
            TriggerSuggestion {
                label: "Main.rs".into(),
                insert: PromptSegment::FileRef {
                    relative_path: "src/Main.rs".into(),
                    is_directory: false,
                },
            },
        ];
        let filtered = filter_suggestions(TriggerKind::File, "main", &candidates);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].label, "Main.rs");

        let mut document = PromptDocument::from_plain_text(None, "see @ma");
        let cursor =
            ComposerCursor::from_expanded(&document, document.serialize_provider_text().len());
        let trigger = detect_trigger(&document, cursor).expect("trigger");
        assert!(apply_suggestion(&mut document, &trigger, filtered[0]));
        assert_eq!(document.serialize_provider_text(), "see @src/Main.rs");
    }

    #[test]
    fn menu_keyboard_wraps() {
        let mut menu = TriggerMenuState {
            trigger: ActiveTrigger {
                kind: TriggerKind::Command,
                query: String::new(),
                range: 0..1,
                text_segment_index: 0,
            },
            selected_index: 0,
            suggestions: vec![
                TriggerSuggestion {
                    label: "a".into(),
                    insert: PromptSegment::CommandRef {
                        command: "/a".into(),
                    },
                },
                TriggerSuggestion {
                    label: "b".into(),
                    insert: PromptSegment::CommandRef {
                        command: "/b".into(),
                    },
                },
            ],
        };
        menu.move_selection(-1);
        assert_eq!(menu.selected_index, 1);
    }
}
