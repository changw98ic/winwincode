import { JobId } from '../../packages/contracts/dist/index.js'
import { StrongFlowGitWorkspaceManager } from '../../packages/strongflow/dist/index.js'

const [operation, home, jobIdInput, repositoryPath, gitExecutable] = process.argv.slice(2)

if (
  operation === undefined
  || home === undefined
  || jobIdInput === undefined
  || repositoryPath === undefined
  || gitExecutable === undefined
) throw new Error('operation, home, jobId, repositoryPath, and gitExecutable are required')

const manager = new StrongFlowGitWorkspaceManager({ home, gitExecutable })
const jobId = JobId(jobIdInput)

switch (operation) {
  case 'create':
    await manager.create({ jobId, repositoryPath })
    break
  case 'freeze':
    await manager.freezeCandidate(jobId, { scope: { mode: 'repository' } })
    break
  case 'dispose':
    await manager.dispose(jobId)
    break
  default:
    throw new Error(`unknown workspace operation ${operation}`)
}
