import { assert, mockHost, mountWorker, test } from 'orca/testing'
import * as worker from './worker'

function hostFor(hostId: string | null) {
  return mockHost({
    capabilities: ['workspace:read', 'terminal:send'],
    results: {
      'workspace.readContext': { workspaceId: 'fixture', branch: '', displayName: 'Fixture', terminals: [] },
      'workspace.list': {
        workspaces: [{ id: 'fixture', kind: 'folder', name: 'Fixture', path: '/fixture', branch: null, hostId }],
        truncated: false
      },
      'terminal.create': { terminalId: 'fixture-terminal' }
    }
  })
}

test('opens a fresh terminal for the fixed setup command', async () => {
  const host = hostFor('local')
  const mounted = await mountWorker(worker, { host })
  assert.deepEqual(await mounted.request('setup.open', { target: 'initial' }), { opened: true })
  assert.deepEqual(host.callsOf('terminal.create'), [{
    workspaceId: 'fixture',
    title: 'P-999K setup',
    command: '"$HOME/.local/bin/999k" init --orca'
  }])
  assert.deepEqual(host.refusals, [])
  await mounted.deactivate()
})

test('never launches setup for a remote workspace', async () => {
  const host = hostFor('ssh:fixture')
  const mounted = await mountWorker(worker, { host })
  assert.deepEqual(await mounted.request('setup.open', { target: 'initial' }), { opened: false })
  assert.deepEqual(host.callsOf('terminal.create'), [])
  await mounted.deactivate()
})

test('opens a terminal for the selected peer', async () => {
  const host = hostFor('local')
  const mounted = await mountWorker(worker, { host })
  assert.deepEqual(await mounted.request('setup.open', { target: 'grok-bot' }), { opened: true })
  assert.deepEqual(host.callsOf('terminal.create'), [{
    workspaceId: 'fixture',
    title: 'P-999K setup · GrokBot',
    command: '"$HOME/.local/bin/999k" init --orca --orca-peer grok-bot'
  }])
  await mounted.deactivate()
})

test('rejects an unsupported target before contacting the host', async () => {
  const host = hostFor('local')
  const mounted = await mountWorker(worker, { host })
  await assert.rejects(mounted.request('setup.open', { target: 'dot; echo fixture' }))
  assert.deepEqual(host.callsOf('workspace.readContext'), [])
  assert.deepEqual(host.callsOf('terminal.create'), [])
  await mounted.deactivate()
})

test('issues a code only for an allowed pairing target on a local workspace', async () => {
  const host = hostFor('local')
  const mounted = await mountWorker(worker, { host })
  await assert.rejects(mounted.request('setup.open', { target: 'dot', action: 'pair' }))
  assert.deepEqual(host.callsOf('terminal.create'), [])
  await mounted.request('setup.open', { target: 'cue', action: 'pair' })
  assert.deepEqual(host.callsOf('terminal.create'), [{
    workspaceId: 'fixture',
    title: 'P-999K pairing · Cue',
    command: '"$HOME/.local/bin/999k" pair offer cue'
  }])
  await mounted.deactivate()
})

test('opens an address reservation before any peer is selected', async () => {
  const host = hostFor('local')
  const mounted = await mountWorker(worker, { host })
  for (const target of ['dot', 'muse', 'instinct', 'grok-bot']) {
    await assert.rejects(mounted.request('setup.open', { target, action: 'hosted' }))
  }
  assert.deepEqual(host.callsOf('terminal.create'), [])
  await mounted.request('setup.open', { target: 'initial', action: 'hosted' })
  await mounted.request('setup.open', { target: 'cue', action: 'hosted' })
  assert.deepEqual(host.callsOf('terminal.create'), [{
    workspaceId: 'fixture',
    title: 'P-999K hosted address',
    command: '"$HOME/.local/bin/999k" init --reserve-address'
  }, {
    workspaceId: 'fixture',
    title: 'P-999K hosted setup · Cue',
    command: '"$HOME/.local/bin/999k" init --orca --orca-peer cue --orca-reserve-address'
  }])
  await mounted.deactivate()
})
