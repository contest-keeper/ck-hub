# Next steps for ck-hub (2026-11-25)

State: the unscoped sole-org routes and the `username` login field are gone (#96), the downloads page shows the latest image per family (#98), layout fixes and an HTML 404 page are in (#99, #100).
Read-only Azure checks passed on 2026-11-24. What is left is written down in [user-checks.md](user-checks.md) and needs a signed-in person.

## 1. Checks that need an organizer login and Azure writes

Follow [user-checks.md](user-checks.md):
- recordings round trip (Put Block, Put Block List, read link, Delete, retention),
- G4 acceptance (manifest and source-offer links; the image download must start).

## 2. Capacity run on Azure (approved: 50 to 100 sticks, hard cap 5 USD, no container size change)

The owner approved C7 for 50 to 100 sticks with the current container (0.25 vCPU). Procedure is in [user-checks.md](user-checks.md) section 3 and [capacity.md](capacity.md):
1. Generate a registry of 50 sticks, then 100: `cargo run --release --example loadsim -- registry --sticks 50 --out registry.json`.
2. Push it into a throwaway organization on the dev Hub.
3. Run each level for 30 minutes and watch Container App CPU in the portal.
4. Put the numbers into `docs/capacity.md`. If CPU is the limit, stop and ask before raising `cpu` in ck-infra (a small PR, deployed by the pipeline).
5. Archive and delete the throwaway sticks afterwards.

## 3. Release automation

`RELEASE_PLEASE_TOKEN` is set on the repos. Release PRs are opened by release-please and merged by the owner.
