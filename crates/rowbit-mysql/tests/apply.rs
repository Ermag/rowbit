//! Applies and undoes previews against the test databases from
//! `compose.yaml`. Every scenario runs on both MariaDB 11.4 and MySQL 8.4.

mod common;

use std::time::Duration;

use mysql_async::prelude::Queryable;
use rowbit_core::preview::{Applied, Preview};
use rowbit_mysql::statement::{Classified, classify};
use rowbit_mysql::{ApplyError, MySqlAdapter, PreviewLimits};

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

    async fn rows(&mut self, sql: &str) -> Vec<Vec<Option<String>>> {
        let rows: Vec<mysql_async::Row> = self.conn.query(sql).await.unwrap();
        rows.into_iter()
            .map(|r| {
                r.unwrap()
                    .into_iter()
                    .map(|v| match v {
                        mysql_async::Value::NULL => None,
                        mysql_async::Value::Bytes(b) => Some(String::from_utf8(b).unwrap()),
                        other => Some(format!("{other:?}")),
                    })
                    .collect()
            })
            .collect()
    }

    async fn preview(&self, sql: &str) -> (Classified, Preview) {
        let c = classify(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let p = self
            .adapter
            .preview(&self.database, &c, PreviewLimits::default())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        (c, p)
    }

    async fn apply(&self, c: &Classified, p: &Preview) -> Result<Applied, ApplyError> {
        self.adapter.apply(&self.database, c, p).await
    }

    async fn undo(&self, p: &Preview, a: &Applied) -> Result<(), ApplyError> {
        self.adapter.undo(&self.database, p, a).await
    }

    async fn close(self) {
        self.conn.disconnect().await.unwrap();
        self.adapter.close().await.unwrap();
    }
}

fn s(v: &str) -> Option<String> {
    Some(v.to_owned())
}

fn expect_conflict<T: std::fmt::Debug>(r: Result<T, ApplyError>, needle: &str) {
    match r {
        Err(e @ ApplyError::Conflict { .. }) => {
            let text = e.to_string();
            assert!(text.contains(needle), "{text:?}, expected {needle:?}");
            assert!(!e.agent_reason().contains("(row"), "{:?}", e.agent_reason());
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
}

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

async fn table(db: &mut Db, name: &str) {
    db.run(&format!("DROP TABLE IF EXISTS {name}")).await;
    db.run(&format!(
        "CREATE TABLE {name} (id INT AUTO_INCREMENT PRIMARY KEY, status VARCHAR(20) NOT NULL, \
         amount DECIMAL(10,2) NOT NULL, doubled DECIMAL(12,2) AS (amount * 2) VIRTUAL, \
         touched TIMESTAMP(6) NULL DEFAULT NULL ON UPDATE CURRENT_TIMESTAMP(6)) ENGINE=InnoDB"
    ))
    .await;
    db.run(&format!(
        "INSERT INTO {name} (id, status, amount) VALUES (1, 'approved', 12.50), \
         (2, 'approved', 80.00), (3, 'refunded', 5.00)"
    ))
    .await;
}

on_both!(update_applies_exactly_and_undoes);
async fn update_applies_exactly_and_undoes(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    table(&mut db, "ap_update").await;
    let original = db.rows("SELECT * FROM ap_update ORDER BY id").await;

    let (c, p) = db
        .preview(
            "UPDATE ap_update SET status = 'refunded', amount = amount * 1.1234 \
             WHERE status = 'approved'",
        )
        .await;
    let applied = db.apply(&c, &p).await.unwrap();
    assert_eq!(applied.keys.len(), 2);

    let now = db
        .rows("SELECT id, status, amount, doubled FROM ap_update ORDER BY id")
        .await;
    assert_eq!(
        now,
        [
            vec![s("1"), s("refunded"), s("14.04"), s("28.08")],
            vec![s("2"), s("refunded"), s("89.87"), s("179.74")],
            vec![s("3"), s("refunded"), s("5.00"), s("10.00")],
        ]
    );
    // The ON UPDATE column holds exactly the reviewed value.
    let touched = db.rows("SELECT touched FROM ap_update WHERE id = 1").await;
    let previewed =
        String::from_utf8(p.rows[0].after.as_ref().unwrap()[4].clone().unwrap()).unwrap();
    assert_eq!(touched[0][0].as_deref(), Some(previewed.as_str()));

    db.undo(&p, &applied).await.unwrap();
    assert_eq!(
        db.rows("SELECT * FROM ap_update ORDER BY id").await,
        original
    );
    db.close().await;
}

on_both!(drift_writes_nothing);
async fn drift_writes_nothing(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    table(&mut db, "ap_drift").await;
    let (c, p) = db
        .preview("UPDATE ap_drift SET status = 'refunded' WHERE status = 'approved'")
        .await;

    // Someone else changes a previewed row.
    db.run("UPDATE ap_drift SET amount = 13.00 WHERE id = 1")
        .await;
    expect_conflict(db.apply(&c, &p).await, "a previewed row changed (row 1)");
    // Put it back, including the ON UPDATE column the change above bumped.
    db.run("UPDATE ap_drift SET amount = 12.50, touched = NULL WHERE id = 1")
        .await;

    // A new row now matches the statement.
    db.run("INSERT INTO ap_drift (id, status, amount) VALUES (4, 'approved', 1.00)")
        .await;
    expect_conflict(db.apply(&c, &p).await, "nobody reviewed (row 4)");
    db.run("DELETE FROM ap_drift WHERE id = 4").await;

    // A previewed row is gone.
    db.run("DELETE FROM ap_drift WHERE id = 2").await;
    expect_conflict(db.apply(&c, &p).await, "a previewed row is gone (row 2)");

    let statuses = db.rows("SELECT status FROM ap_drift ORDER BY id").await;
    assert_eq!(statuses, [vec![s("approved")], vec![s("refunded")]]);
    db.close().await;
}

// Found in review: NOW() in a filter made every cleanup statement drift.
on_both!(now_in_a_filter_means_the_preview_time);
async fn now_in_a_filter_means_the_preview_time(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    db.run("DROP TABLE IF EXISTS ap_sessions").await;
    db.run("CREATE TABLE ap_sessions (id INT PRIMARY KEY, expires_at DATETIME(6)) ENGINE=InnoDB")
        .await;
    db.run(
        "INSERT INTO ap_sessions VALUES (1, NOW(6) - INTERVAL 1 DAY), \
         (2, NOW(6) + INTERVAL 1 SECOND), (3, NOW(6) + INTERVAL 1 DAY)",
    )
    .await;
    let (c, p) = db
        .preview("DELETE FROM ap_sessions WHERE expires_at < NOW()")
        .await;
    assert_eq!(p.rows.len(), 1);
    // Row 2 expires before the apply runs.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    db.apply(&c, &p).await.unwrap();
    assert_eq!(
        db.rows("SELECT id FROM ap_sessions ORDER BY id").await,
        [vec![s("2")], vec![s("3")]]
    );
    db.close().await;
}

on_both!(insert_records_assigned_keys_and_undoes);
async fn insert_records_assigned_keys_and_undoes(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    table(&mut db, "ap_insert").await;
    let (c, p) = db
        .preview("INSERT INTO ap_insert (status, amount) VALUES ('new', 1), ('new', 2.5)")
        .await;
    let applied = db.apply(&c, &p).await.unwrap();
    let keys: Vec<_> = applied
        .keys
        .iter()
        .map(|k| String::from_utf8(k.key.clone().unwrap()[0].clone().unwrap()).unwrap())
        .collect();
    assert_eq!(keys, ["4", "5"]);
    assert_eq!(
        db.rows("SELECT id, amount, doubled FROM ap_insert WHERE id > 3 ORDER BY id")
            .await,
        [
            vec![s("4"), s("1.00"), s("2.00")],
            vec![s("5"), s("2.50"), s("5.00")]
        ]
    );
    db.undo(&p, &applied).await.unwrap();
    assert!(
        db.rows("SELECT id FROM ap_insert WHERE id > 3")
            .await
            .is_empty()
    );
    db.close().await;
}

on_both!(delete_undoes_with_generated_columns);
async fn delete_undoes_with_generated_columns(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    table(&mut db, "ap_delete").await;
    let original = db.rows("SELECT * FROM ap_delete ORDER BY id").await;
    let (c, p) = db.preview("DELETE FROM ap_delete WHERE id < 3").await;
    let applied = db.apply(&c, &p).await.unwrap();
    assert_eq!(db.rows("SELECT id FROM ap_delete").await, [vec![s("3")]]);
    db.undo(&p, &applied).await.unwrap();
    assert_eq!(
        db.rows("SELECT * FROM ap_delete ORDER BY id").await,
        original
    );
    db.close().await;
}

on_both!(undo_refuses_to_overwrite_later_changes);
async fn undo_refuses_to_overwrite_later_changes(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    table(&mut db, "ap_conflict").await;
    let (c, p) = db
        .preview("UPDATE ap_conflict SET status = 'refunded' WHERE id IN (1, 2)")
        .await;
    let applied = db.apply(&c, &p).await.unwrap();
    db.run("UPDATE ap_conflict SET status = 'disputed' WHERE id = 2")
        .await;
    expect_conflict(db.undo(&p, &applied).await, "someone else's change (row 2)");
    // Nothing was restored, including row 1.
    assert_eq!(
        db.rows("SELECT status FROM ap_conflict WHERE id < 3 ORDER BY id")
            .await,
        [vec![s("refunded")], vec![s("disputed")]]
    );
    db.close().await;
}

on_both!(ddl_runs_as_reviewed);
async fn ddl_runs_as_reviewed(var: &str) {
    let Some(mut db) = Db::open(var).await else {
        return;
    };
    table(&mut db, "ap_ddl").await;
    let c = classify("alter table ap_ddl add column extra int").unwrap();
    db.adapter
        .run_ddl(&db.database, &c.canonical)
        .await
        .unwrap();
    assert_eq!(
        db.rows(
            "SELECT COUNT(*) FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'ap_ddl' AND COLUMN_NAME = 'extra'"
        )
        .await,
        [vec![s("1")]]
    );
    db.close().await;
}
