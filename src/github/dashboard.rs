use anyhow::Result;

use super::client::gh_api;

fn parse_total_count(json: &serde_json::Value) -> u32 {
    json.get("total_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32
}

pub async fn fetch_mentioned_issues_count(repo: &str) -> Result<u32> {
    let endpoint = format!(
        "search/issues?q=mentions:@me+is:issue+is:open+repo:{}&per_page=1",
        repo
    );
    let json = gh_api(&endpoint).await?;
    Ok(parse_total_count(&json))
}

pub async fn fetch_review_requested_prs_count(repo: &str) -> Result<u32> {
    let endpoint = format!(
        "search/issues?q=review-requested:@me+is:pr+is:open+repo:{}&per_page=1",
        repo
    );
    let json = gh_api(&endpoint).await?;
    Ok(parse_total_count(&json))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_total_count_from_valid_json() {
        let json: serde_json::Value =
            serde_json::json!({"total_count": 42, "incomplete_results": false, "items": []});
        assert_eq!(parse_total_count(&json), 42);
    }

    #[test]
    fn parse_total_count_missing_field() {
        let json: serde_json::Value = serde_json::json!({"items": []});
        assert_eq!(parse_total_count(&json), 0);
    }

    #[test]
    fn parse_total_count_non_numeric() {
        let json: serde_json::Value = serde_json::json!({"total_count": "not a number"});
        assert_eq!(parse_total_count(&json), 0);
    }
}
