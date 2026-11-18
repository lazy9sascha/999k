/* Run inside an authorized app-tool host, not a standalone shell or daemon.
 * Queue ownership and completion stay in the existing queue adapter. */
const {runCodexAppWatcher} = require('./codex_app_watcher');

function appResult(result) {
  if (!result || result.isError) throw Error('app_tool_error');
  if (result.structuredContent != null) return result.structuredContent;
  const text = result.content?.filter(item => item.type === 'text');
  if (text?.length !== 1) throw Error('invalid_app_tool_result');
  try { return JSON.parse(text[0].text); }
  catch { throw Error('invalid_app_tool_result'); }
}

async function runCodexAppHost(app, queue, config, hooks) {
  // The host supplies an authorized queue callback and the existing lifecycle
  // hooks. This adapter does not select a mailbox, create a queue, or retry sends.
  return runCodexAppWatcher({
    ...hooks,
    queue,
    read: async (threadId, hostId) => appResult(await app.read_thread({
      threadId, hostId, turnLimit: 10, includeOutputs: true,
      maxOutputCharsPerItem: 20000,
    })),
    send: async (threadId, hostId, prompt) => {
      const result = await app.send_message_to_thread({threadId, hostId, prompt});
      if (!result || result.isError) throw Error('app_send_uncertain');
    },
  }, config);
}

module.exports = {runCodexAppHost};
