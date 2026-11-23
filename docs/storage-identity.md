# Signing storage links with the Hub's managed identity (R2.12, Hub half)

By default the Hub signs the short-lived download and upload links with the storage account key
(`CK_DOWNLOAD_KEY`). Without that variable, and with `CK_DOWNLOAD_ACCOUNT` set, it signs them with a
**user delegation key** of its managed identity instead, so no key is stored or handed to the app.

How it works (`crates/ck-hub/src/delegation.rs`): Azure Container Apps gives the app
`IDENTITY_ENDPOINT` and `IDENTITY_HEADER`. The Hub trades them for a token for
`https://storage.azure.com/` (the `AZURE_CLIENT_ID` variable names a user-assigned identity), asks the
blob service for a user delegation key valid for six hours, and signs links with it. A thread asks
again every hour; a link never outlives the key. While there is no key, the routes that need a link
answer `503` and recordings fail, and the log says why (never the token or the key).

The identity needs (ck-infra makes them): `Storage Blob Delegator` on the account, `Storage Blob Data
Reader` on the downloads container and `Storage Blob Data Contributor` on the recordings container.

## Self-test at start

At start the Hub reads a blob that does not exist in each container with a freshly signed link. The storage service checks the signature before it looks for the blob, so `404` means the signature and role are good and `403` means they are not. The log says `storage self-test: <container> accepts the links (404)` or `storage self-test failed: ... refuses the links: 403, <error code>`. Nothing is written and the link is never logged. It does not cover Put Block or delete, so check one recording upload by hand.

## Checking it on develop (needs someone signed in to Azure and an organizer login)

1. Deploy a Hub without `CK_DOWNLOAD_KEY` (ck-infra removes the variable and its secret).
2. The log says `storage links are signed with the managed identity` and no `delegation key` error.
3. Open `/download/forge` and an image link: both redirect to a `...blob.core.windows.net/...?sv=...&skoid=...` link that downloads.
4. Upload a recording from a stick (Put Block, Put Block List), play it from the dashboard, delete it (retention).
5. If a link answers 403 `AuthorizationPermissionMismatch` or `AuthenticationFailed`, the signature format or a role is
   wrong: roll back by setting `CK_DOWNLOAD_KEY` again.
