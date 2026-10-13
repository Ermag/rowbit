//! Runs previews against the test databases from `compose.yaml`. Every
//! scenario runs on both MariaDB 11.4 and MySQL 8.4, each on its own tables.

mod common;

use std::time::{Duration, Instant};

use mysql_async::prelude::Queryable;
use rowbit_core::preview::{Cell, Preview, WriteKind};
use rowbit_mysql::statement::classify;
use rowbit_mysql::{MySqlAdapter, PreviewError, PreviewLimits};

struct Db {
    adapter: MySqlAdapter,
    conn: mysql_async::Conn,
    database: String,
}

impl Db {
    async fn open(var: &str) -> Option<Db> {
        let url = common::url(var)?;
        let opts = mysql_async::Opts::from_url(&url).expect("valid URL");
        let database = opts.db_name().expect("URL names a database").to_owned();
        Some(Db {
            adapter: MySqlAdapter::new(&url).unwrap(),
            conn: mysql_async::Conn::new(opts).await.expect("server answers"),
            database,
        })
    }

    async fn run(&mut self, sql: &str) {
        self.conn
            .query_drop(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    async fn preview(&self, sql: &str) -> Result<Preview, PreviewError> {
        self.preview_with(sql, PreviewLimits::default()).await
    }

    async fn preview_with(
        &self,
        sql: &str,
        limits: PreviewLimits,
    ) -> Result<Preview, PreviewError> {
        let statement = classify(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        self.adapter
            .preview(&self.database, &statement, limits)
            .await
    }

    async fn refused(&self, sql: &str, needle: &str) {
        match self.preview(sql).await {
            Err(PreviewError::Refused(reason)) => {
                assert!(
                    reason.contains(needle),
                    "{sql}: {reason:?}, expected {needle:?}"
                )
            }
            other => panic!("{sql}: expected a refusal, got {other:?}"),
        }
    }

    async fn close(self) {
        self.conn.disconnect().await.unwrap();
        self.adapter.close().await.unwrap();
    }
}

fn c(s: &str) -> Cell {
    Some(s.as_bytes().to_vec())
}

/// Defines one `#[tokio::test]` per database for a scenario.
macro_rules! on_both {
    ($scenario:ident) => {
        mod $scenario {
            #[tokio::test]
            async fn mariadb() {
                super::$scenario(super::common::MARIADB).await
            }
            #[tokio::test]
            async fn mysql() {
                super::$scenario(super::common::MYSQL).await
            }
        }
    };
}

async fn refunds(db: &mut Db, table: &str) {
    db.run(&format!("DROP TABLE IF EXISTS {table}")).await;
    db.run(&format!(
        "CREATE TABLE {table} (id INT PRIMARY KEY, status VARCHAR(20) NOT NULL, \
         amount DECIMAL(10,2) NOT NULL, note TEXT NULL) ENGINE=InnoDB"
    ))
    .await;
    db.run(&format!(
        "INSERT INTO {table} VALUES (1, 'approved', 12.50, NULL), \
         (2, 'approved', 480.00, 'big'), (3, 'approved', 99.99, NULL), (4, 'refunded', 5.00, NULL)"
    ))
    .await;
}

on_both!(update_shows_before_and_after_without_writing);
async fn update_shows_before_and_after_without_writing(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    refunds(&mut db, "pv_update").await;

    let p = db
        .preview(
            "UPDATE pv_update SET note = CONCAT('was ', status), status = 'refunded' \
             WHERE status = 'approved' AND amount < 100 ORDER BY id",
        )
        .await
        .unwrap();
    assert_eq!(p.kind, WriteKind::Update);
    assert_eq!(
        p.table.qualified_name(),
        format!("{}.pv_update", db.database)
    );
    assert_eq!(p.table.columns, ["id", "status", "amount", "note"]);
    assert_eq!(p.table.primary_key, [0]);
    assert_eq!(p.written, [3, 1]);
    assert_eq!(p.rows.len(), 2);
    assert_eq!(
        p.rows[0].before,
        Some(vec![c("1"), c("approved"), c("12.50"), None])
    );
    assert_eq!(
        p.rows[0].after,
        Some(vec![c("was approved"), c("refunded")])
    );
    assert_eq!(p.key(&p.rows[1]), Some(vec![c("3")]));

    let still: Vec<String> = db
        .conn
        .query("SELECT status FROM pv_update ORDER BY id")
        .await
        .unwrap();
    assert_eq!(still, ["approved", "approved", "approved", "refunded"]);
    db.close().await;
}

on_both!(delete_shows_whole_rows);
async fn delete_shows_whole_rows(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    refunds(&mut db, "pv_delete").await;

    let p = db
        .preview("DELETE FROM pv_delete WHERE status = 'refunded'")
        .await
        .unwrap();
    assert_eq!(p.kind, WriteKind::Delete);
    assert_eq!(p.written, [0, 1, 2, 3]);
    assert_eq!(p.rows.len(), 1);
    assert_eq!(
        p.rows[0].before,
        Some(vec![c("4"), c("refunded"), c("5.00"), None])
    );
    assert_eq!(p.rows[0].after, None);
    db.close().await;
}

on_both!(insert_previews_new_rows_and_keys);
async fn insert_previews_new_rows_and_keys(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    db.run("DROP TABLE IF EXISTS pv_insert").await;
    db.run(
        "CREATE TABLE pv_insert (id INT AUTO_INCREMENT PRIMARY KEY, body TEXT, \
         created_at DATETIME NULL) ENGINE=InnoDB",
    )
    .await;

    let p = db
        .preview("INSERT INTO pv_insert (body, created_at) VALUES ('a', NOW()), ('b', NULL)")
        .await
        .unwrap();
    assert_eq!(p.kind, WriteKind::Insert);
    assert_eq!(p.written, [1, 2]);
    assert_eq!(p.rows.len(), 2);
    assert_eq!(p.rows[0].after.as_ref().unwrap()[0], c("a"));
    assert!(
        p.rows[0].after.as_ref().unwrap()[1].is_some(),
        "NOW() is evaluated once"
    );
    assert_eq!(p.rows[1].after, Some(vec![c("b"), None]));
    assert_eq!(p.key(&p.rows[0]), None, "the server assigns the key");

    let p = db
        .preview("INSERT INTO pv_insert (id, body) SELECT 10 + 1, 'x'")
        .await
        .unwrap();
    assert_eq!(p.key(&p.rows[0]), Some(vec![c("11")]));

    // Previews never consume AUTO_INCREMENT values.
    db.run("INSERT INTO pv_insert (body) VALUES ('real')").await;
    let id: Option<u64> = db
        .conn
        .query_first("SELECT MAX(id) FROM pv_insert")
        .await
        .unwrap();
    assert_eq!(id, Some(1));
    db.close().await;
}

on_both!(preview_takes_no_locks);
async fn preview_takes_no_locks(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    refunds(&mut db, "pv_locks").await;

    // Another session holds every row's lock, as a busy production writer would.
    db.run("START TRANSACTION").await;
    db.run("SELECT * FROM pv_locks FOR UPDATE").await;

    let started = Instant::now();
    let p = db
        .preview("UPDATE pv_locks SET status = 'refunded' WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(p.rows.len(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    db.run("ROLLBACK").await;
    db.close().await;
}

on_both!(refuses_tables_it_cannot_guard);
async fn refuses_tables_it_cannot_guard(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    for t in [
        "pv_child",
        "pv_parent",
        "pv_myisam",
        "pv_nokey",
        "pv_trigger",
        "pv_log",
    ] {
        db.run(&format!("DROP TABLE IF EXISTS {t}")).await;
    }
    db.run("DROP VIEW IF EXISTS pv_view").await;

    db.run("CREATE TABLE pv_myisam (id INT PRIMARY KEY, v INT) ENGINE=MyISAM")
        .await;
    db.refused("UPDATE pv_myisam SET v = 1", "not InnoDB").await;

    db.run("CREATE TABLE pv_nokey (v INT) ENGINE=InnoDB").await;
    db.refused("DELETE FROM pv_nokey", "no primary key").await;

    db.run("CREATE VIEW pv_view AS SELECT 1 AS id").await;
    db.refused("DELETE FROM pv_view", "not a base table").await;

    db.refused("DELETE FROM pv_missing", "not found").await;

    db.run("CREATE TABLE pv_log (id INT AUTO_INCREMENT PRIMARY KEY, msg TEXT) ENGINE=InnoDB")
        .await;
    db.run("CREATE TABLE pv_trigger (id INT PRIMARY KEY, v INT) ENGINE=InnoDB")
        .await;
    db.run(
        "CREATE TRIGGER pv_trigger_log AFTER UPDATE ON pv_trigger \
         FOR EACH ROW INSERT INTO pv_log (msg) VALUES ('changed')",
    )
    .await;
    db.refused("UPDATE pv_trigger SET v = 1", "has triggers")
        .await;

    db.run("CREATE TABLE pv_parent (id INT PRIMARY KEY, code INT UNIQUE, name TEXT) ENGINE=InnoDB")
        .await;
    db.run(
        "CREATE TABLE pv_child (id INT PRIMARY KEY, parent_code INT, \
         FOREIGN KEY (parent_code) REFERENCES pv_parent (code) \
         ON DELETE CASCADE ON UPDATE CASCADE) ENGINE=InnoDB",
    )
    .await;
    db.refused(
        "DELETE FROM pv_parent WHERE id = 1",
        "cascading foreign key",
    )
    .await;
    db.refused("UPDATE pv_parent SET code = 2", "cascading foreign key")
        .await;
    // Columns the foreign key does not reference are safe to update.
    db.preview("UPDATE pv_parent SET name = 'x'").await.unwrap();
    db.preview("INSERT INTO pv_parent (id, code) VALUES (1, 1)")
        .await
        .unwrap();

    db.refused("INSERT INTO pv_parent (code) VALUES (1)", "primary key")
        .await;
    db.refused("UPDATE pv_parent SET nope = 1", "").await;
    db.close().await;
}

on_both!(refuses_more_rows_than_the_limit);
async fn refuses_more_rows_than_the_limit(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    refunds(&mut db, "pv_limit").await;
    let limits = PreviewLimits {
        max_rows: 3,
        ..PreviewLimits::default()
    };
    let p = db
        .preview_with("DELETE FROM pv_limit WHERE status = 'approved'", limits)
        .await
        .unwrap();
    assert_eq!(p.rows.len(), 3);
    match db.preview_with("DELETE FROM pv_limit", limits).await {
        Err(PreviewError::Refused(r)) => assert!(r.contains("more than 3 rows"), "{r}"),
        other => panic!("{other:?}"),
    }
    db.close().await;
}
