English | [한국어](ko/agents.md)

# Use from an agent

Install the CLI, configure the requester, [download the support files](install.md#download-examples-and-agent-support-files), and install the bundled skill globally or per project:

```bash
bash "$YLCC_SUPPORT_DIR/scripts/install-skill.sh" --global
# Or:
bash "$YLCC_SUPPORT_DIR/scripts/install-skill.sh" --project /path/to/project
```

Start a new agent session. For Codex, skills are discovered from `~/.agents/skills` or the project's `.agents/skills`; the included skill permits normal automatic selection. See [Codex skills](https://learn.chatgpt.com/docs/customization/overview#skills). Other agents supporting the open skill format can load [skills/ylcc/SKILL.md](../skills/ylcc/SKILL.md) from their documented skill directory.

Ask naturally: “Build this on a Linux x86_64 worker and bring back the binary,” or explicitly invoke `$ylcc`. The skill discovers workers, chooses requirements and selected inputs, submits asynchronously, checks logs/status and downloads declared results. It does not add an MCP server or run another agent on the worker.

Try these natural prompts after installing the skill:
- "Check the pool and workers, but don't run anything."
- "Run this only on a powered Windows x86_64 worker."
- "Build this on Linux x86_64 and bring back the binary."
- "Use the reviewed `ylcc.yaml` build task, then await and fetch its result."
- "Open the dashboard without giving me the token in chat."
