//! Embedded, reversible schema migrations, applied in timestamp order.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::{Error, Result};

struct Migration {
    version: i64,
    up: &'static str,
    down: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 20261105120000,
        up: include_str!("../migrations/20261105120000_create_clip_tables.up.sql"),
        down: include_str!("../migrations/20261105120000_create_clip_tables.down.sql"),
    },
    Migration {
        version: 20261105120100,
        up: include_str!("../migrations/20261105120100_add_clip_indexes.up.sql"),
        down: include_str!("../migrations/20261105120100_add_clip_indexes.down.sql"),
    },
    Migration {
        version: 20261106090000,
        up: include_str!("../migrations/20261106090000_create_excluded_app.up.sql"),
        down: include_str!("../migrations/20261106090000_create_excluded_app.down.sql"),
    },
    Migration {
        version: 20261106090100,
        up: include_str!("../migrations/20261106090100_add_clip_thumbnail.up.sql"),
        down: include_str!("../migrations/20261106090100_add_clip_thumbnail.down.sql"),
    },
];

/// Schema version this build of ClipVault writes.
pub const LATEST_SCHEMA_VERSION: i64 = MIGRATIONS[MIGRATIONS.len() - 1].version;

const TRACKING_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS schema_migration (version INTEGER PRIMARY KEY)";

/// Applies every migration not yet recorded, each in its own transaction.
pub(crate) fn migrate_up(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(TRACKING_TABLE)?;
    let newest: Option<i64> = conn
        .query_row("SELECT MAX(version) FROM schema_migration", [], |r| {
            r.get(0)
        })
        .optional()?
        .flatten();
    if let Some(found) = newest.filter(|v| *v > LATEST_SCHEMA_VERSION) {
        return Err(Error::NewerSchema {
            found,
            known: LATEST_SCHEMA_VERSION,
        });
    }
    for migration in MIGRATIONS {
        // IMMEDIATE: two ClipVault processes starting at once must not both run a migration.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if is_applied(&tx, migration.version)? {
            continue;
        }
        tx.execute_batch(migration.up)?;
        tx.execute(
            "INSERT INTO schema_migration (version) VALUES (?1)",
            [migration.version],
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// Reverts applied migrations newer than `target`, newest first, e.g. before
/// downgrading ClipVault. `target = 0` reverts everything.
pub fn migrate_down_to(conn: &mut Connection, target: i64) -> Result<()> {
    conn.execute_batch(TRACKING_TABLE)?;
    for migration in MIGRATIONS.iter().rev().filter(|m| m.version > target) {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !is_applied(&tx, migration.version)? {
            continue;
        }
        tx.execute_batch(migration.down)?;
        tx.execute(
            "DELETE FROM schema_migration WHERE version = ?1",
            [migration.version],
        )?;
        tx.commit()?;
    }
    Ok(())
}

fn is_applied(conn: &Connection, version: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM schema_migration WHERE version = ?1)",
        [version],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE name IN ('clip', 'clip_format', 'clip_fts', 'setting', 'excluded_app')",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn migrating_down_removes_every_table() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_up(&mut conn).unwrap();

        migrate_down_to(&mut conn, 0).unwrap();

        assert_eq!(table_count(&conn), 0);
    }

    #[test]
    fn migrating_down_then_up_restores_a_working_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_up(&mut conn).unwrap();
        migrate_down_to(&mut conn, 0).unwrap();

        migrate_up(&mut conn).unwrap();

        assert_eq!(table_count(&conn), 5);
    }

    #[test]
    fn migrating_up_twice_is_harmless() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_up(&mut conn).unwrap();

        let result = migrate_up(&mut conn);

        assert!(result.is_ok());
    }

    #[test]
    fn migrating_down_to_the_first_version_keeps_only_its_tables() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_up(&mut conn).unwrap();

        migrate_down_to(&mut conn, MIGRATIONS[0].version).unwrap();

        let indexes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name LIKE 'clip_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!((indexes, table_count(&conn)), (0, 4));
    }

    #[test]
    fn migrating_down_one_step_removes_the_thumbnail_column() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_up(&mut conn).unwrap();

        migrate_down_to(&mut conn, MIGRATIONS[MIGRATIONS.len() - 2].version).unwrap();

        let thumbnail_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('clip') WHERE name = 'thumbnail'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(thumbnail_columns, 0);
    }

    #[test]
    fn refuses_a_database_from_a_newer_clipvault() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_up(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO schema_migration (version) VALUES (?1)",
            [LATEST_SCHEMA_VERSION + 1],
        )
        .unwrap();

        let result = migrate_up(&mut conn);

        assert!(matches!(result, Err(Error::NewerSchema { .. })));
    }
}
