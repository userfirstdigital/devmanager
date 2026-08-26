//! Closed PromptSegment vocabulary and serialization for the native composer.

use crate::domain::id::TaskId;

/// Closed composer segment vocabulary. Identity is stable across expand/collapse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSegment {
    Text(String),
    /// `@` file or folder mention. `relative_path` is workspace-relative.
    FileRef {
        relative_path: String,
        is_directory: bool,
    },
    /// `$` skill insertion. Catalog identity is the skill name only.
    SkillRef {
        name: String,
    },
    /// `/` provider command. Serialized as the exact command token.
    CommandRef {
        command: String,
    },
}

impl PromptSegment {
    pub fn serialize_provider_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::FileRef {
                relative_path,
                is_directory,
            } => {
                if *is_directory {
                    format!("@{relative_path}/")
                } else {
                    format!("@{relative_path}")
                }
            }
            Self::SkillRef { name } => format!("${name}"),
            Self::CommandRef { command } => {
                if command.starts_with('/') {
                    command.clone()
                } else {
                    format!("/{command}")
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptDocument {
    pub task_id: Option<TaskId>,
    pub segments: Vec<PromptSegment>,
}

impl PromptDocument {
    pub fn from_plain_text(task_id: Option<TaskId>, text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            task_id,
            segments: if text.is_empty() {
                Vec::new()
            } else {
                vec![PromptSegment::Text(text)]
            },
        }
    }

    pub fn serialize_provider_text(&self) -> String {
        self.segments
            .iter()
            .map(PromptSegment::serialize_provider_text)
            .collect()
    }

    pub fn insert_segment(&mut self, index: usize, segment: PromptSegment) {
        let index = index.min(self.segments.len());
        self.segments.insert(index, segment);
    }

    pub fn replace_text_range_with_segment(
        &mut self,
        text_segment_index: usize,
        range: std::ops::Range<usize>,
        segment: PromptSegment,
    ) -> bool {
        let Some(PromptSegment::Text(text)) = self.segments.get(text_segment_index).cloned() else {
            return false;
        };
        if range.start > range.end || range.end > text.len() {
            return false;
        }
        let before = text[..range.start].to_string();
        let after = text[range.end..].to_string();
        let mut replacement = Vec::new();
        if !before.is_empty() {
            replacement.push(PromptSegment::Text(before));
        }
        replacement.push(segment);
        if !after.is_empty() {
            replacement.push(PromptSegment::Text(after));
        }
        self.segments
            .splice(text_segment_index..=text_segment_index, replacement);
        true
    }
}

/// Expanded (serialized) vs collapsed (segment-aware) cursor positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposerCursor {
    pub expanded: usize,
    pub segment_index: usize,
    pub offset_in_segment: usize,
}

impl ComposerCursor {
    pub fn at_start() -> Self {
        Self {
            expanded: 0,
            segment_index: 0,
            offset_in_segment: 0,
        }
    }

    pub fn from_expanded(document: &PromptDocument, expanded: usize) -> Self {
        let mut remaining = expanded;
        for (segment_index, segment) in document.segments.iter().enumerate() {
            let text = segment.serialize_provider_text();
            if remaining <= text.len() {
                return Self {
                    expanded,
                    segment_index,
                    offset_in_segment: remaining,
                };
            }
            remaining = remaining.saturating_sub(text.len());
        }
        let last = document.segments.len().saturating_sub(1);
        let offset = document
            .segments
            .last()
            .map(|segment| segment.serialize_provider_text().len())
            .unwrap_or(0);
        Self {
            expanded: document.serialize_provider_text().len(),
            segment_index: last,
            offset_in_segment: offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_closed_segment_vocabulary_exactly() {
        let mut document = PromptDocument::default();
        document.segments = vec![
            PromptSegment::Text("Please review ".into()),
            PromptSegment::FileRef {
                relative_path: "src/ui/mod.rs".into(),
                is_directory: false,
            },
            PromptSegment::Text(" with ".into()),
            PromptSegment::SkillRef {
                name: "code-review".into(),
            },
            PromptSegment::Text(" via ".into()),
            PromptSegment::CommandRef {
                command: "/review".into(),
            },
        ];
        assert_eq!(
            document.serialize_provider_text(),
            "Please review @src/ui/mod.rs with $code-review via /review"
        );
    }

    #[test]
    fn expanded_and_collapsed_cursors_round_trip() {
        let document = PromptDocument {
            task_id: None,
            segments: vec![
                PromptSegment::Text("hi ".into()),
                PromptSegment::FileRef {
                    relative_path: "a.rs".into(),
                    is_directory: false,
                },
            ],
        };
        let cursor = ComposerCursor::from_expanded(&document, 4);
        assert_eq!(cursor.segment_index, 1);
        assert_eq!(cursor.offset_in_segment, 1);
        assert_eq!(cursor.expanded, 4);
    }

    #[test]
    fn replacing_a_trigger_range_keeps_surrounding_text() {
        let mut document = PromptDocument::from_plain_text(None, "see @mo and go");
        assert!(document.replace_text_range_with_segment(
            0,
            4..7,
            PromptSegment::FileRef {
                relative_path: "mod.rs".into(),
                is_directory: false,
            },
        ));
        assert_eq!(document.serialize_provider_text(), "see @mod.rs and go");
    }
}
