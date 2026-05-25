use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    pub reviewer: String,
    pub reviewee: String,
    pub max_iterations: u32,
    pub timeout_secs: u64,
    /// Custom prompt directory (default: ~/.config/octorus/prompts/)
    pub prompt_dir: Option<String>,
    /// Additional tools for reviewer (Claude adapter only).
    /// Use Claude Code's --allowedTools format (e.g., "Skill", "Bash(git push:*)").
    #[serde(default)]
    pub reviewer_additional_tools: Vec<String>,
    /// Additional tools for reviewee (Claude adapter only).
    /// Use Claude Code's --allowedTools format (e.g., "Skill", "Bash(git push:*)").
    #[serde(default)]
    pub reviewee_additional_tools: Vec<String>,
    /// Additional read-only tools for reviewee in proposal mode (Claude adapter only).
    /// MUST NOT include Edit/Write or git-mutating commands.
    #[serde(default)]
    pub reviewee_proposal_additional_tools: Vec<String>,
    /// If true, AI Rally posts reviews/fix comments to PR without confirmation.
    /// Default is false (confirmation prompt before posting).
    #[serde(default)]
    pub auto_post: bool,
    /// If true, AI Rally runs in "proposal iteration" mode.
    ///
    /// Flow:
    /// - Reviewer reviews the diff (same as normal mode).
    /// - If Approve → emits `Approved` and returns `RallyResult::Approved`.
    /// - Otherwise, the reviewee produces a `RevieweeProposal` using read-only
    ///   tools only (no code modification) describing how it would address the
    ///   feedback.
    /// - The reviewer re-reviews the proposal.
    /// - Loop continues until reviewer Approves or `max_iterations` is hit.
    /// - On max_iterations: emits `RallyEvent::ReviewOnlyCompleted(reviewer_output)`
    ///   carrying the final reviewer verdict, and returns
    ///   `RallyResult::ReviewOnlyCompleted { iteration, action, summary }`.
    #[serde(default)]
    pub review_only: bool,
    /// Controls when reviewee proposals are posted to the PR as comments.
    ///
    /// - `Final` (default): only post the final proposal once the loop terminates.
    /// - `Each`: post every proposal as it is produced.
    /// - `None`: never post proposals; they remain in session history only.
    #[serde(default)]
    pub post_reviewee_proposals: ProposalPostStrategy,
}

/// When to post reviewee proposals to the PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProposalPostStrategy {
    /// Post only the final proposal (either at Approve or at max_iterations).
    #[default]
    Final,
    /// Post every proposal as it is produced.
    Each,
    /// Never post proposals; only keep them in session history.
    None,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            reviewer: "claude".to_owned(),
            reviewee: "claude".to_owned(),
            max_iterations: 10,
            timeout_secs: 600,
            prompt_dir: None,
            reviewer_additional_tools: Vec::new(),
            reviewee_additional_tools: Vec::new(),
            reviewee_proposal_additional_tools: Vec::new(),
            auto_post: false,
            review_only: false,
            post_reviewee_proposals: ProposalPostStrategy::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitOpsConfig {
    /// コミット diff キャッシュ（プリフェッチ含む）の最大エントリ数
    #[serde(default = "default_max_diff_cache")]
    pub max_diff_cache: usize,
}

fn default_max_diff_cache() -> usize {
    20
}

impl Default for GitOpsConfig {
    fn default() -> Self {
        Self {
            max_diff_cache: default_max_diff_cache(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffConfig {
    pub theme: String,
    #[serde(deserialize_with = "deserialize_tab_width")]
    pub tab_width: u8,
    /// 追加/削除行に背景色を表示するかどうか
    #[serde(default = "default_true")]
    pub bg_color: bool,
}

fn default_true() -> bool {
    true
}

/// Deserialize tab_width with clamping: values below 1 are clamped to 1.
fn deserialize_tab_width<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = u8::deserialize(deserializer)?;
    Ok(value.max(1))
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            theme: "base16-ocean.dark".to_owned(),
            tab_width: 4,
            bg_color: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    #[serde(
        default = "default_left_panel_width",
        deserialize_with = "deserialize_left_panel_width"
    )]
    pub left_panel_width: u16,
    #[serde(default)]
    pub zen_mode: bool,
}

fn default_left_panel_width() -> u16 {
    35
}

fn deserialize_left_panel_width<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = u16::deserialize(deserializer)?;
    Ok(value.clamp(10, 90))
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            left_panel_width: default_left_panel_width(),
            zen_mode: false,
        }
    }
}

impl LayoutConfig {
    pub fn left_panel_percent(&self) -> u16 {
        self.left_panel_width.clamp(10, 90)
    }

    pub fn right_panel_percent(&self) -> u16 {
        100u16.saturating_sub(self.left_panel_percent())
    }
}

const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    #[serde(default = "default_shell_timeout")]
    pub timeout_secs: u64,
}

fn default_shell_timeout() -> u64 {
    DEFAULT_SHELL_TIMEOUT_SECS
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_shell_timeout(),
        }
    }
}
