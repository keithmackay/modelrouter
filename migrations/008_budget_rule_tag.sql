-- migrations/008_budget_rule_tag.sql
ALTER TABLE budget_rules ADD COLUMN tag TEXT;
