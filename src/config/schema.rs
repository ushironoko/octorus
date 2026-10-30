use ratatui::style::Color;
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
    /// Additional read-only tools to layer on top of the proposal-mode
    /// allowlist (Claude adapter only).
    ///
    /// Proposal mode is a hard no-shell / no-mutation boundary. The base
    /// allowlist is `Read,Glob,Grep` and a separate `--disallowedTools`
    /// layer denies `Bash,Edit,Write,NotebookEdit` at the agent layer.
    ///
    /// To preserve that boundary, this field is validated **fail-closed** at
    /// `ClaudeAdapter::new`: any entry that equals `Bash`, `Edit`, `Write`,
    /// or `NotebookEdit`, or that matches the pattern `Bash(...)`, causes
    /// adapter construction (and therefore orchestrator construction) to
    /// return an error. Only purely read-only tool keys (e.g. `WebSearch`,
    /// `Skill`) should be added here.
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
    /// Proposals are always produced under the same fail-closed boundary as
    /// `reviewee_proposal_additional_tools` (no shell, no Edit/Write); this
    /// setting only governs *when* the resulting plans are visible on the PR.
    ///
    /// - `Final` (default): only post the final proposal once the loop terminates
    ///   (either at reviewer Approve or at `max_iterations`).
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MouseConfig {
    /// マウスキャプチャを起動時に有効化するかどうか。
    /// 有効中は端末ネイティブのテキスト選択が効かなくなる(F2 で一時切替)。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// ホイール1ノッチあたりのカーソル/選択移動行数
    #[serde(
        default = "default_wheel_step",
        deserialize_with = "deserialize_wheel_step"
    )]
    pub wheel_step: u16,
}

fn default_wheel_step() -> u16 {
    3
}

fn deserialize_wheel_step<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = u16::deserialize(deserializer)?;
    Ok(value.clamp(1, 10))
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            wheel_step: default_wheel_step(),
        }
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreset {
    #[default]
    Dark,
    Light,
    Ansi16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeColors {
    pub accent: Option<Color>,
    pub muted: Option<Color>,
    pub warning: Option<Color>,
    pub danger: Option<Color>,
    pub success: Option<Color>,
    pub info: Option<Color>,
    pub draft_you: Option<Color>,
    pub draft_agent: Option<Color>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeConfig {
    pub preset: ThemePreset,
    pub colors: ThemeColors,
}

impl ThemeConfig {
    pub fn is_untouched(&self) -> bool {
        self.preset == ThemePreset::Dark
            && self.colors.accent.is_none()
            && self.colors.muted.is_none()
            && self.colors.warning.is_none()
            && self.colors.danger.is_none()
            && self.colors.success.is_none()
            && self.colors.info.is_none()
            && self.colors.draft_you.is_none()
            && self.colors.draft_agent.is_none()
    }
}

#[derive(Deserialize)]
struct RawThemeConfig {
    #[serde(default)]
    preset: ThemePreset,
    #[serde(default)]
    colors: RawThemeColors,
}

#[derive(Deserialize, Default)]
struct RawThemeColors {
    #[serde(default)]
    accent: Option<String>,
    #[serde(default)]
    muted: Option<String>,
    #[serde(default)]
    warning: Option<String>,
    #[serde(default)]
    danger: Option<String>,
    #[serde(default)]
    success: Option<String>,
    #[serde(default)]
    info: Option<String>,
    #[serde(default)]
    draft_you: Option<String>,
    #[serde(default)]
    draft_agent: Option<String>,
}

fn parse_color(key: &str, raw: &str) -> Result<Color, String> {
    raw.parse::<Color>()
        .map_err(|_| format!("{key}: invalid color \"{raw}\""))
}

fn optional_color<E: serde::de::Error>(key: &str, raw: Option<String>) -> Result<Option<Color>, E> {
    raw.map(|value| parse_color(key, &value).map_err(E::custom))
        .transpose()
}

impl<'de> Deserialize<'de> for ThemeConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawThemeConfig::deserialize(deserializer)?;
        Ok(Self {
            preset: raw.preset,
            colors: ThemeColors {
                accent: optional_color("theme.colors.accent", raw.colors.accent)?,
                muted: optional_color("theme.colors.muted", raw.colors.muted)?,
                warning: optional_color("theme.colors.warning", raw.colors.warning)?,
                danger: optional_color("theme.colors.danger", raw.colors.danger)?,
                success: optional_color("theme.colors.success", raw.colors.success)?,
                info: optional_color("theme.colors.info", raw.colors.info)?,
                draft_you: optional_color("theme.colors.draft_you", raw.colors.draft_you)?,
                draft_agent: optional_color("theme.colors.draft_agent", raw.colors.draft_agent)?,
            },
        })
    }
}

#[derive(Serialize)]
struct SerializedThemeColors {
    #[serde(skip_serializing_if = "Option::is_none")]
    accent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    muted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    danger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    success: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    info: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    draft_you: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    draft_agent: Option<String>,
}

fn color_name(color: Option<Color>) -> Option<String> {
    color.map(|color| color.to_string())
}

impl Serialize for ThemeConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ThemeConfig", 2)?;
        state.serialize_field("preset", &self.preset)?;
        state.serialize_field(
            "colors",
            &SerializedThemeColors {
                accent: color_name(self.colors.accent),
                muted: color_name(self.colors.muted),
                warning: color_name(self.colors.warning),
                danger: color_name(self.colors.danger),
                success: color_name(self.colors.success),
                info: color_name(self.colors.info),
                draft_you: color_name(self.colors.draft_you),
                draft_agent: color_name(self.colors.draft_agent),
            },
        )?;
        state.end()
    }
}
