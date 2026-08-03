use serde_json::Value;

/// Parse token usage from a terminal Responses SSE event.
pub(crate) fn parse_usage(line: &str) -> Option<(u64, u64, Option<u64>)> {
    let json_str = line.trim().strip_prefix("data:")?.trim();
    let value: Value = serde_json::from_str(json_str).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("response.completed") {
        return None;
    }
    let usage = value.get("response")?.get("usage")?;
    let input_total = usage.get("input_tokens")?.as_u64()?;
    let output = usage.get("output_tokens")?.as_u64()?;
    let cache_read = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64);
    Some((
        input_total.saturating_sub(cache_read.unwrap_or(0)),
        output,
        cache_read,
    ))
}

#[cfg(test)]
mod tests {
    use super::parse_usage;

    #[test]
    fn parses_completed_usage_and_splits_cached_tokens() {
        assert_eq!(
            parse_usage(
                r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":3,"input_tokens_details":{"cached_tokens":5}}}}"#
            ),
            Some((7, 3, Some(5)))
        );
    }

    #[test]
    fn ignores_non_terminal_or_malformed_events() {
        assert_eq!(
            parse_usage("data: {\"type\":\"response.output_text.delta\"}"),
            None
        );
        assert_eq!(parse_usage("not json"), None);
    }
}
