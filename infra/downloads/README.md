# Umoja downloads (Cloudflare R2 + Worker)

Release ISOs (~4.7 GB) are too big for GitHub releases (2 GiB per file), so
they live in a Cloudflare R2 bucket, which has no egress fees. A small
Worker in front of the bucket serves them and counts each download in
Workers Analytics Engine. Everything here fits Cloudflare's free tiers
(R2: 10 GB stored free, then $0.015/GB-month; Workers: 100k requests/day).

```
GitHub Actions (Image workflow, release)  --S3 API-->  R2 bucket umoja-releases
                                                          |
user --> https://<worker>/latest --> Worker (this dir) ---+--> Analytics Engine
```

## How releases get published

1. Run the **Packages** workflow on main with `release` ticked, as before.
   It uploads to the PPA and pushes the `v<version>` tag.
2. When it succeeds, the **Image** workflow builds the image and the ISO.
   Because the commit carries the release tag, the ISO is named
   `ubuntu-umoja-desktop-v<version>_amd64.iso` (no commit hash).
3. Its **Publish the release ISO to R2** job uploads to the bucket:
   - `releases/v<version>/ubuntu-umoja-desktop-v<version>_amd64.iso`
   - the same with `.iso.sha256` and `.list` (package manifest)
   - `latest.json` last, so `/latest` only moves once all files are there

   and adds the download link and SHA-256 to the GitHub release notes.

Edge builds (pushes to main) still keep their ISO as a 7-day workflow
artifact only. Without the secrets below the publish job warns and skips.

## Worker URLs

| Path | What |
|---|---|
| `/` | page with the latest release, size, SHA-256 |
| `/latest` | 302 to the latest ISO |
| `/latest.json` | the manifest CI writes |
| `/releases/v<version>/<file>` | any release file (supports Range and resume) |

A download is counted once per ISO fetch: a plain GET, or a ranged GET
starting at byte 0 (download managers split a file into several ranges, and
resumed downloads aren't counted again). Each data point records the tag,
file, country and client kind (browser, curl, wget, other).

## One-time setup

### 1. Cloudflare account and R2

1. Sign up (free) at <https://dash.cloudflare.com/sign-up>.
2. In the dashboard sidebar open **R2 Object Storage** and enable it. R2
   asks for a payment method even on the free tier.
3. **Create bucket**, name it `umoja-releases`, location Automatic, default
   storage class Standard. (Another name works too: then set the repository
   variable `R2_BUCKET` and change `bucket_name` in `wrangler.toml`.)
4. Copy your **Account ID** (R2 overview page, right-hand side, or the URL
   `dash.cloudflare.com/<account id>/...`).

### 2. API tokens

1. **R2 upload key** (for the Image workflow): R2 Object Storage, **Manage
   API tokens** (under "API Tokens" on the R2 overview), **Create Account
   API token**. Permission **Object Read & Write**, "Apply to specific
   buckets only": `umoja-releases`, TTL forever. Create, then copy the
   **Access Key ID** and **Secret Access Key** (shown once).
2. **Worker deploy token** (for the Downloads Worker workflow): avatar menu,
   **Profile**, **API Tokens**, **Create Token**, template **Edit Cloudflare
   Workers**. Account Resources: your account; Zone Resources: All zones
   (or the zone of your custom domain). Create and copy it.
3. **Stats token** (optional, for `infra/downloads/stats` on your machine):
   **Create Token**, **Create Custom Token**, permission *Account ›
   Account Analytics › Read*. Keep it locally; it isn't a GitHub secret.

### 3. GitHub secrets and variables

In <https://github.com/kenvandine/umoja/settings/secrets/actions>:

| Secret | Value |
|---|---|
| `CLOUDFLARE_ACCOUNT_ID` | the Account ID |
| `CLOUDFLARE_API_TOKEN` | the Worker deploy token |
| `R2_ACCESS_KEY_ID` | the R2 Access Key ID |
| `R2_SECRET_ACCESS_KEY` | the R2 Secret Access Key |

### 4. Deploy the Worker

1. In the dashboard open **Workers & Pages** once; if asked, pick your
   `workers.dev` subdomain (e.g. `kenvandine.workers.dev`).
2. Enable **Analytics Engine** (free) at
   <https://dash.cloudflare.com/?to=/:account/workers/analytics-engine>;
   without it the deploy fails with code 10089.
3. Run the **Downloads Worker** workflow (Actions tab, Run workflow, main).
   It deploys to `https://umoja-downloads.<subdomain>.workers.dev`; the log
   prints the URL.
4. Add the repository **variable** (Variables tab, same settings page)
   `DOWNLOAD_URL` = that URL, so release notes link to it.

The workflow redeploys by itself whenever `infra/downloads/` changes on main.

### 5. Custom domain (optional)

With a domain on Cloudflare (DNS managed there), uncomment `routes` in
`wrangler.toml` with e.g. `download.example.org`, push, and set
`DOWNLOAD_URL` to `https://download.example.org`. Cloudflare creates the DNS
record and certificate.

## Download counts

```sh
export CLOUDFLARE_ACCOUNT_ID=... CLOUDFLARE_STATS_TOKEN=...
infra/downloads/stats            # per release, last 90 days
infra/downloads/stats 30 day     # per day, last 30 days
infra/downloads/stats 90 country # or: client
```

Analytics Engine keeps data for three months. The Worker's page in the
dashboard (Workers & Pages › umoja-downloads › Metrics) also shows request
counts and bandwidth.

## Sending events to your own analytics (optional)

The Worker can also send each landing-page view and ISO download, server
side, to a Plausible, Umami or Matomo server (e.g. one on PikaPods). No
cookies or scripts reach the visitor. In the dashboard open Workers & Pages
› umoja-downloads › Settings › Variables and secrets and add:

| Variable | Value |
|---|---|
| `ANALYTICS` | `plausible`, `umami` or `matomo` |
| `ANALYTICS_URL` | the server, e.g. `https://my-stats.pikapod.net` |
| `ANALYTICS_SITE` | Plausible: the site's domain as added there; Umami: the website ID; Matomo: the site ID |
| `ANALYTICS_TOKEN` | Matomo only, optional, as a *secret*: a `token_auth`, so Matomo sees the visitor's IP (country) instead of Cloudflare's |

Downloads arrive as a custom event named `Download` (Plausible: add a goal
called `Download` to see it; props `tag`, `file`, `client`), a Umami event
with the same data, or a Matomo download. Deploys keep these variables
(`keep_vars`). Plausible and Umami drop requests from bot-like user agents
such as curl and wget, so their counts are browser downloads only; the
Analytics Engine count above has all of them.

## Local testing

```sh
cd infra/downloads
npx wrangler r2 object put umoja-releases/latest.json --file latest.json --local
npx wrangler dev --local
```
