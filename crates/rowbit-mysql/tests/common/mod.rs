//! Shared by the integration tests. Each test reads its database URL from
//! the environment and skips when it is unset, unless `ROWBIT_REQUIRE_DB=1`
//! (set in CI), which turns a missing URL into a failure so the integration
//! job can never pass by skipping.

pub const MARIADB: &str = "ROWBIT_TEST_MARIADB_URL";
pub const MYSQL: &str = "ROWBIT_TEST_MYSQL_URL";

pub fn url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(url) => Some(url),
        Err(_) if std::env::var("ROWBIT_REQUIRE_DB").as_deref() == Ok("1") => {
            panic!("{var} is not set and ROWBIT_REQUIRE_DB=1")
        }
        Err(_) => {
            eprintln!("skipping: {var} is not set (run `docker compose up -d --wait`)");
            None
        }
    }
}

/// The same server as another user from `scripts/test-db-init.sql`, whose
/// password is its name.
#[allow(dead_code)]
pub fn as_user(url: &str, user: &str) -> String {
    url.replacen("//rowbit:rowbit@", &format!("//{user}:{user}@"), 1)
}
