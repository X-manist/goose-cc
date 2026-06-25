use serde::{Deserialize, Serialize};
use strum::{Display, EnumMessage, EnumString, IntoStaticStr, VariantNames};
use utoipa::ToSchema;

#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Eq,
    Hash,
    PartialEq,
    Serialize,
    Deserialize,
    Display,
    EnumMessage,
    EnumString,
    IntoStaticStr,
    VariantNames,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum GooseMode {
    #[default]
    #[strum(message = "Automatically approve tool calls")]
    Auto,
    #[strum(message = "Ask before every tool call")]
    Approve,
    #[strum(message = "Ask only for sensitive tool calls")]
    SmartApprove,
    #[strum(message = "Chat only, no tool calls")]
    Chat,
    #[strum(message = "Read-only tools only")]
    Readonly,
    #[strum(message = "Allow clearly safe reads and ask before sensitive actions")]
    Guarded,
    #[strum(message = "Ask before every tool call")]
    Standard,
    #[strum(message = "Approve tool calls without prompts")]
    Yolo,
}

impl GooseMode {
    pub const DISPLAY_VARIANTS: &'static [&'static str] = &[
        "readonly",
        "guarded",
        "standard",
        "yolo",
        "auto",
        "approve",
        "smart_approve",
        "chat",
    ];

    pub fn effective_mode(self) -> Self {
        match self {
            Self::Guarded => Self::SmartApprove,
            Self::Standard => Self::Approve,
            Self::Yolo => Self::Auto,
            mode => mode,
        }
    }

    pub fn is_autonomous(self) -> bool {
        matches!(self.effective_mode(), Self::Auto)
    }

    pub fn is_chat_only(self) -> bool {
        matches!(self.effective_mode(), Self::Chat)
    }

    pub fn is_smart_approve(self) -> bool {
        matches!(self.effective_mode(), Self::SmartApprove)
    }

    pub fn is_approval_required(self) -> bool {
        matches!(self.effective_mode(), Self::Approve | Self::SmartApprove)
    }
}
