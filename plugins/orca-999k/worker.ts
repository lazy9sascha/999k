import type { OrcaWorkerApi } from 'orca/worker'
import { setupCommand, setupTitle } from './setup'

export default function activate(orca: OrcaWorkerApi): void {
  orca.panels.handle('setup.open', async () => {
    const context = await orca.host.call('workspace.readContext')
    if (!context?.workspaceId) return { opened: false }

    const { workspaces } = await orca.host.call('workspace.list')
    const workspace = workspaces.find(({ id }) => id === context.workspaceId)
    if (!workspace || (workspace.hostId !== 'local' && workspace.hostId !== null)) {
      return { opened: false }
    }

    await orca.host.call('terminal.create', {
      workspaceId: workspace.id,
      title: setupTitle,
      command: setupCommand
    })
    return { opened: true }
  })
}
