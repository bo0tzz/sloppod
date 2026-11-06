/**
 * The session's right sidebar (#205): open beside the chat when there's room, folded away when there isn't. What the
 * user chose holds as they move between sessions, until they choose again.
 */
export const sessionSidebar = $state<{ choice: boolean | null }>({ choice: null });
