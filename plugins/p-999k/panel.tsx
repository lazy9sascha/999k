import { atom, Button, Code, CodeView, Heading, Image, plugin, read, Select, set, Stack, Text } from 'orca'
import { cueSetupPrompt, initialTarget, managementBotPurpose, setupFor, setupTargets } from './setup'

const opening = atom(false)
const status = atom('')
const target = atom(initialTarget)
const showManagement = atom(false)
const setupMode = atom<'guide' | 'delegate' | null>(null)

function toggleManagement(): void {
  set(showManagement, !read(showManagement))
}

function selectTarget(value: string): void {
  set(target, value)
  set(setupMode, null)
  set(status, '')
}

async function openSetup(action?: 'pair' | 'hosted'): Promise<void> {
  if (read(opening)) return
  const selection = read(target)
  const setup = setupFor(selection, action)
  if (!setup) return

  set(opening, true)
  set(status, '')
  try {
    const { opened } = await plugin.request('setup.open', action ? { target: selection, action } : { target: selection })
    set(status, opened
      ? `Terminal opened. Continue in “${setup.title}”.`
      : 'Open a local workspace, then try again.')
  } catch {
    set(status, 'Orca could not confirm that the terminal opened. Check for the setup terminal before retrying.')
  } finally {
    set(opening, false)
  }
}

export default function Panel() {
  const message = read(status)
  const selection = read(target)
  const setup = setupFor(selection)
  const managementOpen = read(showManagement)
  const mode = read(setupMode)
  const isInitial = selection === initialTarget
  const isCue = selection === 'cue'
  const pairing = setupFor(selection, 'pair')
  const hosted = setupFor(selection, 'hosted')
  return (
    <Stack gap="md">
      <Heading>Orca relay setup</Heading>
      <Image src="999k-field-radio-ink.png" alt="Ink drawing of a field radio" size="md" fit="contain" />
      <Text>Open setup on the Mac that will run your local hub.</Text>
      <Select label="Setup target" value={selection} options={setupTargets}
        disabled={read(opening)} onChange={selectTarget} />
      <Text>Choose Initial setup for your hub or select one peer to set up its relay.</Text>
      {!isInitial && (
        <Stack gap="sm">
          {!isCue && <Text>Set up the peer yourself, or copy a request to an assistant with Computer Use.</Text>}
          <Button disabled={read(opening)} onPress={() => set(setupMode, 'guide')}>I’ll do it myself</Button>
          {!isCue && <Button disabled={read(opening)} onPress={() => set(setupMode, 'delegate')}>Ask my assistant to do it</Button>}
        </Stack>
      )}
      {(isInitial || mode === 'guide') && (
        <Stack gap="sm">
          {!isInitial && <Text>Follow the terminal instructions. Complete any approval in the app and use its private settings or secure input for credentials.</Text>}
          <Button icon="terminal" disabled={read(opening)} onPress={() => openSetup()}>Open setup terminal</Button>
          {hosted && (
            <Stack gap="sm">
              <Text>Install cloudflared on this Mac. Enter your email and verification code in the browser. Check the terminal for reservation status.</Text>
              <Button disabled={read(opening)} onPress={() => openSetup('hosted')}>Set up hosted address</Button>
            </Stack>
          )}
          {isCue && (
            <Stack gap="sm">
              <Text>After the terminal reports authenticated MCP access, copy this message and send it yourself in your existing Cue chat.</Text>
              <CodeView label="Message for your Cue chat" code={cueSetupPrompt} wrap />
              <Text>Wait for Cue’s private form before getting a code. If the form is unavailable, pause here.</Text>
            </Stack>
          )}
          {pairing && (
            <Stack gap="sm">
              <Text>{isCue ? 'Enter the hub address and code from the terminal in Cue’s private form.' : 'When this peer’s HTTPS route is ready, get a single-use code.'}</Text>
              <Button disabled={read(opening)} onPress={() => openSetup('pair')}>Get a setup code</Button>
            </Stack>
          )}
          {isCue && <Text>After Cue confirms access to this Mac’s mailbox tools, send a question in your Orca project chat. Setup is complete when Cue’s matching reply appears in that chat.</Text>}
        </Stack>
      )}
      {!isInitial && !isCue && mode === 'delegate' && setup && (
        <Stack gap="sm">
          <Text>Copy this request to your own assistant if it has Computer Use. You can still choose guided setup at any time.</Text>
          <CodeView label="Computer Use request" code={[
            `Help me connect the selected P-999K peer. Start with ${setup.command} on my Mac.`,
            'Follow the printed handoff in the existing peer app. Pause for my approval or private credential entry whenever required. Keep credentials out of logs and ordinary chat.',
            pairing ? `When the peer's HTTPS route is ready, use ${pairing.command} in an owner-controlled terminal. Let me enter the one-time code privately.` : 'Use the existing Dot workspace and its private Secure MCP Tunnel.',
            'Verify tool discovery in the existing peer app, then use one fresh project request to check the linked reply and original acknowledgement. Report any missing action.'
          ].join(' ')} wrap />
        </Stack>
      )}
      {message && <Text>{message}</Text>}
      <Text>Use a local workspace. For a custom installation, run setup in a local terminal with your paths.</Text>
      {setup && <Text><Code>{setup.command}</Code></Text>}
      {isInitial && (
        <Stack gap="sm">
          <Button onPress={toggleManagement}>
            {managementOpen ? 'Hide management bot setup' : 'Set up a management bot'}
          </Button>
          {managementOpen && (
            <Stack gap="sm">
              <Text>Open Bots → New bot, name it P-999K, and paste this purpose. Choose a provider and select Create.</Text>
              <CodeView label="Bot purpose" code={managementBotPurpose} wrap />
            </Stack>
          )}
        </Stack>
      )}
    </Stack>
  )
}
