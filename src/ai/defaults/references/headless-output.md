# Headless AI Rally — JSON Output & Exit Codes

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

## Exit Codes

| Code | Meaning |
|------|---------|
| `0`  | Approved |
| `1`  | Not approved or error |

## Session Storage

AI Rally results are persisted to disk regardless of stdout capture. **Always check this path to read results.**

```
~/.cache/octorus/rally/{repo}_{pr}/
├── session.json              # Current state (iteration, state, timestamps)
└── history/
    ├── 001_review.json       # First review (action, summary, comments, blocking_issues)
    ├── 001_fix.json          # First fix (status, summary, files_modified)
    ├── 002_review.json       # Second review (re-review after fix)
    └── ...
```

- **Path pattern**: `{repo}` uses `_` as separator (e.g., `owner/repo` PR #123 → `owner_repo_123/`)
- **Local mode**: stored as `local_0/`
- **session.json**: `"state": "Completed"` means the rally finished successfully
- **history/{N}_review.json**: contains the full review with `action` (`approve`/`request_changes`/`comment`), `summary`, and `comments` array

### Reading results

```bash
# Check session state
cat ~/.cache/octorus/rally/{repo}_{pr}/session.json

# Read the latest review
cat ~/.cache/octorus/rally/{repo}_{pr}/history/001_review.json

# Extract review action
cat ~/.cache/octorus/rally/{repo}_{pr}/history/001_review.json | jq '.entry_type.Review.action'
```

## CI Example

```bash
# Returns exit code 0 if approved, 1 otherwise
or --repo "$GITHUB_REPOSITORY" --pr "$PR_NUMBER" --ai-rally

# Parse JSON output
or --repo owner/repo --pr 123 --ai-rally 2>/dev/null | jq '.result'
```
