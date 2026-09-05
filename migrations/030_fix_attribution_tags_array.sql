-- Fix #50: health probe wrote attribution_tags as "[]" (array) instead of "{}" (object).
-- This broke distinct_attribution_tag_keys on Postgres, since jsonb_object_keys raises on arrays.
-- Operator note: on a large cost_ledger, this UPDATE may lock the table; consider running
-- during a maintenance window or with statement_timeout set to avoid indefinite blocking.

UPDATE cost_ledger SET attribution_tags = '{}' WHERE attribution_tags = '[]';
