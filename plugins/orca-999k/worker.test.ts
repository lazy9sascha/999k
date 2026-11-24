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
  assert.deepEqual(await mounted.request('setup.open'), { opened: true })
  assert.deepEqual(host.callsOf('terminal.create'), [{
    workspaceId: 'fixture',
    title: '999K setup',
    command: '"$HOME/.local/bin/999k" init --orca'
  }])
  assert.deepEqual(host.refusals, [])
  await mounted.deactivate()
})

test('never launches setup for a remote workspace', async () => {
  const host = hostFor('ssh:fixture')
  const mounted = await mountWorker(worker, { host })
  assert.deepEqual(await mounted.request('setup.open'), { opened: false })
  assert.deepEqual(host.callsOf('terminal.create'), [])
  await mounted.deactivate()
})
