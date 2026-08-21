use crate::ui::components::interaction::{AccessibilityMetadata, AccessibleRole};

use super::{
    take_scalars, InteractionEligibility, ProviderKind, RenderModelError, RendererSelection,
    SemanticEvent, SemanticEventBody, SemanticKind, SemanticRenderer, TimelineItemContent,
    TimelineItemId, TimelineItemModel,
};

pub struct MessageRenderer;

/// Closed conversation role. Classification must read this, never the
/// human-facing `role` label, which is a display string and may be reworded
/// or localized at any time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    Reasoning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    /// Display label only. Never match on this.
    pub role: String,
    pub role_kind: MessageRole,
    pub streaming: bool,
    pub markdown: MarkdownDocument,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownDocument {
    pub selectable: bool,
    pub copyable: bool,
    pub html_executed: bool,
    pub prose_wraps: bool,
    pub blocks: Vec<MarkdownBlock>,
    pub pending_links: Vec<PendingLink>,
}

impl MarkdownDocument {
    pub fn plain_text(&self) -> String {
        let mut text = String::new();
        for block in &self.blocks {
            if !text.is_empty() {
                text.push('\n');
            }
            match block {
                MarkdownBlock::Heading { text: heading, .. } => text.push_str(heading),
                MarkdownBlock::Paragraph { text: paragraph } => text.push_str(paragraph),
                MarkdownBlock::Code { text: code, .. } => text.push_str(code),
            }
        }
        text
    }

    pub fn estimated_height(&self) -> u32 {
        let lines = self
            .blocks
            .iter()
            .map(|block| match block {
                MarkdownBlock::Heading { .. } => 1,
                MarkdownBlock::Paragraph { text } => text.lines().count().max(1),
                MarkdownBlock::Code { text, .. } => text.lines().count().max(1) + 1,
            })
            .sum::<usize>();
        48 + u32::try_from(lines.saturating_mul(18)).unwrap_or(u32::MAX)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownBlock {
    Heading {
        level: u8,
        text: String,
    },
    Paragraph {
        text: String,
    },
    Code {
        language: Option<String>,
        text: String,
        horizontal_scroll: bool,
    },
}

impl MarkdownBlock {
    pub fn is_heading(&self, title: &str) -> bool {
        matches!(self, Self::Heading { text, .. } if text == title)
    }

    pub fn is_horizontally_scrollable_code(&self) -> bool {
        matches!(
            self,
            Self::Code {
                horizontal_scroll: true,
                ..
            }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLink {
    pub href: String,
    pub requires_confirmation: bool,
}

impl SemanticRenderer for MessageRenderer {
    fn kind(&self) -> SemanticKind {
        SemanticKind::Message
    }

    fn project(&self, event: &SemanticEvent) -> Result<TimelineItemModel, RenderModelError> {
        let SemanticEventBody::Message {
            role,
            role_kind,
            text,
            streaming,
        } = &event.body
        else {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Message));
        };
        if role.trim().is_empty() || text.is_empty() {
            return Err(RenderModelError::MalformedKnown(SemanticKind::Message));
        }
        let markdown = parse_markdown(text);
        let name = accessible_name(role, text);
        Ok(TimelineItemModel {
            id: TimelineItemId::Event(event.event_id),
            task_id: event.task_id,
            renderer_selection: RendererSelection::Specialized(SemanticKind::Message),
            interaction: InteractionEligibility::None,
            content: TimelineItemContent::Message(MessageView {
                role: role.clone(),
                role_kind: *role_kind,
                streaming: *streaming,
                markdown,
            }),
            activated_on_enter: false,
            accessibility: AccessibilityMetadata::new(AccessibleRole::Region, name)?,
            turn_id: event.turn_id.clone(),
            related_event_id: event.related_event_id,
        })
    }
}

fn accessible_name(role: &str, text: &str) -> String {
    let first = text.lines().next().unwrap_or(role);
    let name = take_scalars(first, 80);
    if name.trim().is_empty() {
        role.to_string()
    } else {
        name
    }
}

fn parse_markdown(source: &str) -> MarkdownDocument {
    let mut blocks = Vec::new();
    let mut pending_links = Vec::new();
    let mut in_code = false;
    let mut language = None;
    let mut code_lines = Vec::new();
    let mut paragraph_lines = Vec::new();

    let flush_paragraph = |lines: &mut Vec<String>,
                           blocks: &mut Vec<MarkdownBlock>,
                           pending_links: &mut Vec<PendingLink>| {
        if lines.is_empty() {
            return;
        }
        let text = lines.join("\n");
        pending_links.extend(extract_links(&text));
        blocks.push(MarkdownBlock::Paragraph { text });
        lines.clear();
    };

    for line in source.lines() {
        if let Some(rest) = line.strip_prefix("```") {
            if in_code {
                blocks.push(MarkdownBlock::Code {
                    language: language.take(),
                    text: code_lines.join("\n"),
                    horizontal_scroll: true,
                });
                code_lines.clear();
                in_code = false;
            } else {
                flush_paragraph(&mut paragraph_lines, &mut blocks, &mut pending_links);
                language = {
                    let lang = rest.trim();
                    if lang.is_empty() {
                        None
                    } else {
                        Some(lang.to_string())
                    }
                };
                in_code = true;
            }
            continue;
        }
        if in_code {
            code_lines.push(line.to_string());
            continue;
        }
        if let Some(heading) = parse_heading(line) {
            flush_paragraph(&mut paragraph_lines, &mut blocks, &mut pending_links);
            blocks.push(heading);
            continue;
        }
        if line.trim().is_empty() {
            flush_paragraph(&mut paragraph_lines, &mut blocks, &mut pending_links);
            continue;
        }
        paragraph_lines.push(line.to_string());
    }
    if in_code {
        blocks.push(MarkdownBlock::Code {
            language,
            text: code_lines.join("\n"),
            horizontal_scroll: true,
        });
    }
    flush_paragraph(&mut paragraph_lines, &mut blocks, &mut pending_links);

    MarkdownDocument {
        selectable: true,
        copyable: true,
        html_executed: false,
        prose_wraps: true,
        blocks,
        pending_links,
    }
}

fn parse_heading(line: &str) -> Option<MarkdownBlock> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.chars().take_while(|ch| *ch == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = trimmed.get(level..)?.trim();
    if rest.is_empty() {
        return None;
    }
    Some(MarkdownBlock::Heading {
        level: u8::try_from(level).unwrap_or(6),
        text: rest.to_string(),
    })
}

fn extract_links(text: &str) -> Vec<PendingLink> {
    let mut links = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'[' {
            index += 1;
            continue;
        }
        let Some(label_end) = text[index + 1..].find(']') else {
            break;
        };
        let label_end = index + 1 + label_end;
        if text[label_end + 1..].starts_with('(') {
            if let Some(href_end) = text[label_end + 2..].find(')') {
                let href = text[label_end + 2..label_end + 2 + href_end].to_string();
                if !href.trim().is_empty() {
                    links.push(PendingLink {
                        href,
                        requires_confirmation: true,
                    });
                }
                index = label_end + 3 + href_end;
                continue;
            }
        }
        index += 1;
    }
    links
}

#[cfg(test)]
mod role_tests {
    use super::*;

    #[test]
    fn role_kind_is_independent_of_the_display_label() {
        let view = MessageView {
            role: "Thinking".to_string(),
            role_kind: MessageRole::Reasoning,
            streaming: false,
            markdown: MarkdownDocument {
                selectable: true,
                copyable: true,
                html_executed: false,
                prose_wraps: true,
                blocks: Vec::new(),
                pending_links: Vec::new(),
            },
        };
        // Renaming the label must not change classification.
        assert_eq!(view.role_kind, MessageRole::Reasoning);
        assert_ne!(view.role_kind, MessageRole::Assistant);
    }

    #[test]
    fn every_role_kind_is_distinct() {
        let all = [
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Reasoning,
            MessageRole::Error,
        ];
        for (index, left) in all.iter().enumerate() {
            for (other, right) in all.iter().enumerate() {
                assert_eq!(index == other, left == right);
            }
        }
    }

    fn message_event(role_kind: MessageRole) -> SemanticEvent {
        use crate::domain::id::{EventId, TaskId};

        SemanticEvent {
            event_id: EventId::new(),
            task_id: TaskId::new(),
            schema_version: 1,
            provider: ProviderKind::parse("codex").expect("provider"),
            source_type: "message".to_string(),
            occurred_at_ms: 0,
            raw_terminal_available: false,
            turn_id: None,
            related_event_id: None,
            body: SemanticEventBody::Message {
                role: "Reasoning".to_string(),
                role_kind,
                text: "checking fences".to_string(),
                streaming: false,
            },
        }
    }

    // These two tests are the real gate for this task: they exercise
    // `MessageRenderer::project`, the only production code path that turns
    // a `SemanticEventBody::Message` into a `MessageView`. A test that
    // builds a `MessageView` by hand (like the two above) cannot catch a
    // projector that ignores `role_kind` and hardcodes a variant.
    #[test]
    fn a_reasoning_payload_projects_to_a_reasoning_role_kind() {
        let event = message_event(MessageRole::Reasoning);
        let model = MessageRenderer.project(&event).expect("message projects");
        let TimelineItemContent::Message(view) = model.content else {
            panic!("expected message content");
        };
        assert_eq!(view.role_kind, MessageRole::Reasoning);
    }

    #[test]
    fn an_error_payload_does_not_project_as_assistant() {
        let event = message_event(MessageRole::Error);
        let model = MessageRenderer.project(&event).expect("message projects");
        let TimelineItemContent::Message(view) = model.content else {
            panic!("expected message content");
        };
        assert_ne!(view.role_kind, MessageRole::Assistant);
        assert_eq!(view.role_kind, MessageRole::Error);
    }
}
