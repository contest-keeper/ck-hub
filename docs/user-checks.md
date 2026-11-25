# Checks that need a person (Azure writes, organizer login, cost approval, secrets)

A session may only read Azure. These are the steps left for someone signed in. Hub and resource names are
the develop ones; see `docs/azure.md` in ck-infra.

## Already checked read-only (2026-11-24, `az ... show/list`, `curl`)

- The develop Container App has no `CK_DOWNLOAD_KEY`, has `CK_DOWNLOAD_ACCOUNT=ckhubdevdl10lhs`, `AZURE_CLIENT_ID`
  and a user-assigned identity. Its log says `storage links are signed with the managed identity` and both
  storage self-tests answer `404` (the signature was accepted).
- The identity `id-ckhubdev-10lhs` has exactly: Storage Blob Delegator on the account, Storage Blob Data Reader on
  `downloads`, Storage Blob Data Contributor on `recordings`, Key Vault Secrets User on the vault.
- `GET /download/forge` redirects to a signed blob link and that link answers `200`.
- The database firewall has only the four `app-outbound-N` rules (one address); there is no `0.0.0.0` rule.
- `max_replicas` is 1 (login lockouts are still per replica; see `docs/replicas.md`).
- Not set: blob soft delete (`deleteRetentionPolicy` is off) and no lifecycle policy, so "retention" below means
  the Hub's own retention of recordings, not the storage account's.

## 1. Recordings round trip (G4 and real Azure writes)

Needs an organizer login on `https://hub.dev.contestkeeper.com` and one enrolled develop stick (or a test tool
that signs like a stick).

1. Log in, open a stick, and start a recording on it. The stick calls `POST /api/v1/recordings`, then
   `PUT /api/v1/recordings/{id}/chunks/{n}` (the Hub signs a Put Block link) and `.../complete` (Put Block List).
2. In the dashboard open the recording: it must play (a read link). Open a new private window to be sure the link,
   not your session, grants access; it must stop working after its short lifetime.
3. Delete the recording in the dashboard (Delete) and confirm the blob is gone:

   ```bash
   az storage blob list --account-name ckhubdevdl10lhs --container-name recordings --auth-mode login --query "[].name" -o tsv
   ```

4. Retention: leave a recording older than the Hub's retention period and check it is removed (see the retention
   setting in `docs/two-places.md` / the recordings design). If any step answers `403` with
   `AuthorizationPermissionMismatch` or `AuthenticationFailed`, see step 5 of "Checking it on develop" in `docs/storage-identity.md`.

## 2. G4 acceptance

Log in as an organizer, open the manifest link and the source-offer link of the current image
(`/download/image/{id}` and `/download/image/{id}/source-offer`). Both must answer for a logged-in organizer and the
image download must start.

The page `/download/image` lists only the newest image of each family and profile (a normal release beats a
development build, then the higher version wins); older images sit under "Older versions". `/api/v1/images` still returns
every image, with `"latest": true` on the shown ones.

## 3. Capacity on Azure (needs C7 cloud-cost approval)

The simulator registers fake sticks in the Hub's database and sends load for 30 minutes per level. It costs
database and container CPU time, and the container may need more than 0.25 vCPU, which is what C7 approves.

1. Approve C7 first (the owner decides; a larger container is a ck-infra change, merged and deployed by the
   pipeline).
2. Generate the registry and push it with an organizer account into a throwaway organization of the develop Hub
   (`POST /api/v1/orgs/{slug}/sticks`, or `ck-forge registry push`):

   ```bash
   cargo run --release --example loadsim -- registry --sticks 500 --out registry.json
   ```

3. Cost control: follow the cost rules in `docs/next-steps.md` section 2 (baseline reading before the first level, run cost = reading minus baseline, stop when the run cost plus 1 USD would pass the approved 5 USD, checked before each level, once during it and after it; never more than 2.5 hours in total; no scaling during the run; reconcile after cleanup and again after 72 hours). Then run each level for 30 minutes and keep the printed table:

   ```bash
   cargo run --release --example loadsim -- run --hub https://hub.dev.contestkeeper.com --registry registry.json --minutes 30
   ```

   Use a registry of 100, 300 and 500 sticks in turn. Watch the Container App CPU in the Azure portal (Metrics:
   CPU usage) during the run.
4. Replace the tables in `docs/capacity.md` with the Azure numbers. If CPU is the limit, raise `cpu` in ck-infra for
   contest days (next-steps list in `docs/capacity.md`).
5. Delete the throwaway organization's sticks afterwards (archive then delete in the dashboard).

## 4. `RELEASE_PLEASE_TOKEN`

`.github/workflows/release.yml` passes `secrets.RELEASE_PLEASE_TOKEN` to the shared release-please workflow. Without it
release PRs cannot be opened by the workflow. Create a fine-grained personal access token (or a GitHub App token) that
can read and write contents and pull requests on the repository, then store it yourself; never paste it in a chat or a
file:

```bash
gh secret set RELEASE_PLEASE_TOKEN --repo contest-keeper/ck-hub
```

Repeat for each repository whose release workflow uses it.
