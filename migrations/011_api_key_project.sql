-- Add project attribution to api_keys.
-- Project is provisioned at key creation time and propagated to cost_ledger.project
-- on every authenticated request, enabling per-project cost reporting.
ALTER TABLE api_keys ADD COLUMN project TEXT;
