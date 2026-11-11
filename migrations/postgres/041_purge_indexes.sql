-- Retention purges select rows by age, and the prompt purge unlinks the
-- ledger rows that point at the prompts it deletes; each lookup needs an
-- index, or every batch scans the whole table.
CREATE INDEX IF NOT EXISTS idx_prompts_created ON prompts(created_at);
CREATE INDEX IF NOT EXISTS idx_cost_ledger_prompt ON cost_ledger(prompt_id);
CREATE INDEX IF NOT EXISTS idx_cost_ledger_created ON cost_ledger(created_at);
CREATE INDEX IF NOT EXISTS idx_request_failures_created ON request_failures(created_at);
CREATE INDEX IF NOT EXISTS idx_hook_metrics_invoked ON hook_metrics(invoked_at);
CREATE INDEX IF NOT EXISTS idx_audit_log_created ON audit_log(created_at);
