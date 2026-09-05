-- Fix #50: health probe wrote attribution_tags as "[]" (array) instead of "{}" (object).
-- This broke distinct_attribution_tag_keys on Postgres, since jsonb_object_keys raises on arrays.

UPDATE cost_ledger SET attribution_tags = '{}' WHERE attribution_tags = '[]';
