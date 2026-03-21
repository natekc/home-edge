# Storage Durability Contract

## Goals

- tolerate abrupt restarts better than naive overwrite-in-place writes
- keep write amplification low on SD cards
- preserve a simple versioned format for bootstrap state
- keep the implementation inspectable and debuggable during early milestones

## Milestone 0 design

- storage root is explicitly configured
- onboarding state is persisted in JSON
- writes use a temporary file in the same directory
- file contents are synced before rename
- the parent directory is synced after rename
- writes are serialized through a mutex to avoid overlapping atomic replace operations

## Constraints

- no unbounded background write queue
- no frequent polling writes
- bootstrap state should change rarely
- richer write coalescing can be added in later milestones if needed

## Failure model

Accepted in Milestone 0:
- the last in-flight state update may be lost if power fails before rename completes

Not accepted in Milestone 0:
- partially written JSON at the final path due to overwrite-in-place writes
- overlapping writes corrupting the final file

## Milestone 1 follow-up

- add corruption quarantine behavior
- add backup snapshot policy for critical bootstrap state
- measure write counts under onboarding and reboot stress
