# Rowbit skill

Teaches a coding agent to change data through [Rowbit](https://github.com/Ermag/rowbit): look at `describe_schema` first, read before writing, propose one narrow write at a time, hand the person the review link when a write is held, and never work around a rule.

The plugin holds the skill only. Connect Rowbit itself with `rowbit connect <client>`, which gives the client its address and a token of its own.

## Claude Code

```
/plugin marketplace add Ermag/rowbit
/plugin install rowbit@rowbit
```

Or from a terminal, in one step: `claude plugin install rowbit --marketplace Ermag/rowbit`.

## Other agents

[`skills/rowbit/SKILL.md`](skills/rowbit/SKILL.md) is a plain [Agent Skill](https://agentskills.io). Clients that read skills from a project's `.agents/skills/` directory (Codex among them) pick it up from there:

```sh
mkdir -p .agents/skills/rowbit
curl -fsSL https://raw.githubusercontent.com/Ermag/rowbit/main/plugins/rowbit/skills/rowbit/SKILL.md -o .agents/skills/rowbit/SKILL.md
```

For a client without skills, paste the file into its rules or instructions.

Apache-2.0, so other tools can carry it.
