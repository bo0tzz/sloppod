# 4. Interaction is asynchronous

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

Waiting is common: for the user's next message, for an approval, for a long command, for another session's reply. If every one of these blocks the agent until the turn ends, we lose time and context. Messages sent while the agent is working should reach it without waiting for the turn to end.

## Decision Outcome

Each session has one **inbox**. Everything addressed to the session goes through it:

- the user's messages
- approval decisions
- grant changes
- wake events
- messages from sub-sessions and peers

Each item has an urgency:

| Urgency | Delivery | Typical use |
|---|---|---|
| `interrupt` | Abort the current generation now | "stop, wrong cluster" |
| `next-boundary` (default) | At the next tool result, within the same turn | messages the user steers in, approval decisions, sub-session replies |
| `idle` | When the turn ends | messages the user sends during a turn, low-priority information |

- **The user's messages during a turn** queue for the next turn, and the UI shows them there. Each queued message can be steered into the running turn, at `next-boundary`. Sending is the common case and shouldn't disrupt work in progress; steering is the deliberate choice.
- **Delivery mechanism:** mid-turn delivery uses the agent's steering support where it has it. `claude-agent-acp` supports this as an `_session/steering` ACP extension. Agents without steering fall back to queueing, or to cancel-then-prompt for `interrupt`.
- **Requests and proposals never block.** They are tools the runtime provides to the agent, e.g. over MCP, and they return a pending ID as their result straight away; the agent doesn't need any special support for this. The decision arrives later through the inbox: steered in mid-turn, or as a new prompt if the session is idle by then. This is separate from the agent's own permission prompts (ACP `session/request_permission`), which do hold the tool call they belong to until answered.
- **Long-running work goes to the background.** Long tool calls are moved to the background automatically (for Claude Code, `CLAUDE_CODE_AUTO_BACKGROUND_TIMEOUT_MS`), so the next tool boundary is rarely more than a few seconds away.
- **Instructions:** the runtime's instruction snippet tells agents to background long work, treat approvals as asynchronous, carry on with independent work while waiting, and park ([ADR 5](0005-park-and-wake.md)) when nothing is left to do.

### Consequences

- Good: corrections and decisions reach the agent within seconds, not at the end of a turn.
- Bad: steering is not yet part of the ACP spec. Two competing proposals are open, RFD PRs [#2255](https://github.com/agentclientprotocol/agent-client-protocol/pull/2255) and [#1261](https://github.com/agentclientprotocol/agent-client-protocol/pull/1261), so we depend on a vendor extension for now.

## More Information

Spike, 2026-10-26, `claude-agent-acp` 0.97.0 with Haiku:

- **During a tool call:** a steer sent 5.5 s into a 40 s foreground command moved the command to the background, and the steer was answered at 8.5 s.
- **During generation:** a steer sent mid-essay cut it off and was answered about 1 s later.

For comparison, in Claude Code's own terminal UI, 93% of 441 messages queued while the agent worked were taken up after 0–1 tool rounds. That is a tendency, not a guarantee: runs of five or more tool calls and a message without the queued message being picked up have been seen since.

2026-10-27: user messages sent during a turn moved from `next-boundary` to `idle`: they queue for the next turn and can be steered in one by one (#20). Steering is still delivered at the adapter's `now` priority, which pre-empts the running generation, rather than at `next-boundary`; `claude-agent-acp` doesn't let the client choose yet.

2026-10-27: questions from the agent no longer block its turn (#29). Agents ask with sloppod's `ask_user` MCP tool, which shows the questions like any other and returns at once; the answer arrives as a notice. Claude Code's `AskUserQuestion`, which keeps the turn open until answered, is disallowed. Codex sessions get the same tool.

2026-10-28: the inbox survives restarts ([#39](https://github.com/bo0tzz/sloppod/issues/39)), as far as it's built: messages and notices queued for a session's agent (behind a turn, or while it isn't up) are kept in the database as the queue changes and restored on startup, then run once the agent is back, or wake a parked session. The agent's own open permission requests and its in-flight turn already survived, by the agent outliving the runtime's restarts (ADR 17). Urgency levels are still to come.

2026-10-28: nothing on its way to the agent is lost to a restart ([#237](https://github.com/bo0tzz/sloppod/issues/237)). Queued messages taken for a turn, and notices sent to an agent that's up (as a new turn or steered into one), stay in the stored queue, with the id of the request carrying them once sent, until the agent answers that request. After a restart, the supervisor reports the agent's open requests: what's kept under one of them reached the agent and isn't sent again, and the rest goes back to the front of the queue. Delivery is at least once, twice only if a restart falls between sending and noting the request. A notice whose steer fails goes back to the queue, and starts a turn if the agent is idle by then. Before, those were only in memory between being taken and being sent, so a sub-session's report or a proposal's decision could be lost.
