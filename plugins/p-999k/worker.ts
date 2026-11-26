import type { OrcaWorkerApi } from 'orca/worker'
import { setupFor } from './setup'

export default function activate(orca: OrcaWorkerApi): void {
  orca.panels.handle('setup.open', async (input) => {
    const target = typeof input === 'object' && input !== null && 'target' in input
      ? input.target
      : undefined
    const action = input && typeof input === 'object' && 'action' in input ? input.action : undefined
    const setup = setupFor(target, action)
    if (!setup) throw new Error('Select a supported setup target.')

    const context = await orca.host.call('workspace.readContext')
    if (!context?.workspaceId) return { opened: false }

    const { workspaces } = await orca.host.call('workspace.list')
    const workspace = workspaces.find(({ id }) => id === context.workspaceId)
    if (!workspace || (workspace.hostId !== 'local' && workspace.hostId !== null)) {
      return { opened: false }
    }

    await orca.host.call('terminal.create', {
      workspaceId: workspace.id,
      title: setup.title,
      command: setup.command
    })
    return { opened: true }
  })
}
