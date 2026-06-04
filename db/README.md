# Database migrations

SQL migrations for the optional link shortening feature (Postgres, e.g. Supabase).

Each file is embedded into the binary at build time and run on startup when
`DATABASE_URL` is set. The runner uses the simple query protocol (no prepared
statements), so it works on Supabase's transaction pooler. Applied versions are
tracked in a `schema_migrations` table, so each file runs once.

## Naming

Files are named `<version>_<description>.sql`. The current set:

- `0001_init.sql` creates the `links` table that maps a short code to a target path.

## Adding a migration

1. Add the next file, for example `0002_add_hits.sql`.
2. Register it in the `MIGRATIONS` list in `src/main.rs` so it is embedded and run.

Do not edit a migration that has already been applied; add a new one instead.

## Applying manually

You can also run the SQL directly with the Supabase SQL editor or `psql`.
