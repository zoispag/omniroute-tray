use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AnalyticsError {
    #[error("network error: {0}")]
    Network(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("parse error: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DayPoint {
    pub date: String,
    pub cost: f64,
    pub tokens: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageTrend {
    pub days: Vec<DayPoint>,
    pub today_cost: f64,
    pub today_tokens: f64,
    pub yesterday_cost: f64,
    pub yesterday_tokens: f64,
    pub total_cost: f64,
    pub total_tokens: f64,
}

pub fn fetch(base_url: &str, api_key: &str, period: &str) -> Result<UsageTrend, AnalyticsError> {
    let url = format!("{base_url}/api/usage/analytics?period={period}");
    let body = match ureq::get(&url)
        .timeout(std::time::Duration::from_secs(4))
        .set("Authorization", &format!("Bearer {api_key}"))
        .call()
    {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| AnalyticsError::Network(e.to_string()))?,
        Err(ureq::Error::Status(401, _)) => return Err(AnalyticsError::Unauthorized),
        Err(e) => return Err(AnalyticsError::Network(e.to_string())),
    };
    parse(&body)
}

pub fn parse(raw: &str) -> Result<UsageTrend, AnalyticsError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| AnalyticsError::Parse(e.to_string()))?;

    let days: Vec<DayPoint> = value
        .get("dailyTrend")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|d| {
                    Some(DayPoint {
                        date: d.get("date").and_then(Value::as_str)?.to_string(),
                        cost: d.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
                        tokens: d.get("totalTokens").and_then(Value::as_f64).unwrap_or(0.0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let n = days.len();
    let today = n.checked_sub(1).and_then(|i| days.get(i));
    let yesterday = n.checked_sub(2).and_then(|i| days.get(i));

    let summary = value.get("summary");
    let total_cost = summary
        .and_then(|s| s.get("totalCost"))
        .and_then(Value::as_f64)
        .unwrap_or_else(|| days.iter().map(|d| d.cost).sum());
    let total_tokens = summary
        .and_then(|s| s.get("totalTokens"))
        .and_then(Value::as_f64)
        .unwrap_or_else(|| days.iter().map(|d| d.tokens).sum());

    Ok(UsageTrend {
        today_cost: today.map(|d| d.cost).unwrap_or(0.0),
        today_tokens: today.map(|d| d.tokens).unwrap_or(0.0),
        yesterday_cost: yesterday.map(|d| d.cost).unwrap_or(0.0),
        yesterday_tokens: yesterday.map(|d| d.tokens).unwrap_or(0.0),
        total_cost,
        total_tokens,
        days,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "summary": {"totalCost": 439.33, "totalTokens": 525825614},
      "dailyTrend": [
        {"date":"2026-06-28","requests":149,"totalTokens":30595763,"cost":31.27},
        {"date":"2026-06-29","requests":547,"totalTokens":113755814,"cost":89.92},
        {"date":"2026-07-05","requests":666,"totalTokens":165639027,"cost":109.48}
      ]
    }"#;

    #[test]
    fn parses_daily_trend_and_totals() {
        let t = parse(SAMPLE).unwrap();
        assert_eq!(t.days.len(), 3);
        assert_eq!(t.days[0].date, "2026-06-28");
        assert_eq!(t.total_cost, 439.33);
    }

    #[test]
    fn today_is_last_day_yesterday_is_second_last() {
        let t = parse(SAMPLE).unwrap();
        assert_eq!(t.today_cost, 109.48);
        assert_eq!(t.yesterday_cost, 89.92);
    }

    #[test]
    fn missing_daily_trend_yields_empty() {
        let t = parse(r#"{"summary":{"totalCost":5.0,"totalTokens":100}}"#).unwrap();
        assert!(t.days.is_empty());
        assert_eq!(t.today_cost, 0.0);
        assert_eq!(t.total_cost, 5.0);
    }

    #[test]
    fn falls_back_to_summing_days_without_summary() {
        let raw = r#"{"dailyTrend":[{"date":"a","cost":1.0,"totalTokens":10},{"date":"b","cost":2.0,"totalTokens":20}]}"#;
        let t = parse(raw).unwrap();
        assert_eq!(t.total_cost, 3.0);
        assert_eq!(t.total_tokens, 30.0);
    }
}
