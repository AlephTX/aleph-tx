# Repository guidance for coding agents

[AGENTS.md](../AGENTS.md) owns the shared repository rules. Root
[CLAUDE.md](../CLAUDE.md) imports it and adds Claude-specific guidance.
Keep instructions short, specific, and verifiable. Add scoped guidance only
where a module needs a different rule or an extra invariant.

Keep commands in the Makefile, operational procedures in the owning runbook,
and protocol details in code and tests. Link to those sources when a rule
needs context. Existing directory-level notes are module maps. Check their
commands and implementation details against the current source before use.

See the [OpenAI Docs for AGENTS.md](https://learn.chatgpt.com/docs/agent-configuration/agents-md)
and [Claude Code project instructions](https://code.claude.com/docs/en/memory)
for how each tool loads guidance.
