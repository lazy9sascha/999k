import { assert, mockHost, mount, test } from 'orca/testing'
import * as panel from './panel'

test('waits for a click and reports a terminal opening', async () => {
  let launches = 0
  const host = mockHost({ requests: { 'setup.open': (input) => {
    assert.deepEqual(input, { target: 'initial' })
    launches++
    return { opened: true }
  } } })
  const view = await mount(panel, { host })
  assert.equal(launches, 0)
  await view.press({ text: 'Open setup terminal' })
  assert.equal(launches, 1)
  assert.ok(view.find({ text: 'Terminal opened. Continue in “P-999K setup”.' }))
  assert.deepEqual(view.errors, [])
  await view.unmount()
})

test('passes the selected peer and names its setup terminal', async () => {
  const host = mockHost({ requests: { 'setup.open': (input) => {
    assert.deepEqual(input, { target: 'cue' })
    return { opened: true }
  } } })
  const view = await mount(panel, { host })
  await view.select({ label: 'Setup target' }, 'cue')
  assert.ok(view.find({ text: '"$HOME/.local/bin/999k" init --orca --orca-peer cue' }))
  await view.press({ text: 'I’ll do it myself' })
  await view.press({ text: 'Open setup terminal' })
  assert.ok(view.find({ text: 'Terminal opened. Continue in “P-999K setup · Cue”.' }))
  assert.deepEqual(view.errors, [])
  await view.unmount()
})

test('shows the optional management bot purpose without opening a terminal', async () => {
  let launches = 0
  const host = mockHost({ requests: { 'setup.open': () => {
    launches++
    return { opened: true }
  } } })
  const view = await mount(panel, { host })
  assert.equal(view.find({ label: 'Bot purpose' }), null)

  await view.press({ text: 'Set up a management bot' })
  const purpose = view.find({ label: 'Bot purpose' })
  assert.ok(purpose)
  assert.match(purpose.props.code, /list peers, run "\$HOME\/\.local\/bin\/999k" init --orca\./)
  assert.match(purpose.props.code, /--orca-peer <id>.*dot, instinct, cue, muse, grok-bot\./)
  assert.match(purpose.props.code, /owner approval needed, and any remote handoff.*Home Chat/)

  await view.select({ label: 'Setup target' }, 'muse')
  assert.equal(view.find({ label: 'Bot purpose' }), null)
  assert.equal(launches, 0)
  assert.deepEqual(view.errors, [])
  await view.unmount()
})

test('asks for a code only after the owner chooses a supported peer and presses the button', async () => {
  const calls: unknown[] = []
  const host = mockHost({ requests: { 'setup.open': (input) => {
    calls.push(input)
    return { opened: true }
  } } })
  const view = await mount(panel, { host })
  assert.equal(view.find({ text: 'Get a setup code' }), null)
  await view.select({ label: 'Setup target' }, 'dot')
  assert.equal(view.find({ text: 'Get a setup code' }), null)
  await view.select({ label: 'Setup target' }, 'muse')
  await view.press({ text: 'I’ll do it myself' })
  assert.deepEqual(calls, [])
  await view.press({ text: 'Get a setup code' })
  assert.deepEqual(calls, [{ target: 'muse', action: 'pair' }])
  assert.ok(view.find({ text: 'Terminal opened. Continue in “P-999K pairing · Muse”.' }))
  await view.unmount()
})


test('offers a copyable Computer Use request without opening a terminal', async () => {
  let launches = 0
  const host = mockHost({ requests: { 'setup.open': () => {
    launches++
    return { opened: true }
  } } })
  const view = await mount(panel, { host })
  await view.select({ label: 'Setup target' }, 'instinct')
  assert.equal(view.find({ text: 'Open setup terminal' }), null)
  await view.press({ text: 'Ask my assistant to do it' })
  const instinct = view.find({ label: 'Computer Use request' })
  assert.ok(instinct)
  assert.match(instinct.props.code, /--orca-peer instinct/)
  assert.match(instinct.props.code, /pair offer instinct/)
  assert.match(instinct.props.code, /linked reply and original acknowledgement/)
  assert.equal(launches, 0)

  await view.select({ label: 'Setup target' }, 'dot')
  assert.equal(view.find({ label: 'Computer Use request' }), null)
  await view.press({ text: 'Ask my assistant to do it' })
  const dot = view.find({ label: 'Computer Use request' })
  assert.ok(dot)
  assert.match(dot.props.code, /Secure MCP Tunnel/)
  assert.ok(!dot.props.code.includes('pair offer'))
  await view.press({ text: 'I’ll do it myself' })
  assert.equal(view.find({ label: 'Computer Use request' }), null)
  assert.ok(view.find({ text: 'Open setup terminal' }))
  assert.equal(launches, 0)
  assert.deepEqual(view.errors, [])
  await view.unmount()
})
