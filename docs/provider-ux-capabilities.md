# Provider UX capability verdicts — 2026-09-07

These verdicts describe DevManager's native interactive adapters. Vendor CLI
features alone do not establish end-to-end acceptance in the installed app.

| UX feature | Claude Code | Codex | Cursor |
| --- | --- | --- | --- |
| Provider task progress | TaskCreated, TaskUpdate and TaskCompleted ingestion implemented; next-launch instruction and additive tool settings implemented | Native adapter has no verified plan-update ingress; settings remain disabled | Native adapter has no verified todo-update ingress; settings remain disabled |
| Child conversation attribution | Native `agent_id`, qualified by explicit `session_id`, with nested tool results and `last_assistant_message` | No verified native child identity ingestion | Vendor hooks exist; current DevManager adapter does not ingest them |
| Child tabs | Negotiated native-client capability; exact admitted identities only | No synthetic tabs | No synthetic tabs |
| Live Windows acceptance | Pending | Pending | Pending |

Claude's native hook context uses `agent_id`; `parent_tool_use_id` belongs to
other integration formats and is not a substitute. Missing identity remains
unattributed. DevManager does not read hook-supplied transcript paths to invent
child membership. The provider continues to own child scheduling and execution.

References: [Claude hooks](https://code.claude.com/docs/en/hooks),
[Cursor hooks](https://prod.cursor.com/docs/hooks), and
[Cursor CLI output format](https://docs.cursor.com/en/cli/reference/output-format).
Cursor's print-mode JSON output is not a reason to replace its interactive shell.

Locally inspected versions were Claude 2.1.263, codex-cli 0.153.4 and Cursor Agent
2026.09.02-c22c1a3. Only version/help probes were run; these are not through-app
provider acceptance. Linux provider launch retains its existing descendant
containment HOLD, so this machine cannot provide that acceptance.

The new `semantic_subagents` capability gates optional fact attribution and tool
result context. Hosts omit both fields for clients that did not negotiate it;
existing browser clients therefore keep their established wire format. Native
clients select children through their host-qualified task owner, retain parent
questions/approvals, and do not expose the parent composer inside a child view.
