---
name: octorus
description: GitHub PR review TUI with AI Rally (automated AI review cycles). Binary name is `or`.
---

# octorus (`or`) - GitHub PR Review TUI

octorus is a TUI tool for GitHub PR review with Vim-style keybindings. It communicates with the GitHub API via the `gh` CLI. The binary name is **`or`**.

## Prerequisites

- **GitHub CLI** (`gh`): Must be installed and authenticated
- **AI Rally** (optional): Requires one of:
  - Claude Code CLI (`claude`): Installed and authenticated
  - OpenAI Codex CLI (`codex`): Installed and authenticated

## CLI Usage

```bash
# Open PR list for current repo (auto-detected from git remote)
or

# Open PR list for a specific repo
or --repo owner/repo

# Open a specific PR
or --repo owner/repo --pr 123

# Local diff mode (no GitHub API, shows git diff against HEAD)
or --local

# Local diff with auto-focus on changed files
or --local --auto-focus

# Start AI Rally in TUI mode:
# Open the PR in TUI, then press `A` to start AI Rally from the UI
or --repo owner/repo --pr 123

# Headless AI Rally (CI/automation, outputs JSON to stdout)
or --repo owner/repo --pr 123 --ai-rally
# With custom working directory
or --repo owner/repo --pr 123 --ai-rally --working-dir /path/to/repo

# Local AI Rally (review local changes without a GitHub PR)
or --local --ai-rally

# Accept local .octorus/ config overrides in headless mode
or --repo owner/repo --pr 123 --ai-rally --accept-local-overrides

# Initialize config and prompt templates
or init
or init --force          # Overwrite existing files
or init --local          # Create project-local .octorus/ config

# Clean AI Rally session data
or clean
```

## Headless AI Rally JSON Output

When running headless (`or --repo owner/repo --pr 123 --ai-rally`), octorus outputs JSON to stdout:

```json
{
  "result": "approved" | "not_approved" | "error",
  "iterations": 3,
  "summary": "All issues resolved after 3 iterations.",
  "last_review": {
    "action": "approve" | "request_changes" | "comment",
    "summary": "Code looks good after fixes.",
    "comments": [
      {
        "path": "src/main.rs",
        "line": 42,
        "body": "Consider using a constant here.",
        "severity": "critical" | "major" | "minor" | "suggestion"
      }
    ],
    "blocking_issues": ["Memory leak in handler"]
  },
  "last_fix": {
    "status": "completed" | "needs_clarification" | "needs_permission" | "error",
    "summary": "Fixed memory leak and added constant.",
    "files_modified": ["src/main.rs", "src/handler.rs"],
    "question": "Optional: clarification question if status is needs_clarification",
    "permission_request": {
      "action": "Optional: action description if status is needs_permission",
      "reason": "Optional: reason for permission request"
    },
    "error_details": "Optional: error message if status is error"
  }
}
```

Exit codes: `0` = approved, `1` = not approved or error.

## AI Rally Configuration

Configuration file: `~/.config/octorus/config.toml`

```toml
[ai]
# Supported agents: "claude" (Claude Code), "codex" (OpenAI Codex CLI)
reviewer = "claude"
reviewee = "claude"
max_iterations = 10       # Max review-fix cycles (clamped to 100)
timeout_secs = 600        # Timeout per agent call (clamped to 7200)
# prompt_dir = "/custom/path/to/prompts"  # Custom prompt directory

# Additional tools for reviewer (Claude only, --allowedTools format)
# reviewer_additional_tools = ["Skill", "WebSearch"]

# Additional tools for reviewee (Claude only, --allowedTools format)
# reviewee_additional_tools = ["Skill", "Bash(git push:*)"]
```

Project-local overrides: `.octorus/config.toml` (created via `or init --local`).

## TUI Keybindings

### File List

| Key | Action |
|-----|--------|
| `j` / `Down` | Move down |
| `k` / `Up` | Move up |
| `Enter` / `Right` / `l` | Open split view |
| `g` | Jump to top |
| `G` | Jump to bottom |
| `a` | Submit approve review |
| `r` | Submit request-changes review |
| `c` | Submit comment review |
| `s` | Submit suggestion review |
| `C` | Open comment list |
| `A` | Start AI Rally |
| `d` | View PR description |
| `R` | Refresh all data |
| `q` / `Esc` | Quit |

### Diff View

| Key | Action |
|-----|--------|
| `j` / `Down` | Scroll down |
| `k` / `Up` | Scroll up |
| `n` | Jump to next comment |
| `N` | Jump to previous comment |
| `g` | Jump to top |
| `G` | Jump to bottom |
| `q` / `Esc` / `Left` / `h` | Go back |

### AI Rally View

| Key | Action |
|-----|--------|
| `j` / `Down` | Scroll down in log |
| `k` / `Up` | Scroll up in log |
| `Enter` | View log detail |
| `g` | Jump to top |
| `G` | Jump to bottom |
| `b` | Run in background (return to file list) |
| `y` | Grant permission / Enter answer |
| `n` | Deny permission / Skip |
| `r` | Retry (on error) |
| `q` / `Esc` | Abort rally and exit |

## Common Task Examples

### Run headless AI Rally in CI

```bash
# Returns exit code 0 if approved, 1 otherwise
or --repo "$GITHUB_REPOSITORY" --pr "$PR_NUMBER" --ai-rally

# Parse JSON output
or --repo owner/repo --pr 123 --ai-rally 2>/dev/null | jq '.result'
```

### Review local changes without a PR

```bash
or --local --ai-rally
```

### Use custom prompts

```bash
or init  # Generate default prompt templates
# Edit ~/.config/octorus/prompts/reviewer.md and reviewee.md
# Template variables: {{repo}}, {{pr_number}}, {{pr_title}}, {{diff}}, etc.
```
