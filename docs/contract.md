# Service contract

What the queue promises, for every agent profile. How each agent reaches it is in [architecture](architecture.md).

## Guarantees

| Area | Promise |
| --- | --- |
| Ownership | The queue lives at an explicit path and is created by a setup command before serving. The listener never creates or migrates it. It persists across restarts. |
| Authorization | The hub checks the mailbox Bearer and its policy before queue access. A configured Cloudflare Access binding resolves a verified assertion to its mailbox Bearer. |
| Observation | `read`, `wait`, and `inbox` never ACK and never change state. |
| Completion | Storing a reply and ACKing the original happen in one transaction. A failure leaves neither. |
| Retries | Repeating the exact same reply (same original, same body) is safe and creates nothing new. A different body for an already-answered original is rejected with `reply_conflict`. A send repeated with the same idempotency key is stored once. |
| Restart | Replies and ACKs survive restart. An answered original is not delivered again. |
| Wake | A wake is a hint. It never claims, leases, or ACKs. Losing one loses no message. |

A queue receipt confirms storage. A read confirms retrieval. A linked reply confirms that the recipient recorded an answer.

A send to the caller's own mailbox follows the normal queue rules when the route permits it. It does not go straight to dead letter. Routes can omit the caller from recipient discovery or require an explicit recipient grant.

## Muse and Grok Bot body limits

Both mailbox adapters accept request and reply bodies up to 16 KiB (16384 UTF-8 bytes).
These are internal defaults, not verified vendor limits.
The adapters reject a body above this limit during mailbox observation or reply.
This is not a universal limit on queue insertion or on other agents.
The HTTP routes allow JSON escaping and envelope fields in addition to the decoded body.

The Codex MCP send tool accepts up to 8 KiB (8192 UTF-8 bytes) per request. The shared Codex request reader uses the same bound. Terminal replies can hold up to 16 KiB.

## Tests

| Promise | Tests |
| --- | --- |
| Queue acceptance and replay | `tests/queue.rs` |
| SQLite persistence | `tests/queue_journal.rs` |
| Muse read, reply, ACK, rollback, mailbox scope | `tests/muse_mailbox.rs` |
| Grok Bot UTF-8 body boundaries and reply replay | `tests/grok_mailbox.rs` |
| Bearer and policy checks | unit tests in `src/auth.rs` and `src/local_http.rs` |
| Grok Bot wake | `src/adapters/grok_bot/grok_wake_tests.rs` |

Run them with `cargo test --locked -- --test-threads=1`. Live tunnel and app behavior is recorded separately in [research log](research.md#key-findings).
