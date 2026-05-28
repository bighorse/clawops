-- Track in-flight SOP name when clawops asks the user for their company name.
-- Set when the gateway intercepts a SOP trigger (no valid enterprise name) and
-- prompts the user; cleared once the company name is received and the SOP is
-- re-triggered automatically.
ALTER TABLE users ADD COLUMN pending_sop_name TEXT;
