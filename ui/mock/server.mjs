#!/usr/bin/env node
// A mock sloppod runtime implementing docs/api.md (REST and ACP over /acp) with a scripted fake agent.
//
//   node mock/server.mjs [--port 8080] [--static build] [--delay 40] [--no-seed]
//
// --static serves a built UI with SPA fallback, like the runtime does. --delay is the pause between
// streamed chunks in milliseconds; turn steps are multiples of it. A new session's agent takes eight
// steps to start, or three seconds when its first message contains "slow start" (for testing the queue).

import { randomBytes, randomUUID } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { parseArgs } from 'node:util';
import { formatPatch, structuredPatch } from 'diff';
import { WebSocketServer } from 'ws';

const { values: args } = parseArgs({
  options: {
    port: { type: 'string', default: process.env.PORT ?? '8080' },
    static: { type: 'string' },
    delay: { type: 'string', default: process.env.MOCK_DELAY ?? '40' },
    'no-seed': { type: 'boolean', default: false },
  },
});

const DELAY = Number(args.delay);
const REPOS = ['bo0tzz/sloppod', 'bo0tzz/memini', 'immich-app/static-pages'];
// The adapter's model picker, as it appears in `configOptions`.
const MODELS = [
  { value: 'default', name: 'Default (recommended)', description: 'Sonnet' },
  { value: 'sonnet', name: 'Sonnet' },
  { value: 'opus', name: 'Opus' },
  { value: 'haiku', name: 'Haiku' },
];
/** The user's defaults for new sessions. `models` fills in once a session has offered them. */
const settings = { model: null, models: [] };
const CANCELLED = Symbol('cancelled');

/** @type {Map<string, MockSession>} */
const sessions = new Map();
const proposals = new Map();
const connections = new Set();
const credentials = [];

const now = () => new Date().toISOString();
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const sha = () => randomBytes(20).toString('hex');

class MockSession {
  constructor(data) {
    this.data = data;
    /** Every `session/update` params object, replayed by `session/load`. */
    this.log = [];
    /** @type {Set<Connection>} */
    this.subscribers = new Set();
    /** Open permission requests by toolCallId. */
    this.pending = new Map();
    this.turn = null;
    /** Set by an "always allow" answer: later edits in this session don't ask. */
    this.alwaysAllow = false;
    this.model = settings.model ?? 'default';
    /** Prompts sent before the agent was up: {text, done(stopReason), fail(error)}. */
    this.queue = [];
    this.startTimer = null;
  }

  emit(update) {
    const params = { sessionId: this.data.id, update };
    this.log.push(params);
    for (const conn of this.subscribers) conn.notify('session/update', params);
  }

  setPhase(phase) {
    this.data.phase = phase;
    this.broadcast();
  }

  broadcast() {
    this.data.updated_at = now();
    for (const conn of this.subscribers) conn.notify('_sloppod/session', { session: this.data });
  }

  configOptions() {
    return [
      {
        id: 'model',
        name: 'Model',
        description: 'AI model to use',
        category: 'model',
        type: 'select',
        currentValue: this.model,
        options: MODELS,
      },
    ];
  }
}

class Connection {
  constructor(ws) {
    this.ws = ws;
    this.loaded = new Set();
    this.nextId = 1;
    this.awaiting = new Map();
  }

  send(message) {
    if (this.ws.readyState === this.ws.OPEN) this.ws.send(JSON.stringify(message));
  }

  notify(method, params) {
    this.send({ jsonrpc: '2.0', method, params });
  }

  request(method, params, onResponse) {
    const id = this.nextId++;
    this.awaiting.set(id, onResponse);
    this.send({ jsonrpc: '2.0', id, method, params });
    return id;
  }

  respond(id, result) {
    this.send({ jsonrpc: '2.0', id, result });
  }

  error(id, code, message) {
    this.send({ jsonrpc: '2.0', id, error: { code, message } });
  }
}

// --- the fake agent -------------------------------------------------------------------------

const FILE = 'ui/src/lib/acp/connection.ts';
const OLD_SNIPPET = `  #scheduleReconnect() {
    this.#timer = setTimeout(() => this.connect(), 1000);
  }`;
const NEW_SNIPPET = `  #scheduleReconnect() {
    const delay = Math.min(500 * 2 ** this.#attempt, MAX_BACKOFF_MS);
    this.#attempt++;
    this.#timer = setTimeout(() => this.connect(), delay);
  }`;
const TEST_OUTPUT = `> sloppod-ui@0.0.0 test
> playwright test reconnect

Running 3 tests using 1 worker

  ✓  1 reconnect.spec.ts:5:1 › reconnects after the socket drops (1.2s)
  ✓  2 reconnect.spec.ts:18:1 › backs off between attempts (2.4s)
  ✓  3 reconnect.spec.ts:31:1 › replays history once after reconnecting (1.1s)

  3 passed (5.3s)`;

function chunks(text) {
  const words = text.match(/\S+\s*/g) ?? [text];
  const out = [];
  for (let i = 0; i < words.length; i += 3) out.push(words.slice(i, i + 3).join(''));
  return out;
}

function promptText(prompt) {
  return (Array.isArray(prompt) ? prompt : [])
    .map((block) => (block?.type === 'text' ? block.text : ''))
    .join('');
}

function echoUser(session, text, { steering = false, notice = false } = {}) {
  const sloppod = { ...(steering ? { steering: true } : {}), ...(notice ? { notice: true } : {}) };
  session.emit({
    sessionUpdate: 'user_message_chunk',
    content: { type: 'text', text },
    messageId: randomUUID(),
    ...(steering || notice ? { _meta: { sloppod } } : {}),
  });
}

/**
 * Tells the agent about something sloppod did, like deciding a proposal: recorded as a notice, steered
 * into a running turn, or answered with a short turn of its own when the agent is idle.
 */
async function notifyAgent(session, text, ack) {
  echoUser(session, text, { notice: true });
  if (session.turn) {
    session.turn.steers.push(text);
    return;
  }
  if (session.data.phase !== 'idle') return;
  session.turn = { cancelled: false, steers: [] };
  session.setPhase('working');
  try {
    await sleep(DELAY * 2);
    const messageId = randomUUID();
    for (const piece of chunks(ack)) {
      session.emit({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: piece }, messageId });
      await sleep(DELAY);
    }
  } finally {
    session.turn = null;
    if (sessions.has(session.data.id) && session.data.phase !== 'stopped') session.setPhase('idle');
  }
}

const sessionBranch = (session) => `sloppod/${session.data.id.slice(0, 8)}`;
const hasRepoGrant = (session, repo) =>
  session.data.grants.some((grant) => grant.capability === 'repo' && grant.params.repo === repo);

/** Gives the session write access to another repository; the first one also becomes its `repo`. */
function grantRepo(session, repo) {
  if (!hasRepoGrant(session, repo)) {
    session.data.grants.push({ capability: 'repo', params: { repo, access: 'rw', branch: sessionBranch(session) } });
  }
  if (!session.data.repo) {
    session.data.repo = repo;
    session.data.branch = sessionBranch(session);
  }
  session.broadcast();
}

function requestPermission(session, toolCall, options) {
  return new Promise((resolve) => {
    const pending = { toolCall, options, resolve, requests: new Map() };
    session.pending.set(toolCall.toolCallId, pending);
    session.setPhase('waiting');
    for (const conn of session.subscribers) sendPermission(session, pending, conn);
  });
}

function sendPermission(session, pending, conn) {
  const toolCallId = pending.toolCall.toolCallId;
  const rpcId = conn.request(
    'session/request_permission',
    { sessionId: session.data.id, toolCall: pending.toolCall, options: pending.options },
    (message) => {
      pending.requests.delete(conn);
      if (session.pending.get(toolCallId) !== pending) return;
      resolvePermission(session, pending, message.result?.outcome ?? { outcome: 'cancelled' }, conn);
    },
  );
  pending.requests.set(conn, rpcId);
}

function resolvePermission(session, pending, outcome, answeredBy) {
  const toolCallId = pending.toolCall.toolCallId;
  session.pending.delete(toolCallId);
  for (const conn of [...pending.requests.keys(), ...(answeredBy ? [answeredBy] : [])]) {
    conn.notify('_sloppod/permission_resolved', { sessionId: session.data.id, toolCallId });
  }
  pending.requests.clear();
  if (session.data.phase === 'waiting') session.setPhase('working');
  pending.resolve(outcome);
}

async function runTurn(session, text, { echo = true } = {}) {
  const turn = { cancelled: false, steers: [] };
  session.turn = turn;
  session.setPhase('working');
  if (echo) echoUser(session, text);
  if (!session.data.title) {
    const title = text.split('\n')[0].slice(0, 60);
    session.emit({ sessionUpdate: 'session_info_update', title, updatedAt: now() });
    session.data.title = title;
    session.broadcast();
  }
  try {
    await agentScript(session, turn, text);
    return 'end_turn';
  } catch (error) {
    if (error === CANCELLED) {
      for (const params of [...session.log]) {
        const update = params.update;
        if (update.sessionUpdate === 'tool_call' && update.status !== 'completed' && !isFinished(session, update.toolCallId)) {
          session.emit({ sessionUpdate: 'tool_call_update', toolCallId: update.toolCallId, status: 'failed' });
        }
      }
      return 'cancelled';
    }
    throw error;
  } finally {
    session.turn = null;
    if (sessions.has(session.data.id) && session.data.phase !== 'stopped') session.setPhase('idle');
  }
}

function isFinished(session, toolCallId) {
  return session.log.some(
    ({ update }) =>
      update.toolCallId === toolCallId &&
      (update.status === 'completed' || update.status === 'failed'),
  );
}

async function agentScript(session, turn, prompt) {
  const delay = session.fast ? 0 : DELAY;
  const checkCancel = () => {
    if (turn.cancelled) throw CANCELLED;
  };
  const say = async (text, kind = 'agent_message_chunk') => {
    const messageId = randomUUID();
    for (const piece of chunks(text)) {
      session.emit({ sessionUpdate: kind, content: { type: 'text', text: piece }, messageId });
      await sleep(delay);
      checkCancel();
    }
  };
  // A step boundary: where a real agent would pick up steering messages.
  const step = async (n = 1) => {
    await sleep(delay * n);
    checkCancel();
    while (turn.steers.length > 0) {
      const steer = turn.steers.shift();
      await say(`Got your message mid-turn: “${steer}”. I'll take that into account for the rest of this turn.`);
    }
  };
  const tool = (fields) => session.emit({ sessionUpdate: 'tool_call', ...fields });
  const update = (toolCallId, fields) => session.emit({ sessionUpdate: 'tool_call_update', toolCallId, ...fields });
  const plan = (statuses) =>
    session.emit({
      sessionUpdate: 'plan',
      entries: [
        { content: 'Read the reconnect code', priority: 'medium', status: statuses[0] },
        { content: 'Add exponential backoff', priority: 'high', status: statuses[1] },
        { content: 'Run the end-to-end tests', priority: 'medium', status: statuses[2] },
        { content: 'Push and propose a pull request', priority: 'low', status: statuses[3] },
      ],
    });

  if (!session.data.repo && /request a repository/i.test(prompt)) {
    const repo = availableRepos().at(-1) ?? REPOS.at(-1);
    const reason = 'The change you asked for lives in that repository, and this session has none yet.';
    await step(2);
    await say("This session has no repository yet, so I'll ask for write access to one.");
    const requestId = randomUUID();
    tool({
      toolCallId: requestId,
      title: 'mcp__sloppod__request_repository',
      kind: 'other',
      status: 'pending',
      rawInput: { repo, reason },
    });
    await step(2);
    const proposal = createRepoGrant(session, repo, reason);
    update(requestId, {
      status: 'completed',
      content: [{ type: 'content', content: { type: 'text', text: `Proposal ${proposal.id} created and waiting for human review.` } }],
    });
    await say(`I've asked for write access to **${repo}**. I'll carry on once you approve it.`);
    return;
  }

  await step(3);
  await say(
    `The request is: “${prompt.trim().slice(0, 200)}”. Before changing anything I should read how the socket reconnects today, then add backoff and run the tests.`,
    'agent_thought_chunk',
  );
  plan(['in_progress', 'pending', 'pending', 'pending']);
  await step(2);
  await say("I'll start by reading the connection code to see how reconnects work today.");
  await step(2);

  const readId = randomUUID();
  tool({
    toolCallId: readId,
    title: `Read ${FILE}`,
    kind: 'read',
    status: 'pending',
    locations: [{ path: `/workspace/sloppod/${FILE}`, line: 40 }],
    rawInput: { file_path: `/workspace/sloppod/${FILE}` },
  });
  await step(2);
  update(readId, { status: 'in_progress' });
  await step(2);
  update(readId, {
    status: 'completed',
    content: [{ type: 'content', content: { type: 'text', text: '```ts\n' + OLD_SNIPPET + '\n```' } }],
  });
  plan(['completed', 'in_progress', 'pending', 'pending']);
  await step(2);

  await say(
    'Reconnects use a **fixed one-second delay**, so a runtime restart gets hammered. I’ll switch to exponential backoff, capped at ten seconds:\n\n- start at 500 ms\n- double on every failed attempt\n- reset once a `session/load` succeeds',
  );
  await step(2);

  const editId = randomUUID();
  const editCall = {
    toolCallId: editId,
    title: `Edit ${FILE}`,
    kind: 'edit',
    status: 'pending',
    content: [{ type: 'diff', path: `/workspace/sloppod/${FILE}`, oldText: OLD_SNIPPET, newText: NEW_SNIPPET }],
    locations: [{ path: `/workspace/sloppod/${FILE}`, line: 40 }],
  };
  tool(editCall);
  let allowed = true;
  if (!session.alwaysAllow) {
    const outcome = await requestPermission(session, editCall, [
      { optionId: 'allow', name: 'Allow', kind: 'allow_once' },
      { optionId: 'allow_always', name: 'Always allow', kind: 'allow_always' },
      { optionId: 'reject', name: 'Reject', kind: 'reject_once' },
    ]);
    checkCancel();
    if (outcome.outcome !== 'selected') throw CANCELLED;
    allowed = outcome.optionId !== 'reject';
    if (outcome.optionId === 'allow_always') session.alwaysAllow = true;
  }
  if (!allowed) {
    update(editId, { status: 'failed' });
    await step(1);
    await say("Understood, I won't make that edit. Let me know how you'd like to approach it instead.");
    plan(['completed', 'pending', 'pending', 'pending']);
    return;
  }
  update(editId, { status: 'in_progress' });
  await step(2);
  update(editId, { status: 'completed' });
  plan(['completed', 'completed', 'in_progress', 'pending']);
  await step(2);

  const testId = randomUUID();
  tool({
    toolCallId: testId,
    title: 'npm test -- reconnect',
    kind: 'execute',
    status: 'in_progress',
    rawInput: { command: 'npm test -- reconnect', description: 'Run the reconnect tests' },
  });
  await step(8);
  update(testId, {
    status: 'completed',
    content: [{ type: 'content', content: { type: 'text', text: '```console\n' + TEST_OUTPUT + '\n```' } }],
  });
  plan(['completed', 'completed', 'completed', 'in_progress']);
  await step(2);

  if (!session.data.repo) {
    plan(['completed', 'completed', 'completed', 'completed']);
    await say("Done. The tests pass. This session has no repository, so there's nothing to push or propose.");
    return;
  }

  const pushId = randomUUID();
  tool({
    toolCallId: pushId,
    title: `git push origin ${session.data.branch}`,
    kind: 'execute',
    status: 'in_progress',
    content: [{ type: 'terminal', terminalId: pushId }],
    _meta: { terminal_info: { terminal_id: pushId } },
  });
  await step(3);
  update(pushId, {
    status: 'completed',
    _meta: {
      terminal_output: {
        terminal_id: pushId,
        data: `To https://github.com/${session.data.repo}.git\n * [new branch]      ${session.data.branch} -> ${session.data.branch}\n`,
      },
      terminal_exit: { terminal_id: pushId, exit_code: 0, signal: null },
    },
  });
  await step(2);

  const proposeId = randomUUID();
  tool({ toolCallId: proposeId, title: 'Propose pull request', kind: 'other', status: 'in_progress' });
  await step(2);
  const proposal = createProposal(session);
  update(proposeId, {
    status: 'completed',
    content: [{ type: 'content', content: { type: 'text', text: `Proposal ${proposal.id} is pending review.` } }],
  });
  plan(['completed', 'completed', 'completed', 'completed']);
  await step(2);
  await say(
    `Done. The tests pass and I've proposed a pull request, **${proposal.payload.title}**, from \`${session.data.branch}\`. It opens once you approve it.`,
  );
}

function createProposal(session) {
  const oldFile = `export class AcpConnection {\n  #timer: ReturnType<typeof setTimeout> | undefined;\n\n  connect() {\n    // …\n  }\n\n${OLD_SNIPPET}\n}\n`;
  const newFile = `const MAX_BACKOFF_MS = 10_000;\n\nexport class AcpConnection {\n  #timer: ReturnType<typeof setTimeout> | undefined;\n  #attempt = 0;\n\n  connect() {\n    // …\n  }\n\n${NEW_SNIPPET}\n}\n`;
  const testFile = `import { expect, test } from '@playwright/test';\n\ntest('reconnects after the socket drops', async ({ page }) => {\n  await page.goto('/');\n  await expect(page.getByTestId('reconnecting')).toBeHidden();\n});\n`;
  const diff = [
    formatPatch({ ...structuredPatch(`a/${FILE}`, `b/${FILE}`, oldFile, newFile, '', '', { context: 3 }), isGit: true }),
    formatPatch({
      ...structuredPatch('/dev/null', 'b/ui/tests/reconnect.spec.ts', '', testFile, '', '', { context: 3 }),
      isGit: true,
      isCreate: true,
    }),
  ].join('');

  const id = randomUUID();
  const proposal = {
    id,
    session_id: session.data.id,
    kind: 'github.pr.create',
    status: 'pending',
    payload: {
      repo: session.data.repo,
      base: 'main',
      head: session.data.branch,
      title: 'Reconnect the ACP socket with exponential backoff',
      body: 'Reconnects used a fixed one-second delay, which hammers a restarting runtime.\n\n## Changes\n\n- Back off exponentially from 500 ms, capped at 10 s\n- Reset the backoff after a successful `session/load`\n- Add an end-to-end test for reconnecting',
      draft: false,
    },
    created_at: now(),
    diff,
    commits: [
      { sha: sha(), message: 'Back off exponentially when reconnecting\n\nCapped at ten seconds.' },
      { sha: sha(), message: 'Test reconnecting after the socket drops' },
    ],
  };
  proposals.set(id, proposal);
  broadcastProposal(proposal);
  return proposal;
}

function createRepoGrant(session, repo, reason) {
  const proposal = {
    id: randomUUID(),
    session_id: session.data.id,
    kind: 'sloppod.repo.grant',
    status: 'pending',
    payload: { repo, reason, branch: sessionBranch(session) },
    created_at: now(),
  };
  proposals.set(proposal.id, proposal);
  broadcastProposal(proposal);
  return proposal;
}

function publicProposal(proposal) {
  const { diff: _diff, commits: _commits, ...rest } = proposal;
  return rest;
}

function broadcastProposal(proposal) {
  const session = sessions.get(proposal.session_id);
  for (const conn of session?.subscribers ?? []) conn.notify('_sloppod/proposal', { proposal: publicProposal(proposal) });
}

const availableRepos = () => (credentials.some((c) => c.kind === 'github-app') ? REPOS : []);

/** An `owner/name` the user can access, or a bare name that matches exactly one of them. */
function deriveRepo(prompt) {
  const repos = availableRepos();
  const full = repos.find((repo) => prompt.includes(repo));
  if (full) return full;
  const bare = repos.filter((repo) => new RegExp(`\\b${repo.split('/')[1]}\\b`, 'i').test(prompt));
  return bare.length === 1 ? bare[0] : null;
}

/** Creates a session from its first message, which is recorded right away and queued for the agent. */
function createSession({ prompt, repo, title }) {
  const id = randomUUID();
  const created = now();
  const sessionRepo = repo ?? deriveRepo(prompt);
  const branch = sessionRepo ? `sloppod/${id.slice(0, 8)}` : null;
  const session = new MockSession({
    id,
    title: title ?? '',
    repo: sessionRepo,
    branch,
    grants: [
      { capability: 'model', params: {} },
      { capability: 'github', params: {} },
      { capability: 'packages', params: {} },
      ...(sessionRepo ? [{ capability: 'repo', params: { repo: sessionRepo, access: 'rw', branch } }] : []),
      { capability: 'memory', params: { namespace: sessionRepo ?? 'bo0tzz', access: 'rw' } },
    ],
    phase: 'starting',
    created_at: created,
    updated_at: created,
  });
  sessions.set(id, session);
  queuePrompt(session, prompt);
  return session;
}

function queuePrompt(session, text, done = () => {}, fail = () => {}) {
  echoUser(session, text);
  session.queue.push({ text, done, fail });
}

/** Brings the agent up after `ms`, then runs everything queued so far as one turn. */
function startAgent(session, ms) {
  if (session.startTimer) return;
  session.startTimer = setTimeout(() => {
    session.startTimer = null;
    if (!sessions.has(session.data.id)) return;
    rememberModels();
    session.setPhase('idle');
    const items = session.queue.splice(0);
    if (items.length === 0) return;
    runTurn(
      session,
      items.map((item) => item.text).join('\n\n'),
      { echo: false },
    ).then(
      (stopReason) => items.forEach((item) => item.done(stopReason)),
      (error) => items.forEach((item) => item.fail(error)),
    );
  }, ms);
}

/** The runtime remembers the models the agent offered, for the settings page. */
const rememberModels = () => (settings.models = MODELS.map(({ value, name }) => ({ value, name })));

const startupDelay = (prompt) => (/slow start/i.test(prompt) ? 3000 : DELAY * 8);

function cancelTurn(session) {
  if (session.turn) session.turn.cancelled = true;
  for (const pending of [...session.pending.values()]) {
    resolvePermission(session, pending, { outcome: 'cancelled' }, null);
  }
}

// --- ACP over WebSocket ---------------------------------------------------------------------

function handleCall(conn, message) {
  const { id, method, params = {} } = message;
  const session = params.sessionId ? sessions.get(params.sessionId) : undefined;
  const needLoaded = () => {
    if (!session || !conn.loaded.has(session.data.id)) {
      if (id !== undefined) conn.error(id, -32602, 'Session not loaded');
      return false;
    }
    return true;
  };

  switch (method) {
    case 'initialize':
      conn.respond(id, {
        protocolVersion: 1,
        agentCapabilities: { loadSession: true, promptCapabilities: { image: false, audio: false, embeddedContext: false } },
        agentInfo: { name: 'sloppod-mock', version: '0.0.0' },
        authMethods: [],
        _meta: { steering: { supported: true } },
      });
      return;

    case 'session/load': {
      if (!session) {
        conn.error(id, -32602, `Unknown session ${params.sessionId}`);
        return;
      }
      for (const entry of session.log) conn.notify('session/update', entry);
      session.subscribers.add(conn);
      conn.loaded.add(session.data.id);
      conn.respond(id, { configOptions: session.configOptions() });
      for (const pending of session.pending.values()) sendPermission(session, pending, conn);
      return;
    }

    case 'session/prompt': {
      if (!needLoaded()) return;
      if (session.data.phase === 'starting' || session.data.phase === 'disconnected') {
        queuePrompt(
          session,
          promptText(params.prompt),
          (stopReason) => conn.respond(id, { stopReason }),
          (error) => conn.error(id, -32603, String(error)),
        );
        startAgent(session, DELAY * 8);
        return;
      }
      if (session.turn) {
        conn.error(id, -32000, 'A turn is already running; use _session/steering');
        return;
      }
      if (session.data.phase !== 'idle') {
        conn.error(id, -32000, `The session is ${session.data.phase}`);
        return;
      }
      runTurn(session, promptText(params.prompt)).then(
        (stopReason) => conn.respond(id, { stopReason }),
        (error) => conn.error(id, -32603, String(error)),
      );
      return;
    }

    case 'session/cancel':
      if (session) cancelTurn(session);
      return;

    case '_session/steering': {
      if (!needLoaded()) return;
      const text = promptText(params.prompt);
      if (session.data.phase === 'starting' || session.data.phase === 'disconnected') {
        queuePrompt(session, text);
        startAgent(session, DELAY * 8);
        conn.respond(id, { outcome: 'queued' });
        return;
      }
      if (!session.turn) {
        if (session.data.phase !== 'idle') {
          conn.error(id, -32000, `The session is ${session.data.phase}`);
          return;
        }
        void runTurn(session, text);
        conn.respond(id, { outcome: 'startedNewTurn' });
        return;
      }
      echoUser(session, text, { steering: true });
      session.turn.steers.push(text);
      conn.respond(id, { outcome: 'injected' });
      return;
    }

    case 'session/set_config_option': {
      if (!needLoaded()) return;
      if (params.configId !== 'model' || !MODELS.some((model) => model.value === params.value)) {
        conn.error(id, -32602, `Invalid value ${params.value} for ${params.configId}`);
        return;
      }
      session.model = params.value;
      const configOptions = session.configOptions();
      // Other viewers hear about it the way the adapter reports it; the load response carries the state.
      for (const other of session.subscribers) {
        if (other !== conn) {
          other.notify('session/update', {
            sessionId: session.data.id,
            update: { sessionUpdate: 'config_option_update', configOptions },
          });
        }
      }
      conn.respond(id, { configOptions });
      return;
    }

    default:
      if (id !== undefined) conn.error(id, -32601, `Method not found: ${method}`);
  }
}

// --- REST -----------------------------------------------------------------------------------

function json(res, status, body) {
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(body === undefined ? '' : JSON.stringify(body));
}

async function readBody(req) {
  let data = '';
  for await (const chunk of req) data += chunk;
  if (!data) return {};
  return JSON.parse(data);
}

const sortedSessions = () =>
  [...sessions.values()].map((s) => s.data).sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at));

async function handleApi(req, res, url) {
  const parts = url.pathname.replace(/^\/api\/v1\/?/, '').split('/').filter(Boolean);
  const method = req.method;

  if (parts[0] === 'repos' && parts.length === 1 && method === 'GET') return json(res, 200, availableRepos());
  if (parts[0] === 'me' && parts.length === 1 && method === 'GET') {
    return json(res, 200, { user: req.headers['x-forwarded-user'] ?? 'bo0tzz' });
  }

  if (parts[0] === 'credentials') return handleCredentials(req, res, parts);

  if (parts[0] === 'settings' && parts.length === 1) {
    if (method === 'GET') return json(res, 200, settings);
    if (method === 'PUT') {
      const body = await readBody(req);
      if ('model' in body) {
        if (body.model !== null && typeof body.model !== 'string') return json(res, 400, { error: 'model must be a string or null' });
        settings.model = body.model || null;
      }
      return json(res, 200, settings);
    }
  }

  if (parts[0] === 'sessions') {
    if (parts.length === 1 && method === 'GET') return json(res, 200, sortedSessions());
    if (parts.length === 1 && method === 'POST') {
      const body = await readBody(req);
      if (typeof body.prompt !== 'string' || !body.prompt.trim()) return json(res, 400, { error: 'prompt is required' });
      if (body.repo !== undefined && !availableRepos().includes(body.repo)) {
        return json(res, 400, { error: `unknown repository ${body.repo}` });
      }
      if (!credentials.some((c) => c.kind === 'claude')) {
        return json(res, 409, { error: 'no Claude account configured' });
      }
      const session = createSession(body);
      startAgent(session, startupDelay(body.prompt));
      return json(res, 201, session.data);
    }
    const session = sessions.get(parts[1]);
    if (!session) return json(res, 404, { error: 'session not found' });
    if (parts.length === 2 && method === 'GET') return json(res, 200, session.data);
    if (parts.length === 3 && parts[2] === 'repos' && method === 'POST') {
      const body = await readBody(req);
      if (!availableRepos().includes(body.repo)) return json(res, 400, { error: `unknown repository ${body.repo}` });
      const granted = hasRepoGrant(session, body.repo);
      grantRepo(session, body.repo);
      if (!granted) {
        void notifyAgent(
          session,
          `sloppod: the user gave this session write access to ${body.repo}.`,
          `Thanks, I can push to ${body.repo} now.`,
        );
      }
      return json(res, 200, session.data);
    }
    if (parts.length === 2 && method === 'DELETE') {
      cancelTurn(session);
      session.setPhase('stopped');
      sessions.delete(session.data.id);
      res.writeHead(204).end();
      return;
    }
  }

  if (parts[0] === 'proposals') {
    if (parts.length === 1 && method === 'GET') {
      const sessionId = url.searchParams.get('session');
      const status = url.searchParams.get('status');
      const list = [...proposals.values()]
        .filter((p) => (!sessionId || p.session_id === sessionId) && (!status || p.status === status))
        .map(publicProposal);
      return json(res, 200, list);
    }
    const proposal = proposals.get(parts[1]);
    if (!proposal) return json(res, 404, { error: 'proposal not found' });
    if (parts.length === 2 && method === 'GET') return json(res, 200, proposal);
    if (parts.length === 3 && method === 'POST' && (parts[2] === 'approve' || parts[2] === 'reject')) {
      if (proposal.status !== 'pending') return json(res, 409, { error: `proposal is already ${proposal.status}` });
      const body = await readBody(req);
      const session = sessions.get(proposal.session_id);
      proposal.decided_at = now();
      let notice;
      let ack;
      if (parts[2] === 'reject') {
        proposal.status = 'rejected';
        if (body.reason) proposal.reason = body.reason;
        notice = `Your proposal was rejected${body.reason ? `: ${body.reason}` : '.'}`;
        ack = "Understood, I'll leave that be.";
      } else if (proposal.kind === 'github.pr.create') {
        if (typeof body.title === 'string') proposal.payload.title = body.title;
        if (typeof body.body === 'string') proposal.payload.body = body.body;
        const number = 100 + proposals.size;
        proposal.status = 'executed';
        proposal.result = { url: `https://github.com/${proposal.payload.repo}/pull/${number}`, number };
        notice = `Your pull request was approved and opened: ${proposal.result.url}`;
        ack = 'Great, the pull request is open.';
      } else if (proposal.kind === 'sloppod.repo.grant') {
        proposal.status = 'executed';
        if (session) grantRepo(session, proposal.payload.repo);
        notice = `You now have write access to ${proposal.payload.repo} on ${proposal.payload.branch}.`;
        ack = `Thanks, I can push to ${proposal.payload.repo} now.`;
      } else {
        proposal.status = 'executed';
        notice = `Your ${proposal.kind} proposal was approved.`;
        ack = 'Thanks.';
      }
      broadcastProposal(proposal);
      if (session) void notifyAgent(session, notice, ack);
      return json(res, 200, publicProposal(proposal));
    }
  }

  return json(res, 404, { error: 'not found' });
}

const CREDENTIAL_CONFIG = { claude: [], 'github-app': ['app_id'], memini: ['url'] };

function addCredential({ kind, name, config }) {
  const credential = {
    id: randomUUID(),
    kind,
    name,
    config,
    default: !credentials.some((c) => c.kind === kind),
    created_at: now(),
  };
  credentials.push(credential);
  return credential;
}

async function handleCredentials(req, res, parts) {
  const method = req.method;
  if (parts.length === 1 && method === 'GET') return json(res, 200, credentials);
  if (parts.length === 1 && method === 'POST') {
    const body = await readBody(req);
    const keys = CREDENTIAL_CONFIG[body.kind];
    if (!keys) return json(res, 400, { error: `unknown credential kind ${body.kind}` });
    if (!body.name?.trim() || !body.secret?.trim()) return json(res, 400, { error: 'name and secret are required' });
    const missing = keys.filter((key) => !body.config?.[key]);
    if (missing.length) return json(res, 400, { error: `missing config: ${missing.join(', ')}` });
    const config = Object.fromEntries(keys.map((key) => [key, String(body.config[key])]));
    return json(res, 201, addCredential({ kind: body.kind, name: body.name.trim(), config }));
  }
  const credential = credentials.find((c) => c.id === parts[1]);
  if (!credential) return json(res, 404, { error: 'credential not found' });
  if (parts.length === 3 && parts[2] === 'default' && method === 'POST') {
    for (const c of credentials) if (c.kind === credential.kind) c.default = c === credential;
    return json(res, 200, credential);
  }
  if (parts.length === 2 && method === 'DELETE') {
    credentials.splice(credentials.indexOf(credential), 1);
    // Assumed: deleting the default promotes the oldest remaining credential of that kind.
    const next = credentials.find((c) => c.kind === credential.kind);
    if (credential.default && next) next.default = true;
    res.writeHead(204).end();
    return;
  }
  return json(res, 404, { error: 'not found' });
}

// --- static files ---------------------------------------------------------------------------

const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript',
  '.css': 'text/css',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.json': 'application/json',
  '.webmanifest': 'application/manifest+json',
};

function serveStatic(res, url) {
  const root = path.resolve(args.static);
  let file = path.join(root, decodeURIComponent(url.pathname));
  if (!file.startsWith(root) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
    file = path.join(root, 'index.html');
  }
  res.writeHead(200, { 'content-type': TYPES[path.extname(file)] ?? 'application/octet-stream' });
  fs.createReadStream(file).pipe(res);
}

// --- seed data ------------------------------------------------------------------------------

async function seed() {
  const ago = (minutes) => new Date(Date.now() - minutes * 60_000).toISOString();
  addCredential({ kind: 'claude', name: 'Personal', config: {} });
  addCredential({ kind: 'github-app', name: 'sloppod-dev', config: { app_id: '1234567' } });
  rememberModels();

  // Seeded sessions skip the startup queue: their history is written directly.
  const seeded = (fields) => {
    const session = createSession(fields);
    session.queue = [];
    return session;
  };
  const reply = (session, text) =>
    session.emit({ sessionUpdate: 'agent_message_chunk', messageId: randomUUID(), content: { type: 'text', text } });

  const prompt = 'Reconnects hammer the runtime when it restarts. Add backoff to the ACP socket in sloppod.';
  const first = createSession({ prompt });
  first.queue = [];
  first.fast = true;
  first.alwaysAllow = true;
  await runTurn(first, prompt, { echo: false });
  first.fast = false;
  first.alwaysAllow = false;
  first.data.created_at = ago(95);
  first.data.updated_at = ago(12);
  for (const proposal of proposals.values()) proposal.created_at = ago(13);

  const second = seeded({
    prompt: 'How does memini decide which namespace to read from?',
    repo: 'bo0tzz/memini',
    title: 'Summarise the namespace resolution rules',
  });
  second.emit({ sessionUpdate: 'compaction_update', compactionId: 'c1', status: 'in_progress' });
  second.emit({
    sessionUpdate: 'compaction_update',
    compactionId: 'c1',
    status: 'completed',
    summary: [
      {
        type: 'text',
        text: 'Explored `resolve.rs` and the namespace tests. The resolution order is: explicit pin, `MEMINI_NAMESPACE`, then the git remote.',
      },
    ],
  });
  reply(
    second,
    'It checks three things, in order:\n\n1. An explicit **pin** in the project config\n2. The `MEMINI_NAMESPACE` environment variable\n3. The `owner/repo` of the git remote\n\n| Source | Wins over |\n|---|---|\n| pin | everything |\n| env | git remote |',
  );
  second.data.phase = 'stopped';
  second.data.created_at = ago(60 * 26);
  second.data.updated_at = ago(60 * 24);

  const third = seeded({ prompt: 'Bump kopium to 0.24 and regenerate the CRDs.', repo: 'bo0tzz/sloppod', title: 'Bump kopium to 0.24' });
  reply(third, 'On it. Updating `mise.toml` first.');
  third.data.phase = 'disconnected';
  third.data.created_at = ago(40);
  third.data.updated_at = ago(3);

  const fourth = seeded({ prompt: 'What does park and wake mean for a session?', title: 'Park and wake' });
  reply(
    fourth,
    'A **parked** session has no pod: its state lives in the runtime until something wakes it, like a reply, a schedule or a webhook.',
  );
  fourth.data.phase = 'idle';
  const unknown = {
    id: randomUUID(),
    session_id: fourth.data.id,
    kind: 'kube.apply',
    status: 'pending',
    payload: { cluster: 'home', namespace: 'sloppod', manifests: ['deployment.yaml'] },
    created_at: ago(60 * 2),
  };
  proposals.set(unknown.id, unknown);
  fourth.data.created_at = ago(60 * 3);
  fourth.data.updated_at = ago(60 * 2);
}

// --- server ---------------------------------------------------------------------------------

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url ?? '/', 'http://localhost');
  try {
    if (url.pathname.startsWith('/api/')) return await handleApi(req, res, url);
    if (url.pathname === '/_mock/drop' && req.method === 'POST') {
      for (const conn of connections) conn.ws.terminate();
      return json(res, 200, { dropped: connections.size });
    }
    if (args.static) return serveStatic(res, url);
    json(res, 404, { error: 'not found' });
  } catch (error) {
    console.error(error);
    json(res, 500, { error: String(error) });
  }
});

const wss = new WebSocketServer({ noServer: true });
server.on('upgrade', (req, socket, head) => {
  if (new URL(req.url ?? '/', 'http://localhost').pathname !== '/acp') {
    socket.destroy();
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => {
    const conn = new Connection(ws);
    connections.add(conn);
    ws.on('message', (raw) => {
      let message;
      try {
        message = JSON.parse(String(raw));
      } catch {
        conn.error(null, -32700, 'Parse error');
        return;
      }
      if (message.method) {
        handleCall(conn, message);
      } else if (message.id !== undefined) {
        const callback = conn.awaiting.get(message.id);
        conn.awaiting.delete(message.id);
        callback?.(message);
      }
    });
    ws.on('close', () => {
      connections.delete(conn);
      for (const id of conn.loaded) {
        const session = sessions.get(id);
        if (!session) continue;
        session.subscribers.delete(conn);
        for (const pending of session.pending.values()) pending.requests.delete(conn);
      }
    });
  });
});

if (!args['no-seed']) await seed();
server.listen(Number(args.port), () => {
  console.log(`mock sloppod runtime on http://localhost:${args.port}${args.static ? ` serving ${args.static}` : ''}`);
});
