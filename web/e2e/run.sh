#!/usr/bin/env bash
# Runs the browser tests against a real `rowbit serve` on the MariaDB test
# database from compose.yaml (docker compose up -d --wait). Builds Rowbit,
# proposes three changes for the tests to review, serves the UI on a free
# loopback port, and stops it afterwards.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'kill "${server:-}" 2>/dev/null || true; rm -rf "$work"' EXIT

cargo build --quiet --locked --manifest-path "$root/Cargo.toml" -p rowbit-cli
rowbit="$root/target/debug/rowbit"
export ROWBIT_DATABASE_URL=${ROWBIT_TEST_MARIADB_URL:-mysql://rowbit:rowbit@127.0.0.1:33061/rowbit_test}

docker compose -f "$root/compose.yaml" exec -T mariadb mariadb -urowbit -prowbit rowbit_test -e "
  DROP TABLE IF EXISTS e2e_refunds;
  CREATE TABLE e2e_refunds (id INT PRIMARY KEY, status VARCHAR(20) NOT NULL,
    amount DECIMAL(10,2) NOT NULL, note TEXT NULL) ENGINE=InnoDB;
  INSERT INTO e2e_refunds VALUES (1,'approved',12.50,NULL), (2,'approved',80,'NULL'),
    (3,'approved',7,'x'), (4,'refunded',5,NULL);"

cd "$work"
printf 'version: 0\nagents:\n  refund-bot:\n    default: hold\n' > rowbit.yml
"$rowbit" propose --agent refund-bot "UPDATE e2e_refunds SET status = 'refunded', note = 'batch 7' WHERE status = 'approved'"
"$rowbit" propose --agent refund-bot "DELETE FROM e2e_refunds WHERE id = 4"
"$rowbit" propose --agent refund-bot "ALTER TABLE e2e_refunds ADD COLUMN x INT"

port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
"$rowbit" serve --listen "127.0.0.1:$port" > serve.log 2>&1 &
server=$!
disown
for _ in $(seq 50); do
  grep -q "review page on" serve.log 2>/dev/null && break
  sleep 0.2
done
# The first `serve` prints the page password once.
password=$(grep -A2 "shown this once" serve.log | tr -d ' ' | grep -E '^[a-z0-9]{6}(-[a-z0-9]{6}){3}$')

cd "$root/web"
ROWBIT_URL="http://127.0.0.1:$port" ROWBIT_PASSWORD="$password" npx playwright test
