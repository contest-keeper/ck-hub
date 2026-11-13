# ck-hub

Keeper Hub: the server sticks report to, the platform watchers, the matcher and the Svelte dashboard.

Part of Contest Keeper (github.com/contest-keeper). Read these before changing anything:

- Code conventions: https://github.com/contest-keeper/.github/tree/main/conventions
- Branches, commits, PRs, releases: https://github.com/contest-keeper/.github/blob/main/CONTRIBUTING.md
- The plan and shared context: `plan/` in the ck-website repository (`plan/context.md` first)

## Checks

Run before committing: cargo fmt --all; cargo clippy --all-targets -- -D warnings; cargo test (set CK_TEST_DATABASE_URL to a throwaway PostgreSQL, see the README, or the store is only tested on memory)

## Rules

- Create work branches (`feature/`, `fix/`, `chore/`, `docs/`, `epic/`) from `develop`.
  Open feature PRs into `develop`; only `develop` may be promoted to `main`.
  Never commit directly to `main` or `develop`. Feature PRs use squash merges;
  promotions into `main` use merge commits after all required checks pass.
- Commit and PR titles: `type: lowercase imperative subject` (e.g. `feat: add ESP config reader`).
- No `Co-Authored-By` trailers, "Generated with" lines or other tool attribution in commits or PRs.
- Never store, log or send passwords, cookies, session ids or tokens. The user types credentials.
- Ask before deleting, force pushing or anything that leaves this machine.
- Skills: `/ck:next-step` for the next plan step, `/ck:commit-pr` to commit and open a PR.