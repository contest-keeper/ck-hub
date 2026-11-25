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
3. Cost control. Cost Management data can arrive hours or days late, so readings alone cannot enforce the cap; the cap is enforced by limits that make a higher cost impossible, and the readings only reconcile it:
   - Pre-run bound: the run adds no new chargeable resource. The Container App stays at 0.25 vCPU with `max_replicas` 1 and the database stays as it is, so the extra cost is only the extra CPU time of the container and database for the run, which is well under 1 USD for 2.5 hours. Do not scale anything up during the run; if a scale-up is wanted, stop and ask first.
   - Runtime limit: at most the 50 and 100 stick levels, 30 minutes each, never more than 2.5 hours in total. Each level ends by itself at its `--minutes` limit and prints the results table; wait for the process to exit. Do not stop `loadsim` with Ctrl+C: it has no signal handler, so an interrupt before the table is printed loses all results of that level. Do not start a further level when the 2.5 hours are used, whatever the cost readings show.
   - Baseline: before the first level, record the accrued cost of `rg-ckhub-dev` for the current month (portal: Cost Management, scope `rg-ckhub-dev`, "Accumulated cost"; or `az consumption usage list --start-date <first day of month> --end-date <today>` filtered to that resource group). The run cost is the later reading minus this baseline, taken in the same month. If the run or the reconciliation crosses a month boundary, read each month separately and add them: (end of the first month minus the baseline) plus (the new month's accumulated cost at the later reading). If other dev usage happens in the same group during the run, say so in the notes; the bound above still holds.
   - Stop margin: stop at once if the run cost plus 1 USD would pass 5 USD, checked before each level, once during the level at about 15 minutes, and after each level.
   - Reconcile: read the cost again after the cleanup in step 5 and mark that number as preliminary: Azure can take up to 72 hours to report usage on pay-as-you-go subscriptions. Read it once more after 72 hours and note both readings in `docs/capacity.md`.
4. Put the numbers into `docs/capacity.md`. If CPU is the limit, stop and ask before raising `cpu` in ck-infra (a small PR, deployed by the pipeline).
5. Archive and delete the throwaway sticks afterwards.

## 3. Release automation

`RELEASE_PLEASE_TOKEN` is set on the repos. Release PRs are opened by release-please and merged by the owner.
