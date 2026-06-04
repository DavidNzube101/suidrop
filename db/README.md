# Database migrations

SQL migrations for the optional link shortening feature (Postgres, e.g. Supabase).

The backend runs every file in `migrations/` in order at startup when `DATABASE_URL`
is set. Applied versions are tracked in a `_sqlx_migrations` table, so each file
runs once.

## Naming

Files are named `<version>_<description>.sql` where version is an increasing
integer. The current set:

- `0001_init.sql` creates the `links` table that maps a short code to a target path.

## Adding a migration

Create the next file, for example `0002_add_hits.sql`, with the new statements.
It will be applied automatically on the next startup. Do not edit a migration that
has already been applied; add a new one instead.

## Applying manually

You can also apply these with the Supabase SQL editor, `psql`, or the sqlx CLI:

```bash
cargo install sqlx-cli --no-default-features --features rustls,postgres
DATABASE_URL=... sqlx migrate run --source db/migrations
```
