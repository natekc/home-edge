# UI Performance Contract

## Goal

The onboarding and shell experience must feel responsive on Raspberry Pi Zero W class hardware.

## Milestone 0 assumptions

- server-rendered HTML by default
- no large frontend framework in the critical path
- CSS embedded or shipped as a tiny static asset
- JavaScript optional and minimal for first-run flows

## Initial budgets

- first HTML response target: under 200 ms server time on warm runtime
- initial document plus inline styling: under 32 KB
- time to first interactive onboarding screen: under 2 seconds on Pi Zero W class hardware in a typical local network setup
- form-submit round trip target: under 500 ms for local bootstrap actions

## Design implications

- keep the HTML shell simple
- avoid client-side hydration for the first milestone unless profiling proves it harmless
- cache immutable assets aggressively once separate assets exist
- prefer instant server-side redirects over client-side routing for onboarding gates
