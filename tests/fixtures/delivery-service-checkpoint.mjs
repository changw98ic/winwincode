import {
  DELIVERY_FIXTURE_BASE_TIME,
  DeliveryServiceFixtureTestkit,
  keylessFixtureEnvironment,
} from './delivery-service-testkit.mjs'

const root = process.argv[2]
if (root === undefined || root.length === 0) {
  throw new TypeError('delivery fixture checkpoint requires a root directory')
}

const leakedCredentialNames = Object.keys(process.env).filter(name => (
  process.env[name] !== undefined
  && process.env[name] !== ''
  && !Object.hasOwn(keylessFixtureEnvironment(), name)
))
if (leakedCredentialNames.length > 0) {
  throw new Error('delivery fixture checkpoint received a credential-bearing environment')
}

const kit = await DeliveryServiceFixtureTestkit.create({
  root,
  clockStart: DELIVERY_FIXTURE_BASE_TIME + 100,
})
const checkpoint = await kit.preparePlanReview()
process.stdout.write(`${JSON.stringify({
  deliveryId: kit.deliveryId,
  revision: checkpoint.delivery.revision,
  status: checkpoint.delivery.status,
  attentionItemId: checkpoint.attention.id,
  reviewSessionId: checkpoint.delivery.sessionBindings.find(binding => (
    binding.stageRunId === checkpoint.attention.stageRunId
  )).dshSessionId,
})}\n`)

// The parent test sends SIGTERM after observing this durable checkpoint.
if (process.argv.includes('--hold')) {
  setInterval(() => {}, 60_000)
  await new Promise(() => {})
}
