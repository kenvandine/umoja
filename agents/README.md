# Umoja agents

`umoja-agents` runs one personal agent per account in an OpenShell sandbox
on LXD, and puts its consent prompts on the Umoja desktop: the panel's
Agents button, notifications, and the "Background agent" row in Ask Umoja's
settings. Hermes is the first agent, but umoja-agentd knows nothing about
Hermes in particular. An agent is two things:

1. **A catalog entry**, `catalog/<id>.json`, installed to
   `/usr/share/umoja/agents/catalog/`:

   ```json
   {
     "id": "hermes",
     "name": "Hermes",
     "description": "One line for the setup picker.",
     "image": "ghcr.io/kenvandine/umoja-hermes:latest",
     "command": ["/usr/local/bin/umoja-agent-start"],
     "min_context": 65536,
     "approval_modes": { "careful": "manual", "balanced": "smart", "hands-off": "smart" },
     "messaging": ["telegram"],
     "quiet_hosts": ["hermes-agent.nousresearch.com"]
   }
   ```

   `id` names the sandbox and the `@id` hand-off in Ask Umoja (`@agent`
   always works too). `min_context` is the smallest model context the agent
   accepts; the local model is loaded with at least that much.
   `quiet_hosts` are sites the agent fetches for itself (update checks,
   plugin catalogs): Umoja keeps them blocked without asking the user.
   `approval_modes` maps Umoja's presets onto the agent's own approval
   setting, which the bridge receives as `UMOJA_APPROVALS`.

2. **A sandbox image** that runs as an unprivileged user with `HOME=/sandbox`,
   ships `/etc/openshell/policy.yaml` (see `hermes-image/policy.yaml`), and
   has two programs:

   - `umoja-agent-start`, the main process. It waits for
     `/sandbox/.config/umoja/agent.env`, starts the agent with those
     settings, and starts it again when it exits.
   - `umoja-agent-bridge`, which implements the protocol below.

## Settings: agent.env

umoja-agentd writes these as shell-quoted `KEY='value'` lines:

| Key | Meaning |
| --- | --- |
| `UMOJA_MODEL_URL`, `UMOJA_MODEL`, `UMOJA_MODEL_KEY` | OpenAI-compatible endpoint, model and key. The local model is reached through a relay at `http://host.openshell.internal:<port>/api/v1` |
| | On the local model, `UMOJA_MODEL` is usually `user.Umoja-Agent-Auto`, a Lemonade router that keeps requests local or, with cloud offload on, sends long tasks and complex requests to the user's cloud model (`bin/umoja-lemonade-routing`) |
| `UMOJA_MODEL_CONTEXT` | context window of the local model, when known |
| `UMOJA_APPROVALS` | the agent's approval mode for the chosen preset |
| `UMOJA_AGENT_KEY` | a random secret for the agent's local API, if it has one |
| `UMOJA_TELEGRAM_TOKEN`, `UMOJA_TELEGRAM_ALLOWED` | messaging, when the user connects it |

Files go in `/sandbox/workspace`, which is the user's `~/Agents/<Name>`.
Shared folders are mounted under `/sandbox/shared`.

## The bridge protocol (v1)

umoja-agentd runs `umoja-agent-bridge VERB [ARG]` through OpenShell's exec
API. The request body, when there is one, is JSON in `$UMOJA_BODY`. Each
answer is one line `@@{json}` on stdout; other output is ignored. An
answer `{"error": "..."}` is shown to the user, so write it for a person.

| Verb | Body / arg | Answer |
| --- | --- | --- |
| `health` | | `{"ok": bool}` |
| `restart` | | `{"ok": true}` after the agent is stopped (the start script brings it back with the new agent.env) |
| `submit` | `{"input", "instructions", "session"}` | `{"run": id, "session": id}`. `session` continues a conversation; it can be empty |
| `status RUN` | | `{"status", "output", "error", "session", "pending"}` |
| `events RUN` | | `{"event", "data"}` lines until the run ends |
| `approve` | `{"run", "id", "generation", "allow": bool}` | `{"ok": true}` |
| `answer` | `{"run", "id", "generation", "text"}` | `{"ok": true}` |
| `stop RUN` | | `{"ok": true}` |
| `write` | `{"path", "hex", "mode"}` | `{"ok": true}` |

`status` is one of `running`, `waiting`, `completed`, `failed`, `cancelled`,
or `gone` (the agent no longer knows the run, for example after a restart).
`pending` lists what the run waits on:
`{"kind": "approval" | "question", "id", "generation", "text", "detail"}`.
`text` is the command or the question.

Events umoja-agentd shows: `tool.started` `{tool, preview}`,
`tool.completed` `{tool, preview, error}`, `message.delta`, and
`run.completed`, `run.failed` or `run.cancelled` at the end. Unknown events
are ignored.

An agent that has no approvals of its own, or no questions, never reports
`pending`. Network consent still works for it: OpenShell holds back every
site the policy doesn't allow, and umoja-agentd asks the user.

## Adding an agent (OpenClaw, PicoClaw, ...)

1. Make an image like `hermes-image/`: the agent, `policy.yaml`, and the two
   scripts. A CLI-only agent can implement `submit` by starting its
   one-shot command in the background with its output in a file, and
   `status`/`events` by reading that file.
2. Add `catalog/<id>.json`, and install it in `debian/rules`.
3. Add a workflow like `.github/workflows/hermes-image.yml` that publishes
   the image.
4. Run `tools/test/umoja-agents-test` against it. Its scripted model
   (`tools/test/agents/fake-model.py`) drives tool calls through any agent
   that speaks OpenAI chat completions.
