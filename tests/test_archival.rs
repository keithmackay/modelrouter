mod common;
use modelrouter::archival::rows_to_ndjson;
use modelrouter::db::models::CostLedgerEntry;

#[test]
fn rows_to_ndjson_produces_one_line_per_row() {
    let rows = vec![
        CostLedgerEntry {
            id: 1, user_id: 42, prompt_id: 7,
            model: "gpt-4o".to_string(), provider: "openai".to_string(),
            project: None, tokens_in: 100, tokens_out: 50,
            cost_usd: 0.01, created_at: "2024-01-01T00:00:00+00:00".to_string(),
            api_key_id: None,
        },
        CostLedgerEntry {
            id: 2, user_id: 42, prompt_id: 8,
            model: "gpt-4o".to_string(), provider: "openai".to_string(),
            project: None, tokens_in: 200, tokens_out: 100,
            cost_usd: 0.02, created_at: "2024-01-02T00:00:00+00:00".to_string(),
            api_key_id: None,
        },
    ];
    let ndjson = rows_to_ndjson(&rows);
    let lines: Vec<&str> = ndjson.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("\"id\":1"));
    assert!(lines[1].contains("\"id\":2"));
    assert!(lines[0].contains("gpt-4o"));
}
