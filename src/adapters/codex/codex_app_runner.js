/* Foreground bridge for an authorized Codex app-tool host.
 * The host supplies its durable queue adapter and explicit request binding.
 * claim: reserve only the exact request; return its event and target.
 * submitting: persist the no-resend fence before the app call.
 * submitted: retain that fence after the app call.
 * complete: validate the bound delivery and commit reply plus ACK atomically.
 * Only another claim of the same binding can start reconciliation. */
const {runCodexAppHost} = require('./codex_app_host');

const namespace = 'pigeonmq-cloud-codex';

async function run(app, queue, config) {
  if (typeof app?.read_thread !== 'function' ||
      typeof app?.send_message_to_thread !== 'function')
    throw Error('app_host_required');
  if (typeof queue !== 'function') throw Error('durable_queue_adapter_required');
  for (const key of ['threadId', 'hostId', 'messageId', 'sender', 'authorization'])
    if (typeof config?.[key] !== 'string' || !config[key].trim())
      throw Error('invalid_runner_config');
  const {threadId, hostId, messageId, sender, authorization} = config;
  const timeoutMs = config.timeoutMs ?? 120000;
  if (!Number.isInteger(timeoutMs) || timeoutMs < 100 || timeoutMs > 240000)
    throw Error('invalid_runner_deadline');
  const binding = Object.freeze({namespace, threadId, hostId, messageId, sender});
  const deadline = Date.now() + timeoutMs;
  const bounded = async (call, code) => {
    const remaining = deadline - Date.now();
    if (remaining <= 0) throw Error('runner_deadline_reconcile_pending');
    let timer;
    try {
      return await Promise.race([
        Promise.resolve().then(call),
        new Promise((_, reject) => {timer = setTimeout(() => reject(Error(code)), remaining);}),
      ]);
    } finally { clearTimeout(timer); }
  };
  const guarded = {
    read_thread: args => bounded(() => app.read_thread(args), 'app_read_timeout'),
    send_message_to_thread: args => bounded(() => app.send_message_to_thread(args), 'app_send_uncertain'),
  };
  // This callback must persist the submitting fence before it returns. A repeat
  // claim must reconcile, not submit again, until an atomic completion exists.
  const queueCall = async (action, data = {}) => {
    if (data.messageId !== undefined && data.messageId !== messageId)
      throw Error('queue_message_mismatch');
    const result = await bounded(() => queue(action, {...data, binding}), 'queue_call_uncertain');
    if (action === 'claim' && ['claimed', 'replied'].includes(result?.state)) {
      const event = result.event;
      if (event?.message_id !== messageId || event?.sender !== sender ||
          event?.recipient !== namespace || result.target?.thread_id !== threadId ||
          result.target?.host_id !== hostId)
        throw Error('queue_binding_mismatch');
      if (result.state === 'claimed' &&
          (!['submit', 'reconcile'].includes(result.runtime_mode) ||
           typeof event.body !== 'string' || typeof event.correlation_id !== 'string'))
        throw Error('invalid_queue_claim');
    }
    if ((action === 'submitting' || action === 'submitted') && result?.state !== action)
      throw Error('queue_fence_unconfirmed');
    return result;
  };
  return runCodexAppHost(guarded, queueCall, {
    threadId, hostId, authorization, timeoutMs, pollMs: 100, maxMessages: 1,
  }, {ready: async () => {}, startSender: async () => {}, progress: async () => {},
    sleep: ms => new Promise(resolve => setTimeout(resolve, ms))});
}

module.exports = {namespace, run};
