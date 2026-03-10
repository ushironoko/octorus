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

## CI Example

```bash
# Returns exit code 0 if approved, 1 otherwise
or --repo "$GITHUB_REPOSITORY" --pr "$PR_NUMBER" --ai-rally

# Parse JSON output
or --repo owner/repo --pr 123 --ai-rally 2>/dev/null | jq '.result'
```
