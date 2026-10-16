# Database privileges

Rowbit connects to MariaDB or MySQL as a database user of its own. This page lists what that user needs and why, what your agents need (nothing), and how to look after the SQLite file Rowbit keeps.

## Rowbit's database user

Give Rowbit a user that is used for nothing else. Replace `app` with each database agents write to, and pick the host part to match where Rowbit runs.

```sql
CREATE USER 'rowbit'@'10.0.0.%' IDENTIFIED BY '...';
GRANT SELECT, INSERT, UPDATE, DELETE, CREATE TEMPORARY TABLES, TRIGGER ON app.* TO 'rowbit'@'10.0.0.%';
GRANT PROCESS ON *.* TO 'rowbit'@'10.0.0.%';
```

| Privilege | Scope | Why |
|---|---|---|
| `SELECT` | per database | Preview reads the rows a write would change, and its table's columns and primary key from `information_schema`, which only lists tables the user has a privilege on. It also reads any table the statement reads, such as a subquery in `SET` or the source of an `INSERT ... SELECT`. |
| `CREATE TEMPORARY TABLES` | per database | Preview replays an `INSERT` or `UPDATE` on a temporary copy of the target table, visible only to Rowbit's connection. The copy is created in the database the change is proposed against, so grant it there, not only where the target table lives. Not used for `DELETE`. |
| `TRIGGER` | per database, or per table | Tables with triggers are refused in v0.1, because a trigger's effects wouldn't show in the preview or be undone. MySQL and MariaDB hide a table's triggers from `information_schema.TRIGGERS` unless the user holds `TRIGGER` on it, so without it Rowbit can't tell a table with no triggers from one it can't see, and refuses every write. Rowbit accepts the grant globally, on the database or on the table. |
| `PROCESS` | global only | Tables that another table references with a cascading foreign key are refused in v0.1, for the same reason as triggers. The referencing table may be one Rowbit's user can't see, so Rowbit reads InnoDB's own list of every foreign key (`information_schema.INNODB_FOREIGN` on MySQL, `INNODB_SYS_FOREIGN` on MariaDB), which needs `PROCESS`. It can't be granted per database. Without it, every `UPDATE` and `DELETE` is refused. |
| `INSERT`, `UPDATE`, `DELETE` | per database | Apply and undo write rows by primary key. Preview doesn't need them. |

Without any of the first four, the preview fails and the change is blocked. Rowbit never falls back to a preview it can't stand behind.

What these grants also allow, so you can weigh them:

- `PROCESS` lets the user see every session's running statement (`SHOW PROCESSLIST`, `information_schema.PROCESSLIST`, `INNODB_TRX`) and InnoDB's status, across the whole server. Agents read with Rowbit's privileges, so Rowbit refuses any statement that names `information_schema`, `performance_schema`, `mysql` or `sys`, and any of them as the default database.
- `TRIGGER` lets the user create and drop triggers. Rowbit never does.
- Previews run with Rowbit's privileges, so reviewers can see values from any table Rowbit's user can read. Agents never see preview values, though a statement can still learn about tables Rowbit can read through how many rows it matches. Scope Rowbit's `SELECT` grants to the tables agents need; per-agent grants arrive in v0.2.

Don't grant anything else: no `ALL`, no `GRANT OPTION`, nothing on `mysql.*`. Approved DDL is run as the reviewed statement by Rowbit's user, so it fails without `CREATE`, `ALTER`, `DROP` or `INDEX`. Leaving those out is a reasonable choice if schema changes go through your migration tool.

## The agent's credentials

None. An agent that has its own connection string can write around Rowbit, and nothing Rowbit does can stop it. Give agents Rowbit's endpoint and nothing else, and keep Rowbit's database password where agents can't read it, including the environment and files of the machine they run on.

## The SQLite file

Change state, previews and the audit log live in a SQLite file next to Rowbit. Previews include full before and after copies of every row a change touched, every column of them, so the file holds real production data and keeps it after the change is applied. Protect it like a replica of the database:

- Make it readable only by the user Rowbit runs as (`chmod 600`). Rowbit doesn't set the file's permissions itself.
- Keep agents away from it. The audit log's hash chain shows edits made by hand, but anyone who can write the file can rewrite the whole chain, and anyone who can read it sees the copies.
- Back it up and encrypt it the way you would the database. Undo needs the before images in it.
