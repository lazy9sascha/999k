import { atom, Button, Code, Heading, plugin, read, set, Stack, Text } from 'orca'
import { setupCommand, setupTitle } from './setup'

const opening = atom(false)
const status = atom('')

async function openSetup(): Promise<void> {
  if (read(opening)) return
  set(opening, true)
  set(status, '')
  try {
    const { opened } = await plugin.request('setup.open')
    set(status, opened
      ? `Terminal opened. Continue in “${setupTitle}”.`
      : 'Open a local workspace, then try again.')
  } catch {
    set(status, 'Orca could not confirm that the terminal opened. Check for the setup terminal before retrying.')
  } finally {
    set(opening, false)
  }
}

export default function Panel() {
  const message = read(status)
  return (
    <Stack gap="md">
      <Heading>Connect an Orca bot</Heading>
      <Text>Install PigeonMQ 0.2.0 and enable stdio on your existing hub before setup.</Text>
      <Button icon="terminal" disabled={read(opening)} onPress={openSetup}>Open setup terminal</Button>
      {message && <Text>{message}</Text>}
      <Text>Use a local workspace. For a custom installation, run setup in a local terminal with your paths.</Text>
      <Text><Code>{setupCommand}</Code></Text>
    </Stack>
  )
}
