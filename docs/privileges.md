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
- Previews run with Rowbit's privileges, so reviewers can see values from any table Rowbit's user can read. Agents never see preview values. An agent without scopes reads with Rowbit's privileges, and a statement can learn about any table Rowbit can read through how many rows it matches. Give agents scopes (below) so the database itself limits what each one reads.

Don't grant anything else: no `ALL`, no `GRANT OPTION`, nothing on `mysql.*`. Approved DDL from an agent without scopes is run as the reviewed statement by Rowbit's user, so it fails without `CREATE`, `ALTER`, `DROP` or `INDEX`; a scoped agent's runs on the agent's own user instead. Leaving those out is a reasonable choice if schema changes go through your migration tool.

## Agents' database users

An agent with `scopes` in `rowbit.yml` gets a database user of its own, `rowbit_<agent>`, holding only what its scopes compile to (see [Scopes](spec/policy.md#scopes)). Rowbit uses it for what the database should check against the agent's own rights: the agent's reads, an `EXPLAIN` of each write before it is previewed and again before it is applied, and approved schema changes. Preview, apply and undo stay on Rowbit's user, which needs every column of the rows and the table's triggers and foreign keys; the agent never sees what they read.

```sh
export ROWBIT_ADMIN_DATABASE_URL=mysql://admin:...@db.internal:3306/
rowbit grants diff     # what would change, per agent
rowbit grants apply    # create the users and run it, after asking
```

`rowbit grants apply` needs an administrator who can create users and grant privileges with `GRANT OPTION`. Pass that administrator's URL for the one run, in `ROWBIT_ADMIN_DATABASE_URL` or `--admin-url`; Rowbit never stores it, and Rowbit's own user never needs those privileges. Each agent's user gets the host part of Rowbit's own user, so it can connect from wherever Rowbit runs; `--agent-host` sets another. Run it again whenever `rowbit.yml`'s scopes change, or when tables are added that a pattern like `billing.ref*` should cover.

Views run with their definer's rights unless created `SQL SECURITY INVOKER`, so reading one reads whatever its definer can. A scope pattern like `billing.*` therefore covers only invoker views; a definer view is granted only when a scope names it exactly, and `grants diff` says so. A `deny_read` entry that matches no table or column is an error, so a typo can't quietly deny nothing, and Rowbit refuses to serve an agent whose scopes have one.

When it starts serving agents, Rowbit compares each agent's user with what `rowbit.yml` compiles to. A user that holds more, or that has a role or a proxy grant, is refused until `rowbit grants apply` runs, and Rowbit says so on its terminal. A user that holds less is served, and the database refuses what is missing.

Rowbit keeps the agents' users' passwords in its state file, to connect as them. Each of those users can do no more than its agent's scopes allow, which is less than Rowbit's own user. Rowbit's own user should be able to see every table agents' scopes name (it needs `SELECT` on them for previews anyway); a table it can't see makes the agent's grants look larger than `rowbit.yml` says, and the agent is refused.

## The agent's credentials

None. An agent that has its own connection string can write around Rowbit, and nothing Rowbit does can stop it. Give agents Rowbit's endpoint and nothing else, and keep Rowbit's database password where agents can't read it, including the environment and files of the machine they run on.

## The SQLite file

Change state, previews and the audit log live in a SQLite file next to Rowbit. Previews include full before and after copies of every row a change touched, every column of them, so the file holds real production data and keeps it after the change is applied. Protect it like a replica of the database:

- Keep it readable only by the user Rowbit runs as. On Unix, Rowbit creates a new file as `600` before writing anything, so the `rowbit.db-wal` and `rowbit.db-shm` files SQLite adds next to it are `600` too. A file that already exists keeps the mode you gave it. It also holds the hash of the review page's password.
- Keep agents away from it. The audit log's hash chain shows edits made by hand, but anyone who can write the file can rewrite the whole chain, and anyone who can read it sees the copies.
- Back it up and encrypt it the way you would the database. Undo needs the before images in it.
- It holds secrets too, all as hashes except one: the review page's password (Argon2id hash), agents' MCP tokens (SHA-256), and **scoped agents' database passwords, in plain text**, because Rowbit has to log in with them. Each of those users can do no more than its agent's scopes allow. `rowbit grants apply --rotate-passwords` replaces them.
- Rowbit keeps everything; it deletes no change, preview or event. See [Running Rowbit](operating.md#how-long-data-is-kept).
