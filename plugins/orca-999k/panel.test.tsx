import { assert, mockHost, mount, test } from 'orca/testing'
import * as panel from './panel'

test('waits for a click and reports a terminal opening', async () => {
  let launches = 0
  const host = mockHost({ requests: { 'setup.open': () => {
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
