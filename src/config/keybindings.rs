use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::keybinding::{KeyBinding, KeySequence, NamedKey};

/// Configurable keybindings
///
/// Supports three formats in TOML:
/// - Simple string: `move_down = "j"`
/// - Object with modifiers: `page_down = { key = "d", ctrl = true }`
/// - Array for sequences: `jump_to_first = ["g", "g"]`
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    pub move_down: KeySequence,
    pub move_up: KeySequence,
    pub move_left: KeySequence,
    pub move_right: KeySequence,
    pub page_down: KeySequence,
    pub page_up: KeySequence,
    pub diff_page_down: KeySequence,
    pub diff_page_up: KeySequence,
    pub jump_to_first: KeySequence,
    pub jump_to_last: KeySequence,
    pub jump_back: KeySequence,
    pub next_comment: KeySequence,
    pub prev_comment: KeySequence,

    pub approve: KeySequence,
    pub request_changes: KeySequence,
    pub comment: KeySequence,
    pub suggestion: KeySequence,
    pub reply: KeySequence,
    pub refresh: KeySequence,
    pub submit: KeySequence,

    pub quit: KeySequence,
    pub help: KeySequence,
    pub comment_list: KeySequence,
    pub ai_rally: KeySequence,
    pub open_panel: KeySequence,

    pub go_to_definition: KeySequence,
    pub go_to_file: KeySequence,
    pub open_in_browser: KeySequence,

    pub toggle_local_mode: KeySequence,
    pub toggle_auto_focus: KeySequence,

    pub toggle_zen_mode: KeySequence,
    pub toggle_markdown_rich: KeySequence,
    pub filter: KeySequence,
    pub multiline_select: KeySequence,
    pub pr_description: KeySequence,
    pub ci_checks: KeySequence,
    pub git_ops: KeySequence,
    pub git_ops_stage: KeySequence,
    pub git_ops_stage_all: KeySequence,
    pub git_ops_discard: KeySequence,
    pub git_ops_commit: KeySequence,
    pub git_ops_undo: KeySequence,
    pub git_ops_reset: KeySequence,
    pub git_ops_push: KeySequence,

    pub issue_list: KeySequence,
    pub tab_switch: KeySequence,
    pub mark_viewed: KeySequence,
    pub mark_viewed_dir: KeySequence,

    pub tree_toggle: KeySequence,
    pub shell_command: KeySequence,
    pub toggle_mouse: KeySequence,

    pub repo_browse: KeySequence,
    pub symbol_outline: KeySequence,
    pub symbol_search: KeySequence,
    pub module_graph: KeySequence,
    pub toggle_blame: KeySequence,
    pub open_blame_commit: KeySequence,
    pub open_blame_pr: KeySequence,
    pub open_line_discussion: KeySequence,

    pub filter_open: KeySequence,
    pub filter_closed: KeySequence,
    pub filter_all: KeySequence,
    pub tab_prev: KeySequence,
    pub tab_next: KeySequence,
    pub rally_background: KeySequence,
    pub rally_pause: KeySequence,
    pub retry: KeySequence,
    pub confirm_yes: KeySequence,
    pub confirm_no: KeySequence,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            move_down: KeySequence::single(KeyBinding::char('j'))
                .with_alt(vec![KeyBinding::named(crate::keybinding::NamedKey::Down)]),
            move_up: KeySequence::single(KeyBinding::char('k'))
                .with_alt(vec![KeyBinding::named(crate::keybinding::NamedKey::Up)]),
            move_left: KeySequence::single(KeyBinding::char('h'))
                .with_alt(vec![KeyBinding::named(crate::keybinding::NamedKey::Left)]),
            move_right: KeySequence::single(KeyBinding::char('l'))
                .with_alt(vec![KeyBinding::named(crate::keybinding::NamedKey::Right)]),
            page_down: KeySequence::single(KeyBinding::ctrl('d')),
            page_up: KeySequence::single(KeyBinding::ctrl('u')),
            diff_page_down: KeySequence::single(KeyBinding::named(NamedKey::PageDown)),
            diff_page_up: KeySequence::single(KeyBinding::named(NamedKey::PageUp)),
            jump_to_first: KeySequence::double(KeyBinding::char('g'), KeyBinding::char('g')),
            jump_to_last: KeySequence::single(KeyBinding::char('G')),
            jump_back: KeySequence::single(KeyBinding::ctrl('o')),
            next_comment: KeySequence::single(KeyBinding::char('n')),
            prev_comment: KeySequence::single(KeyBinding::char('N')),

            approve: KeySequence::single(KeyBinding::char('a')),
            request_changes: KeySequence::single(KeyBinding::char('r')),
            comment: KeySequence::single(KeyBinding::char('c')),
            suggestion: KeySequence::single(KeyBinding::char('s')),
            reply: KeySequence::single(KeyBinding::char('r')),
            refresh: KeySequence::single(KeyBinding::char('R')),
            submit: KeySequence::single(KeyBinding::ctrl('s')),

            quit: KeySequence::single(KeyBinding::char('q'))
                .with_alt(vec![KeyBinding::named(crate::keybinding::NamedKey::Esc)]),
            help: KeySequence::single(KeyBinding::char('?')),
            comment_list: KeySequence::single(KeyBinding::char('C')),
            ai_rally: KeySequence::single(KeyBinding::char('A')),
            open_panel: KeySequence::single(KeyBinding::named(NamedKey::Enter)),

            go_to_definition: KeySequence::double(KeyBinding::char('g'), KeyBinding::char('d')),
            go_to_file: KeySequence::double(KeyBinding::char('g'), KeyBinding::char('f')),
            open_in_browser: KeySequence::single(KeyBinding::char('O')),

            toggle_local_mode: KeySequence::single(KeyBinding::char('L')),
            toggle_auto_focus: KeySequence::single(KeyBinding::char('F')),

            toggle_zen_mode: KeySequence::single(KeyBinding::char('Z')),
            toggle_markdown_rich: KeySequence::single(KeyBinding::char('M')),
            filter: KeySequence::double(KeyBinding::char(' '), KeyBinding::char('/')),
            multiline_select: KeySequence::single(KeyBinding::char('V')),
            pr_description: KeySequence::single(KeyBinding::char('d')),
            ci_checks: KeySequence::single(KeyBinding::char('S')),
            git_ops: KeySequence::single(KeyBinding::char('G')),
            git_ops_stage: KeySequence::single(KeyBinding::char(' ')),
            git_ops_stage_all: KeySequence::single(KeyBinding::char('s')),
            git_ops_discard: KeySequence::single(KeyBinding::char('d')),
            git_ops_commit: KeySequence::single(KeyBinding::char('c')),
            git_ops_undo: KeySequence::single(KeyBinding::char('u')),
            git_ops_reset: KeySequence::single(KeyBinding::char('r')),
            git_ops_push: KeySequence::single(KeyBinding::char('P')),

            issue_list: KeySequence::single(KeyBinding::char('I')),
            tab_switch: KeySequence::single(KeyBinding::named(crate::keybinding::NamedKey::Tab)),
            mark_viewed: KeySequence::single(KeyBinding::char('v')),
            mark_viewed_dir: KeySequence::single(KeyBinding::char('V')),
            tree_toggle: KeySequence::single(KeyBinding::char('t')),
            shell_command: KeySequence::single(KeyBinding::char('!')),
            toggle_mouse: KeySequence::single(KeyBinding::named(NamedKey::F(2))),

            repo_browse: KeySequence::single(KeyBinding::char('b')),
            symbol_outline: KeySequence::single(KeyBinding::char('o')),
            symbol_search: KeySequence::single(KeyBinding::char('s')),
            module_graph: KeySequence::single(KeyBinding::char('i')),
            toggle_blame: KeySequence::double(KeyBinding::char('g'), KeyBinding::char('b')),
            open_blame_commit: KeySequence::double(KeyBinding::char('g'), KeyBinding::char('c')),
            open_blame_pr: KeySequence::double(KeyBinding::char('g'), KeyBinding::char('p')),
            open_line_discussion: KeySequence::double(KeyBinding::char('g'), KeyBinding::char('r')),

            filter_open: KeySequence::single(KeyBinding::char('o')),
            filter_closed: KeySequence::single(KeyBinding::char('c')),
            filter_all: KeySequence::single(KeyBinding::char('a')),
            tab_prev: KeySequence::single(KeyBinding::char('[')),
            tab_next: KeySequence::single(KeyBinding::char(']')),
            rally_background: KeySequence::single(KeyBinding::char('b')),
            rally_pause: KeySequence::single(KeyBinding::char('p')),
            retry: KeySequence::single(KeyBinding::char('r')),
            confirm_yes: KeySequence::single(KeyBinding::char('y'))
                .with_alt(vec![KeyBinding::char('Y')]),
            confirm_no: KeySequence::single(KeyBinding::char('n'))
                .with_alt(vec![KeyBinding::char('N')]),
        }
    }
}

impl KeybindingsConfig {
    /// Validate keybindings for conflicts
    ///
    /// Detects:
    /// - Single keys that conflict with sequence prefixes
    /// - Duplicate keybindings for different actions
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        let mut single_keys: HashMap<KeyBinding, Vec<&str>> = HashMap::new();
        let mut sequence_prefixes: HashMap<KeyBinding, Vec<&str>> = HashMap::new();
        let mut exact_sequences: HashMap<Vec<KeyBinding>, Vec<(&str, bool)>> = HashMap::new();

        let bindings: Vec<(&str, &KeySequence)> = vec![
            ("move_down", &self.move_down),
            ("move_up", &self.move_up),
            ("move_left", &self.move_left),
            ("move_right", &self.move_right),
            ("page_down", &self.page_down),
            ("page_up", &self.page_up),
            ("diff_page_down", &self.diff_page_down),
            ("diff_page_up", &self.diff_page_up),
            ("jump_to_first", &self.jump_to_first),
            ("jump_to_last", &self.jump_to_last),
            ("jump_back", &self.jump_back),
            ("next_comment", &self.next_comment),
            ("prev_comment", &self.prev_comment),
            ("approve", &self.approve),
            ("request_changes", &self.request_changes),
            ("comment", &self.comment),
            ("suggestion", &self.suggestion),
            ("reply", &self.reply),
            ("refresh", &self.refresh),
            ("submit", &self.submit),
            ("quit", &self.quit),
            ("help", &self.help),
            ("comment_list", &self.comment_list),
            ("ai_rally", &self.ai_rally),
            ("open_panel", &self.open_panel),
            ("go_to_definition", &self.go_to_definition),
            ("go_to_file", &self.go_to_file),
            ("open_in_browser", &self.open_in_browser),
            ("toggle_local_mode", &self.toggle_local_mode),
            ("toggle_auto_focus", &self.toggle_auto_focus),
            ("toggle_zen_mode", &self.toggle_zen_mode),
            ("toggle_markdown_rich", &self.toggle_markdown_rich),
            ("filter", &self.filter),
            ("multiline_select", &self.multiline_select),
            ("pr_description", &self.pr_description),
            ("ci_checks", &self.ci_checks),
            ("git_ops", &self.git_ops),
            ("git_ops_stage", &self.git_ops_stage),
            ("git_ops_stage_all", &self.git_ops_stage_all),
            ("git_ops_discard", &self.git_ops_discard),
            ("git_ops_commit", &self.git_ops_commit),
            ("git_ops_undo", &self.git_ops_undo),
            ("git_ops_reset", &self.git_ops_reset),
            ("git_ops_push", &self.git_ops_push),
            ("issue_list", &self.issue_list),
            ("tab_switch", &self.tab_switch),
            ("mark_viewed", &self.mark_viewed),
            ("mark_viewed_dir", &self.mark_viewed_dir),
            ("tree_toggle", &self.tree_toggle),
            ("repo_browse", &self.repo_browse),
            ("symbol_outline", &self.symbol_outline),
            ("symbol_search", &self.symbol_search),
            ("module_graph", &self.module_graph),
            ("toggle_blame", &self.toggle_blame),
            ("open_blame_commit", &self.open_blame_commit),
            ("open_blame_pr", &self.open_blame_pr),
            ("open_line_discussion", &self.open_line_discussion),
            ("shell_command", &self.shell_command),
            ("toggle_mouse", &self.toggle_mouse),
            ("filter_open", &self.filter_open),
            ("filter_closed", &self.filter_closed),
            ("filter_all", &self.filter_all),
            ("tab_prev", &self.tab_prev),
            ("tab_next", &self.tab_next),
            ("rally_background", &self.rally_background),
            ("rally_pause", &self.rally_pause),
            ("retry", &self.retry),
            ("confirm_yes", &self.confirm_yes),
            ("confirm_no", &self.confirm_no),
        ];

        let tab = KeyBinding::named(NamedKey::Tab);
        if self
            .module_graph
            .all_sequences()
            .any(|keys| keys.first() == Some(&tab))
        {
            errors.push(
                "keybinding 'module_graph' cannot use the reserved Tab graph control or start with it"
                    .to_string(),
            );
        }

        for (name, seq) in &bindings {
            if seq.keys.is_empty() {
                errors.push(format!("keybinding '{}' is empty", name));
                continue;
            }

            for keys in seq.all_sequences() {
                if keys.is_empty() {
                    errors.push(format!("keybinding '{}' has an empty alternative", name));
                    continue;
                }
                let is_single = keys.len() == 1;
                let exact_owners = exact_sequences.entry(keys.to_vec()).or_default();
                for (existing, existing_is_single) in exact_owners.iter() {
                    // Single-key duplicates are diagnosed once by `single_keys`,
                    // including alternatives. Multi-key exact collisions stay here.
                    if is_single && *existing_is_single {
                        continue;
                    }
                    if !is_context_compatible(name, existing) {
                        let display: String = keys.iter().map(KeyBinding::display).collect();
                        errors.push(format!(
                            "duplicate key sequence: '{}' and '{}' both use {}",
                            name, existing, display
                        ));
                    }
                }
                exact_owners.push((name, is_single));

                if is_single {
                    let key = keys[0];
                    let owners = single_keys.entry(key).or_default();
                    for existing in owners.iter() {
                        // Tracking every primary and alternative owner prevents an
                        // earlier disjoint-context action from hiding a later collision.
                        if !is_context_compatible(name, existing) {
                            errors.push(format!(
                                "duplicate keybinding: '{}' and '{}' both use {}",
                                name,
                                existing,
                                key.display()
                            ));
                        }
                    }
                    owners.push(name);
                } else if let Some(first) = keys.first() {
                    sequence_prefixes.entry(*first).or_default().push(name);
                }
            }
        }

        // Check for conflicts between single keys and sequence prefixes
        for (key, single_names) in &single_keys {
            if let Some(seq_names) = sequence_prefixes.get(key) {
                // Only warn if they're in the same context.
                for single_name in single_names {
                    for seq_name in seq_names {
                        if !is_context_compatible(single_name, seq_name) {
                            errors.push(format!(
                                "keybinding conflict: '{}' ({}) conflicts with sequence prefix for '{}' ({})",
                                single_name,
                                key.display(),
                                seq_name,
                                key.display()
                            ));
                        }
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Check if two keybindings are in compatible contexts
/// (i.e., they won't conflict because they're used in different views)
fn is_context_compatible(name1: &str, name2: &str) -> bool {
    // These keybindings are used in different contexts:
    // - 'r' is used for 'reply' in comment panel and 'request_changes' in file list
    //
    // NOTE: 'comment' and 'suggestion' are NOT compatible - both are active in diff view
    // and comment panel contexts, so they must have different bindings.
    // 特定画面でのみ有効なキー（他の全キーと context compatible）
    const SCREEN_SPECIFIC_KEYS: &[&str] = &[
        "git_ops_stage",
        "git_ops_stage_all",
        "git_ops_discard",
        "git_ops_commit",
        "git_ops_undo",
        "git_ops_reset",
        "git_ops_push",
        "tab_switch",
        "mark_viewed",
        "mark_viewed_dir",
        "tree_toggle",
        "shell_command",
        "filter_open",
        "filter_closed",
        "filter_all",
        "tab_prev",
        "tab_next",
        "rally_background",
        "rally_pause",
        "retry",
        "confirm_yes",
        "confirm_no",
        // Repository Browser keys are only live on the browse screen.
        "repo_browse",
        "symbol_outline",
        "symbol_search",
        "module_graph",
    ];

    let context_groups: &[&[&str]] = &[
        &["reply", "request_changes"],
        &["toggle_local_mode", "move_right"], // L vs l: different cases
        &["toggle_auto_focus", "go_to_file"], // F vs gf: different sequence lengths
        // Default `G` is shared. The File List and the split-view file pane
        // match `git_ops` first, then `jump_to_last`. Other screens use
        // `jump_to_last` for that key.
        &["git_ops", "jump_to_last"],
        &["filter_closed", "comment"],
        &["filter_all", "approve"],
        &["retry", "reply", "request_changes"],
        &["confirm_no", "next_comment"],
    ];

    // The module-graph action is file-pane specific, but these actions are
    // active in that same pane. Treating every screen-specific key as a
    // wildcard made valid-looking custom bindings unreachable at runtime.
    const MODULE_GRAPH_OVERLAPS: &[&str] = &[
        "move_down",
        "move_up",
        "move_left",
        "move_right",
        "page_down",
        "page_up",
        "jump_to_first",
        "jump_to_last",
        "jump_back",
        "quit",
        "help",
        "open_panel",
        "go_to_definition",
        "go_to_file",
        "toggle_zen_mode",
        "symbol_outline",
        "symbol_search",
        "toggle_blame",
        "open_blame_commit",
        "open_blame_pr",
        "open_line_discussion",
    ];
    if name1 == "module_graph" || name2 == "module_graph" {
        let other = if name1 == "module_graph" {
            name2
        } else {
            name1
        };
        return !MODULE_GRAPH_OVERLAPS.contains(&other);
    }

    // git ops 固有キーは git ops 画面でのみ有効なので、他の全キーと context compatible
    if SCREEN_SPECIFIC_KEYS.contains(&name1) || SCREEN_SPECIFIC_KEYS.contains(&name2) {
        return true;
    }

    for group in context_groups {
        if group.contains(&name1) && group.contains(&name2) {
            return true;
        }
    }

    false
}

// Custom Serialize for KeybindingsConfig to maintain backwards compatibility
impl Serialize for KeybindingsConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        // For serialization, we output a simplified format
        let mut map = serializer.serialize_map(None)?;

        // Helper to serialize a KeySequence
        fn seq_to_value(seq: &KeySequence) -> toml::Value {
            if seq.is_single() && seq.alt.is_empty() {
                toml::Value::String(seq.display())
            } else if seq.alt.is_empty() {
                toml::Value::Array(
                    seq.keys
                        .iter()
                        .map(|k| toml::Value::String(k.display()))
                        .collect(),
                )
            } else {
                // primary | alt1 | alt2 ...
                toml::Value::String(seq.display())
            }
        }

        map.serialize_entry("move_down", &seq_to_value(&self.move_down))?;
        map.serialize_entry("move_up", &seq_to_value(&self.move_up))?;
        map.serialize_entry("move_left", &seq_to_value(&self.move_left))?;
        map.serialize_entry("move_right", &seq_to_value(&self.move_right))?;
        map.serialize_entry("page_down", &seq_to_value(&self.page_down))?;
        map.serialize_entry("page_up", &seq_to_value(&self.page_up))?;
        map.serialize_entry("diff_page_down", &seq_to_value(&self.diff_page_down))?;
        map.serialize_entry("diff_page_up", &seq_to_value(&self.diff_page_up))?;
        map.serialize_entry("jump_to_first", &seq_to_value(&self.jump_to_first))?;
        map.serialize_entry("jump_to_last", &seq_to_value(&self.jump_to_last))?;
        map.serialize_entry("jump_back", &seq_to_value(&self.jump_back))?;
        map.serialize_entry("next_comment", &seq_to_value(&self.next_comment))?;
        map.serialize_entry("prev_comment", &seq_to_value(&self.prev_comment))?;
        map.serialize_entry("approve", &seq_to_value(&self.approve))?;
        map.serialize_entry("request_changes", &seq_to_value(&self.request_changes))?;
        map.serialize_entry("comment", &seq_to_value(&self.comment))?;
        map.serialize_entry("suggestion", &seq_to_value(&self.suggestion))?;
        map.serialize_entry("reply", &seq_to_value(&self.reply))?;
        map.serialize_entry("refresh", &seq_to_value(&self.refresh))?;
        map.serialize_entry("submit", &seq_to_value(&self.submit))?;
        map.serialize_entry("quit", &seq_to_value(&self.quit))?;
        map.serialize_entry("help", &seq_to_value(&self.help))?;
        map.serialize_entry("comment_list", &seq_to_value(&self.comment_list))?;
        map.serialize_entry("ai_rally", &seq_to_value(&self.ai_rally))?;
        map.serialize_entry("open_panel", &seq_to_value(&self.open_panel))?;
        map.serialize_entry("go_to_definition", &seq_to_value(&self.go_to_definition))?;
        map.serialize_entry("go_to_file", &seq_to_value(&self.go_to_file))?;
        map.serialize_entry("open_in_browser", &seq_to_value(&self.open_in_browser))?;
        map.serialize_entry("toggle_local_mode", &seq_to_value(&self.toggle_local_mode))?;
        map.serialize_entry("toggle_auto_focus", &seq_to_value(&self.toggle_auto_focus))?;
        map.serialize_entry("toggle_zen_mode", &seq_to_value(&self.toggle_zen_mode))?;
        map.serialize_entry(
            "toggle_markdown_rich",
            &seq_to_value(&self.toggle_markdown_rich),
        )?;
        map.serialize_entry("filter", &seq_to_value(&self.filter))?;
        map.serialize_entry("multiline_select", &seq_to_value(&self.multiline_select))?;
        map.serialize_entry("pr_description", &seq_to_value(&self.pr_description))?;
        map.serialize_entry("ci_checks", &seq_to_value(&self.ci_checks))?;
        map.serialize_entry("git_ops", &seq_to_value(&self.git_ops))?;
        map.serialize_entry("git_ops_stage", &seq_to_value(&self.git_ops_stage))?;
        map.serialize_entry("git_ops_stage_all", &seq_to_value(&self.git_ops_stage_all))?;
        map.serialize_entry("git_ops_discard", &seq_to_value(&self.git_ops_discard))?;
        map.serialize_entry("git_ops_commit", &seq_to_value(&self.git_ops_commit))?;
        map.serialize_entry("git_ops_undo", &seq_to_value(&self.git_ops_undo))?;
        map.serialize_entry("git_ops_reset", &seq_to_value(&self.git_ops_reset))?;
        map.serialize_entry("git_ops_push", &seq_to_value(&self.git_ops_push))?;
        map.serialize_entry("issue_list", &seq_to_value(&self.issue_list))?;
        map.serialize_entry("tab_switch", &seq_to_value(&self.tab_switch))?;
        map.serialize_entry("mark_viewed", &seq_to_value(&self.mark_viewed))?;
        map.serialize_entry("mark_viewed_dir", &seq_to_value(&self.mark_viewed_dir))?;
        map.serialize_entry("tree_toggle", &seq_to_value(&self.tree_toggle))?;
        map.serialize_entry("toggle_mouse", &seq_to_value(&self.toggle_mouse))?;
        map.serialize_entry("repo_browse", &seq_to_value(&self.repo_browse))?;
        map.serialize_entry("symbol_outline", &seq_to_value(&self.symbol_outline))?;
        map.serialize_entry("symbol_search", &seq_to_value(&self.symbol_search))?;
        map.serialize_entry("module_graph", &seq_to_value(&self.module_graph))?;
        map.serialize_entry("toggle_blame", &seq_to_value(&self.toggle_blame))?;
        map.serialize_entry("open_blame_commit", &seq_to_value(&self.open_blame_commit))?;
        map.serialize_entry("open_blame_pr", &seq_to_value(&self.open_blame_pr))?;
        map.serialize_entry(
            "open_line_discussion",
            &seq_to_value(&self.open_line_discussion),
        )?;
        map.serialize_entry("shell_command", &seq_to_value(&self.shell_command))?;
        map.serialize_entry("filter_open", &seq_to_value(&self.filter_open))?;
        map.serialize_entry("filter_closed", &seq_to_value(&self.filter_closed))?;
        map.serialize_entry("filter_all", &seq_to_value(&self.filter_all))?;
        map.serialize_entry("tab_prev", &seq_to_value(&self.tab_prev))?;
        map.serialize_entry("tab_next", &seq_to_value(&self.tab_next))?;
        map.serialize_entry("rally_background", &seq_to_value(&self.rally_background))?;
        map.serialize_entry("rally_pause", &seq_to_value(&self.rally_pause))?;
        map.serialize_entry("retry", &seq_to_value(&self.retry))?;
        map.serialize_entry("confirm_yes", &seq_to_value(&self.confirm_yes))?;
        map.serialize_entry("confirm_no", &seq_to_value(&self.confirm_no))?;

        map.end()
    }
}
