---
name: octorus
description: GitHub PR review TUI with AI Rally (automated AI review cycles). Binary name is `or`.
---

# octorus (`or`) - GitHub PR Review TUI

octorus is a TUI tool for GitHub PR review with Vim-style keybindings.
It communicates with the GitHub API via the `gh` CLI. The binary name is **`or`**.

## Prerequisites

- **GitHub CLI** (`gh`): Must be installed and authenticated
- **AI Rally** (optional): Claude Code CLI (`claude`) or OpenAI Codex CLI (`codex`)

## Use Cases

### Review a PR interactively

```bash
or                                  # Open PR list (auto-detect repo)
or --repo owner/repo                # Open PR list for specific repo
or --repo owner/repo --pr 123       # Open a specific PR
```

Navigate with Vim-style keybindings (j/k to move, Enter to open, q to go back). Press `?` for full keybinding help in-app.

### Run AI Rally (automated review)

```bash
# TUI: launch `or`, select PR, press `A`

# Headless (CI/automation):
or --repo owner/repo --pr 123 --ai-rally
or --repo owner/repo --pr 123 --ai-rally --working-dir /path/to/repo
```

Results are also persisted to `~/.cache/octorus/rally/{repo}_{pr}/` (see `references/headless-output.md` for details).

### Preview local changes (no PR)

```bash
or --local                          # Show git diff against HEAD
or --local --auto-focus             # Auto-focus on changed files
or --local --ai-rally               # AI review of local changes
```

### Set up and configure

```bash
or init                             # Initialize config + prompt templates
or init --force                     # Overwrite existing files
or init --local                     # Create project-local .octorus/ config
```

See `references/config-reference.md` for full config reference.

### Migrate after version upgrade

```bash
or migrate                          # Apply migrations
or migrate --dry-run                # Preview without applying
or migrate --local                  # Migrate project-local config
```

### Clean session data

```bash
or clean                            # Remove AI Rally session data
```

### Customize prompt templates

```bash
or init                             # Generates default templates
# Edit files in ~/.config/octorus/prompts/
# Template variables: {{repo}}, {{pr_number}}, {{pr_title}}, {{diff}}, etc.
```

## Reference

Read these files in the same directory for detailed specifications:

- `references/headless-output.md` — Headless mode JSON output & exit codes
- `references/config-reference.md` — Configuration file reference
