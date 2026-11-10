<script lang="ts">
  import { goto } from '$app/navigation';
  import {
    Alert,
    Button,
    Icon,
    IconButton,
    LoadingSpinner,
    Text,
    modalManager,
    toastManager,
  } from '@immich/ui';
  import {
    mdiArrowDown,
    mdiChevronDown,
    mdiCheckDecagramOutline,
    mdiCogOutline,
    mdiPauseCircleOutline,
    mdiPlayCircleOutline,
    mdiMagnifyScan,
    mdiPageLayoutSidebarRight,
    mdiShieldKeyOutline,
    mdiSourcePull,
    mdiSourceRepository,
    mdiTimerSandComplete,
    mdiTrashCanOutline,
  } from '@mdi/js';
  import { onMount, tick } from 'svelte';
  import { ConnectionClosedError } from '#lib/acp/connection.ts';
  import { api, type Proposal } from '#lib/api.ts';
  import { absoluteTime, bytes, clockTime, countdown } from '#lib/format.ts';
  import { toolTitle } from '#lib/presentation.ts';
  import { isSnoozed } from '#lib/snooze.ts';
  import { sessionView, type ReaderPlace } from '#lib/session.svelte.ts';
  import { sessionSidebar } from '#lib/session-sidebar.svelte.ts';
  import { sessions } from '#lib/sessions.svelte.ts';
  import AddRepoModal from './AddRepoModal.svelte';
  import ChatHeader from './ChatHeader.svelte';
  import MenuButton from './MenuButton.svelte';
  import GrantsModal from './GrantsModal.svelte';
  import PermissionCard from './PermissionCard.svelte';
  import QuestionCard from './QuestionCard.svelte';
  import PhaseBadge from './PhaseBadge.svelte';
  import HighlightCard from './HighlightCard.svelte';
  import ProposalCard from './ProposalCard.svelte';
  import ProposalListModal from './ProposalListModal.svelte';
  import ProposalModal from './ProposalModal.svelte';
  import SessionComposer from './SessionComposer.svelte';
  import SessionSidebar from './SessionSidebar.svelte';
  import TranscriptView from './TranscriptView.svelte';
  import WaitingSummary from './WaitingSummary.svelte';

  let { id }: { id: string } = $props();

  // The route wraps this component in {#key id}, so capturing the first id is intended.
  // svelte-ignore state_referenced_locally
  const view = sessionView(id);

  const session = $derived(view.session);
  const title = $derived(session?.title || 'New session');
  const pendingProposals = $derived(view.proposals.filter((proposal) => proposal.status === 'pending'));
  // Approved proposals are carried out in the background; their cards stay until that's done. Failures go to the
  // agent, not here.
  // The runtime pushes the session when questions or permission requests change, but not when a proposal is decided,
  // so the proposals this view already follows give the count (#368).
  const openHighlights = $derived(view.highlights.filter((highlight) => !highlight.seen_at && !highlight.retracted_at));
  // Put off until later (#467): still open, but not waiting on the user until its time passes.
  const snoozedHighlights = $derived(openHighlights.filter((highlight) => isSnoozed(highlight)));
  const waitingHighlights = $derived(openHighlights.filter((highlight) => !isSnoozed(highlight)));
  const waitingOnYou = $derived(
    session?.waiting && view.proposalsLoaded
      ? { ...session.waiting, proposals: pendingProposals.length, highlights: waitingHighlights.length }
      : session?.waiting,
  );
  const openProposals = $derived(
    view.proposals.filter((proposal) => proposal.status === 'pending' || proposal.status === 'approved'),
  );
  const showReconnecting = $derived(view.loaded && view.connection !== 'open');
  let now = $state(Date.now());
  $effect(() => {
    if (!session?.limited_until) return;
    now = Date.now();
    const timer = setInterval(() => (now = Date.now()), 30_000);
    return () => clearInterval(timer);
  });
  const activity = $derived.by(() => {
    const sentence = (text: string) => `${text[0].toUpperCase()}${text.slice(1)}`;
    // Why its pod can't be placed comes first: nothing happens in it until it is.
    const status = session?.unschedulable
      ? `${sentence(session.unschedulable)}…`
      : session?.agent_status
        ? `${sentence(session.agent_status)}…`
        : null;
    switch (session?.phase) {
      case 'starting':
        return status ?? (session.agent_error ? 'Starting the agent again…' : 'Starting the agent…');
      case 'disconnected':
        return status ?? 'Bringing the agent back…';
      case 'waiting':
        return view.questions.length > 0 && view.permissions.length === 0 ? 'Waiting for your answer' : 'Waiting for your decision';
      default:
        return view.turnRunning ? 'Working…' : null;
    }
  });

  let openProposalId = $state<string | null>(null);
  const openProposal = $derived(view.proposals.find((proposal) => proposal.id === openProposalId));
  /** The proposal decided just before, when the modal moved on to the next one waiting. */
  let decidedProposalId = $state<string | null>(null);
  const decidedProposal = $derived(view.proposals.find((proposal) => proposal.id === decidedProposalId));

  const oldestPending = $derived(
    [...pendingProposals].sort((a, b) => a.created_at.localeCompare(b.created_at))[0],
  );

  /** After a decision, the modal moves on to the oldest proposal still waiting for the user, or closes. */
  function openNextProposal(decided: Proposal) {
    const next = pendingProposals
      .filter((proposal) => proposal.id !== decided.id)
      .sort((a, b) => a.created_at.localeCompare(b.created_at))[0];
    decidedProposalId = next ? decided.id : null;
    openProposalId = next?.id ?? null;
  }

  function openProposalModal(proposalId: string) {
    decidedProposalId = null;
    openProposalId = proposalId;
  }
  /** The proposals list: everything (from the menu), or just the open ones (from the bar above the composer). */
  let proposalList = $state<'all' | 'open' | null>(null);
  let showAddRepo = $state(false);
  let showGrants = $state(false);
  /** This session's standing approvals for pull requests; any one of them approves its pull requests. */
  let autoApprovals = $state<string[]>([]);
  let togglingApproval = $state(false);

  async function loadAutoApprovals() {
    const approvals = await api.listStandingApprovals();
    autoApprovals = approvals.filter((a) => a.session_id === id && a.kind === 'github.pr.create').map((a) => a.id);
  }

  const waiting = $derived(
    view.permissions.length > 0 || view.questions.length > 0 || openProposals.length > 0 || waitingHighlights.length > 0,
  );
  let width = $state(0);
  // Room for the chat at its full width (max-w-3xl and its padding) with the sidebar beside it; otherwise it opens
  // over the chat, and while it's folded away what waits on the user shows below the chat.
  const docked = $derived(width >= 1184);
  const sidebarOpen = $derived(sessionSidebar.choice ?? docked);
  // Below the chat on a narrower screen, the waiting panel folds to one line (#374). On a phone it starts folded
  // while a question or a permission request waits, so the chat stays readable; what the user chose holds until
  // nothing waits any more.
  const small = $derived(width > 0 && width < 640);
  let foldChoice = $state<boolean | null>(null);
  $effect(() => {
    if (!waiting) foldChoice = null;
  });
  const asking = $derived(view.questions.length > 0 || view.permissions.length > 0);
  // Highlights fold away too: their cards are in the chat, where the user reads them.
  const folded = $derived(foldChoice ?? (small && (asking || waitingHighlights.length > 0)));
  let waitingItemsElement = $state<HTMLElement>();
  /** Unfolds the panel; a question card that was there all along, hidden, takes focus as if it had just appeared. */
  async function unfold() {
    foldChoice = false;
    await tick();
    waitingItemsElement?.querySelector<HTMLElement>('[data-testid="question-card"]')?.focus({ preventScroll: true });
  }
  const waitingSummary = $derived.by((): { kind: 'question' | 'permission' | 'proposal' | 'highlight'; text: string } => {
    const question = view.questions[0];
    if (question) {
      // A tool's question is an MCP server's, named when the runtime could tell (#481); the agent's own is the agent's.
      const from = question._meta?.sloppod?.questionId ? 'The agent' : (question._meta?.sloppod?.server ?? 'A tool');
      return { kind: 'question', text: `${from} asks: ${question.message.split('\n')[0]}` };
    }
    const permission = view.permissions[0];
    if (permission) {
      const tool = view.transcript.tool(permission.toolCall.toolCallId);
      return { kind: 'permission', text: `Permission needed: ${toolTitle(permission.toolCall.title ?? tool?.title ?? 'a tool call').title}` };
    }
    if (openProposals.length === 0) {
      const count = waitingHighlights.length;
      return { kind: 'highlight', text: `${count} ${count === 1 ? 'highlight' : 'highlights'} for you` };
    }
    const count = pendingProposals.length || openProposals.length;
    return { kind: 'proposal', text: `${count} ${count === 1 ? 'proposal' : 'proposals'} ${pendingProposals.length ? 'to review' : 'being carried out'}` };
  });

  let scroller = $state<HTMLElement | null>(null);
  let content = $state<HTMLElement | null>(null);
  let atBottom = $state(true);
  /**
   * Whether to show the spinner while the session loads. A session whose view the UI already has shows at once (#93),
   * so the spinner only appears once the load is slow enough to be worth saying something about.
   */
  const SPINNER_AFTER_MS = 300;
  let showSpinner = $state(false);
  $effect(() => {
    if (view.loaded) {
      showSpinner = false;
      return;
    }
    const timer = setTimeout(() => (showSpinner = true), SPINNER_AFTER_MS);
    return () => clearTimeout(timer);
  });

  /**
   * Earlier history loads by itself only once the reader has scrolled — by wheel, touch, key or the scrollbar — and not
   * when the view opens at the top, where the scrolling is the view's own. A reader who only drags the scrollbar has no
   * other way to it, the button that used to be there being gone.
   */
  let userScrolled = false;
  /**
   * Which way the reader last asked the view to go: `true` for what's above. A page shorter than the view has nowhere to
   * scroll, so its wheel, swipe and keys leave no scroll event behind, and being at the bottom of it says nothing about
   * whether there is anything above — only they can.
   */
  let askedEarlier = false;
  /**
   * Set while a page of older history is on its way in. The page's rows are laid out as it lands, and a view that looks
   * like it has nothing below it for a moment is not one the reader has gone to the bottom of: following that would
   * take them away from where they were reading, the one thing the page must not do.
   */
  let holdingPlace = false;

  onMount(() => {
    loadAutoApprovals().catch(() => {});
    view.start();
    return () => {
      clearTimeout(readTimer);
      reportRead();
      view.stop();
    };
  });

  /**
   * How tall the view was, and how far from the bottom, when it last settled. The view getting taller or shorter (the
   * message box growing as it's typed in) can come with a scroll event of its own, from the browser, and that says
   * nothing about the reader: whether the view was following the bottom is what it was before the resize.
   */
  let settled = { height: 0, gap: 0 };
  function settle() {
    if (scroller) settled = { height: scroller.clientHeight, gap: bottomGap() };
  }
  const bottomGap = () => (scroller ? scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight : 0);
  const viewResized = () => Boolean(scroller) && scroller!.clientHeight !== settled.height;

  function onScroll() {
    if (!scroller || !positioned) return;
    scheduleRead();
    if (holdBar) return;
    const resized = viewResized();
    if (!holdingPlace && bottomGap() < 80) {
      atBottom = true;
    } else if (!resized && (pointerHeld || performance.now() - lastUserScroll < 1000)) {
      atBottom = false;
    }
    if (!resized) settle();
    // The reader has taken the view where they want it: nothing to hold them to any more (#93).
    if (pointerHeld || performance.now() - lastUserScroll < 1000) kept = null;
    // Recorded as it happens: by the time this view is taken down its rows are gone, and switching back shows the chat
    // where the reader had it (#93).
    view.place = readerPlace();
    loadEarlierIfNear();
  }

  /** How far up the reader gets before the page before is fetched: two screens, so it's usually there before they are. */
  const EARLIER_WITHIN_SCREENS = 2;

  /** Fetches the page before while the reader is reading up into it, as any chat does; never on opening at the top. */
  function loadEarlierIfNear() {
    if (!scroller || !positioned || !userScrolled) return;
    // Being at the bottom says nothing when the page fits the view and there is nowhere below to be: what the reader
    // asked for above them is what is fetched, and being able to see the bottom doesn't make that not so.
    if (atBottom && !askedEarlier) return;
    if (view.historyBefore === null || view.loadingHistory || view.historyError) return;
    if (scroller.scrollTop > EARLIER_WITHIN_SCREENS * scroller.clientHeight) return;
    void loadEarlier();
  }

  /** A row's bottom as the content has it: what a page going in above it moves, whatever the reader scrolls. */
  function endOf(row: HTMLElement) {
    return content ? row.getBoundingClientRect().bottom - content.getBoundingClientRect().top : 0;
  }

  /**
   * Where the reader is being kept while they are back in a session the UI had (#93). The load that catches it up
   * replaces the transcript when it lands, which moves what they are looking at, so their place is put back over the
   * frames after that too — and dropped the moment they scroll themselves.
   */
  let kept: ReaderPlace | null = null;
  $effect(() => {
    const transcript = view.transcript;
    if (!kept || !positioned || !transcript) return;
    void restorePlace(kept);
  });

  /** The row a view is at the top of: the first whose bottom is below the top of the view. */
  function topRow() {
    const top = scroller?.getBoundingClientRect().top;
    return top === undefined ? undefined : rowElements().find((row) => row.getBoundingClientRect().bottom > top);
  }

  /** Where the reader is now, for showing them this session's chat here again (#93): the row at the top of the view, by
   *  the names it has (#485), the offset it sits at, and whether the view is following the bottom. */
  function readerPlace(): ReaderPlace | null {
    if (!scroller) return null;
    const row = topRow();
    if (atBottom || !row) return { names: [], seq: 0, offset: 0, atBottom };
    return {
      names: (row.dataset.rowNames ?? '').split(' ').filter(Boolean),
      seq: Number(row.dataset.seq),
      offset: row.getBoundingClientRect().top - scroller.getBoundingClientRect().top,
      atBottom: false,
    };
  }

  /**
   * Puts the reader back where they were in a session they are switching back to (#93). The rows off screen are laid
   * out at a guess until they come near (#446), so the place is found again and held over the frames after, as a page
   * going in is (`loadEarlier`).
   */
  async function restorePlace(place: ReaderPlace) {
    if (!scroller) return;
    if (place.atBottom) {
      scrollToBottom();
      void tick().then(() => scrollToBottom());
      return;
    }
    let still = 0;
    for (let round = 0; round < 20 && still < 3; round++) {
      await new Promise((resolve) => requestAnimationFrame(resolve));
      // The reader has taken the view where they want it — scrolled, jumped to the latest, sent something — and this
      // one, still on its way, has nothing left to hold them at (#93).
      if (!kept || pointerHeld || performance.now() - lastUserScroll < 1000) return;
      const rows = rowElements();
      const row = place.names.length
        ? rows.find((row) => (row.dataset.rowNames ?? '').split(' ').some((name) => place.names.includes(name)))
        : rows.find((row) => Number(row.dataset.seq) === place.seq);
      if (!row) return;
      const moved = row.getBoundingClientRect().top - scroller.getBoundingClientRect().top - place.offset;
      still = Math.abs(moved) < 0.5 ? still + 1 : 0;
      if (still === 0) scroller.scrollTop += moved;
    }
  }

  /**
   * Loads the page of history before, keeping the reader's place. The page goes in above the row at the top of the view,
   * so scrolling by as much as that row has been pushed down holds it there — measured, and measured again over the
   * frames after, the rows off screen being laid out at a guess until they come near (#446). The row's end and not its
   * start is what's held: where the page continues a message the reader is in, what it adds is above them too.
   */
  async function loadEarlier() {
    const rows = rowElements();
    const at = rows.findIndex((row) => row.getBoundingClientRect().bottom > (scroller?.getBoundingClientRect().top ?? 0));
    // Where the reader is while the page is on its way, not only where they were when they asked: the rows that come
    // near settle as they read (#446), and that settling is no part of the page that then goes in above them.
    const page = view.loadEarlier();
    holdingPlace = true;
    let held = at < 0 ? undefined : endOf(rows[at]);
    // The reader's row by the names it holds — a message's id, a tool call's — which a page going in above it leaves
    // alone, as an update of a live turn to that row does; rows of an earlier page have no name, and there the rows
    // that went in above them are counted instead. Every name a fold holds is kept: the page before can widen it.
    const names = (rows[at]?.dataset.rowNames ?? '').split(' ').filter(Boolean);
    try {
      while (view.loadingHistory && held !== undefined) {
        await new Promise((resolve) => requestAnimationFrame(resolve));
        // The page once it lands is not the reader's place any more: the rows are its own by then.
        if (!view.loadingHistory) break;
        const row = rowElements()[at];
        if (row) held = endOf(row);
      }
      await page;
      if (held === undefined || !scroller) return;
      const scrolledBy = lastUserScroll;
      let scrolled = 0;
      let still = 0;
      for (let round = 0; round < 20 && still < 3; round++) {
        await new Promise((resolve) => requestAnimationFrame(resolve));
        // Scrolling on themselves, the reader has taken the view where they want it: what a page did above them is
        // theirs to scroll past as well, and holding them back would be the jump.
        if (pointerHeld || lastUserScroll !== scrolledBy) return;
        // A row holding any of those names is the reader's: a fold widened by the page above it holds the tool call the
        // reader was at still, under a name of its own from an older page.
        const now = rowElements();
        const row = names.length
          ? now.find((candidate) => (candidate.dataset.rowNames ?? '').split(' ').some((name) => names.includes(name)))
          : now[at + (now.length - rows.length)];
        // Past the end: what's here isn't the transcript this page went into, so there's no place to hold.
        if (!row) return;
        // What has gone in above the reader so far; scrolling there doesn't move it, and where it couldn't be scrolled
        // (a view too short to hold it yet) it is tried again.
        const drift = endOf(row) - held - scrolled;
        still = Math.abs(drift) < 0.5 ? still + 1 : 0;
        if (still === 0) {
          const from = scroller.scrollTop;
          scroller.scrollTop += drift;
          scrolled += scroller.scrollTop - from;
        }
      }
    } finally {
      holdingPlace = false;
    }
  }

  // Only the reader stops the view following new output. The browser scrolls by itself too, e.g. to keep what's on
  // screen in place while something above it changes size, and that's no sign they've scrolled up.
  let lastUserScroll = -Infinity;
  /** A pointer pressed on the transcript, e.g. dragging the scrollbar, counts as the reader scrolling until it lets go. */
  let pointerHeld = false;
  $effect(() => {
    if (!scroller) return;
    const scrolling = (event: Event) => {
      lastUserScroll = performance.now();
      holdBar = false;
      userScrolled = true;
      // Which way they asked to go is kept here, and not read off the scrolling: a page that fits the view says nothing,
      // having no scroll event to come of a wheel or a key, and what's above it is reached only by asking for it.
      const up = asksForEarlier(event);
      if (up !== null) askedEarlier = up;
      loadEarlierIfNear();
    };
    const press = () => (pointerHeld = true);
    const release = () => (pointerHeld = false);
    // A finger's direction only means anything within the gesture it started: a gesture beginning where the last ended
    // would read as going back the way it came.
    const touchStart = (event: TouchEvent) => (lastTouchY = event.touches[0]?.clientY ?? null);
    const touchEnd = () => (lastTouchY = null);
    const events = ['wheel', 'touchmove', 'keydown', 'pointerdown'] as const;
    for (const event of events) scroller.addEventListener(event, scrolling, { passive: true });
    scroller.addEventListener('pointerdown', press);
    scroller.addEventListener('touchstart', touchStart, { passive: true });
    scroller.addEventListener('touchend', touchEnd);
    scroller.addEventListener('touchcancel', touchEnd);
    window.addEventListener('pointerup', release);
    window.addEventListener('pointercancel', release);
    return () => {
      events.forEach((event) => scroller?.removeEventListener(event, scrolling));
      scroller?.removeEventListener('pointerdown', press);
      scroller?.removeEventListener('touchstart', touchStart);
      scroller?.removeEventListener('touchend', touchEnd);
      scroller?.removeEventListener('touchcancel', touchEnd);
      window.removeEventListener('pointerup', release);
      window.removeEventListener('pointercancel', release);
    };
  });

  /**
   * Where the finger was last seen in the gesture it is in, for telling which way it is going: a swipe down is a reader
   * asking for what's above. The gesture's own start sets it and its end clears it, so one that begins where the last
   * one stopped isn't read as going back the way it came.
   */
  let lastTouchY: number | null = null;
  const EARLIER_KEYS = new Set(['PageUp', 'Home', 'ArrowUp']);
  const LATER_KEYS = new Set(['PageDown', 'End', 'ArrowDown']);

  /** Which way a wheel, touch or key asks the view to go: `true` for what's above, `false` for what's below, `null` for neither. */
  function asksForEarlier(event: Event): boolean | null {
    if (event instanceof WheelEvent) return event.deltaY === 0 ? null : event.deltaY < 0;
    if (event instanceof KeyboardEvent) return EARLIER_KEYS.has(event.key) ? true : LATER_KEYS.has(event.key) ? false : null;
    if (event instanceof TouchEvent) {
      const at = event.touches[0]?.clientY ?? null;
      const was = lastTouchY;
      lastTouchY = at;
      return at === null || was === null || at === was ? null : at > was;
    }
    return null;
  }

  function scrollToBottom(behavior: ScrollBehavior = 'instant') {
    scroller?.scrollTo({ top: scroller.scrollHeight, behavior });
  }

  function jumpToLatest() {
    holdBar = false;
    atBottom = true;
    // Asked for the latest: nothing to hold the reader at the history they were in (#93).
    kept = null;
    scrollToBottom('smooth');
  }

  /** What the reader has scrolled past, and where the "Unread" bar goes (#48). */
  const CONTEXT_ABOVE_UNREAD = 96;
  /**
   * Whether the reader is here: the page is showing, and its window has focus. A page in a background tab, or a window
   * behind others, follows the bottom as output comes in but isn't being read. Focus counts as well as visibility, as a
   * window behind another is still visible; it's reliable in desktop browsers and on phones, though someone reading
   * a window beside the one they type in isn't counted until they click it.
   */
  const attending = () => document.visibilityState === 'visible' && document.hasFocus();
  /** Here, and for a moment already: only then does what's on screen count as read, and the bar give way. */
  let reading = $state(typeof document === 'undefined' || attending());
  const BACK_FOR_A_MOMENT_MS = 1500;
  let backTimer: ReturnType<typeof setTimeout> | undefined;
  function attentionChanged() {
    clearTimeout(backTimer);
    if (!attending()) {
      reading = false;
      return;
    }
    backTimer = setTimeout(() => {
      reading = true;
      scheduleRead();
    }, BACK_FOR_A_MOMENT_MS);
  }
  onMount(() => {
    document.addEventListener('visibilitychange', attentionChanged);
    window.addEventListener('focus', attentionChanged);
    window.addEventListener('blur', attentionChanged);
    // Opened in the background: the first time the reader looks, not now.
    if (!reading) attentionChanged();
    return () => {
      document.removeEventListener('visibilitychange', attentionChanged);
      window.removeEventListener('focus', attentionChanged);
      window.removeEventListener('blur', attentionChanged);
      clearTimeout(backTimer);
    };
  });

  /** Set once the view has started where it's going to, at the bar or the bottom: until then nothing counts as read. */
  let positioned = $state(false);
  /** The read position the bar is drawn from. It follows reading while the view is at the bottom, and holds still while scrolled up, so the bar stays put as the reader passes it. */
  let unreadAfter = $state<number | null>(null);
  /** The bar stays where the view opened at it until the reader moves, even if the view ends up at the bottom. */
  let holdBar = $state(false);

  $effect(() => {
    if (positioned && atBottom && !holdBar && reading) unreadAfter = view.readSeq;
  });

  const rowElements = () => (content ? [...content.querySelectorAll<HTMLElement>('[data-seq]')] : []);

  /**
   * Reports what's been scrolled past as read: the last event of the rows fully above the bottom of the view, but never
   * beyond the first event of the row below them. An update to a row that is already read (a plan, a tool call) comes
   * later in the log than rows after it that haven't been seen.
   */
  function reportRead() {
    if (!positioned || !scroller || !reading) return;
    const bottom = scroller.getBoundingClientRect().bottom + 1;
    let seq = 0;
    let below = Infinity;
    for (const row of rowElements()) {
      if (row.getBoundingClientRect().bottom > bottom) {
        below = Number(row.dataset.firstSeq);
        break;
      }
      seq = Math.max(seq, Number(row.dataset.seq));
    }
    seq = Math.min(seq, below - 1);
    if (seq > 0) view.markRead(seq);
  }

  /**
   * Measures once the view has settled for a moment: rows off screen are laid out at a guess until they come near, so
   * what's measured while scrolling or resizing can show a tall row as short and fully in view.
   */
  let readTimer: ReturnType<typeof setTimeout> | undefined;
  function scheduleRead() {
    clearTimeout(readTimer);
    readTimer = setTimeout(reportRead, 200);
  }

  // An item changing in place, a message getting another word on its line, moves no scroll or size.
  $effect(() => {
    if (view.transcript.seq === 0) return;
    void tick().then(scheduleRead);
  });

  /** On opening, the view starts at the "Unread" bar with a little context above it, or at the bottom with nothing unread. */
  $effect(() => {
    if (positioned || !view.loaded || !view.session || !scroller) return;
    const read = view.session.read_seq;
    const place = view.place;
    positioned = true;
    unreadAfter = read ?? 0;
    // Back in a session the reader was in a moment ago (#93): their place, not the bar or the bottom. The bar is still
    // drawn where the read position is, so anything that came in while they were away is under it.
    if (place) {
      view.place = null;
      atBottom = place.atBottom;
      kept = place;
      void tick().then(() => restorePlace(place));
      return;
    }
    if (read === undefined || !view.transcript.items.some((item) => item.addedSeq > read)) {
      // Nothing read yet is a baseline of 0, so what comes in before the first report is unread if nobody is looking.
      unreadAfter = view.readSeq;
      scrollToBottom();
      void tick().then(() => scrollToBottom());
      return;
    }
    atBottom = false;
    holdBar = true;
    void tick().then(() => {
      scrollToBar();
      scheduleRead();
    });
  });

  /** Puts the bar near the top of the view, with some context above it. */
  function scrollToBar() {
    const bar = content?.querySelector<HTMLElement>('[data-unread-divider]');
    if (!bar || !scroller) return;
    scroller.scrollTo({
      top: bar.getBoundingClientRect().top - scroller.getBoundingClientRect().top + scroller.scrollTop - CONTEXT_ABOVE_UNREAD,
    });
  }

  // Follow new output while the reader is at the bottom, but leave them alone when they scrolled up.
  $effect(() => {
    if (!content || !scroller) return;
    const observer = new ResizeObserver(() => {
      if (!positioned) return;
      // A view that was at the bottom before it was resized stays there, whatever scroll events the resize caused.
      if (settled.height > 0 && viewResized() && settled.gap < 80) atBottom = true;
      // Rows off screen are laid out at a guess until they come near, so the bar's place keeps moving as they do.
      if (holdBar) scrollToBar();
      else if (atBottom && !holdingPlace) scrollToBottom();
      settle();
      scheduleRead();
      // Rows settling move what the reader is looking at, so where they are is read again here: what was recorded as
      // they scrolled can be a layout behind (#93, #446).
      view.place = readerPlace();
      // And a session being shown again, and the load catching it up, move it too: their place is put back as it does.
      if (kept) void restorePlace(kept);
      // A page shorter than the room above it leaves the reader near the top still: the next one is due as well.
      loadEarlierIfNear();
    });
    observer.observe(content);
    observer.observe(scroller);
    return () => observer.disconnect();
  });

  /** Carries on with a turn that was stopped or failed, as one click (#406): a short message asking the agent to. */
  function resume() {
    const message = { id: crypto.randomUUID(), text: 'Carry on where you left off.' };
    if (view.connection !== 'open') {
      view.sendWhenConnected(message);
      return;
    }
    view.prompt(message).catch((error) => {
      // A dropped connection is the session view's to settle once it has reloaded.
      if (error instanceof ConnectionClosedError) return;
      toastManager.danger({
        title: "Couldn't resume the turn",
        description: error instanceof Error ? error.message : String(error),
      });
    });
  }

  async function toggleReview() {
    if (!session) return;
    try {
      view.setSession(await api.setSessionReview(id, session.review === 'required' ? 'off' : 'required'));
    } catch (error) {
      toastManager.danger({
        title: "Couldn't change the review setting",
        description: error instanceof Error ? error.message : String(error),
      });
    }
  }

  /** Turns automatic approval on or off as chosen, whatever another tab did since the menu was drawn. */
  async function setAutoApproval(on: boolean) {
    if (togglingApproval) return;
    togglingApproval = true;
    try {
      // Fresh, so another tab's changes count: any live approval keeps approving until all are revoked.
      await loadAutoApprovals();
      if (!on) {
        await Promise.all(autoApprovals.map((approval) => api.revokeStandingApproval(approval)));
      } else if (autoApprovals.length === 0) {
        await api.createStandingApproval({ kind: 'github.pr.create', session_id: id, edits: { auto_merge: true } });
      }
      await loadAutoApprovals();
    } catch (error) {
      toastManager.danger({
        title: "Couldn't change automatic approval",
        description: error instanceof Error ? error.message : String(error),
      });
    } finally {
      togglingApproval = false;
    }
  }

  async function setParked(parked: boolean) {
    try {
      view.setSession(await (parked ? api.parkSession(id) : api.wakeSession(id)));
    } catch (error) {
      toastManager.danger({
        title: parked ? "Couldn't park the session" : "Couldn't wake the session",
        description: error instanceof Error ? error.message : String(error),
      });
    }
  }

  async function deleteSession() {
    const confirmed = await modalManager.showDialog({
      title: 'Delete session?',
      prompt:
        'It stops and leaves your sessions. For a week you can restore it from Settings, with its files, branch and ' +
        'memory; after that it is gone for good. To just stop it for now, park it instead.',
      confirmText: 'Delete',
      icon: mdiTrashCanOutline,
    });
    if (!confirmed) return;
    try {
      await api.deleteSession(id);
      sessions.remove(id);
      await goto('/');
    } catch (error) {
      toastManager.danger({
        title: "Couldn't delete the session",
        description: error instanceof Error ? error.message : String(error),
      });
    }
  }
</script>

<svelte:head>
  <title>{title} · sloppod</title>
</svelte:head>

{#snippet waitingItems()}
      <!-- What the agent highlighted and the user hasn't marked seen: each stays until they do. -->
      {#each waitingHighlights as highlight (highlight.id)}
        <HighlightCard
          {highlight}
          testid="highlight-item"
          onSeen={(h) => view.markHighlightSeen(h)}
          onSnooze={(h, until) => view.snoozeHighlight(h, until)}
        />
      {/each}
      <!-- One request at a time; the rest wait their turn behind it. -->
      {#each view.questions.slice(0, 1) as question (question.key)}
        <QuestionCard
          request={question}
          queued={view.questions.length - 1}
          onRespond={(response) => view.answerQuestion(question, response)}
        />
      {/each}
      {#each view.permissions as request (request.rpcId)}
        <PermissionCard
          {request}
          grants={session?.grants ?? []}
          tool={view.transcript.tool(request.toolCall.toolCallId)}
          onAnswer={(optionId) => view.answerPermission(request, optionId)}
        />
      {/each}
      {#if sidebarOpen}
        {#each openProposals as proposal (proposal.id)}
          <ProposalCard
            {proposal}
            onOpen={() => openProposalModal(proposal.id)}
            onChange={(updated) => view.upsertProposal(updated)}
          />
        {/each}
      {:else if openProposals.length === 1}
        <ProposalCard
          proposal={openProposals[0]}
          onOpen={() => openProposalModal(openProposals[0].id)}
          onChange={(proposal) => view.upsertProposal(proposal)}
        />
      {:else if openProposals.length > 1}
        <!-- Several at once would crowd out the chat, especially on a phone. -->
        <div class="border-primary/40 bg-primary/5 flex items-center gap-3 rounded-2xl border px-3 py-2" data-testid="proposals-bar">
          <span class="grow text-sm">
            {#if pendingProposals.length > 0}
              <span class="font-medium">{pendingProposals.length} {pendingProposals.length === 1 ? 'proposal' : 'proposals'} to review</span>
            {/if}
            {#if openProposals.length > pendingProposals.length}
              <span class="text-muted">
                {pendingProposals.length > 0 ? ', ' : ''}{openProposals.length - pendingProposals.length} being carried out
              </span>
            {/if}
          </span>
          {#if oldestPending}
            <Button size="small" shape="round" variant="ghost" onclick={() => (proposalList = 'open')}>List</Button>
            <Button size="small" shape="round" onclick={() => openProposalModal(oldestPending.id)}>Review</Button>
          {:else}
            <Button size="small" shape="round" onclick={() => (proposalList = 'open')}>View</Button>
          {/if}
        </div>
      {/if}
{/snippet}

<!-- Put off until later (#467): out of what waits on the user, but still there to be found, and to push further. -->
{#snippet snoozedItems()}
  {#each snoozedHighlights as highlight (highlight.id)}
    <HighlightCard
      {highlight}
      testid="snoozed-item"
      onSeen={(h) => view.markHighlightSeen(h)}
      onSnooze={(h, until) => view.snoozeHighlight(h, until)}
    />
  {/each}
{/snippet}

{#if view.notFound}
  <div class="mx-auto flex max-w-md flex-col items-center gap-4 px-4 py-16 text-center">
    <Text size="large" fontWeight="semi-bold">Session not found</Text>
    <Text color="muted">It may have been deleted.</Text>
    <Button href="/" shape="round">All sessions</Button>
  </div>
{:else}
  <div class="flex h-full min-h-0 flex-col" data-testid="session-screen">
    <ChatHeader {title} subtitle={session ? (session.repo ?? 'No repository') : undefined} mono={Boolean(session?.repo)}>
      {#snippet actions()}
        {#if session}
          <PhaseBadge phase={session.phase} commands={session.commands} commandLines={session.command_lines} waiting={waitingOnYou} unknown={showReconnecting} />
          {#if session.disk}
            <span
              class="text-muted hidden shrink-0 text-xs sm:inline"
              title="{bytes(session.disk.used)} of {bytes(session.disk.size)} of its volume used, as of {absoluteTime(session.disk.seen_at)}"
              data-testid="session-disk"
            >
              {bytes(session.disk.used)}
            </span>
          {/if}
        {/if}
        <IconButton
          icon={mdiPageLayoutSidebarRight}
          aria-label={sidebarOpen ? 'Hide details' : 'Show details'}
          aria-expanded={sidebarOpen}
          variant="ghost"
          shape="round"
          color="secondary"
          indicator={!sidebarOpen && waiting ? 'warning' : undefined}
          onclick={() => (sessionSidebar.choice = !sidebarOpen)}
        />
        <IconButton
          icon={mdiSourcePull}
          aria-label="Proposals"
          variant="ghost"
          shape="round"
          color="secondary"
          indicator={pendingProposals.length > 0 ? 'warning' : undefined}
          onclick={() => (proposalList = 'all')}
        />
        <MenuButton
          aria-label="Session actions"
          items={[
            {
              title: 'Grants',
              icon: mdiShieldKeyOutline,
              $if: () => Boolean(session),
              onAction: () => (showGrants = true),
            },
            {
              title: 'Add repository…',
              icon: mdiSourceRepository,
              $if: () => Boolean(session) && session?.phase !== 'stopped',
              onAction: () => (showAddRepo = true),
            },
            {
              title: session?.review === 'required' ? 'Stop requiring review' : 'Require review by Codex',
              icon: mdiMagnifyScan,
              $if: () => session?.kind === 'work',
              onAction: toggleReview,
            },
            {
              title: autoApprovals.length > 0 ? 'Stop approving its pull requests' : 'Approve its pull requests automatically',
              icon: mdiCheckDecagramOutline,
              $if: () => session?.kind === 'work',
              onAction: () => setAutoApproval(autoApprovals.length === 0),
            },
            {
              title: 'Park',
              icon: mdiPauseCircleOutline,
              $if: () => session?.phase === 'idle' || session?.phase === 'disconnected',
              onAction: () => setParked(true),
            },
            {
              title: 'Wake',
              icon: mdiPlayCircleOutline,
              $if: () => session?.phase === 'parked',
              onAction: () => setParked(false),
            },
            { title: 'Settings', icon: mdiCogOutline, href: '/settings' },
            { title: 'Delete session', icon: mdiTrashCanOutline, color: 'danger', onAction: deleteSession },
          ]}
        />
      {/snippet}
    </ChatHeader>

    {#if showReconnecting}
      <div
        class="bg-warning-100 text-warning-800 flex shrink-0 items-center justify-center gap-2 px-3 py-1 text-xs"
        role="status"
        data-testid="reconnecting"
      >
        <LoadingSpinner size="tiny" />
        Connection lost, reconnecting…
        <button type="button" class="font-medium underline" onclick={() => view.retryNow()}>Retry now</button>
      </div>
    {/if}

    {#if session?.limited_until}
      <div
        class="bg-warning-100 text-warning-800 flex shrink-0 items-center justify-center gap-2 px-3 py-1 text-center text-xs"
        role="status"
        data-testid="limited"
      >
        <Icon icon={mdiTimerSandComplete} size="1.2em" class="shrink-0" aria-hidden />
        <span>
          The account's usage limit is reached. The session carries on at
          <time datetime={session.limited_until} title={absoluteTime(session.limited_until)}
            >{clockTime(session.limited_until, now)}</time
          >
          ({countdown(session.limited_until, now)}). Messages wait until then; steer one in to try now.
        </span>
      </div>
    {/if}

    <div class="relative flex min-h-0 grow" bind:clientWidth={width}>
      <div class="flex min-w-0 grow flex-col">
        <div class="relative min-h-0 grow">
          <div
            bind:this={scroller}
            onscroll={onScroll}
            class="immich-scrollbar relative h-full overflow-y-auto"
            data-testid="transcript-scroller"
          >
            <div bind:this={content} class="mx-auto flex w-full max-w-3xl flex-col gap-3 px-3 py-4 md:px-4">
              {#if view.loadError}
                <div data-testid="load-error">
                  <Alert color="warning" size="small" title="Couldn't load the session's details">
                    {view.loadError}. Trying again…
                  </Alert>
                </div>
              {/if}
              {#if !view.loaded}
                {#if view.connectionError}
                  <div class="flex justify-center py-16">
                    <Alert color="danger" size="small" title="Couldn't load the session">{view.connectionError}</Alert>
                  </div>
                {:else if showSpinner}
                  <!-- Nothing before it: a session the UI already has shows at once, and this is for what's left (#93). -->
                  <div class="flex justify-center py-16" data-testid="session-loading">
                    <LoadingSpinner size="large" />
                  </div>
                {/if}
              {:else}
                {#if view.transcript.items.length === 0 && session?.phase !== 'starting'}
                  <Text color="muted" class="py-16 text-center">Nothing here yet. Send a message to get going.</Text>
                {/if}
                {#if view.historyError}
                  <!-- The one place the reader is asked anything: a page that didn't arrive, and only they can say again. -->
                  <div class="flex items-center justify-center gap-2 py-1" data-testid="history-error">
                    <Text color="muted" size="small" title={view.historyError}>Couldn't load earlier messages</Text>
                    <Button size="tiny" variant="ghost" onclick={loadEarlier}>Retry</Button>
                  </div>
                {:else if view.historyBefore === null && view.transcript.items.length > 0}
                  <div class="text-muted py-1 text-center text-xs" data-testid="history-start">Start of the session</div>
                {/if}
                <TranscriptView
                  items={view.transcript.items}
                  running={view.turnRunning}
                  sessionId={id}
                  highlights={view.highlights}
                  onSeen={(highlight) => view.markHighlightSeen(highlight)}
                  onSnooze={(highlight, until) => view.snoozeHighlight(highlight, until)}
                  onResume={view.unsent.length > 0 ? undefined : resume}
                  awaiting={new Set(view.permissions.map((request) => request.toolCall.toolCallId))}
                  unreadAfter={atBottom && !holdBar && reading ? null : unreadAfter}
                />
                {#if activity}
                  <div class="text-muted flex items-center gap-2 py-1 text-sm" data-testid="activity">
                    <LoadingSpinner size="tiny" />
                    {activity}
                  </div>
                {/if}
              {/if}
            </div>
          </div>
          {#if view.loadingHistory}
            <!-- Over the top of the chat rather than in it: a page on its way moves nothing the reader is looking at. -->
            <div class="pointer-events-none absolute inset-x-0 top-2 flex justify-center" data-testid="loading-earlier">
              <LoadingSpinner size="tiny" />
            </div>
          {/if}
          {#if !atBottom}
            <div class="absolute end-4 bottom-3">
              <IconButton
                icon={mdiArrowDown}
                aria-label="Scroll to the latest message"
                shape="round"
                color="secondary"
                size="small"
                class="shadow-md"
                onclick={jumpToLatest}
              />
            </div>
          {/if}
        </div>

        {#if waiting && !sidebarOpen}
          <div
            class={['border-light-200 shrink-0 border-t px-3 py-2', !folded && 'max-h-[40dvh] overflow-y-auto sm:max-h-[55dvh]']}
            data-testid="waiting-panel"
            data-folded={folded || undefined}
          >
            <div class="mx-auto flex w-full max-w-3xl flex-col gap-2">
              {#if folded}
                <!-- Keyed, so each new question takes focus as its card would. -->
                {#key view.questions[0]?.key}
                  <WaitingSummary {...waitingSummary} onUnfold={unfold} />
                {/key}
              {/if}
              <!-- Hidden rather than unmounted while folded, so a half-answered question keeps its answers and step
                   whether or not they could be kept in storage. -->
              <div class={['flex flex-col gap-2', folded && 'hidden']} bind:this={waitingItemsElement}>
                <div class="-mb-2 flex justify-end">
                  <IconButton
                    icon={mdiChevronDown}
                    aria-label="Fold"
                    title="Fold to one line"
                    size="tiny"
                    shape="round"
                    variant="ghost"
                    color="secondary"
                    onclick={() => (foldChoice = true)}
                  />
                </div>
                {@render waitingItems()}
              </div>
            </div>
          </div>
        {/if}

        <SessionComposer
          {view}
          onSend={() => {
            lastUserScroll = -Infinity;
            pointerHeld = false;
            holdBar = false;
            atBottom = true;
            // Sending takes the reader to the latest, as the jump button does: their history place is done with (#93).
            kept = null;
            scrollToBottom();
          }}
        />
      </div>
      {#if sidebarOpen}
        <SessionSidebar
          waiting={waitingItems}
          hasWaiting={waiting}
          snoozed={snoozedItems}
          snoozedCount={snoozedHighlights.length}
          workDone={view.workDone}
          {docked}
          onClose={() => (sessionSidebar.choice = false)}
        />
      {/if}
    </div>
  </div>
{/if}

{#if proposalList}
  <ProposalListModal
    proposals={[...(proposalList === 'open' ? openProposals : view.proposals)].reverse()}
    onOpen={(proposalId) => {
      proposalList = null;
      openProposalModal(proposalId);
    }}
    onChange={(proposal) => view.upsertProposal(proposal)}
    onClose={() => (proposalList = null)}
  />
{/if}

{#if showGrants && session}
  <GrantsModal {session} onChange={(updated) => view.setSession(updated)} onClose={() => (showGrants = false)} />
{/if}

{#if showAddRepo && session}
  <AddRepoModal
    {session}
    onAdded={(updated, repo) => {
      view.setSession(updated);
      toastManager.success(`Added ${repo}`);
    }}
    onClose={() => (showAddRepo = false)}
  />
{/if}

{#if openProposal}
  <!-- Not keyed: moving on to the next proposal swaps the content in the same frame, and the modal starts its form
       afresh itself. -->
  <ProposalModal
    proposal={openProposal}
    previous={decidedProposal}
    onChange={(proposal) => view.upsertProposal(proposal)}
    onDecided={openNextProposal}
    onClose={() => (openProposalId = null)}
  />
{/if}
