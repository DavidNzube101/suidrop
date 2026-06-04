CREATE TABLE IF NOT EXISTS links (
    code text PRIMARY KEY,
    target text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
