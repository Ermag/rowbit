//! The JSON API the web UI talks to, under `/api`, behind the same session
//! as everything else. Every request needs the session cookie; every one
//! that changes something also needs the session's token in the
//! `X-Rowbit-Token` header and no `Origin` but our own.
//!
//! Values go out typed (see [`cell`]), with hidden characters already made
//! visible, and the UI only ever inserts them as text.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rowbit_core::change::State as ChangeState;
use rowbit_core::preview::{Cell, Column, Preview, WriteKind};
use rowbit_store::{Change, Review};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{App, Session, constant_time_eq, same_origin, session, visible};

/// Above this many rows, approving also needs the row count typed in.
pub const CONFIRM_ROWS_ABOVE: usize = 50;
/// Rows sent per page of a change.
const PAGE_ROWS: usize = 50;
const MAX_PAGE_ROWS: usize = 1_000;
/// Changes listed at most.
const MAX_LIST: usize = 500;

pub(crate) const TOKEN_HEADER: &str = "x-rowbit-token";

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// Why a request was refused before it did anything.
pub(crate) struct Denied(StatusCode, &'static str);

impl IntoResponse for Denied {
    fn into_response(self) -> Response {
        error(self.0, self.1)
    }
}

fn signed_in(app: &App, headers: &HeaderMap) -> Result<Session, Denied> {
    session(app, headers).ok_or(Denied(StatusCode::UNAUTHORIZED, "sign in first"))
}

/// The session for a request that changes something.
fn acting(app: &App, headers: &HeaderMap) -> Result<Session, Denied> {
    let s = signed_in(app, headers)?;
    let token = headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !same_origin(app, headers) || !constant_time_eq(token.as_bytes(), s.token.as_bytes()) {
        return Err(Denied(
            StatusCode::FORBIDDEN,
            "refused; reload the page and try again",
        ));
    }
    Ok(s)
}

pub(crate) async fn me(State(app): State<App>, headers: HeaderMap) -> Response {
    match signed_in(&app, &headers) {
        Ok(s) => {
            Json(json!({ "reviewer": visible(&s.reviewer), "token": s.token })).into_response()
        }
        Err(r) => r.into_response(),
    }
}

pub(crate) async fn logout(State(app): State<App>, headers: HeaderMap) -> Response {
    let s = match acting(&app, &headers) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    app.sessions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|_, x| x.token != s.token);
    let mut r = Json(json!({})).into_response();
    r.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_static(
            "rowbit_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        ),
    );
    r
}

#[derive(Deserialize)]
pub(crate) struct ListQuery {
    state: Option<String>,
    agent: Option<String>,
    table: Option<String>,
    /// Only changes a rule would have held or blocked, in shadow mode.
    #[serde(default)]
    shadowed: bool,
    limit: Option<usize>,
}

/// Changes, newest first, filtered; and those whose apply or undo was
/// interrupted.
pub(crate) async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Response {
    if let Err(r) = signed_in(&app, &headers) {
        return r.into_response();
    }
    let state = match q.state.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => match ChangeState::parse(s) {
            Some(state) => Some(state),
            None => return error(StatusCode::BAD_REQUEST, "unknown state"),
        },
    };
    let limit = q.limit.unwrap_or(MAX_LIST).min(MAX_LIST);
    let ids = match app.engine.list(state) {
        Ok(ids) => ids,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut out = Vec::new();
    for id in ids.iter().rev() {
        if out.len() == limit {
            break;
        }
        let Ok(change) = app.engine.change(*id) else {
            continue;
        };
        if q.agent
            .as_deref()
            .is_some_and(|a| !a.is_empty() && change.agent != a)
        {
            continue;
        }
        let summary = summary(&app, &change);
        if q.shadowed && summary["shadowed"].is_null() {
            continue;
        }
        if let Some(t) = q.table.as_deref().filter(|t| !t.is_empty())
            && summary["table"].as_str().is_none_or(|x| !x.contains(t))
        {
            continue;
        }
        out.push(summary);
    }
    let interrupted = app.engine.interrupted().unwrap_or_default();
    Json(json!({ "changes": out, "interrupted": interrupted })).into_response()
}

fn table_and_kind(change: &Change) -> (Option<String>, Option<&'static str>, Option<usize>) {
    match &change.review {
        Some(Review::Rows(p)) => (
            Some(p.table.qualified_name()),
            Some(match p.kind {
                WriteKind::Insert => "insert",
                WriteKind::Update => "update",
                WriteKind::Delete => "delete",
            }),
            Some(p.rows.len()),
        ),
        Some(Review::Ddl { .. }) => (None, Some("ddl"), None),
        None => (None, None, None),
    }
}

/// What a list shows of a change.
fn summary(app: &App, change: &Change) -> Value {
    let (table, kind, rows) = table_and_kind(change);
    let history = app.engine.history(change.id).unwrap_or_default();
    // The last event that moved the change, and its reason if it had one.
    let decided = history
        .iter()
        .rev()
        .find(|e| e.from.as_deref() != Some(e.to.as_str()) || e.event == "Proposed");
    let reason = decided
        .filter(|e| {
            matches!(
                e.event.as_str(),
                "PolicyHeld" | "PolicyBlocked" | "Reject" | "ApplyFailed"
            )
        })
        .and_then(|e| e.detail.clone());
    let shadowed = history.iter().find(|e| e.event == "PolicyShadowed");
    json!({
        "id": change.id,
        "state": change.state.as_str(),
        // What a rule would have done, for an agent in shadow mode.
        "shadowed": shadowed.map(|e| json!({
            "by": visible(&e.actor),
            "would": e.detail.as_deref().map(visible),
        })),
        "agent": visible(&change.agent),
        "database": visible(&change.database),
        "table": table.map(|t| visible(&t)),
        "kind": kind,
        "rows": rows,
        "reason": reason.map(|r| visible(&r)),
        "decided_by": decided.map(|e| visible(&e.actor)),
        "created_at_ms": change.created_at_ms,
    })
}

#[derive(Deserialize)]
pub(crate) struct DetailQuery {
    offset: Option<usize>,
    limit: Option<usize>,
}

/// Everything about one change, a page of its rows at a time.
pub(crate) async fn detail(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(q): Query<DetailQuery>,
) -> Response {
    if let Err(r) = signed_in(&app, &headers) {
        return r.into_response();
    }
    match app.engine.change(id) {
        Ok(change) => Json(detail_json(
            &app,
            &change,
            q.offset.unwrap_or(0),
            q.limit.unwrap_or(PAGE_ROWS).clamp(1, MAX_PAGE_ROWS),
        ))
        .into_response(),
        Err(e) => error(StatusCode::NOT_FOUND, &e.to_string()),
    }
}

fn detail_json(app: &App, change: &Change, offset: usize, limit: usize) -> Value {
    let mut out = summary(app, change);
    let (_, _, rows) = table_and_kind(change);
    out["statement"] = json!(visible(&crate::statement(&change.sql)));
    out["review_hash"] = json!(change.review_hash);
    out["confirm_rows"] = json!(rows.is_some_and(|n| n > CONFIRM_ROWS_ABOVE));
    out["undoable"] = json!(
        change.state == ChangeState::Applied && matches!(change.review, Some(Review::Rows(_)))
    );
    match &change.review {
        Some(Review::Rows(p)) => out["preview"] = preview_json(p, offset, limit),
        Some(Review::Ddl { sql }) => out["ddl"] = json!(visible(sql)),
        None => {}
    }
    out["history"] = json!(
        app.engine
            .history(change.id)
            .unwrap_or_default()
            .iter()
            .map(|e| json!({
                "seq": e.seq,
                "event": e.event,
                "from": e.from,
                "to": e.to,
                "actor": visible(&e.actor),
                // A proposal's detail repeats the statement as JSON.
                "detail": if e.event == "Proposed" { None } else { e.detail.as_deref().map(visible) },
                "at_ms": e.at_ms,
            }))
            .collect::<Vec<_>>()
    );
    out
}

fn preview_json(p: &Preview, offset: usize, limit: usize) -> Value {
    let columns = &p.table.columns;
    let rows: Vec<Value> = p
        .rows
        .iter()
        .skip(offset)
        .take(limit)
        .map(|row| {
            let key = p.key(row).map(|k| {
                k.iter()
                    .zip(p.table.primary_key.iter())
                    .map(|(c, &i)| cell(c, &columns[i]))
                    .collect::<Vec<_>>()
            });
            let cells: Vec<Value> = columns
                .iter()
                .enumerate()
                .map(|(i, column)| {
                    let before = row.before.as_ref().map(|b| &b[i]);
                    let after = row.after.as_ref().map(|a| &a[i]);
                    json!({
                        "column": visible(&column.name),
                        "before": before.map(|c| cell(c, column)),
                        "after": after.map(|c| cell(c, column)),
                        "changed": before != after || before.is_none() || after.is_none(),
                    })
                })
                .collect();
            json!({ "key": key, "cells": cells })
        })
        .collect();
    json!({
        "kind": match p.kind { WriteKind::Insert => "insert", WriteKind::Update => "update", WriteKind::Delete => "delete" },
        "table": visible(&p.table.qualified_name()),
        "total_rows": p.rows.len(),
        "offset": offset,
        "rows": rows,
    })
}

/// A value as the UI shows it: `NULL` apart from the text `'NULL'`,
/// numbers apart from text by the column's type, bytes that aren't UTF-8
/// as hex.
pub(crate) fn cell(c: &Cell, column: &Column) -> Value {
    match c {
        None => json!({ "kind": "null" }),
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) if numeric(&column.column_type) => {
                json!({ "kind": "number", "text": visible(text) })
            }
            Ok(text) => json!({ "kind": "text", "text": visible(text) }),
            Err(_) => {
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                json!({ "kind": "binary", "text": format!("0x{hex}") })
            }
        },
    }
}

fn numeric(column_type: &str) -> bool {
    let t = column_type.to_ascii_lowercase();
    [
        "tinyint",
        "smallint",
        "mediumint",
        "int",
        "bigint",
        "decimal",
        "numeric",
        "float",
        "double",
        "real",
        "year",
    ]
    .iter()
    .any(|n| t.starts_with(n))
}

#[derive(Deserialize)]
pub(crate) struct ApproveBody {
    review: String,
    rows: Option<usize>,
}

pub(crate) async fn approve(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<ApproveBody>,
) -> Response {
    let s = match acting(&app, &headers) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    let change = match app.engine.change(id) {
        Ok(c) => c,
        Err(e) => return error(StatusCode::NOT_FOUND, &e.to_string()),
    };
    if let (_, _, Some(total)) = table_and_kind(&change)
        && total > CONFIRM_ROWS_ABOVE
        && body.rows != Some(total)
    {
        return error(
            StatusCode::BAD_REQUEST,
            &format!("type the row count, {total}, to approve this many rows"),
        );
    }
    done(
        &app,
        id,
        app.engine
            .approve(&s.reviewer, id, &body.review)
            .await
            .map(|_| ()),
    )
}

#[derive(Deserialize)]
pub(crate) struct RejectBody {
    reason: String,
}

pub(crate) async fn reject(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<RejectBody>,
) -> Response {
    let s = match acting(&app, &headers) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    let reason = body.reason.trim();
    if reason.is_empty() {
        return error(StatusCode::BAD_REQUEST, "give a reason");
    }
    done(
        &app,
        id,
        app.engine.reject(&s.reviewer, id, reason).map(|_| ()),
    )
}

pub(crate) async fn undo(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    let s = match acting(&app, &headers) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    done(&app, id, app.engine.undo(&s.reviewer, id).await.map(|_| ()))
}

/// The change as it is now, or why the action didn't happen.
fn done(app: &App, id: i64, result: Result<(), rowbit_engine::EngineError>) -> Response {
    match result.and_then(|()| app.engine.change(id)) {
        Ok(change) => Json(detail_json(app, &change, 0, PAGE_ROWS)).into_response(),
        Err(e) => error(StatusCode::CONFLICT, &visible(&e.to_string())),
    }
}

pub(crate) async fn verify(State(app): State<App>, headers: HeaderMap) -> Response {
    if let Err(r) = signed_in(&app, &headers) {
        return r.into_response();
    }
    match app.engine.verify() {
        Ok(Ok(())) => Json(json!({ "ok": true })).into_response(),
        Ok(Err(problem)) => {
            Json(json!({ "ok": false, "problem": problem.to_string() })).into_response()
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(t: &str) -> Column {
        Column {
            name: "c".into(),
            column_type: t.into(),
            generated: false,
            auto_increment: false,
        }
    }

    // Found in review 4: the text 'NULL' looked like NULL, '' like nothing.
    #[test]
    fn values_cannot_pass_for_other_values() {
        let v = |c: Cell, t: &str| cell(&c, &col(t));
        assert_eq!(v(None, "varchar(5)"), json!({"kind": "null"}));
        assert_eq!(
            v(Some(b"NULL".to_vec()), "varchar(5)"),
            json!({"kind": "text", "text": "NULL"})
        );
        assert_eq!(
            v(Some(b"12".to_vec()), "varchar(5)"),
            json!({"kind": "text", "text": "12"})
        );
        assert_eq!(
            v(Some(Vec::new()), "varchar(5)"),
            json!({"kind": "text", "text": ""})
        );
        assert_eq!(
            v(Some(b" x ".to_vec()), "varchar(5)"),
            json!({"kind": "text", "text": " x "})
        );
        assert_eq!(
            v(Some(b"12.50".to_vec()), "decimal(10,2)"),
            json!({"kind": "number", "text": "12.50"})
        );
        assert_eq!(
            v(Some(vec![0xff, 0x00]), "blob"),
            json!({"kind": "binary", "text": "0xff00"})
        );
        assert_eq!(
            v(Some("a\u{202e}b".as_bytes().to_vec()), "text"),
            json!({"kind": "text", "text": "a\\u{202e}b"})
        );
    }
}
