/* Local CLI seam for the bounded Codex app bridge. No network or app discovery. */
const fs = require('node:fs/promises');
const path = require('node:path');
const {createHash, randomUUID} = require('node:crypto');
const {execFile} = require('node:child_process');
const {namespace, run} = require('./codex_app_runner');

const digest = value => createHash('sha256').update(value).digest('hex');

async function persist(file, state) {
  const temp = file + '.' + randomUUID() + '.tmp';
  const handle = await fs.open(temp, 'wx', 0o600);
  try {
    try {
      await handle.writeFile(JSON.stringify(state));
      await handle.sync();
    } finally {
      await handle.close();
    }
    await fs.rename(temp, file);
    const directory = await fs.open(path.dirname(file), 'r');
    try { await directory.sync(); } finally { await directory.close(); }
  } finally {
    await fs.unlink(temp).catch(error => { if (error.code !== 'ENOENT') throw error; });
  }
}

async function runWithLocalQueue(app, config) {
  for (const key of ['binary', 'queuePath'])
    if (!path.isAbsolute(config?.[key] || '')) throw Error('explicit_local_queue_required');
  config = Object.freeze({...config});
  const binary = await fs.realpath(config.binary);
  const db = await fs.realpath(config.queuePath);
  const lockPath = db + '.pigeonmq-cloud-codex.lock';
  let lock;
  try { lock = await fs.open(lockPath, 'wx', 0o600); }
  catch (error) { if (error.code === 'EEXIST') throw Error('queue_bridge_owned_or_uncertain'); throw error; }
  const stateFile = db + '.pigeonmq-cloud-codex-' + digest(String(config.messageId)) + '.json';
  const cli = (binding, action, input) => new Promise((resolve, reject) => {
    const proc = execFile(binary, ['codex-queue', '--db', db, '--id', binding.messageId,
      '--sender', binding.sender, action], {timeout: 16000}, (error, stdout) => {
      if (error) return reject(Error('local_queue_call_uncertain'));
      try { resolve(JSON.parse(stdout)); }
      catch { reject(Error('invalid_local_queue_result')); }
    });
    proc.stdin.on('error', () => reject(Error('local_queue_input_uncertain')));
    proc.stdin.end(input === undefined ? undefined : JSON.stringify(input));
  });
  let state, pending;
  const target = {thread_id: config.threadId, host_id: config.hostId};
  const savedConfig = digest(JSON.stringify([namespace, target, config.messageId,
    config.sender, config.authorization]));
  try {
    try { state = JSON.parse(await fs.readFile(stateFile, 'utf8')); }
    catch (error) { if (error.code !== 'ENOENT') throw error; }
    if (state && state.binding !== savedConfig) throw Error('persisted_bridge_binding_mismatch');
    const operate = async (action, data) => {
      if (action === 'claim') {
        if (!state) {
          const delivery = await cli(data.binding, 'claim');
          if (!delivery) return {state: 'empty'};
          const envelope = delivery.message.envelope;
          state = {binding: savedConfig, phase: 'claimed', receipt: delivery.receipt, event: {
            message_id: delivery.message.id, sender: envelope.sender, recipient: envelope.recipient,
            correlation_id: envelope.correlation_id ?? delivery.message.id, body: envelope.body,
          }};
          await persist(stateFile, state);
        }
        if (state.phase === 'replied')
          return {state: 'replied', event: state.event, target, reply_id: state.reply_id};
        return {state: 'claimed', event: state.event, target,
          runtime_mode: state.phase === 'claimed' ? 'submit' : 'reconcile'};
      }
      const advance = async phase => {
        const next = {...state, phase};
        await persist(stateFile, next);
        state = next;
        return {state: phase};
      };
      if (action === 'submitting' && state?.phase === 'claimed') return advance(action);
      if (action === 'submitted' && state?.phase === 'submitting') return advance(action);
      if (action === 'complete' && ['submitting', 'submitted'].includes(state?.phase)) {
        const result = await cli(data.binding, 'complete', {receipt: state.receipt, body: data.answer});
        const next = {...state, phase: 'replied', reply_id: result.id};
        delete next.receipt;
        delete next.event.body;
        await persist(stateFile, next);
        state = next;
        return {state: 'replied', reply_id: next.reply_id};
      }
      throw Error('invalid_local_queue_transition');
    };
    return await run(app, (action, data) => pending = operate(action, data), config);
  } finally {
    await pending?.catch(() => {});
    await lock.close();
    await fs.unlink(lockPath);
  }
}

module.exports = {runWithLocalQueue};
