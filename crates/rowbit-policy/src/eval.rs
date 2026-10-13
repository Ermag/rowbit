use std::collections::BTreeMap;

use crate::schema::{Decision, Match, PolicyFile, StatementKind};

/// What the preview learned about a change, in the form rules match on.
#[derive(Debug, Clone)]
pub struct ChangeFacts {
    pub statement: StatementKind,
    /// Canonical `database.table`, as the server names it.
    pub table: String,
    /// Columns the change writes. Every column of the table for a delete.
    pub columns: Vec<String>,
    pub rows: u64,
    /// Column sums the preview computed, keyed by column name.
    pub sums: BTreeMap<String, f64>,
    pub has_primary_key: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub decision: Decision,
    /// The rule that decided, or `None` for the agent default or a floor.
    pub rule: Option<String>,
    pub reason: Option<String>,
}

impl Verdict {
    fn floor(decision: Decision, reason: &str) -> Self {
        Verdict {
            decision,
            rule: None,
            reason: Some(reason.to_owned()),
        }
    }
}

/// Decides what happens to a change proposed by `agent`.
///
/// First matching rule wins, then the built-in floors are applied, which can
/// only make the decision stricter.
pub fn evaluate(policy: &PolicyFile, agent: &str, facts: &ChangeFacts) -> Verdict {
    let Some(agent_policy) = policy.agents.get(agent) else {
        return Verdict::floor(Decision::Block, "agent is not listed in rowbit.yml");
    };
    if !facts.has_primary_key && facts.statement != StatementKind::Ddl {
        return Verdict::floor(Decision::Block, "table has no primary key");
    }

    let mut verdict = Verdict {
        decision: agent_policy.default,
        rule: None,
        reason: Some("no rule matched".to_owned()),
    };
    for rule in &agent_policy.rules {
        match matches(&rule.when, facts) {
            Ok(true) => {
                verdict = Verdict {
                    decision: rule.then,
                    rule: Some(rule.name.clone()),
                    reason: rule.reason.clone(),
                };
                break;
            }
            Ok(false) => {}
            Err(missing) => {
                return Verdict {
                    decision: Decision::Block,
                    rule: Some(rule.name.clone()),
                    reason: Some(format!("preview did not compute the sum of {missing}")),
                };
            }
        }
    }

    if facts.statement == StatementKind::Ddl && verdict.decision < Decision::Hold {
        return Verdict::floor(Decision::Hold, "schema changes are always held");
    }
    verdict
}

/// `Err(column)` when the rule needs a sum the facts do not carry.
fn matches(m: &Match, facts: &ChangeFacts) -> Result<bool, String> {
    if let Some(kinds) = &m.statement
        && !kinds.as_slice().contains(&facts.statement)
    {
        return Ok(false);
    }
    if let Some(tables) = &m.table
        && !tables.as_slice().iter().any(|p| glob(p, &facts.table))
    {
        return Ok(false);
    }
    if let Some(cols) = &m.columns
        && !cols.as_slice().iter().any(|c| facts.columns.contains(c))
    {
        return Ok(false);
    }
    if let Some(r) = m.rows
        && (r.min.is_some_and(|lo| facts.rows < lo) || r.max.is_some_and(|hi| facts.rows > hi))
    {
        return Ok(false);
    }
    if let Some(s) = &m.sum {
        let Some(&total) = facts.sums.get(&s.column) else {
            return Err(s.column.clone());
        };
        if s.min.is_some_and(|lo| total < lo) || s.max.is_some_and(|hi| total > hi) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `*` matches any run of characters, including none. Everything else is literal.
fn glob(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../examples/rowbit.yml");

    fn refund_update(rows: u64, amount: f64) -> ChangeFacts {
        ChangeFacts {
            statement: StatementKind::Update,
            table: "billing.refunds".into(),
            columns: vec!["status".into()],
            rows,
            sums: BTreeMap::from([("amount".into(), amount)]),
            has_primary_key: true,
        }
    }

    fn example() -> PolicyFile {
        PolicyFile::from_yaml(EXAMPLE).expect("example policy is valid")
    }

    #[test]
    fn glob_matching() {
        assert!(glob("billing.*", "billing.refunds"));
        assert!(glob("*.audit_*", "app.audit_log"));
        assert!(glob("billing.refunds", "billing.refunds"));
        assert!(!glob("billing.refunds", "billing.refunds_old"));
        assert!(!glob("billing.*", "crm.refunds"));
    }

    #[test]
    fn small_refund_batch_is_allowed() {
        let v = evaluate(&example(), "refund-bot", &refund_update(37, 412.50));
        assert_eq!(v.decision, Decision::Allow);
        assert_eq!(v.rule.as_deref(), Some("small-refund-batches"));
    }

    #[test]
    fn large_refund_batch_is_held() {
        let v = evaluate(&example(), "refund-bot", &refund_update(1284, 48_210.0));
        assert_eq!(v.decision, Decision::Hold);
        assert_eq!(v.rule.as_deref(), Some("refunds-need-a-person"));
    }

    #[test]
    fn unknown_agent_is_blocked() {
        let v = evaluate(&example(), "stranger", &refund_update(1, 1.0));
        assert_eq!(v.decision, Decision::Block);
    }

    #[test]
    fn missing_sum_blocks_instead_of_falling_through() {
        let mut facts = refund_update(1, 1.0);
        facts.sums.clear();
        let v = evaluate(&example(), "refund-bot", &facts);
        assert_eq!(v.decision, Decision::Block);
    }

    #[test]
    fn no_primary_key_is_blocked() {
        let mut facts = refund_update(1, 1.0);
        facts.has_primary_key = false;
        assert_eq!(
            evaluate(&example(), "refund-bot", &facts).decision,
            Decision::Block
        );
    }

    #[test]
    fn ddl_is_held_even_when_a_rule_allows_everything() {
        let p = PolicyFile::from_yaml(
            "version: 0\nagents:\n  bot:\n    rules:\n      - name: yolo\n        then: allow\n",
        )
        .unwrap();
        let facts = ChangeFacts {
            statement: StatementKind::Ddl,
            table: "app.users".into(),
            columns: vec![],
            rows: 0,
            sums: BTreeMap::new(),
            has_primary_key: true,
        };
        assert_eq!(evaluate(&p, "bot", &facts).decision, Decision::Hold);
    }

    #[test]
    fn unmatched_change_gets_the_agent_default() {
        let mut facts = refund_update(1, 1.0);
        facts.table = "crm.contacts".into();
        let v = evaluate(&example(), "refund-bot", &facts);
        assert_eq!(v.decision, Decision::Block);
        assert_eq!(v.rule, None);
    }

    #[test]
    fn invalid_files_are_rejected() {
        let bad = [
            ("version: 1\n", "unsupported version"),
            ("version: 0\nagents:\n  Bot:\n    rules: []\n", "agent id"),
            (
                "version: 0\nagents:\n  bot:\n    default: allow\n",
                "cannot be allow",
            ),
            (
                "version: 0\nagents:\n  bot:\n    scopes: {}\n",
                "unknown field",
            ),
            (
                "version: 0\nagents:\n  bot:\n    rules:\n      - {name: a, then: allow, when: {statement: ddl}}\n",
                "DDL",
            ),
            (
                "version: 0\nagents:\n  bot:\n    rules:\n      - {name: a, then: hold}\n      - {name: a, then: hold}\n",
                "duplicate",
            ),
            (
                "version: 0\nagents:\n  bot:\n    rules:\n      - {name: a, then: hold, when: {table: refunds}}\n",
                "database.table",
            ),
            (
                "version: 0\nagents:\n  bot:\n    rules:\n      - {name: a, then: hold, when: {rows: {min: 5, max: 1}}}\n",
                "rows.min",
            ),
            (
                "version: 0\nagents:\n  bot:\n    rules:\n      - {name: a, then: hold, when: {sum: {column: amount}}}\n",
                "sum needs",
            ),
        ];
        for (src, needle) in bad {
            let err = PolicyFile::from_yaml(src).unwrap_err().to_string();
            assert!(err.contains(needle), "{src:?} gave {err:?}");
        }
    }
}
