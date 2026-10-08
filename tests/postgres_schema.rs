//! The Postgres ladders build exactly the shape `migrations/postgres/
//! schema.sql` declares, as `the_ladder_produces_exactly_the_declared_schema`
//! holds the SQLite ladder to `migrations/schema.sql`. Ignored unless asked
//! for: `scripts/test-postgres.sh` starts a database.
//!
//! The declared file is the catalogue's own description of every column,
//! constraint and index, one per line. After adding a step, run this test
//! with TOOLSITE_WRITE_SCHEMA=1 to rewrite the file, and read the diff: it is
//! the change the step makes to every site.

use toolsite::state::pg;

const DECLARED: &str = "migrations/postgres/schema.sql";

#[tokio::test]
#[ignore = "needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one"]
async fn the_postgres_ladders_produce_exactly_the_declared_schema() {
    let server = std::env::var("TOOLSITE_TEST_DATABASE_URL")
        .expect("needs TOOLSITE_TEST_DATABASE_URL; scripts/test-postgres.sh starts one");
    let name = format!("t_{}", toolsite::content::slug::random_token(12).to_lowercase());
    let (admin, connection) = tokio_postgres::connect(&server, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(connection);
    admin.batch_execute(&format!("create database {name}")).await.unwrap();
    let mut url = url::Url::parse(&server).unwrap();
    url.set_path(&name);

    let postgres = pg::connect(url.as_str(), 2).await.unwrap();
    pg::migrate(&postgres.pool, pg::LADDERS).await.unwrap();
    let schemas: Vec<String> = pg::LADDERS.iter().map(|ladder| ladder.store.to_string()).collect();
    let client = postgres.pool.get().await.unwrap();
    let mut lines = Vec::new();
    for (kind, sql) in [
        (
            "column",
            "select table_schema || '.' || table_name || '.' || column_name || ' ' || data_type
                    || case when is_nullable = 'NO' then ' not null' else '' end
                    || coalesce(' default ' || column_default, '')
               from information_schema.columns where table_schema = any($1)",
        ),
        (
            "constraint",
            "select n.nspname || '.' || c.relname || ' ' || k.conname || ': ' || pg_get_constraintdef(k.oid)
               from pg_constraint k
               join pg_class c on c.oid = k.conrelid
               join pg_namespace n on n.oid = c.relnamespace
              where n.nspname = any($1)",
        ),
        ("index", "select schemaname || ' ' || indexdef from pg_indexes where schemaname = any($1)"),
    ] {
        let mut found: Vec<String> = client
            .query(sql, &[&schemas])
            .await
            .unwrap()
            .iter()
            .map(|row| format!("{kind} {}", row.get::<_, String>(0)))
            .collect();
        found.sort();
        lines.extend(found);
    }
    drop(client);
    postgres.pool.close();
    admin.batch_execute(&format!("drop database if exists {name} with (force)")).await.unwrap();

    let actual = lines.join("\n");
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(DECLARED);
    if std::env::var_os("TOOLSITE_WRITE_SCHEMA").is_some() {
        let header = "-- The Postgres platform schema as it should look right now: every column,\n\
                      -- constraint and index the ladders in migrations/postgres/<store>/ build, as\n\
                      -- the catalogue describes them. tests/postgres_schema.rs builds a database\n\
                      -- from the ladders and compares, so the two cannot drift. Add a step, then\n\
                      -- rewrite this file with TOOLSITE_WRITE_SCHEMA=1 and read the diff.\n\n";
        std::fs::write(&path, format!("{header}{actual}\n")).unwrap();
        return;
    }
    let declared = std::fs::read_to_string(&path).unwrap();
    let declared: Vec<&str> = declared.lines().filter(|line| !line.starts_with("--") && !line.trim().is_empty()).collect();
    assert_eq!(
        declared.join("\n"),
        actual,
        "\n{DECLARED} no longer matches what the ladders build.\n\
         Add a numbered step for the change, then rewrite the file with TOOLSITE_WRITE_SCHEMA=1.\n"
    );
}
