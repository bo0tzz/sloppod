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
/** How many updates a UI gets at once, as the runtime sends them. */
const HISTORY_PAGE = 2000;
const REPOS = ['bo0tzz/sloppod', 'bo0tzz/memini', 'immich-app/static-pages'];
// The adapter's model picker, as it appears in `configOptions`.
const MODELS = [
  { value: 'default', name: 'Default (recommended)', description: 'Sonnet' },
  { value: 'sonnet', name: 'Sonnet' },
  { value: 'opus', name: 'Opus' },
  { value: 'haiku', name: 'Haiku' },
];
/** The user's defaults for new sessions. `models` fills in once a session has offered them. */
const settings = { model: null, models: [], review: 'off', timezone: null };
const CANCELLED = Symbol('cancelled');

/** @type {Map<string, MockSession>} */
const sessions = new Map();
const proposals = new Map();
/** Review rounds by proposal id (ADR 24). */
const reviews = new Map();
const connections = new Set();
const credentials = [];
/** The user's own capabilities, `{id, name, description, mcp_url, credential_id?, created_at}`. */
const definitions = [];
/** Set by `/_mock/quotas`: what `GET /api/v1/quotas` answers. */
let quotas = [];

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
    this.data.active_at = now();
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

const QUESTIONS = [
  {
    question: 'Which backoff strategy should reconnects use?',
    header: 'Backoff',
    multiSelect: false,
    options: [
      {
        label: 'Exponential',
        description: 'Double the delay after each failure, capped at 10 s',
        preview: 'delay = Math.min(500 * 2 ** attempt, 10_000);',
      },
      { label: 'Fixed', description: 'Retry every second' },
      { label: 'Jittered' },
    ],
  },
  {
    question: 'Which tests should I add?',
    header: 'Tests',
    multiSelect: true,
    options: [{ label: 'Unit' }, { label: 'End-to-end', description: 'Playwright against the mock runtime' }],
  },
];

/** AskUserQuestion as an ACP form elicitation, built the way claude-agent-acp builds it. */
function questionsToElicitation(questions, sessionId, toolCallId) {
  const single = questions.length === 1;
  const properties = {};
  questions.forEach((question, index) => {
    const options = question.options.map((option) => ({
      const: option.label,
      title: option.label,
      ...(option.description ? { description: option.description } : {}),
      ...(option.preview ? { _meta: { '_claude/askUserQuestionOption': { preview: option.preview } } } : {}),
    }));
    const title = question.header || undefined;
    const description = single ? undefined : question.question;
    properties[`question_${index}`] = question.multiSelect
      ? { type: 'array', title, description, items: { anyOf: options } }
      : { type: 'string', title, description, oneOf: options };
    properties[`question_${index}_custom`] = {
      type: 'string',
      title: 'Other',
      description: question.multiSelect
        ? 'Type your own answer to add to your selection above (optional).'
        : 'Type your own answer, or add a note to the option you chose above (optional).',
    };
  });
  return {
    mode: 'form',
    sessionId,
    toolCallId,
    message: single ? questions[0].question : 'Please answer the following questions.',
    requestedSchema: { type: 'object', properties },
  };
}

function summariseAnswers(questions, content) {
  return questions.flatMap((question, index) => {
    const value = content[`question_${index}`];
    const picks = Array.isArray(value) ? value : value ? [value] : [];
    const custom = typeof content[`question_${index}_custom`] === 'string' ? content[`question_${index}_custom`].trim() : '';
    const answer = question.multiSelect || !picks.length ? [...picks, ...(custom ? [custom] : [])].join(', ') : picks[0];
    const note = !question.multiSelect && picks.length && custom ? ` (note: ${custom})` : '';
    return answer ? [`${question.header}: ${answer}${note}.`] : [];
  });
}

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

function echoUser(
  session,
  text,
  { steering = false, notice = false, summary = null, messageId = randomUUID(), images = [] } = {},
) {
  const sloppod = {
    ...(steering ? { steering: true } : {}),
    ...(notice ? { notice: true } : {}),
    ...(summary ? { summary } : {}),
  };
  session.emit({
    sessionUpdate: 'user_message_chunk',
    content: { type: 'text', text },
    messageId,
    ...(steering || notice ? { _meta: { sloppod } } : {}),
  });
  for (const image of images) session.emit({ sessionUpdate: 'user_message_chunk', content: image, messageId });
}

/**
 * Tells the agent about something sloppod did, like deciding a proposal: recorded as a notice, steered
 * into a running turn, or answered with a short turn of its own when the agent is idle.
 */
async function notifyAgent(session, text, ack, echo = true) {
  if (echo) echoUser(session, text, { notice: true });
  if (session.turn) {
    session.turn.steers.push({ text, ack });
    return;
  }
  // Like the runtime, which queues a notice until the agent can take it rather than dropping it.
  if (session.data.phase !== 'idle') {
    if (sessions.has(session.data.id) && !['stopped', 'parked'].includes(session.data.phase)) {
      setTimeout(() => void notifyAgent(session, text, ack, false), DELAY * 4);
    }
    return;
  }
  const turn = { cancelled: false, steers: [{ text, ack }] };
  session.turn = turn;
  session.setPhase('working');
  try {
    await sleep(DELAY * 2);
    // This one, and any that arrive while it's answered.
    while (turn.steers.length > 0) {
      const messageId = randomUUID();
      for (const piece of chunks(steerReply(turn.steers.shift()))) {
        session.emit({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: piece }, messageId });
        await sleep(DELAY);
      }
    }
  } finally {
    session.turn = null;
    if (sessions.has(session.data.id) && session.data.phase !== 'stopped') session.setPhase('idle');
  }
}

const sessionBranch = (session) => `sloppod/${session.data.id.slice(0, 8)}`;
const hasRepoGrant = (session, repo) =>
  session.data.grants.some((grant) => grant.capability === 'repo' && grant.params.repo === repo);

/** What the runtime says of each capability that can be granted while a session runs (`grants::describe`). */
const grantable = {
  model: "Claude's API, with the owner's Claude account.",
  github: "GitHub's API and git.",
  packages: 'Package registries and tool downloads.',
  docs: 'Library documentation, through Context7.',
  codex: "Codex's API on chatgpt.com, with the owner's ChatGPT login.",
};

const newGrant = (capability, params, description, expires_at) => ({
  id: randomUUID(),
  capability,
  params,
  description,
  created_at: now(),
  ...(expires_at ? { expires_at } : {}),
});

/** Has a session act on a repository with one of the user's GitHub tokens (ADR 29). */
function addRepoAs(res, session, body) {
  const token = credentials.find((c) => c.id === body.credential && c.kind === 'github-user');
  if (!token) return json(res, 400, { error: "that isn't one of your GitHub tokens" });
  const listed = token.config.repositories.find((r) => r.toLowerCase() === String(body.repo).toLowerCase());
  // A token that reaches more than it lists can take any well-formed name; GitHub decides when it's used.
  const wellFormed = /^[A-Za-z0-9-]{1,39}\/[A-Za-z0-9._-]{1,100}$/.test(String(body.repo)) && !['.', '..'].includes(String(body.repo).split('/')[1]);
  const repo = listed ?? (token.config.more_repositories && wellFormed ? body.repo : undefined);
  if (!repo) return json(res, 400, { error: `the token ${token.name} doesn't reach ${body.repo}` });
  grantRepo(session, repo);
  const grant = session.data.grants.find((g) => g.capability === 'repo' && g.params.repo === repo);
  const login = token.config.login;
  grant.params = { ...grant.params, credential: token.id, as: login };
  grant.description = `${repo}: read, and push the session branch \`${grant.params.branch}\` and branches named after it, as ${login} (the user's own GitHub token).`;
  session.broadcast();
  void notifyAgent(
    session,
    `sloppod: the user gave this session write access to ${repo}, as ${login}.`,
    `Thanks, on ${repo} I now act as ${login}.`,
  );
  return json(res, 200, session.data);
}

/** Gives the session write access to another repository; the first one also becomes its `repo`. */
function grantRepo(session, repo) {
  if (!hasRepoGrant(session, repo)) {
    session.data.grants.push(
      newGrant(
        'repo',
        { repo, access: 'rw', branch: sessionBranch(session) },
        `${repo}: read, and push the session branch \`${sessionBranch(session)}\` and branches named after it.`,
      ),
    );
  }
  if (!session.data.repo) {
    session.data.repo = repo;
    session.data.branch = sessionBranch(session);
  }
  session.broadcast();
}

/**
 * A request for the user: a permission request or a question (form elicitation). Sent to every viewer, re-sent to
 * UIs that load the session while it's open; first answer wins. A blocking one belongs to the turn: the session
 * waits on it and cancelling the turn cancels it. An `ask_user` question isn't blocking.
 */
function requestFromUser(session, { method, params, toolCallId, cancelled, blocking = true }) {
  return new Promise((resolve) => {
    const pending = { method, params, toolCallId, cancelled, blocking, resolve, requests: new Map() };
    session.pending.set(pending, pending);
    if (blocking) session.setPhase('waiting');
    for (const conn of session.subscribers) sendPending(session, pending, conn);
  });
}

const requestPermission = (session, toolCall, options) =>
  requestFromUser(session, {
    method: 'session/request_permission',
    params: { sessionId: session.data.id, toolCall, options },
    toolCallId: toolCall.toolCallId,
    cancelled: { outcome: { outcome: 'cancelled' } },
  }).then((result) => result.outcome);

function sendPending(session, pending, conn) {
  const rpcId = conn.request(pending.method, pending.params, (message) => {
    if (!session.pending.has(pending)) return;
    resolvePending(session, pending, message.result ?? pending.cancelled);
  });
  pending.requests.set(conn, rpcId);
}

function resolvePending(session, pending, result) {
  session.pending.delete(pending);
  for (const [conn, requestId] of pending.requests) {
    conn.notify('_sloppod/permission_resolved', {
      sessionId: session.data.id,
      toolCallId: pending.toolCallId ?? null,
      requestId,
    });
  }
  pending.requests.clear();
  const blocked = [...session.pending.values()].some((other) => other.blocking);
  if (session.data.phase === 'waiting' && !blocked) session.setPhase('working');
  pending.resolve(result);
}

async function runTurn(session, text, { echo = true, messageId, images = [] } = {}) {
  const turn = { cancelled: false, steers: [] };
  session.turn = turn;
  session.setPhase('working');
  if (echo) echoUser(session, text, { messageId, images });
  if (!session.data.title) {
    const title = text.split('\n')[0].slice(0, 60);
    session.emit({ sessionUpdate: 'session_info_update', title, updatedAt: now() });
    session.data.title = title;
    session.broadcast();
  }
  try {
    await agentScript(session, turn, text);
    // Like a real agent, pick up messages that arrived after the last step instead of dropping them.
    while (turn.steers.length > 0) {
      const messageId = randomUUID();
      for (const piece of chunks(steerReply(turn.steers.shift()))) {
        session.emit({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: piece }, messageId });
        await sleep(session.fast ? 0 : DELAY);
      }
    }
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
    if (sessions.has(session.data.id) && session.data.phase !== 'stopped') {
      session.setPhase('idle');
      startWaiting(session);
    }
  }
}

/** What the agent says to a message steered into its turn: a notice's own reply, or an acknowledgement. */
const steerReply = ({ text, ack }) =>
  ack ?? `Got your message mid-turn: “${text}”. I'll take that into account for the rest of this turn.`;

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
    while (turn.steers.length > 0) await say(steerReply(turn.steers.shift()));
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

  if (/ask_user/i.test(prompt)) {
    await step(1);
    // "twice" asks each question with a call of its own, so the second request waits behind the first.
    // "the same question twice" asks one question in two calls, as distinct questions that look alike.
    const asked = /same question twice/i.test(prompt) ? [QUESTIONS[0], QUESTIONS[0]] : /twice/i.test(prompt) ? QUESTIONS : [QUESTIONS[0]];
    for (const question of asked) {
      const askId = randomUUID();
      tool({ toolCallId: askId, title: 'mcp__sloppod__ask_user', kind: 'other', status: 'pending', rawInput: { questions: [question] } });
      void requestFromUser(session, {
        method: 'elicitation/create',
        // As the runtime sends an ask_user question: with its own id.
        params: { ...questionsToElicitation([question], session.data.id), _meta: { sloppod: { questionId: randomUUID() } } },
        cancelled: { action: 'cancel' },
        blocking: false,
      }).then((response) => {
        const answers = response.action === 'accept' ? summariseAnswers([question], response.content ?? {}) : [];
        const notice = answers.length
          ? `sloppod: the user answered your question:\n- ${question.question}: ${answers.join(' ')}`
          : `sloppod: the user dismissed your question without answering it:\n- ${question.question}`;
        void notifyAgent(session, notice, answers.length ? `Thanks, going with that. ${answers.join(' ')}` : "I'll use my own judgement then.");
      });
      update(askId, { status: 'completed' });
    }
    await say("I've asked you about the backoff; I'll carry on meanwhile.");
    await step(4);
    await say('Done with the parts that don\'t depend on your answer.');
    return;
  }

  if (/long answer/i.test(prompt)) {
    // Several screens of output in one go, as a long agent message arrives.
    const messageId = randomUUID();
    for (let i = 0; i < 2000; i++) {
      session.emit({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'word ' }, messageId });
    }
    await say('The end.');
    return;
  }

  if (/long history/i.test(prompt)) {
    // More updates than one page of history, so a reload starts partway through.
    const messageId = randomUUID();
    for (let i = 0; i < HISTORY_PAGE + 100; i++) {
      session.emit({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'word ' }, messageId });
    }
    await say('The end.');
    return;
  }

  if (/ask me/i.test(prompt)) {
    await step(2);
    await say('Before I start, a couple of questions.');
    const askId = randomUUID();
    tool({ toolCallId: askId, title: 'AskUserQuestion', kind: 'other', status: 'pending', rawInput: { questions: QUESTIONS } });
    const response = await requestFromUser(session, {
      method: 'elicitation/create',
      params: questionsToElicitation(QUESTIONS, session.data.id, askId),
      toolCallId: askId,
      cancelled: { action: 'cancel' },
    });
    if (response.action === 'cancel') {
      update(askId, { status: 'failed' });
      throw CANCELLED;
    }
    update(askId, { status: 'completed' });
    await step(1);
    if (response.action === 'decline') {
      await say("You skipped the questions, so I'll go with my own judgement.");
      return;
    }
    const answers = summariseAnswers(QUESTIONS, response.content ?? {});
    await say(answers.length ? `Thanks! ${answers.join(' ')}` : "Thanks, though you didn't pick anything.");
    return;
  }

  if (session.data.repo && /propose a stack/i.test(prompt)) {
    await step(2);
    const proposeId = randomUUID();
    tool({ toolCallId: proposeId, title: 'mcp__sloppod__propose_stack', kind: 'other', status: 'pending' });
    await step(2);
    const proposal = createStack(session);
    update(proposeId, {
      status: 'completed',
      content: [{ type: 'content', content: { type: 'text', text: `Proposal ${proposal.id} created and waiting for human review.` } }],
    });
    await say("I've proposed the change as a stack of two pull requests.");
    return;
  }

  if (session.data.repo && /tidy the issues/i.test(prompt)) {
    await step(2);
    const proposeId = randomUUID();
    tool({ toolCallId: proposeId, title: 'mcp__sloppod__propose_issue_updates', kind: 'other', status: 'pending' });
    await step(2);
    const proposal = createIssueUpdates(session);
    update(proposeId, {
      status: 'completed',
      content: [{ type: 'content', content: { type: 'text', text: `Proposal ${proposal.id} created and waiting for human review.` } }],
    });
    await say("I've proposed closing two finished issues and relabelling another.");
    return;
  }

  if (session.data.repo && /file issues/i.test(prompt)) {
    await step(2);
    const proposeId = randomUUID();
    tool({ toolCallId: proposeId, title: 'mcp__sloppod__propose_issues', kind: 'other', status: 'pending' });
    await step(2);
    const proposal = createIssues(session);
    update(proposeId, {
      status: 'completed',
      content: [{ type: 'content', content: { type: 'text', text: `Proposal ${proposal.id} created and waiting for human review.` } }],
    });
    await say("I've proposed three issues for the backlog. They're filed once you approve them.");
    return;
  }

  if (/act as me on (\S+)/i.test(prompt)) {
    const repo = prompt.match(/act as me on (\S+)/i)[1];
    const reason = "sloppod's GitHub App isn't installed there.";
    await step(2);
    const token = credentials.find((c) => c.kind === 'github-user' && c.config.repositories.includes(repo));
    if (!token) {
      await say(`None of your GitHub tokens reach **${repo}**, so I can't ask to act as you there.`);
      return;
    }
    const proposal = {
      id: randomUUID(),
      session_id: session.data.id,
      kind: 'sloppod.repo.act_as_owner',
      status: 'pending',
      payload: { repo, reason, branch: sessionBranch(session), credential: token.id, login: token.config.login },
      created_at: now(),
    };
    proposals.set(proposal.id, proposal);
    broadcastProposal(proposal);
    await say(`I've asked to act as you on **${repo}**. I'll carry on once you approve it.`);
    return;
  }

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

/** A review that asked for changes once, then passed: what a proposal looks like when it reaches the user. */
function addReview(proposal) {
  const created = now();
  const reviewer = new MockSession({
    id: randomUUID(),
    title: `Review: ${proposal.payload.title}`,
    repo: proposal.payload.repo,
    branch: null,
    grants: [],
    phase: 'parked',
    agent: 'codex',
    kind: 'review',
    review: 'off',
    created_at: created,
    updated_at: created,
    active_at: created,
  });
  sessions.set(reviewer.data.id, reviewer);
  proposal.review_session_id = reviewer.data.id;
  reviews.set(proposal.id, [
    {
      round: 1,
      head_sha: sha(),
      verdict: 'changes',
      summary: 'The backoff works, but it never resets.',
      findings: [
        {
          id: '1.1',
          severity: 'p1',
          category: 'reliability',
          title: 'The backoff never resets',
          path: FILE,
          line: 12,
          explanation: 'After any run of failed reconnects, every later reconnect waits the full 10 s.',
          suggested_fix: 'Reset the attempt counter after a successful session/load.',
        },
        { id: '1.2', severity: 'p2', category: 'tests', title: 'No test covers the cap', explanation: 'Nothing checks the 10 s cap.' },
      ],
      resolved: [],
      created_at: now(),
      finished_at: now(),
    },
    {
      round: 2,
      head_sha: sha(),
      replies: 'Reset it after a successful session/load. The cap is a constant; a test would only restate it.',
      verdict: 'pass',
      summary: 'The reset is in place, and the reply about the cap is fair.',
      findings: [],
      resolved: ['1.1', '1.2'],
      created_at: now(),
      finished_at: now(),
    },
  ]);
}

/** `{as}` when the session acts on `repo` with one of the user's GitHub tokens (ADR 29), as the runtime adds it. */
function actingAs(session, repo) {
  const grant = session.data.grants.find((g) => g.capability === 'repo' && g.params.repo === repo && g.params.as);
  return grant ? { as: grant.params.as } : {};
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
      auto_merge: true,
      ...actingAs(session, session.data.repo),
    },
    created_at: now(),
    diff,
    commits: [
      { sha: sha(), message: 'Back off exponentially when reconnecting\n\nCapped at ten seconds.' },
      { sha: sha(), message: 'Test reconnecting after the socket drops' },
    ],
  };
  if (session.data.review === 'required') addReview(proposal);
  proposals.set(id, proposal);
  broadcastProposal(proposal);
  applyStandingApproval(proposal);
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
  applyStandingApproval(proposal);
  return proposal;
}

function createIssues(session) {
  const proposal = {
    id: randomUUID(),
    session_id: session.data.id,
    kind: 'github.issue.create',
    status: 'pending',
    payload: {
      repo: session.data.repo,
      issues: [
        { title: 'Steers should land at the next tool boundary', body: 'Steering interrupts the running generation.', labels: ['dogfood'] },
        { title: 'Fail fast on blocked egress', body: 'Blocked connections hang until the client times out.', labels: ['dogfood', 'enforcement'] },
        { title: 'Show the version in the UI', body: '', labels: [] },
      ],
    },
    created_at: now(),
  };
  proposals.set(proposal.id, proposal);
  broadcastProposal(proposal);
  applyStandingApproval(proposal);
  return proposal;
}

function createIssueUpdates(session) {
  const proposal = {
    id: randomUUID(),
    session_id: session.data.id,
    kind: 'github.issue.update',
    status: 'pending',
    payload: {
      repo: session.data.repo,
      updates: [
        { number: 100, comment: 'Fixed by #148.', close: 'completed', remove_labels: ['priority'] },
        { number: 65, comment: 'Superseded by #196.', close: 'duplicate' },
        {
          number: 59,
          add_labels: ['priority'],
          title: 'Startup failures show in the UI',
          body: 'A pod that fails to start says why in the UI.\n\nToday it only shows in the logs.',
        },
      ],
    },
    // Like the runtime's detail: what the rewritten issue says now.
    current: [
      {
        number: 59,
        title: 'Startup failures are only in the logs',
        body: 'A pod that fails to start says nothing.\n\nToday it only shows in the logs.',
      },
    ],
    created_at: now(),
  };
  proposals.set(proposal.id, proposal);
  broadcastProposal(proposal);
  applyStandingApproval(proposal);
  return proposal;
}

function createStack(session) {
  const branch = session.data.branch;
  const layer = (file, before, after, message) => ({
    diff: formatPatch({ ...structuredPatch(`a/${file}`, `b/${file}`, before, after, '', '', { context: 3 }), isGit: true }),
    commits: [{ sha: sha(), message }],
  });
  const proposal = {
    id: randomUUID(),
    session_id: session.data.id,
    kind: 'github.stack.create',
    status: 'pending',
    payload: {
      repo: session.data.repo,
      base: 'main',
      pull_requests: [
        { head: `${branch}-backoff`, base: 'main', head_sha: sha(), title: 'Back off when reconnecting', body: 'The backoff itself.', draft: false },
        { head: `${branch}-ui`, base: `${branch}-backoff`, head_sha: sha(), title: 'Show the reconnect countdown', body: 'Uses the backoff.', draft: false },
      ],
    },
    created_at: now(),
    layers: [
      layer(FILE, 'const DELAY = 1000;\n', 'const MAX_BACKOFF_MS = 10_000;\n', 'Back off exponentially'),
      layer('ui/src/lib/components/Reconnecting.svelte', '<p>Reconnecting…</p>\n', '<p>Reconnecting in {seconds} s…</p>\n', 'Show the countdown'),
    ],
  };
  proposals.set(proposal.id, proposal);
  broadcastProposal(proposal);
  applyStandingApproval(proposal);
  return proposal;
}

function publicProposal(proposal) {
  const { diff: _diff, commits: _commits, layers: _layers, current: _current, ...rest } = proposal;
  return rest;
}

function broadcastProposal(proposal) {
  const session = sessions.get(proposal.session_id);
  for (const conn of session?.subscribers ?? []) conn.notify('_sloppod/proposal', { proposal: publicProposal(proposal) });
}

const availableRepos = () => (credentials.some((c) => c.kind === 'github-app') ? REPOS : []);

/** An `owner/name` the user can access, or a bare name that matches exactly one of them. */
/** What a new session can start with besides model access, memory and the repository: the runtime's `starting::defaults`. */
// Model access and docs come with every session, so they aren't chosen.
const startingCapabilities = () => ['github', 'packages', ...(credentials.some((c) => c.kind === 'chatgpt') ? ['codex'] : [])];

/** How long a suggestion takes, so a test can see the pending hint and a later suggestion overtaking an edit. */
const suggestDelay = 60;

/** The message's first line, shortened, as the runtime's `default_title`. */
const defaultTitle = (prompt) => prompt.trim().split('\n')[0].slice(0, 60).trim();

function deriveRepo(prompt) {
  const repos = availableRepos();
  const full = repos.find((repo) => prompt.includes(repo));
  if (full) return full;
  const bare = repos.filter((repo) => new RegExp(`\\b${repo.split('/')[1]}\\b`, 'i').test(prompt));
  return bare.length === 1 ? bare[0] : null;
}

/** Creates a session from its first message, which is recorded right away and queued for the agent; `parent` makes it a
 * sub-session. */
function createSession({ prompt, repo, title, capabilities, parent }) {
  const id = randomUUID();
  const created = now();
  // Absent, the repository is derived from the prompt; null means none.
  const sessionRepo = repo === undefined ? deriveRepo(prompt) : repo;
  const branch = sessionRepo ? `sloppod/${id.slice(0, 8)}` : null;
  // Like the runtime: the chosen capabilities, and GitHub too when there's a repository, which needs it.
  const chosen = (capability) => (capabilities ?? ['github', 'packages']).includes(capability) || (capability === 'github' && sessionRepo);
  const session = new MockSession({
    id,
    title: title ?? '',
    repo: sessionRepo,
    branch,
    grants: [
      newGrant('model', {}, grantable.model),
      newGrant('docs', {}, grantable.docs),
      ...['github', 'packages', 'codex'].filter(chosen).map((capability) => newGrant(capability, {}, grantable[capability])),
      ...(sessionRepo
        ? [
            newGrant(
              'repo',
              { repo: sessionRepo, access: 'rw', branch },
              `${sessionRepo}: read, and push the session branch \`${branch}\` and branches named after it.`,
            ),
          ]
        : []),
      newGrant(
        'memory',
        { namespace: sessionRepo ?? 'bo0tzz', access: 'rw' },
        `Long-term memory (memini), in the namespace ${sessionRepo ?? 'bo0tzz'}.`,
      ),
    ],
    phase: 'starting',
    agent: 'claude-code',
    kind: 'work',
    review: settings.review,
    created_at: created,
    updated_at: created,
    active_at: created,
    ...(parent ? { parent_id: parent } : {}),
  });
  sessions.set(id, session);
  queuePrompt(session, prompt);
  return session;
}

/** Messages queued behind the running turn, `{ id, text, resolve, reject }`; UIs show them and can steer them in. */
const waitingList = (session) => (session.waiting ??= []);

/** What a waiting message holds: its text, unless a test queued more. */
const waitingPrompt = ({ text, prompt }) => prompt ?? [{ type: 'text', text }];

function broadcastQueue(session, conns = session.subscribers) {
  const messages = waitingList(session).map((message) => ({ id: message.id, prompt: waitingPrompt(message) }));
  for (const conn of conns ?? []) conn.notify('_sloppod/queue', { sessionId: session.data.id, messages });
}

/** Like the runtime, runs the next queued message once a turn ends. */
function startWaiting(session) {
  const next = waitingList(session).shift();
  if (!next) return;
  broadcastQueue(session);
  runTurn(session, next.text, { messageId: next.id }).then(next.resolve, next.reject);
}

function queuePrompt(session, text, done = () => {}, fail = () => {}, messageId = undefined) {
  echoUser(session, text, { messageId });
  session.queue.push({ text, done, fail });
}

/** Brings the agent up after `ms`, then runs everything queued so far as one turn. */
function startAgent(session, ms) {
  if (session.startTimer) return;
  session.startTimer = setTimeout(() => {
    session.startTimer = null;
    if (!sessions.has(session.data.id)) return;
    rememberModels();
    delete session.data.agent_status;
    delete session.data.agent_error;
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

/** Like a pod whose first start fails: a status, then the error, then a start that works. */
function failStart(session) {
  session.data.agent_status = 'cloning https://github.com/bo0tzz/sloppod.git';
  setTimeout(() => {
    const error = 'preparing the pod: git clone exited with status 128';
    delete session.data.agent_status;
    session.data.agent_error = error;
    session.emit({ sessionUpdate: 'notice', severity: 'error', title: "The session's pod couldn't start the agent", description: error });
    session.broadcast();
  }, 1000);
}

const startupDelay = (prompt) => (/slow start|failing start/i.test(prompt) ? 3000 : DELAY * 8);

function cancelTurn(session) {
  if (session.turn) session.turn.cancelled = true;
  for (const pending of [...session.pending.values()]) {
    if (pending.blocking) resolvePending(session, pending, pending.cancelled);
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

  /** Takes a message out of the queue behind the running turn, answering with an error if it isn't there. */
  const takeWaiting = () => {
    const waiting = waitingList(session);
    const index = waiting.findIndex((message) => message.id === params.id);
    if (index < 0) {
      conn.error(id, -32002, "that message isn't queued any more");
      return undefined;
    }
    const [message] = waiting.splice(index, 1);
    broadcastQueue(session);
    return message;
  };

  switch (method) {
    case 'initialize':
      conn.respond(id, {
        protocolVersion: 1,
        agentCapabilities: { loadSession: true, promptCapabilities: { image: true, audio: false, embeddedContext: false } },
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
      // Like the runtime: only the latest of the log, with where to ask for older pages. An update's seq is its
      // position in the log, from 1.
      const start = Math.max(0, session.log.length - HISTORY_PAGE);
      for (const entry of session.log.slice(start)) conn.notify('session/update', entry);
      session.subscribers.add(conn);
      conn.loaded.add(session.data.id);
      // Like the runtime: the queue before the response.
      broadcastQueue(session, [conn]);
      conn.respond(id, {
        configOptions: session.configOptions(),
        _meta: { sloppod: { historyBefore: start > 0 ? start + 1 : null } },
      });
      for (const pending of session.pending.values()) sendPending(session, pending, conn);
      return;
    }

    case '_sloppod/history': {
      if (!needLoaded()) return;
      const end = Math.max(0, Math.min(session.log.length, Number(params.before) - 1));
      const start = Math.max(0, end - HISTORY_PAGE);
      conn.respond(id, {
        updates: session.log.slice(start, end).map((entry) => entry.update),
        before: start > 0 ? start + 1 : null,
      });
      return;
    }

    case 'session/prompt': {
      if (!needLoaded()) return;
      const messageId = params._meta?.sloppod?.messageId ?? randomUUID();
      // Like the runtime: a message taken back out of the queue doesn't run when it's sent again.
      if (session.removed?.has(messageId)) {
        conn.respond(id, { stopReason: 'cancelled' });
        return;
      }
      // Like the runtime: a message sent again isn't run twice.
      const received = session.log.some(
        (entry) => entry.update.sessionUpdate === 'user_message_chunk' && entry.update.messageId === messageId,
      );
      if (received || waitingList(session).some((waiting) => waiting.id === messageId)) {
        conn.respond(id, { stopReason: 'end_turn', _meta: { sloppod: { alreadyReceived: true } } });
        return;
      }
      // Tests drop the connection as a prompt arrives, before or after the runtime takes it.
      const drop = dropNextPrompt;
      dropNextPrompt = null;
      if (drop === 'before') {
        conn.ws.terminate();
        return;
      }
      // Dropped as soon as the prompt is taken, so this connection never hears about it, e.g. its queue entry.
      if (drop === 'after') conn.ws.terminate();
      if (failNextPrompt && drop === null) {
        failNextPrompt = false;
        conn.error(id, -32000, 'the runtime refused it');
        return;
      }
      if (session.data.phase === 'parked') session.setPhase('starting');
      if (session.data.phase === 'starting' || session.data.phase === 'disconnected') {
        queuePrompt(
          session,
          promptText(params.prompt),
          (stopReason) => conn.respond(id, { stopReason }),
          (error) => conn.error(id, -32603, String(error)),
          messageId,
        );
        startAgent(session, DELAY * 8);
        return;
      }
      if (session.turn) {
        waitingList(session).push({
          id: messageId,
          text: promptText(params.prompt),
          resolve: (stopReason) => conn.respond(id, { stopReason }),
          reject: (error) => conn.error(id, -32603, String(error)),
        });
        broadcastQueue(session);
        return;
      }
      if (session.data.phase !== 'idle') {
        conn.error(id, -32000, `The session is ${session.data.phase}`);
        return;
      }
      const images = (params.prompt ?? []).filter((block) => block?.type === 'image');
      runTurn(session, promptText(params.prompt), { messageId, images }).then(
        (stopReason) => conn.respond(id, { stopReason }),
        (error) => conn.error(id, -32603, String(error)),
      );
      return;
    }

    case 'session/cancel':
      if (session) cancelTurn(session);
      return;

    case '_sloppod/remove_queued': {
      if (!needLoaded()) return;
      const message = takeWaiting();
      if (!message) return;
      (session.removed ??= new Set()).add(message.id);
      // Tests drop the connection before either answer reaches the UI.
      if (dropNextRemove) {
        dropNextRemove = false;
        conn.ws.terminate();
      }
      conn.respond(id, { prompt: waitingPrompt(message) });
      message.resolve('cancelled');
      return;
    }

    case '_sloppod/steer_queued': {
      if (!needLoaded()) return;
      const message = takeWaiting();
      if (!message) return;
      if (session.turn) {
        echoUser(session, message.text, { steering: true });
        session.turn.steers.push({ text: message.text });
        conn.respond(id, { outcome: 'injected' });
        message.resolve('end_turn');
      } else {
        runTurn(session, message.text).then(message.resolve, message.reject);
        conn.respond(id, { outcome: 'startedNewTurn' });
      }
      return;
    }

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
      session.turn.steers.push({ text });
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
  [...sessions.values()]
    .map((s) => s.data)
    .filter((d) => d.kind === 'work' && !d.deleted_at)
    .sort((a, b) => Date.parse(b.active_at) - Date.parse(a.active_at));

async function handleApi(req, res, url) {
  const parts = url.pathname.replace(/^\/api\/v1\/?/, '').split('/').filter(Boolean);
  const method = req.method;

  if (parts[0] === 'repos' && parts.length === 1 && method === 'GET') return json(res, 200, availableRepos());
  if (parts[0] === 'me' && parts.length === 1 && method === 'GET') {
    return json(res, 200, { user: req.headers['x-forwarded-user'] ?? 'bo0tzz', ...runtimeBuild });
  }

  if (parts[0] === 'credentials') return handleCredentials(req, res, parts);
  if (parts[0] === 'capabilities') return handleCapabilities(req, res, parts);
  if (parts[0] === 'quotas' && parts.length === 1 && method === 'GET') return json(res, 200, quotas);
  if (parts[0] === 'standing-approvals') return handleStandingApprovals(req, res, parts);

  if (parts[0] === 'settings' && parts.length === 1) {
    if (method === 'GET') return json(res, 200, settings);
    if (method === 'PUT') {
      const body = await readBody(req);
      if ('model' in body) {
        if (body.model !== null && typeof body.model !== 'string') return json(res, 400, { error: 'model must be a string or null' });
        settings.model = body.model || null;
      }
      if ('review' in body) {
        if (!['off', 'required'].includes(body.review)) return json(res, 400, { error: 'review must be one of off, required' });
        settings.review = body.review;
      }
      if ('timezone' in body) {
        if (body.timezone !== 'UTC' && !Intl.supportedValuesOf('timeZone').includes(body.timezone)) {
          return json(res, 400, { error: `${body.timezone} isn't a timezone, e.g. Europe/Amsterdam` });
        }
        settings.timezone = body.timezone;
      }
      return json(res, 200, settings);
    }
  }

  if (parts[0] === 'sessions') {
    if (parts.length === 1 && method === 'GET') return json(res, 200, sortedSessions());
    if (parts.length === 2 && parts[1] === 'suggest' && method === 'POST') {
      const body = await readBody(req);
      const prompt = typeof body.prompt === 'string' ? body.prompt : '';
      await sleep(suggestDelay);
      // A prompt with "question:" gets what a judgement would give for one: no repository, and packages alone.
      if (/question:/i.test(prompt)) {
        return json(res, 200, { title: defaultTitle(prompt), repo: null, capabilities: ['packages'], source: 'judgement' });
      }
      return json(res, 200, { title: defaultTitle(prompt), repo: deriveRepo(prompt), capabilities: startingCapabilities(), source: 'match' });
    }
    if (parts.length === 1 && method === 'POST') {
      const body = await readBody(req);
      if (typeof body.prompt !== 'string' || !body.prompt.trim()) return json(res, 400, { error: 'prompt is required' });
      if (body.repo !== undefined && body.repo !== null && !availableRepos().includes(body.repo)) {
        return json(res, 400, { error: `unknown repository ${body.repo}` });
      }
      if (!credentials.some((c) => c.kind === 'claude')) {
        return json(res, 409, { error: 'no Claude account configured' });
      }
      const unknown = (body.capabilities ?? []).find((c) => !startingCapabilities().includes(c));
      if (unknown) return json(res, 400, { error: `a new session can start with ${startingCapabilities().join(', ')}, not ${unknown}` });
      const session = createSession(body);
      if (typeof body.model === 'string' && body.model) session.data.model = body.model;
      startAgent(session, startupDelay(body.prompt));
      if (/failing start/i.test(body.prompt)) failStart(session);
      return json(res, 201, session.data);
    }
    if (parts.length === 2 && parts[1] === 'deleted' && method === 'GET') {
      return json(res, 200, [...sessions.values()].map((s) => s.data).filter((d) => d.deleted_at));
    }
    const session = sessions.get(parts[1]);
    if (!session) return json(res, 404, { error: 'session not found' });
    if (parts.length === 2 && method === 'GET') {
      if (Date.now() < slowSessionsUntil) await sleep(slowSessionsUntil - Date.now());
      return json(res, 200, session.data);
    }
    if (parts.length === 3 && parts[2] === 'review' && method === 'PUT') {
      const body = await readBody(req);
      if (!['off', 'required'].includes(body.review)) return json(res, 400, { error: 'review must be one of off, required' });
      session.data.review = body.review;
      session.broadcast();
      return json(res, 200, session.data);
    }
    if (parts.length === 3 && parts[2] === 'park' && method === 'POST') {
      if (session.data.phase === 'working' || session.data.phase === 'waiting') {
        return json(res, 409, { error: 'the session is in a turn; stop it or let it finish before parking' });
      }
      session.setPhase('parked');
      return json(res, 200, session.data);
    }
    if (parts.length === 3 && parts[2] === 'wake' && method === 'POST' && session.data.deleted_at) {
      return json(res, 409, { error: 'the session is deleted; restore it first' });
    }
    if (parts.length === 3 && parts[2] === 'wake' && method === 'POST') {
      if (session.data.phase === 'parked') {
        session.setPhase('starting');
        startAgent(session, DELAY * 8);
      }
      return json(res, 200, session.data);
    }
    if (parts.length === 3 && parts[2] === 'repos' && method === 'POST') {
      const body = await readBody(req);
      if (body.credential) return addRepoAs(res, session, body);
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
    if (parts.length === 3 && parts[2] === 'grants' && method === 'POST') {
      const body = await readBody(req);
      // A built-in, or one of the user's own by its name.
      const own = definitions.find((d) => d.name === body.capability);
      if (!grantable[body.capability] && !own) {
        return json(res, 400, { error: `${body.capability} can't be granted here; repositories are added with POST /sessions/{id}/repos` });
      }
      if (session.data.grants.some((grant) => grant.capability === body.capability)) {
        return json(res, 409, { error: `the session already has ${body.capability}` });
      }
      if (body.expires_at && Date.parse(body.expires_at) <= Date.now()) return json(res, 400, { error: 'expires_at is in the past' });
      session.data.grants.push(
        own
          ? newGrant(body.capability, { definition: own.id, description: own.description }, own.description, body.expires_at)
          : newGrant(body.capability, {}, grantable[body.capability], body.expires_at),
      );
      session.broadcast();
      void notifyAgent(session, `sloppod: the user granted this session ${body.capability}.`, `Thanks, I have ${body.capability} now.`);
      return json(res, 201, session.data);
    }
    if (parts.length === 4 && parts[2] === 'grants' && (method === 'PATCH' || method === 'DELETE')) {
      const index = session.data.grants.findIndex((grant) => grant.id === parts[3]);
      if (index < 0) return json(res, 404, { error: 'not found' });
      const grant = session.data.grants[index];
      if (method === 'DELETE') {
        session.data.grants.splice(index, 1);
        session.broadcast();
        void notifyAgent(session, `sloppod: the user revoked this session's grant ${grant.capability}.`, 'Understood.');
        return json(res, 200, session.data);
      }
      const body = await readBody(req);
      if (!body.narrow && body.expires_at === undefined) return json(res, 400, { error: 'give expires_at or narrow' });
      if (body.expires_at && Date.parse(body.expires_at) <= Date.now()) {
        return json(res, 400, { error: 'expires_at is in the past; revoke the grant instead' });
      }
      if (body.narrow && !(grant.capability === 'repo' && grant.params.access === 'rw')) {
        return json(res, 400, { error: "this grant can't be narrowed" });
      }
      if (body.narrow) {
        grant.params = { repo: grant.params.repo, access: 'ro' };
        grant.description = `${grant.params.repo}: read only.`;
      }
      if (body.expires_at === null) delete grant.expires_at;
      else if (body.expires_at) grant.expires_at = body.expires_at;
      session.broadcast();
      void notifyAgent(session, `sloppod: the user changed this session's grant ${grant.capability}.`, 'Noted.');
      return json(res, 200, session.data);
    }
    if (parts.length === 2 && method === 'DELETE') {
      // Its sub-sessions go with it, at any depth, also below ones already deleted.
      const tree = [session];
      for (let i = 0; i < tree.length; i++) {
        for (const other of sessions.values()) {
          if (other.data.parent_id === tree[i].data.id) tree.push(other);
        }
      }
      for (const doomed of tree.filter((s) => s === session || !s.data.deleted_at)) {
        cancelTurn(doomed);
        doomed.data.deleted_at = now();
        doomed.setPhase('parked');
      }
      res.writeHead(204).end();
      return;
    }
    if (parts.length === 3 && parts[2] === 'restore' && method === 'POST') {
      if (!session.data.deleted_at) return json(res, 404, { error: 'not found' });
      delete session.data.deleted_at;
      session.broadcast();
      return json(res, 200, session.data);
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
    if (parts.length === 2 && method === 'GET') {
      if (proposal.kind !== 'sloppod.repo.act_as_owner') return json(res, 200, proposal);
      // As the runtime's detail: the tokens the user can pick from.
      const tokens = credentials
        .filter((c) => c.kind === 'github-user')
        .map((c) => ({ id: c.id, name: c.name, login: c.config.login }));
      return json(res, 200, { ...proposal, tokens });
    }
    if (parts.length === 3 && parts[2] === 'reviews' && method === 'GET') return json(res, 200, reviews.get(proposal.id) ?? []);
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
      } else {
        return json(res, 200, approve(proposal, body, 'dev'));
      }
      broadcastProposal(proposal);
      if (session) void notifyAgent(session, notice, ack);
      return json(res, 200, publicProposal(proposal));
    }
  }

  return json(res, 404, { error: 'not found' });
}

/** Approves a proposal; like the runtime, it answers right away and the outcome follows as a proposal update. */
function approve(proposal, body, decidedBy) {
  const session = sessions.get(proposal.session_id);
  proposal.decided_at = now();
  proposal.decided_by = decidedBy;
  let notice;
  let ack;
  if (proposal.kind === 'github.pr.create') {
    if (typeof body.title === 'string') proposal.payload.title = body.title;
    if (typeof body.body === 'string') proposal.payload.body = body.body;
    if (typeof body.auto_merge === 'boolean') proposal.payload.auto_merge = body.auto_merge;
    const number = 100 + proposals.size;
    proposal.status = 'executed';
    proposal.result = { url: `https://github.com/${proposal.payload.repo}/pull/${number}`, number };
    notice = `Your pull request was approved and opened: ${proposal.result.url}`;
    ack = 'Great, the pull request is open.';
  } else if (proposal.kind === 'github.stack.create') {
    if (typeof body.auto_merge === 'boolean') proposal.payload.auto_merge = body.auto_merge;
    const opened = proposal.payload.pull_requests.map((_, index) => {
      const number = 300 + proposals.size * 10 + index;
      return { url: `https://github.com/${proposal.payload.repo}/pull/${number}`, number };
    });
    proposal.status = 'executed';
    const merging = proposal.payload.auto_merge ? ' The stack merges once its required checks pass.' : '';
    proposal.result = { pull_requests: opened, stack: 7, auto_merge: Boolean(proposal.payload.auto_merge), message: `Opened ${opened.map((p) => p.url).join(', ')} as stack #7.${merging}` };
    notice = `Your stack was approved. ${proposal.result.message}`;
    ack = 'Great, the stack is open.';
  } else if (proposal.kind === 'github.issue.update') {
    if (Array.isArray(body.updates)) proposal.payload.updates = body.updates;
    const updated = proposal.payload.updates.map((u) => ({ number: u.number }));
    proposal.status = 'executed';
    proposal.result = { updated, failed: [], message: `Updated ${updated.map((u) => `#${u.number}`).join(', ')}.` };
    notice = `Your issue updates were approved and made: ${proposal.result.message}`;
    ack = 'Thanks, the tracker is tidy.';
  } else if (proposal.kind === 'github.issue.create') {
    if (Array.isArray(body.issues)) proposal.payload.issues = body.issues;
    const filed = proposal.payload.issues.map((issue, index) => {
      const number = 200 + proposals.size * 10 + index;
      return { title: issue.title, url: `https://github.com/${proposal.payload.repo}/issues/${number}`, number };
    });
    proposal.status = 'executed';
    proposal.result = { issues: filed, failed: [], message: `Filed ${filed.map((i) => `#${i.number}`).join(', ')}.` };
    notice = `Your issues were approved and filed: ${filed.map((i) => i.url).join(', ')}`;
    ack = 'Thanks, the issues are filed.';
  } else if (proposal.kind === 'sloppod.repo.act_as_owner') {
    if (typeof body.credential === 'string') proposal.payload.credential = body.credential;
    const token = credentials.find((c) => c.id === proposal.payload.credential && c.kind === 'github-user');
    const repo = proposal.payload.repo;
    if (!token || !token.config.repositories.includes(repo)) {
      proposal.status = 'failed';
      proposal.error = `the token doesn't reach ${repo}`;
      notice = `Your proposal was approved, but carrying it out failed: ${proposal.error}`;
      ack = 'I see.';
    } else {
      proposal.status = 'executed';
      proposal.result = { login: token.config.login };
      if (session) {
        grantRepo(session, repo);
        const grant = session.data.grants.find((g) => g.capability === 'repo' && g.params.repo === repo);
        const login = token.config.login;
        grant.params = { ...grant.params, credential: token.id, as: login };
        grant.description = `${repo}: read, and push the session branch \`${grant.params.branch}\` and branches named after it, as ${login} (the user's own GitHub token).`;
        session.broadcast();
      }
      notice = `On ${repo} you now act as ${token.config.login}.`;
      ack = `Thanks, on ${repo} I now act as ${token.config.login}.`;
    }
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
  // Like the runtime, approving answers right away and the outcome follows as a proposal update.
  const approved = { ...publicProposal(proposal), status: 'approved', result: undefined };
  setTimeout(() => {
    broadcastProposal(proposal);
    if (session) void notifyAgent(session, notice, ack);
  }, DELAY * 4);
  return approved;
}

/** Standing approvals (`{ id, kind, target, edits, expires_at }`); matching proposals are approved as they're made. */
const standingApprovals = [];

function applyStandingApproval(proposal) {
  const match = standingApprovals.find(
    (a) =>
      a.kind === proposal.kind &&
      a.session_id === proposal.session_id &&
      (!a.target || a.target.toLowerCase() === String(proposal.payload.repo ?? '').toLowerCase()) &&
      (!a.expires_at || Date.parse(a.expires_at) > Date.now()),
  );
  if (match) broadcastProposal(approve(proposal, match.edits ?? {}, `standing-approval:${match.id}`));
}

function handleStandingApprovals(req, res, parts) {
  const method = req.method;
  if (parts.length === 1 && method === 'GET') return json(res, 200, standingApprovals);
  if (parts.length === 1 && method === 'POST') {
    return readBody(req).then((body) => {
      if (!body.session_id) return json(res, 400, { error: 'a standing approval is for one session; give its session_id' });
      const approval = { id: randomUUID(), kind: body.kind, session_id: body.session_id, target: body.target, edits: body.edits ?? {}, expires_at: body.expires_at, created_at: now() };
      standingApprovals.push(approval);
      return json(res, 201, approval);
    });
  }
  if (parts.length === 2 && method === 'DELETE') {
    const index = standingApprovals.findIndex((a) => a.id === parts[1]);
    if (index < 0) return json(res, 404, { error: 'not found' });
    standingApprovals.splice(index, 1);
    res.writeHead(204).end();
    return;
  }
  return json(res, 404, { error: 'not found' });
}

const CREDENTIAL_CONFIG = { claude: [], 'github-app': ['app_id'], memini: ['url'], chatgpt: [], 'github-user': [], token: [] };

/** Names a definition can't take: the built-in capabilities' (the runtime's `capability::BUILTIN`) and the MCP servers sloppod serves itself. */
const RESERVED_NAMES = ['model', 'github', 'packages', 'docs', 'codex', 'repo', 'memory', 'sloppod', 'memini'];

/** Why a new definition is refused, as the runtime does, or null. */
function definitionProblem(body) {
  const name = String(body.name ?? '');
  if (!/^[a-z][a-z0-9-]{0,31}$/.test(name)) {
    return 'a name is 1 to 32 lowercase letters, digits and dashes, starting with a letter';
  }
  if (RESERVED_NAMES.includes(name)) return `${name} is a name sloppod uses; pick another`;
  if (!String(body.description ?? '').trim()) return 'give the capability a description';
  let url;
  try {
    url = new URL(String(body.mcp_url ?? ''));
  } catch {
    return 'the MCP server needs an http or https URL';
  }
  if (!['http:', 'https:'].includes(url.protocol)) return 'the MCP server needs an http or https URL';
  if (url.username || url.password) return 'the URL must not have credentials in it; attach a token credential instead';
  if (body.credential_id && !credentials.some((c) => c.id === body.credential_id && c.kind === 'token')) {
    return "that isn't one of your token credentials";
  }
  return null;
}

function handleCapabilities(req, res, parts) {
  const method = req.method;
  if (parts.length === 1 && method === 'GET') return json(res, 200, definitions);
  if (parts.length === 1 && method === 'POST') {
    return readBody(req).then((body) => {
      const problem = definitionProblem(body);
      if (problem) return json(res, 400, { error: problem });
      if (definitions.some((d) => d.name === body.name)) return json(res, 409, { error: `you already have a capability called ${body.name}` });
      const definition = {
        id: randomUUID(),
        name: body.name,
        description: body.description.trim(),
        mcp_url: body.mcp_url,
        ...(body.credential_id ? { credential_id: body.credential_id } : {}),
        created_at: now(),
      };
      definitions.push(definition);
      return json(res, 201, definition);
    });
  }
  if (parts.length === 2 && method === 'DELETE') {
    const index = definitions.findIndex((d) => d.id === parts[1]);
    if (index < 0) return json(res, 404, { error: 'not found' });
    definitions.splice(index, 1);
    res.writeHead(204).end();
    return;
  }
  return json(res, 404, { error: 'not found' });
}

/** What the runtime learns of a GitHub token from GitHub; a token with "soon" in it expires in three days. */
function inspectUserToken(secret) {
  if (secret.startsWith('ghp_')) return { error: "that's a classic token, which can't be narrowed to repositories; make a fine-grained one instead" };
  if (!secret.startsWith('github_pat_')) return { error: 'expected a fine-grained personal access token, which starts with github_pat_' };
  const days = secret.includes('soon') ? 3 : 60;
  return {
    config: {
      login: secret.includes('work') ? 'bo0tzz-work' : 'bo0tzz',
      expires_at: new Date(Date.now() + days * 24 * 3600_000).toISOString(),
      repositories: ['bo0tzz/sloppod', 'bo0tzz/dotfiles'],
      more_repositories: secret.includes('many'),
    },
  };
}

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
    let config = Object.fromEntries(keys.map((key) => [key, String(body.config[key])]));
    // A token's header is optional: sent as that header, else as a bearer token.
    if (body.kind === 'token' && body.config?.header) {
      const header = String(body.config.header);
      if (!/^[A-Za-z0-9-]+$/.test(header)) return json(res, 400, { error: 'a header name is letters, digits and dashes' });
      config.header = header.toLowerCase();
    }
    if (body.kind === 'github-user') {
      const inspected = inspectUserToken(body.secret.trim());
      if (inspected.error) return json(res, 400, { error: inspected.error });
      config = inspected.config;
    }
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
    // A capability that named it keeps existing without it (the runtime's `on delete set null`).
    for (const definition of definitions) if (definition.credential_id === credential.id) delete definition.credential_id;
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
  first.data.active_at = ago(12);
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
  second.data.active_at = ago(60 * 24);

  const third = seeded({ prompt: 'Bump kopium to 0.24 and regenerate the CRDs.', repo: 'bo0tzz/sloppod', title: 'Bump kopium to 0.24' });
  reply(third, 'On it. Updating `mise.toml` first.');
  third.data.phase = 'disconnected';
  third.data.created_at = ago(40);
  third.data.updated_at = ago(3);
  third.data.active_at = ago(3);

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
  fourth.data.active_at = ago(60 * 2);
}

// --- server ---------------------------------------------------------------------------------

const DEFAULT_BUILD = { version: '0.0.1-main.1', build: '11396d515413b2bec5e7c052b06d5df78cc68052' };
/** What `/me` says the runtime is; `/_mock/build` changes it, as a deploy would. */
let runtimeBuild = DEFAULT_BUILD;

/** Set by `/_mock/drop-next-prompt`: drop the connection the next prompt arrives on, before or after taking it. */
let dropNextPrompt = null;
/** Set by `/_mock/drop-next-remove`: drop the connection right after the next queued message is taken back. */
let dropNextRemove = false;
/** Set by `/_mock/fail-next-prompt`: refuse the next prompt that isn't dropped, without taking it. */
let failNextPrompt = false;

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url ?? '/', 'http://localhost');
  try {
    if (url.pathname.startsWith('/api/')) return await handleApi(req, res, url);
    if (url.pathname === '/_mock/quotas' && req.method === 'POST') {
      quotas = await readBody(req);
      return json(res, 200, {});
    }
    if (url.pathname === '/_mock/forget-timezone' && req.method === 'POST') {
      settings.timezone = null;
      return json(res, 200, {});
    }
    if (url.pathname === '/_mock/fail-next-prompt' && req.method === 'POST') {
      failNextPrompt = true;
      return json(res, 200, { failNextPrompt });
    }
    if (url.pathname === '/_mock/drop-next-remove' && req.method === 'POST') {
      dropNextRemove = true;
      return json(res, 200, { dropNextRemove });
    }
    if (url.pathname === '/_mock/drop-next-prompt' && req.method === 'POST') {
      dropNextPrompt = url.searchParams.get('when') === 'after' ? 'after' : 'before';
      return json(res, 200, { dropNextPrompt });
    }
    if (url.pathname === '/_mock/queue-image' && req.method === 'POST') {
      // A message with an image waiting behind the running turn, as another client could send.
      const session = sessions.get(url.searchParams.get('session') ?? '');
      if (!session) return json(res, 404, { error: 'no such session' });
      const prompt = [
        { type: 'text', text: 'Look at this' },
        { type: 'image', mimeType: 'image/png', data: 'iVBORw0KGgo=' },
      ];
      waitingList(session).push({ id: randomUUID(), text: 'Look at this', prompt, resolve: () => {}, reject: () => {} });
      broadcastQueue(session);
      return json(res, 200, {});
    }
    if (url.pathname === '/_mock/say' && req.method === 'POST') {
      // More output in every session, as if its agent carried on.
      const text = url.searchParams.get('text') ?? '';
      for (const session of sessions.values()) {
        session.emit({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text }, messageId: randomUUID() });
      }
      return json(res, 200, {});
    }
    if (url.pathname === '/_mock/spawn' && req.method === 'POST') {
      // A sub-session of `parent`, as if its agent called spawn_session.
      const parent = sessions.get(url.searchParams.get('parent') ?? '');
      if (!parent) return json(res, 404, { error: 'no such session' });
      const task = url.searchParams.get('task') ?? 'A sub-task';
      const child = createSession({ prompt: task, repo: parent.data.repo ?? undefined, title: task, parent: parent.data.id });
      return json(res, 200, child.data);
    }
    if (url.pathname === '/_mock/notice' && req.method === 'POST') {
      // A notice in every session, folded under `summary` if given, as a message from another session is.
      const text = url.searchParams.get('text') ?? '';
      const summary = url.searchParams.get('summary');
      for (const session of sessions.values()) echoUser(session, text, { notice: true, summary });
      return json(res, 200, {});
    }
    if (url.pathname === '/_mock/build' && req.method === 'POST') {
      // `?version=…&build=…`; without either, back to the default.
      const version = url.searchParams.get('version');
      const build = url.searchParams.get('build');
      runtimeBuild = version || build ? { version, build } : DEFAULT_BUILD;
      return json(res, 200, runtimeBuild);
    }
    if (url.pathname === '/_mock/limit' && req.method === 'POST') {
      // Every session hits the usage limit, carrying on in `?minutes=` minutes; `0` ends the wait.
      const minutes = Number(url.searchParams.get('minutes') ?? 90);
      for (const session of sessions.values()) {
        if (minutes > 0) session.data.limited_until = new Date(Date.now() + minutes * 60_000).toISOString();
        else delete session.data.limited_until;
        session.broadcast();
      }
      return json(res, 200, {});
    }
    if (url.pathname === '/_mock/drop' && req.method === 'POST') {
      // With `?for=<ms>`, new connections are refused that long, so a UI stays disconnected for a while.
      refuseConnectionsUntil = Date.now() + Number(url.searchParams.get('for') ?? 0);
      slowSessionsUntil = Date.now() + Number(url.searchParams.get('slow') ?? 0);
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

/** Set by `/_mock/drop?for=<ms>`: until then, UIs can't connect. */
let refuseConnectionsUntil = 0;
/** Set by `/_mock/drop?slow=<ms>`: until then, reading a session waits, as a slow runtime's REST does. */
let slowSessionsUntil = 0;

const wss = new WebSocketServer({ noServer: true });
server.on('upgrade', (req, socket, head) => {
  if (new URL(req.url ?? '/', 'http://localhost').pathname !== '/acp' || Date.now() < refuseConnectionsUntil) {
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
