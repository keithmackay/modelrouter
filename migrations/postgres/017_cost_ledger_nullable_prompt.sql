-- Make prompt_id nullable in cost_ledger to support X-No-Log requests.
ALTER TABLE cost_ledger ALTER COLUMN prompt_id DROP NOT NULL;
