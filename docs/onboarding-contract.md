# Onboarding Contract

## Milestone 0 purpose

This contract defines the first-run shape that Milestone 1 must implement.

## Required logical steps

1. Fetch onboarding status.
2. If onboarding is incomplete, redirect the user into onboarding.
3. Present installation context if needed.
4. Create the first admin user.
5. Finish core configuration.
6. Mark onboarding complete.
7. Redirect to the first post-onboarding shell.

## Reference behavior

- `GET /api/onboarding`
- `GET /api/onboarding/installation_type`
- `POST /api/onboarding/users`
- `POST /api/onboarding/core_config`
- frontend redirect to onboarding when not onboarded

These behaviors are modeled after the existing Home Assistant onboarding implementation and should preserve the same step ordering even if the implementation is lighter.

## Milestone 0 implementation state

Currently implemented:
- onboarding status persistence
- root redirect behavior
- onboarding-complete persistence endpoint

Deferred to Milestone 1:
- admin-user creation
- installation type API
- core-config completion API
- pixel-close onboarding UI
