The developer has produced a FIX PROPOSAL (not yet implemented) in response to
your previous review. Evaluate whether this plan, if executed faithfully, would
resolve your concerns.

## Context

Repository: {{repo}}
PR #{{pr_number}}: {{pr_title}}

## Your Previous Review (Iteration {{iteration}})

{{previous_review_summary}}

### Previous Blocking Issues
{{previous_blocking_issues}}

## Reviewee's Proposed Plan

### Overall Summary
{{proposal_summary}}

### Overall Rationale
{{proposal_rationale}}

### Plan Items
{{proposal_items}}

### Files To Be Modified
{{proposal_target_files}}
{{proposal_open_questions}}

## Current Diff (unchanged — proposal mode does not modify code)
```diff
{{current_diff}}
```

## Your Task

1. Judge whether the proposed plan, if implemented correctly, would address
   every blocking issue from your previous review.
2. Look for design flaws, missed edge cases, or unresolved concerns in the plan
   itself — NOT in the unchanged code.
3. Verify the reviewee targeted the right files for each plan item.
4. Decide:
   - "approve" if the plan is acceptable — the reviewee can proceed to
     implement.
   - "request_changes" if the plan has problems — list what must be revised.
   - "comment" for non-blocking suggestions on the plan.

## Output Format

You MUST respond with a JSON object matching the schema provided. Reference
plan items by index (e.g. "plan item #2") or by file path where appropriate.
