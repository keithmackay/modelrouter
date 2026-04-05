-- migrations/006_concurrent_limit.sql
ALTER TABLE budget_rules ADD COLUMN max_concurrent INTEGER;
